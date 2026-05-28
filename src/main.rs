use std::time::Duration;

use alloy::{
    primitives::{Address, TxHash, U256, address},
    providers::{DynProvider, Provider, ProviderBuilder},
    signers::trezor::{HDPath, TrezorSigner},
};
use alloy_chains::NamedChain;
use async_trait::async_trait;
use cctp_rs::{
    AttestationBytes, CctpV2Bridge, CctpV2Route, DomainId, MintResult, PollingConfig, TransferMode,
    UsdcAmount,
};
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

    /// Trezor Live account index used only for --self-relay on HyperEVM.
    ///
    /// Defaults to --trezor-account when omitted.
    #[arg(long)]
    relay_trezor_account: Option<u32>,

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
        .source_wallet
        .trezor_signer(config.route.source_chain_id())
        .await
        .wrap_err("failed to initialize Trezor signer for Ethereum mainnet")?;
    let source_signer_address = source_signer
        .get_address()
        .await
        .wrap_err("failed to read Ethereum address from Trezor")?;

    let relay_signer = relay_signer(&config).await?;
    let relay_signer_address = relay_signer.as_ref().map(|runtime| runtime.address);
    let recipient = config.recipient.resolve(source_signer_address);

    println!("Route: {}", config.route);
    println!("Source wallet: {}", config.source_wallet);
    println!("Source signer: {source_signer_address}");
    println!("Recipient: {recipient}");
    println!("USDC: {}", config.usdc);
    println!("Amount: {} USDC", config.amount);
    println!("Mode: {}", mode_label(&config.transfer_mode));
    println!("Relay: {}", config.relay);
    match relay_signer_address {
        Some(address) => println!("Relay signer: {address}"),
        None => println!("Destination provider: read-only"),
    }

    if config.dry_run {
        println!("Dry run complete. No transactions sent.");
        return Ok(());
    }

    let source_provider: DynProvider = ProviderBuilder::new()
        .wallet(source_signer)
        .connect_http(config.rpc.source.clone())
        .erased();
    let destination_provider = destination_provider(&config, relay_signer);

    let bridge = CctpV2Bridge::builder()
        .source_chain(config.route.source_chain())
        .destination_chain(config.route.destination_chain())
        .source_provider(source_provider.clone())
        .destination_provider(destination_provider.clone())
        .recipient(recipient)
        .transfer_mode(config.transfer_mode.clone())
        .build();

    println!("Starting bridge workflow.");
    let runtime = CctpBridgeRuntime::new(bridge, source_provider, destination_provider);
    let mut workflow = BridgeWorkflow::new(
        BridgeWorkflowConfig::from(&config),
        runtime,
        source_signer_address,
        recipient,
        relay_signer_address,
    );
    let outcome = workflow.run().await?;
    print_bridge_outcome(&outcome);
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
            source_wallet: WalletConfig::from_kind(args.wallet, args.trezor_account),
            relay_wallet: RelayWalletConfig::new(
                args.self_relay,
                args.wallet,
                args.relay_trezor_account,
                args.trezor_account,
            ),
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
    source_wallet: WalletConfig,
    relay_wallet: RelayWalletConfig,
    recipient: RecipientConfig,
    usdc: Address,
    transfer_mode: TransferMode,
    relay: RelayMode,
    receive_polling: ReceivePolling,
    dry_run: bool,
}

#[derive(Clone, Debug)]
struct BridgeWorkflowConfig {
    amount: UsdcAmount,
    usdc: Address,
    transfer_mode: TransferMode,
    relay: RelayMode,
    receive_polling: ReceivePolling,
}

impl BridgeWorkflowConfig {
    fn attestation_polling_config(&self) -> PollingConfig {
        if self.transfer_mode.is_fast() {
            PollingConfig::fast_transfer()
        } else {
            PollingConfig::default()
        }
    }
}

