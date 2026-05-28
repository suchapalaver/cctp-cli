use std::time::Duration;

use alloy::{
    primitives::{Address, TxHash, address},
    providers::{Provider, ProviderBuilder},
    signers::trezor::{HDPath, TrezorSigner},
};
use alloy_chains::NamedChain;
use cctp_rs::{CctpV2Bridge, CctpV2Route, MintResult, PollingConfig, TransferMode, UsdcAmount};
use clap::{Args, Parser, Subcommand, ValueEnum};
use eyre::{Result, WrapErr, bail, eyre};
use tokio::time::sleep;
use tracing_subscriber::EnvFilter;
use url::Url;

const MAINNET_USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const DEFAULT_LOG_FILTER: &str = "info,cctp_rs=info";

#[derive(Debug, Parser)]
#[command(name = "cctp")]
#[command(about = "Bridge USDC with cctp-rs and a Trezor-backed Alloy signer")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Bridge USDC from Ethereum mainnet to HyperEVM.
    Bridge(BridgeArgs),
}

#[derive(Debug, Args)]
struct BridgeArgs {
    /// Source chain. The first implementation supports ethereum only.
    #[arg(long, default_value_t = ChainArg::Ethereum)]
    from: ChainArg,

    /// Destination chain. The first implementation supports hyperevm only.
    #[arg(long, default_value_t = ChainArg::HyperEvm)]
    to: ChainArg,

    /// USDC amount, in decimal units, e.g. 10 or 10.25.
    #[arg(long)]
    amount: String,

    /// Destination recipient. Defaults to the Trezor account address.
    #[arg(long)]
    recipient: Option<Address>,

    /// Ethereum mainnet RPC URL.
    #[arg(long, env = "ETHEREUM_RPC_URL")]
    ethereum_rpc: String,

    /// HyperEVM RPC URL.
    #[arg(long, env = "HYPEREVM_RPC_URL")]
    hyperevm_rpc: String,

    /// Wallet backend.
    #[arg(long, value_enum, default_value_t = WalletKind::Trezor)]
    wallet: WalletKind,

    /// Trezor Live account index: m/44'/60'/account'/0/0.
    #[arg(long, default_value_t = 0)]
    trezor_account: u32,

    /// Override the Ethereum mainnet USDC address.
    #[arg(long)]
    usdc: Option<Address>,

    /// Request fast CCTP v2 finality.
    #[arg(long)]
    fast: bool,

    /// Fast-transfer fee cap in USDC decimal units. Required with --fast.
    #[arg(long)]
    max_fee_usdc: Option<String>,

    /// Submit receiveMessage from the Trezor account on HyperEVM.
    ///
    /// Without this flag the CLI waits for any permissionless relayer to complete
    /// the mint, which avoids requiring HyperEVM gas in the Trezor account.
    #[arg(long)]
    self_relay: bool,

    /// Optional receive-status polling attempt override.
    #[arg(long)]
    receive_attempts: Option<u32>,

    /// Optional receive-status polling interval, in seconds.
    #[arg(long)]
    receive_interval_secs: Option<u64>,

    /// Print the route and signer details without sending transactions.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ChainArg {
    #[value(name = "ethereum")]
    Ethereum,
    #[value(name = "hyperevm", alias = "hyper-evm", alias = "hyperliquid")]
    HyperEvm,
}

impl std::fmt::Display for ChainArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ethereum => f.write_str("ethereum"),
            Self::HyperEvm => f.write_str("hyperevm"),
        }
    }
}

