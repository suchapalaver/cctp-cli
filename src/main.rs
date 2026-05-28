use std::time::Duration;

use alloy::{
    primitives::{Address, TxHash, U256, address},
    providers::{Provider, ProviderBuilder},
    signers::trezor::{HDPath, TrezorSigner},
};
use alloy_chains::NamedChain;
use cctp_rs::{CctpV2Bridge, MintResult, PollingConfig, TransferMode};
use clap::{Args, Parser, Subcommand, ValueEnum};
use eyre::{Result, WrapErr, bail, eyre};
use tokio::time::sleep;
use tracing_subscriber::EnvFilter;

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
    validate_route(args.from, args.to)?;
    validate_polling(args.receive_attempts, args.receive_interval_secs)?;

    let source_chain = args.from.named_chain();
    let destination_chain = args.to.named_chain();
    let amount = parse_usdc_amount(&args.amount)?;
    let transfer_mode = transfer_mode(&args)?;
    let usdc = args.usdc.unwrap_or(MAINNET_USDC);

    let source_rpc = args
        .ethereum_rpc
        .parse()
        .wrap_err("failed to parse --ethereum-rpc as a URL")?;
    let destination_rpc = args
        .hyperevm_rpc
        .parse()
        .wrap_err("failed to parse --hyperevm-rpc as a URL")?;

    let wallet = WalletConfig::from_args(&args);
    let source_signer = wallet
        .trezor_signer(u64::from(source_chain))
        .await
        .wrap_err("failed to initialize Trezor signer for Ethereum mainnet")?;
    let signer_address = source_signer
        .get_address()
        .await
        .wrap_err("failed to read Ethereum address from Trezor")?;

    let destination_signer = wallet
        .trezor_signer(u64::from(destination_chain))
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

    let recipient = args.recipient.unwrap_or(signer_address);

    println!("Route: Ethereum mainnet -> HyperEVM");
    println!("Wallet: {} account {}", args.wallet, args.trezor_account);
    println!("Signer: {signer_address}");
    println!("Recipient: {recipient}");
    println!("USDC: {usdc}");
    println!("Amount: {} USDC", args.amount);
    println!("Mode: {}", mode_label(&transfer_mode));
    if args.self_relay {
        println!("Relay: self-relay on HyperEVM");
    } else {
        println!("Relay: wait for any permissionless relayer");
    }

    if args.dry_run {
        println!("Dry run complete. No transactions sent.");
        return Ok(());
    }

    let source_provider = ProviderBuilder::new()
        .wallet(source_signer)
        .connect_http(source_rpc);
    let destination_provider = ProviderBuilder::new()
        .wallet(destination_signer)
        .connect_http(destination_rpc);

    let bridge = CctpV2Bridge::builder()
        .source_chain(source_chain)
        .destination_chain(destination_chain)
        .source_provider(source_provider.clone())
        .destination_provider(destination_provider.clone())
        .recipient(recipient)
        .transfer_mode(transfer_mode.clone())
        .build();

    println!(
        "TokenMessengerV2: {}",
        bridge.token_messenger_v2_contract()?
    );
    println!("Destination domain: {}", bridge.destination_domain_id()?);

    let allowance = bridge
        .get_allowance(usdc, signer_address)
        .await
        .wrap_err("failed to read USDC allowance")?;

    if allowance < amount {
        println!("Approving TokenMessengerV2 to spend {amount} atomic USDC units.");
        let approval_tx = bridge
            .approve(usdc, signer_address, amount)
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

    println!("Burning {amount} atomic USDC units on Ethereum mainnet.");
    let burn_tx = bridge
        .burn(amount, signer_address, usdc)
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

    let polling_config = if transfer_mode.is_fast() {
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

    if args.self_relay {
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
            .wait_for_receive(&message, args.receive_attempts, args.receive_interval_secs)
            .await
            .wrap_err("timed out waiting for HyperEVM receive status")?;
    }

    println!("Transfer complete.");
    Ok(())
}

fn validate_route(from: ChainArg, to: ChainArg) -> Result<()> {
    if from != ChainArg::Ethereum || to != ChainArg::HyperEvm {
        bail!("only --from ethereum --to hyperevm is supported in this first CLI version");
    }
    Ok(())
}

fn validate_polling(
    receive_attempts: Option<u32>,
    receive_interval_secs: Option<u64>,
) -> Result<()> {
    if matches!(receive_attempts, Some(0)) {
        bail!("--receive-attempts must be greater than 0");
    }
    if matches!(receive_interval_secs, Some(0)) {
        bail!("--receive-interval-secs must be greater than 0");
    }
    Ok(())
}

fn transfer_mode(args: &BridgeArgs) -> Result<TransferMode> {
    if !args.fast {
        if args.max_fee_usdc.is_some() {
            bail!("--max-fee-usdc is only valid with --fast");
        }
        return Ok(TransferMode::Standard);
    }

    let max_fee = args
        .max_fee_usdc
        .as_deref()
        .ok_or_else(|| eyre!("--fast requires --max-fee-usdc"))?;

    Ok(TransferMode::Fast {
        max_fee: parse_usdc_amount(max_fee)?,
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
    const fn from_args(args: &BridgeArgs) -> Self {
        match args.wallet {
            WalletKind::Trezor => Self::Trezor {
                account: args.trezor_account,
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

fn parse_usdc_amount(input: &str) -> Result<U256> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("amount must not be empty");
    }
    if trimmed.starts_with('-') || trimmed.starts_with('+') {
        bail!("amount must be unsigned");
    }

    let mut parts = trimmed.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if parts.next().is_some() {
        bail!("amount must contain at most one decimal point");
    }

    if whole.is_empty() && fraction.is_none_or(str::is_empty) {
        bail!("amount must include digits");
    }
    if !whole.chars().all(|c| c.is_ascii_digit()) {
        bail!("amount whole-number part must contain only digits");
    }

    let whole_units = if whole.is_empty() {
        0
    } else {
        whole
            .parse::<u128>()
            .wrap_err("amount whole-number part is too large")?
    };

    let fractional_units = match fraction {
        Some(value) => parse_usdc_fraction(value)?,
        None => 0,
    };

    let atomic_units = whole_units
        .checked_mul(1_000_000)
        .and_then(|value| value.checked_add(fractional_units))
        .ok_or_else(|| eyre!("amount is too large"))?;

    if atomic_units == 0 {
        bail!("amount must be greater than zero");
    }

    Ok(U256::from(atomic_units))
}

fn parse_usdc_fraction(input: &str) -> Result<u128> {
    if input.len() > 6 {
        bail!("USDC amounts support at most 6 decimal places");
    }
    if !input.chars().all(|c| c.is_ascii_digit()) {
        bail!("amount fractional part must contain only digits");
    }

    let mut padded = input.to_owned();
    while padded.len() < 6 {
        padded.push('0');
    }

    if padded.is_empty() {
        return Ok(0);
    }

    padded
        .parse::<u128>()
        .wrap_err("amount fractional part is too large")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_usdc_amounts() {
        assert_eq!(
            parse_usdc_amount("1").expect("valid amount"),
            U256::from(1_000_000u64)
        );
        assert_eq!(
            parse_usdc_amount("1.25").expect("valid amount"),
            U256::from(1_250_000u64)
        );
        assert_eq!(
            parse_usdc_amount(".5").expect("valid amount"),
            U256::from(500_000u64)
        );
        assert_eq!(
            parse_usdc_amount("0.000001").expect("valid amount"),
            U256::from(1u64)
        );
    }

    #[test]
    fn rejects_invalid_usdc_amounts() {
        for amount in ["", "0", "-1", "+1", "1.0000001", "1.2.3", "abc", "1.a"] {
            assert!(parse_usdc_amount(amount).is_err(), "{amount} should fail");
        }
    }
}