impl From<&BridgeConfig> for BridgeWorkflowConfig {
    fn from(config: &BridgeConfig) -> Self {
        Self {
            amount: config.amount,
            usdc: config.usdc,
            transfer_mode: config.transfer_mode.clone(),
            relay: config.relay,
            receive_polling: config.receive_polling,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BridgeOutcome {
    source_sender: Address,
    recipient: Address,
    token_messenger: Address,
    destination_domain: DomainId,
    approval: ApprovalOutcome,
    burn_tx: TxHash,
    attestation: AttestationOutcome,
    completion: CompletionOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ApprovalOutcome {
    Skipped { allowance: U256 },
    Sent { tx_hash: TxHash },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AttestationOutcome {
    message_len: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CompletionOutcome {
    RelayerCompleted,
    SelfRelayMinted { tx_hash: TxHash },
    SelfRelayAlreadyCompleted,
}

struct BridgeWorkflow<R> {
    config: BridgeWorkflowConfig,
    runtime: R,
    source_sender: Address,
    recipient: Address,
    relay_submitter: Option<Address>,
}

impl<R> BridgeWorkflow<R>
where
    R: BridgeRuntime,
{
    const fn new(
        config: BridgeWorkflowConfig,
        runtime: R,
        source_sender: Address,
        recipient: Address,
        relay_submitter: Option<Address>,
    ) -> Self {
        Self {
            config,
            runtime,
            source_sender,
            recipient,
            relay_submitter,
        }
    }

    async fn run(&mut self) -> Result<BridgeOutcome> {
        if self.config.relay == RelayMode::SelfRelay && self.relay_submitter.is_none() {
            bail!("self-relay workflow requires a destination relay submitter");
        }

        let token_messenger = self.runtime.token_messenger_v2_contract()?;
        let destination_domain = self.runtime.destination_domain_id()?;
        let amount = self.config.amount.atomic();

        let allowance = self
            .runtime
            .get_allowance(self.config.usdc, self.source_sender)
            .await
            .wrap_err("failed to read USDC allowance")?;

        let approval = if allowance < amount {
            let tx_hash = self
                .runtime
                .approve(self.config.usdc, self.source_sender, amount)
                .await
                .wrap_err("failed to send USDC approval transaction")?;
            self.runtime
                .wait_source_receipt(tx_hash, "approval", 120, Duration::from_secs(12))
                .await?;
            ApprovalOutcome::Sent { tx_hash }
        } else {
            ApprovalOutcome::Skipped { allowance }
        };

        let burn_tx = self
            .runtime
            .burn(amount, self.source_sender, self.config.usdc)
            .await
            .wrap_err("failed to send CCTP burn transaction")?;
        self.runtime
            .wait_source_receipt(burn_tx, "burn", 120, Duration::from_secs(12))
            .await?;

        let (message, attestation) = self
            .runtime
            .get_attestation(burn_tx, self.config.attestation_polling_config())
            .await
            .wrap_err("failed to get CCTP attestation from Iris")?;
        let attestation_outcome = AttestationOutcome {
            message_len: message.len(),
        };

        let completion = match self.config.relay {
            RelayMode::WaitForRelayer => {
                self.runtime
                    .wait_for_receive(
                        &message,
                        self.config.receive_polling.attempts,
                        self.config.receive_polling.interval_secs,
                    )
                    .await
                    .wrap_err("timed out waiting for HyperEVM receive status")?;
                CompletionOutcome::RelayerCompleted
            }
            RelayMode::SelfRelay => {
                let relay_submitter = self
                    .relay_submitter
                    .ok_or_else(|| eyre!("self-relay workflow requires a relay submitter"))?;
                match self
                    .runtime
                    .mint_if_needed(message, attestation, relay_submitter)
                    .await
                    .wrap_err("failed to self-relay CCTP mint on HyperEVM")?
                {
                    MintResult::Minted(tx_hash) => {
                        self.runtime
                            .wait_destination_receipt(tx_hash, "mint", 120, Duration::from_secs(2))
                            .await?;
                        CompletionOutcome::SelfRelayMinted { tx_hash }
                    }
                    MintResult::AlreadyRelayed => CompletionOutcome::SelfRelayAlreadyCompleted,
                }
            }
        };

        Ok(BridgeOutcome {
            source_sender: self.source_sender,
            recipient: self.recipient,
            token_messenger,
            destination_domain,
            approval,
            burn_tx,
            attestation: attestation_outcome,
            completion,
        })
    }
}

#[async_trait(?Send)]
trait BridgeRuntime {
    fn token_messenger_v2_contract(&self) -> Result<Address>;

    fn destination_domain_id(&self) -> Result<DomainId>;

    async fn get_allowance(&mut self, token: Address, owner: Address) -> Result<U256>;

    async fn approve(&mut self, token: Address, owner: Address, amount: U256) -> Result<TxHash>;

    async fn burn(&mut self, amount: U256, burn_sender: Address, token: Address) -> Result<TxHash>;

    async fn get_attestation(
        &mut self,
        burn_tx: TxHash,
        polling_config: PollingConfig,
    ) -> Result<(Vec<u8>, AttestationBytes)>;

    async fn wait_for_receive(
        &mut self,
        message: &[u8],
        max_attempts: Option<u32>,
        poll_interval: Option<u64>,
    ) -> Result<()>;

    async fn mint_if_needed(
        &mut self,
        message: Vec<u8>,
        attestation: AttestationBytes,
        from: Address,
    ) -> Result<MintResult>;

    async fn wait_source_receipt(
        &mut self,
        tx_hash: TxHash,
        label: &str,
        max_attempts: u32,
        interval: Duration,
    ) -> Result<()>;

    async fn wait_destination_receipt(
        &mut self,
        tx_hash: TxHash,
        label: &str,
        max_attempts: u32,
        interval: Duration,
    ) -> Result<()>;
}

struct CctpBridgeRuntime<P>
where
    P: Provider + Clone,
{
    bridge: CctpV2Bridge<P>,
    source_provider: P,
    destination_provider: P,
}

impl<P> CctpBridgeRuntime<P>
where
    P: Provider + Clone,
{
    const fn new(bridge: CctpV2Bridge<P>, source_provider: P, destination_provider: P) -> Self {
        Self {
            bridge,
            source_provider,
            destination_provider,
        }
    }
}

#[async_trait(?Send)]
impl<P> BridgeRuntime for CctpBridgeRuntime<P>
where
    P: Provider + Clone,
{
    fn token_messenger_v2_contract(&self) -> Result<Address> {
        Ok(self.bridge.token_messenger_v2_contract()?)
    }

    fn destination_domain_id(&self) -> Result<DomainId> {
        Ok(self.bridge.destination_domain_id()?)
    }

    async fn get_allowance(&mut self, token: Address, owner: Address) -> Result<U256> {
        Ok(self.bridge.get_allowance(token, owner).await?)
    }

    async fn approve(&mut self, token: Address, owner: Address, amount: U256) -> Result<TxHash> {
        Ok(self.bridge.approve(token, owner, amount).await?)
    }

    async fn burn(&mut self, amount: U256, burn_sender: Address, token: Address) -> Result<TxHash> {
        Ok(self.bridge.burn(amount, burn_sender, token).await?)
    }

    async fn get_attestation(
        &mut self,
        burn_tx: TxHash,
        polling_config: PollingConfig,
    ) -> Result<(Vec<u8>, AttestationBytes)> {
        Ok(self.bridge.get_attestation(burn_tx, polling_config).await?)
    }

    async fn wait_for_receive(
        &mut self,
        message: &[u8],
        max_attempts: Option<u32>,
        poll_interval: Option<u64>,
    ) -> Result<()> {
        Ok(self
            .bridge
            .wait_for_receive(message, max_attempts, poll_interval)
            .await?)
    }

    async fn mint_if_needed(
        &mut self,
        message: Vec<u8>,
        attestation: AttestationBytes,
        from: Address,
    ) -> Result<MintResult> {
        Ok(self
            .bridge
            .mint_if_needed(message, attestation, from)
            .await?)
    }

    async fn wait_source_receipt(
        &mut self,
        tx_hash: TxHash,
        label: &str,
        max_attempts: u32,
        interval: Duration,
    ) -> Result<()> {
        wait_for_receipt(
            &self.source_provider,
            tx_hash,
            label,
            max_attempts,
            interval,
        )
        .await
    }

    async fn wait_destination_receipt(
        &mut self,
        tx_hash: TxHash,
        label: &str,
        max_attempts: u32,
        interval: Duration,
    ) -> Result<()> {
        wait_for_receipt(
            &self.destination_provider,
            tx_hash,
            label,
            max_attempts,
            interval,
        )
        .await
    }
}

fn print_bridge_outcome(outcome: &BridgeOutcome) {
    println!("Source sender: {}", outcome.source_sender);
    println!("Recipient: {}", outcome.recipient);
    println!("TokenMessengerV2: {}", outcome.token_messenger);
    println!("Destination domain: {}", outcome.destination_domain);
    match outcome.approval {
        ApprovalOutcome::Skipped { allowance } => {
            println!("Existing USDC allowance is sufficient: {allowance} atomic units.");
        }
        ApprovalOutcome::Sent { tx_hash } => {
            println!("Approval tx: {tx_hash}");
        }
    }
    println!("Burn tx: {}", outcome.burn_tx);
    println!(
        "Attestation ready. Canonical message bytes: {}",
        outcome.attestation.message_len
    );
    match outcome.completion {
        CompletionOutcome::RelayerCompleted => {
            println!("Transfer completed by a permissionless relayer.");
        }
        CompletionOutcome::SelfRelayMinted { tx_hash } => {
            println!("Mint tx: {tx_hash}");
        }
        CompletionOutcome::SelfRelayAlreadyCompleted => {
            println!("Transfer was already completed by a relayer.");
        }
    }
    println!("Transfer complete.");
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelayWalletConfig {
    None,
    Trezor { account: u32 },
}

impl RelayWalletConfig {
    const fn new(
        self_relay: bool,
        kind: WalletKind,
        relay_trezor_account: Option<u32>,
        source_trezor_account: u32,
    ) -> Self {
        if !self_relay {
            return Self::None;
        }

        match kind {
            WalletKind::Trezor => Self::Trezor {
                account: match relay_trezor_account {
                    Some(account) => account,
                    None => source_trezor_account,
                },
            },
        }
    }

    const fn wallet(self) -> Option<WalletConfig> {
        match self {
            Self::None => None,
            Self::Trezor { account } => Some(WalletConfig::Trezor { account }),
        }
    }
}

struct RelaySignerRuntime {
    signer: TrezorSigner,
    address: Address,
}

async fn relay_signer(config: &BridgeConfig) -> Result<Option<RelaySignerRuntime>> {
    let Some(wallet) = config.relay_wallet.wallet() else {
        return Ok(None);
    };

    let signer = wallet
        .trezor_signer(config.route.destination_chain_id())
        .await
        .wrap_err("failed to initialize Trezor signer for HyperEVM self-relay")?;
    let address = signer
        .get_address()
        .await
        .wrap_err("failed to read HyperEVM relay address from Trezor")?;

    Ok(Some(RelaySignerRuntime { signer, address }))
}

fn destination_provider(
    config: &BridgeConfig,
    relay_signer: Option<RelaySignerRuntime>,
) -> DynProvider {
    match relay_signer {
        Some(runtime) => ProviderBuilder::new()
            .wallet(runtime.signer)
            .connect_http(config.rpc.destination.clone())
            .erased(),
        None => ProviderBuilder::new()
            .connect_http(config.rpc.destination.clone())
            .erased(),
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
            relay_trezor_account: None,
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
        assert_eq!(config.source_wallet, WalletConfig::Trezor { account: 0 });
        assert_eq!(config.relay_wallet, RelayWalletConfig::None);
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

    #[test]
    fn config_service_uses_source_wallet_for_default_self_relay_account() {
        let mut args = sample_args();
        args.self_relay = true;

        let config = CliConfigService.bridge_config(args).expect("valid config");
        assert_eq!(config.relay, RelayMode::SelfRelay);
        assert_eq!(
            config.relay_wallet,
            RelayWalletConfig::Trezor { account: 0 }
        );
    }

    #[test]
    fn config_service_accepts_distinct_self_relay_account() {
        let mut args = sample_args();
        args.self_relay = true;
        args.relay_trezor_account = Some(2);

        let config = CliConfigService.bridge_config(args).expect("valid config");
        assert_eq!(config.source_wallet, WalletConfig::Trezor { account: 0 });
        assert_eq!(
            config.relay_wallet,
            RelayWalletConfig::Trezor { account: 2 }
        );
    }

    #[test]
    fn config_service_ignores_relay_account_without_self_relay() {
        let mut args = sample_args();
        args.relay_trezor_account = Some(2);

        let config = CliConfigService.bridge_config(args).expect("valid config");
        assert_eq!(config.relay, RelayMode::WaitForRelayer);
        assert_eq!(config.relay_wallet, RelayWalletConfig::None);
    }

    #[tokio::test]
    async fn workflow_waits_for_relayer_without_destination_submitter() {
        let allowance = U256::from(2_000_000u64);
        let runtime = MockBridgeRuntime {
            allowance,
            ..Default::default()
        };
        let mut workflow = mock_workflow(RelayMode::WaitForRelayer, None, runtime);

        let outcome = workflow.run().await.expect("workflow succeeds");

        assert_eq!(outcome.approval, ApprovalOutcome::Skipped { allowance });
        assert_eq!(outcome.burn_tx, tx_hash(0x22));
        assert_eq!(
            outcome.attestation,
            AttestationOutcome {
                message_len: MOCK_MESSAGE.len()
            }
        );
        assert_eq!(outcome.completion, CompletionOutcome::RelayerCompleted);
        assert_eq!(
            workflow.runtime.calls,
            vec![
                "get_allowance",
                "burn",
                "wait_source_receipt",
                "get_attestation",
                "wait_for_receive"
            ]
        );
        assert_eq!(workflow.runtime.last_mint_from, None);
    }

    #[tokio::test]
    async fn workflow_self_relays_with_distinct_relay_submitter() {
        let relay_submitter = address!("0000000000000000000000000000000000000003");
        let runtime = MockBridgeRuntime {
            allowance: U256::ZERO,
            mint_result: MintResult::Minted(tx_hash(0x33)),
            ..Default::default()
        };
        let mut workflow = mock_workflow(RelayMode::SelfRelay, Some(relay_submitter), runtime);

        let outcome = workflow.run().await.expect("workflow succeeds");

        assert_eq!(
            outcome.approval,
            ApprovalOutcome::Sent {
                tx_hash: tx_hash(0x11)
            }
        );
        assert_eq!(
            outcome.completion,
            CompletionOutcome::SelfRelayMinted {
                tx_hash: tx_hash(0x33)
            }
        );
        assert_eq!(
            workflow.runtime.calls,
            vec![
                "get_allowance",
                "approve",
                "wait_source_receipt",
                "burn",
                "wait_source_receipt",
                "get_attestation",
                "mint_if_needed",
                "wait_destination_receipt"
            ]
        );
        assert_eq!(workflow.runtime.last_mint_from, Some(relay_submitter));
    }

    #[tokio::test]
    async fn workflow_rejects_self_relay_without_relay_submitter_before_side_effects() {
        let mut workflow = mock_workflow(RelayMode::SelfRelay, None, MockBridgeRuntime::default());

        let error = workflow
            .run()
            .await
            .expect_err("workflow rejects missing relay");

        assert!(
            error.to_string().contains("destination relay submitter"),
            "unexpected error: {error}"
        );
        assert!(workflow.runtime.calls.is_empty());
    }

    const MOCK_MESSAGE: &[u8] = &[0xaa, 0xbb, 0xcc];

    fn source_sender() -> Address {
        address!("0000000000000000000000000000000000000001")
    }

    fn recipient() -> Address {
        address!("0000000000000000000000000000000000000002")
    }

    fn tx_hash(byte: u8) -> TxHash {
        TxHash::from([byte; 32])
    }

    fn mock_workflow(
        relay: RelayMode,
        relay_submitter: Option<Address>,
        runtime: MockBridgeRuntime,
    ) -> BridgeWorkflow<MockBridgeRuntime> {
        BridgeWorkflow::new(
            BridgeWorkflowConfig {
                amount: UsdcAmount::from_atomic(U256::from(1_000_000u64)),
                usdc: MAINNET_USDC,
                transfer_mode: TransferMode::Standard,
                relay,
                receive_polling: ReceivePolling {
                    attempts: Some(1),
                    interval_secs: Some(1),
                },
            },
            runtime,
            source_sender(),
            recipient(),
            relay_submitter,
        )
    }

    struct MockBridgeRuntime {
        allowance: U256,
        approve_tx: TxHash,
        burn_tx: TxHash,
        message: Vec<u8>,
        attestation: AttestationBytes,
        mint_result: MintResult,
        calls: Vec<&'static str>,
        last_mint_from: Option<Address>,
    }

    impl Default for MockBridgeRuntime {
        fn default() -> Self {
            Self {
                allowance: U256::MAX,
                approve_tx: tx_hash(0x11),
                burn_tx: tx_hash(0x22),
                message: MOCK_MESSAGE.to_vec(),
                attestation: vec![0xdd],
                mint_result: MintResult::AlreadyRelayed,
                calls: Vec::new(),
                last_mint_from: None,
            }
        }
    }

    #[async_trait(?Send)]
    impl BridgeRuntime for MockBridgeRuntime {
        fn token_messenger_v2_contract(&self) -> Result<Address> {
            Ok(address!("0000000000000000000000000000000000000010"))
        }

        fn destination_domain_id(&self) -> Result<DomainId> {
            Ok(DomainId::HyperEvm)
        }

        async fn get_allowance(&mut self, _token: Address, _owner: Address) -> Result<U256> {
            self.calls.push("get_allowance");
            Ok(self.allowance)
        }

        async fn approve(
            &mut self,
            _token: Address,
            _owner: Address,
            _amount: U256,
        ) -> Result<TxHash> {
            self.calls.push("approve");
            Ok(self.approve_tx)
        }

        async fn burn(
            &mut self,
            _amount: U256,
            _burn_sender: Address,
            _token: Address,
        ) -> Result<TxHash> {
            self.calls.push("burn");
            Ok(self.burn_tx)
        }

        async fn get_attestation(
            &mut self,
            _burn_tx: TxHash,
            _polling_config: PollingConfig,
        ) -> Result<(Vec<u8>, AttestationBytes)> {
            self.calls.push("get_attestation");
            Ok((self.message.clone(), self.attestation.clone()))
        }

        async fn wait_for_receive(
            &mut self,
            _message: &[u8],
            _max_attempts: Option<u32>,
            _poll_interval: Option<u64>,
        ) -> Result<()> {
            self.calls.push("wait_for_receive");
            Ok(())
        }

        async fn mint_if_needed(
            &mut self,
            _message: Vec<u8>,
            _attestation: AttestationBytes,
            from: Address,
        ) -> Result<MintResult> {
            self.calls.push("mint_if_needed");
            self.last_mint_from = Some(from);
            Ok(self.mint_result.clone())
        }

        async fn wait_source_receipt(
            &mut self,
            _tx_hash: TxHash,
            _label: &str,
            _max_attempts: u32,
            _interval: Duration,
        ) -> Result<()> {
            self.calls.push("wait_source_receipt");
            Ok(())
        }

        async fn wait_destination_receipt(
            &mut self,
            _tx_hash: TxHash,
            _label: &str,
            _max_attempts: u32,
            _interval: Duration,
        ) -> Result<()> {
            self.calls.push("wait_destination_receipt");
            Ok(())
        }
    }
}