impl ChainArg {
    const fn named_chain(self) -> NamedChain {
        match self {
            Self::Ethereum => NamedChain::Mainnet,
            Self::HyperEvm => NamedChain::Hyperliquid,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum WalletKind {
    Trezor,
}

impl std::fmt::Display for WalletKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Trezor => f.write_str("trezor"),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let cli = Cli::parse();
    match cli.command {
        Commands::Bridge(args) => run_bridge(args).await,
    }
}

async fn run_bridge(args: BridgeArgs) -> Result<()> {
    let config = CliConfigService.bridge_config(args)?;

    let source_signer = config
        .wallet
        .trezor_signer(config.route.source_chain_id())
        .await
        .wrap_err("failed to initialize Trezor signer for Ethereum mainnet")?;
    let signer_address = source_signer
        .get_address()
        .await
        .wrap_err("failed to read Ethereum address from Trezor")?;

    let destination_signer = config
        .wallet
        .trezor_signer(config.route.destination_chain_id())
        .await
        .wrap_err("failed to initialize Trezor signer for HyperEVM")?;
    let destination_signer_address = destination_signer
        .get_address()
        .await
        .wrap_err("failed to read HyperEVM address from Trezor")?;

    if signer_address != destination_signer_address {
        bail!(
            "Trezor returned different source and destination addresses: {signer_address} vs {destination_signer_address}"
        );
    }

    let recipient = config.recipient.resolve(signer_address);

    println!("Route: {}", config.route);
    println!("Wallet: {}", config.wallet);
    println!("Signer: {signer_address}");
    println!("Recipient: {recipient}");
    println!("USDC: {}", config.usdc);
    println!("Amount: {} USDC", config.amount);
    println!("Mode: {}", mode_label(&config.transfer_mode));
    println!("Relay: {}", config.relay);

    if config.dry_run {
        println!("Dry run complete. No transactions sent.");
        return Ok(());
    }

    let source_provider = ProviderBuilder::new()
        .wallet(source_signer)
        .connect_http(config.rpc.source.clone());
    let destination_provider = ProviderBuilder::new()
        .wallet(destination_signer)
        .connect_http(config.rpc.destination.clone());

    let bridge = CctpV2Bridge::builder()
        .source_chain(config.route.source_chain())
        .destination_chain(config.route.destination_chain())
        .source_provider(source_provider.clone())
        .destination_provider(destination_provider.clone())
        .recipient(recipient)
        .transfer_mode(config.transfer_mode.clone())
        .build();

    println!(
        "TokenMessengerV2: {}",
        bridge.token_messenger_v2_contract()?
    );
    println!("Destination domain: {}", bridge.destination_domain_id()?);

    let allowance = bridge
        .get_allowance(config.usdc, signer_address)
        .await
        .wrap_err("failed to read USDC allowance")?;

    if allowance < config.amount.atomic() {
        println!(
            "Approving TokenMessengerV2 to spend {} atomic USDC units.",
            config.amount.atomic()
        );
        let approval_tx = bridge
            .approve(config.usdc, signer_address, config.amount.atomic())
            .await
            .wrap_err("failed to send USDC approval transaction")?;
        println!("Approval tx: {approval_tx}");
        wait_for_receipt(
            &source_provider,
            approval_tx,
            "approval",
            120,
            Duration::from_secs(12),
        )
        .await?;
    } else {
        println!("Existing USDC allowance is sufficient.");
    }

    println!(
        "Burning {} atomic USDC units on Ethereum mainnet.",
        config.amount.atomic()
    );
    let burn_tx = bridge
        .burn(config.amount.atomic(), signer_address, config.usdc)
        .await
        .wrap_err("failed to send CCTP burn transaction")?;
    println!("Burn tx: {burn_tx}");
    wait_for_receipt(
        &source_provider,
        burn_tx,
        "burn",
        120,
        Duration::from_secs(12),
    )
    .await?;

    let polling_config = if config.transfer_mode.is_fast() {
        PollingConfig::fast_transfer()
    } else {
        PollingConfig::default()
    };

    println!("Polling Circle Iris for the canonical v2 message and attestation.");
    let (message, attestation) = bridge
        .get_attestation(burn_tx, polling_config)
        .await
        .wrap_err("failed to get CCTP attestation from Iris")?;
    println!(
        "Attestation ready. Canonical message bytes: {}",
        message.len()
    );

    if config.relay == RelayMode::SelfRelay {
        println!("Submitting receiveMessage on HyperEVM.");
        match bridge
            .mint_if_needed(message, attestation, signer_address)
            .await
            .wrap_err("failed to self-relay CCTP mint on HyperEVM")?
        {
            MintResult::Minted(tx_hash) => {
                println!("Mint tx: {tx_hash}");
                wait_for_receipt(
                    &destination_provider,
                    tx_hash,
                    "mint",
                    120,
                    Duration::from_secs(2),
                )
                .await?;
            }
            MintResult::AlreadyRelayed => {
                println!("Transfer was already completed by a relayer.");
            }
        }
    } else {
        println!("Waiting for destination-chain receipt by any relayer.");
        bridge
            .wait_for_receive(
                &message,
                config.receive_polling.attempts,
                config.receive_polling.interval_secs,
            )
            .await
            .wrap_err("timed out waiting for HyperEVM receive status")?;
    }

    println!("Transfer complete.");
    Ok(())
}

trait ConfigService {
    fn bridge_config(&self, args: BridgeArgs) -> Result<BridgeConfig>;
}

#[derive(Clone, Copy, Debug, Default)]
struct CliConfigService;

impl ConfigService for CliConfigService {
    fn bridge_config(&self, args: BridgeArgs) -> Result<BridgeConfig> {
        let route = RouteConfig::new(args.from, args.to)?;
        let receive_polling =
            ReceivePolling::new(args.receive_attempts, args.receive_interval_secs)?;

        Ok(BridgeConfig {
            route,
            amount: UsdcAmount::parse_decimal(&args.amount)?,
            rpc: RpcEndpoints::parse(args.ethereum_rpc, args.hyperevm_rpc)?,
            wallet: WalletConfig::from_kind(args.wallet, args.trezor_account),
            recipient: RecipientConfig::from(args.recipient),
            usdc: args.usdc.unwrap_or(MAINNET_USDC),
            transfer_mode: transfer_mode(args.fast, args.max_fee_usdc.as_deref())?,
            relay: RelayMode::from_self_relay(args.self_relay),
            receive_polling,
            dry_run: args.dry_run,
        })
    }
}

#[derive(Clone, Debug)]
struct BridgeConfig {
    route: RouteConfig,
    amount: UsdcAmount,
    rpc: RpcEndpoints,
    wallet: WalletConfig,
    recipient: RecipientConfig,
    usdc: Address,
    transfer_mode: TransferMode,
    relay: RelayMode,
    receive_polling: ReceivePolling,
    dry_run: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RouteConfig {
    route: CctpV2Route,
    source_label: &'static str,
    destination_label: &'static str,
}

impl RouteConfig {
    fn new(from: ChainArg, to: ChainArg) -> Result<Self> {
        if from != ChainArg::Ethereum || to != ChainArg::HyperEvm {
            bail!("only --from ethereum --to hyperevm is supported in this first CLI version");
        }

        let source = from.named_chain();
        let destination = to.named_chain();

        Ok(Self {
            route: CctpV2Route::new(source, destination)?,
            source_label: chain_label(from),
            destination_label: chain_label(to),
        })
    }

    fn source_chain_id(&self) -> u64 {
        u64::from(self.route.source_chain())
    }

    fn destination_chain_id(&self) -> u64 {
        u64::from(self.route.destination_chain())
    }

    const fn source_chain(&self) -> NamedChain {
        self.route.source_chain()
    }

    const fn destination_chain(&self) -> NamedChain {
        self.route.destination_chain()
    }
}

impl std::fmt::Display for RouteConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} -> {}", self.source_label, self.destination_label)
    }
}

const fn chain_label(chain: ChainArg) -> &'static str {
    match chain {
        ChainArg::Ethereum => "Ethereum mainnet",
        ChainArg::HyperEvm => "HyperEVM",
    }
}

#[derive(Clone, Debug)]
struct RpcEndpoints {
    source: Url,
    destination: Url,
}

impl RpcEndpoints {
    fn parse(ethereum_rpc: String, hyperevm_rpc: String) -> Result<Self> {
        Ok(Self {
            source: ethereum_rpc
                .parse()
                .wrap_err("failed to parse --ethereum-rpc as a URL")?,
            destination: hyperevm_rpc
                .parse()
                .wrap_err("failed to parse --hyperevm-rpc as a URL")?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecipientConfig {
    Signer,
    Address(Address),
}

impl RecipientConfig {
    const fn resolve(self, signer_address: Address) -> Address {
        match self {
            Self::Signer => signer_address,
            Self::Address(address) => address,
        }
    }
}

impl From<Option<Address>> for RecipientConfig {
    fn from(value: Option<Address>) -> Self {
        match value {
            Some(address) => Self::Address(address),
            None => Self::Signer,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelayMode {
    WaitForRelayer,
    SelfRelay,
}

impl RelayMode {
    const fn from_self_relay(self_relay: bool) -> Self {
        if self_relay {
            Self::SelfRelay
        } else {
            Self::WaitForRelayer
        }
    }
}

impl std::fmt::Display for RelayMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WaitForRelayer => f.write_str("wait for any permissionless relayer"),
            Self::SelfRelay => f.write_str("self-relay on HyperEVM"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReceivePolling {
    attempts: Option<u32>,
    interval_secs: Option<u64>,
}

impl ReceivePolling {
    fn new(attempts: Option<u32>, interval_secs: Option<u64>) -> Result<Self> {
        if matches!(attempts, Some(0)) {
            bail!("--receive-attempts must be greater than 0");
        }
        if matches!(interval_secs, Some(0)) {
            bail!("--receive-interval-secs must be greater than 0");
        }

        Ok(Self {
            attempts,
            interval_secs,
        })
    }
}

fn transfer_mode(fast: bool, max_fee_usdc: Option<&str>) -> Result<TransferMode> {
    if !fast {
        if max_fee_usdc.is_some() {
            bail!("--max-fee-usdc is only valid with --fast");
        }
        return Ok(TransferMode::Standard);
    }

    let max_fee = max_fee_usdc.ok_or_else(|| eyre!("--fast requires --max-fee-usdc"))?;

    Ok(TransferMode::Fast {
        max_fee: UsdcAmount::parse_decimal(max_fee)?.atomic(),
    })
}

fn mode_label(mode: &TransferMode) -> &'static str {
    if mode.is_fast() { "fast" } else { "standard" }
}

#[derive(Clone, Copy, Debug)]
enum WalletConfig {
    Trezor { account: u32 },
}

impl WalletConfig {
    const fn from_kind(kind: WalletKind, trezor_account: u32) -> Self {
        match kind {
            WalletKind::Trezor => Self::Trezor {
                account: trezor_account,
            },
        }
    }

    async fn trezor_signer(self, chain_id: u64) -> Result<TrezorSigner> {
        match self {
            Self::Trezor { account } => {
                let account_index =
                    usize::try_from(account).wrap_err("Trezor account index is too large")?;
                TrezorSigner::new(HDPath::TrezorLive(account_index), Some(chain_id))
                    .await
                    .map_err(Into::into)
            }
        }
    }
}

impl std::fmt::Display for WalletConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Trezor { account } => write!(f, "trezor account {account}"),
        }
    }
}

async fn wait_for_receipt<P>(
    provider: &P,
    tx_hash: TxHash,
    label: &str,
    max_attempts: u32,
    interval: Duration,
) -> Result<()>
where
    P: Provider,
{
    for attempt in 1..=max_attempts {
        let receipt = provider
            .get_transaction_receipt(tx_hash)
            .await
            .wrap_err_with(|| format!("failed to poll {label} transaction receipt"))?;

        if receipt.is_some() {
            println!("{label} transaction confirmed.");
            return Ok(());
        }

        if attempt == 1 || attempt % 10 == 0 {
            println!("Waiting for {label} confirmation, attempt {attempt}/{max_attempts}.");
        }

        sleep(interval).await;
    }

    bail!("{label} transaction {tx_hash} was not confirmed before timeout")
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy::primitives::U256;

    fn sample_args() -> BridgeArgs {
        BridgeArgs {
            from: ChainArg::Ethereum,
            to: ChainArg::HyperEvm,
            amount: "1.25".to_owned(),
            recipient: None,
            ethereum_rpc: "https://ethereum.example".to_owned(),
            hyperevm_rpc: "https://hyperevm.example".to_owned(),
            wallet: WalletKind::Trezor,
            trezor_account: 0,
            usdc: None,
            fast: false,
            max_fee_usdc: None,
            self_relay: false,
            receive_attempts: None,
            receive_interval_secs: None,
            dry_run: false,
        }
    }

    #[test]
    fn config_service_builds_bridge_config() {
        let config = CliConfigService
            .bridge_config(sample_args())
            .expect("valid config");

        assert_eq!(config.route.source_chain(), NamedChain::Mainnet);
        assert_eq!(config.route.destination_chain(), NamedChain::Hyperliquid);
        assert_eq!(config.amount.atomic(), U256::from(1_250_000u64));
        assert_eq!(config.recipient, RecipientConfig::Signer);
        assert_eq!(config.relay, RelayMode::WaitForRelayer);
        assert_eq!(config.rpc.source.as_str(), "https://ethereum.example/");
        assert_eq!(config.rpc.destination.as_str(), "https://hyperevm.example/");
        assert!(matches!(config.transfer_mode, TransferMode::Standard));
    }

    #[test]
    fn config_service_rejects_unsupported_route() {
        let mut args = sample_args();
        args.to = ChainArg::Ethereum;

        assert!(CliConfigService.bridge_config(args).is_err());
    }

    #[test]
    fn config_service_requires_fast_fee_for_fast_mode() {
        let mut args = sample_args();
        args.fast = true;

        assert!(CliConfigService.bridge_config(args).is_err());
    }

    #[test]
    fn config_service_parses_fast_fee() {
        let mut args = sample_args();
        args.fast = true;
        args.max_fee_usdc = Some("0.01".to_owned());

        let config = CliConfigService.bridge_config(args).expect("valid config");
        assert_eq!(
            config.transfer_mode,
            TransferMode::Fast {
                max_fee: U256::from(10_000u64)
            }
        );
    }
}
