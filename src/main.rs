use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

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
use serde::Deserialize;
use tokio::time::sleep;
use tracing_subscriber::EnvFilter;
use url::Url;

const MAINNET_USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const DEFAULT_LOG_FILTER: &str = "info,cctp_rs=info";
const ETHEREUM_RPC_ENV: &str = "ETHEREUM_RPC_URL";
const HYPEREVM_RPC_ENV: &str = "HYPEREVM_RPC_URL";

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
    /// Optional TOML config file. CLI flags override env, env overrides file.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Source chain. The first implementation supports ethereum only.
    #[arg(long)]
    from: Option<ChainArg>,

    /// Destination chain. The first implementation supports hyperevm only.
    #[arg(long)]
    to: Option<ChainArg>,

    /// USDC amount, in decimal units, e.g. 10 or 10.25.
    #[arg(long)]
    amount: Option<String>,

    /// Destination recipient. Defaults to the Trezor account address.
    #[arg(long)]
    recipient: Option<Address>,

    /// Ethereum mainnet RPC URL.
    #[arg(long)]
    ethereum_rpc: Option<String>,

    /// HyperEVM RPC URL.
    #[arg(long)]
    hyperevm_rpc: Option<String>,

    /// Wallet backend.
    #[arg(long, value_enum)]
    wallet: Option<WalletKind>,

    /// Trezor Live account index: m/44'/60'/account'/0/0.
    #[arg(long)]
    trezor_account: Option<u32>,

    /// Trezor Live account index used only for --self-relay on HyperEVM.
    ///
    /// Defaults to --trezor-account when omitted.
    #[arg(long)]
    relay_trezor_account: Option<u32>,

    /// Override the Ethereum mainnet USDC address.
    #[arg(long)]
    usdc: Option<Address>,

    /// Request fast CCTP v2 finality.
    #[arg(
        long,
        default_missing_value = "true",
        num_args = 0..=1,
        require_equals = true,
        value_parser = clap::value_parser!(bool),
    )]
    fast: Option<bool>,

    /// Fast-transfer fee cap in USDC decimal units. Required with --fast.
    #[arg(long)]
    max_fee_usdc: Option<String>,

    /// Submit receiveMessage from the Trezor account on HyperEVM.
    ///
    /// Without this flag the CLI waits for any permissionless relayer to complete
    /// the mint, which avoids requiring HyperEVM gas in the Trezor account.
    #[arg(
        long,
        default_missing_value = "true",
        num_args = 0..=1,
        require_equals = true,
        value_parser = clap::value_parser!(bool),
    )]
    self_relay: Option<bool>,

    /// Optional receive-status polling attempt override.
    #[arg(long)]
    receive_attempts: Option<u32>,

    /// Optional receive-status polling interval, in seconds.
    #[arg(long)]
    receive_interval_secs: Option<u64>,

    /// Print the bridge intent without sending transactions.
    #[arg(
        long,
        default_missing_value = "true",
        num_args = 0..=1,
        require_equals = true,
        value_parser = clap::value_parser!(bool),
    )]
    dry_run: Option<bool>,

    /// Print the bridge intent but skip the interactive CONFIRM prompt.
    ///
    /// Intended for explicit non-interactive automation. Ignored by --dry-run.
    #[arg(long)]
    yes: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum ChainArg {
    #[value(name = "ethereum")]
    #[serde(rename = "ethereum")]
    Ethereum,
    #[value(name = "hyperevm", alias = "hyper-evm", alias = "hyperliquid")]
    #[serde(rename = "hyperevm", alias = "hyper-evm", alias = "hyperliquid")]
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

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum WalletKind {
    #[serde(rename = "trezor")]
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
    let config = CliConfigService::default().bridge_config(args)?;
    let wallet_service = TrezorWalletService;
    let provider_service = AlloyProviderService;
    let provider_validation_service = AlloyProviderValidationService;
    let approval_service = TerminalIntentApprovalService;
    let reporter = HumanReporter;

    let validation_providers = provider_service.read_only_providers(&config);
    let provider_validation = provider_validation_service
        .validate(&config, &validation_providers)
        .await?;

    let source_signer = wallet_service.source_signer(&config).await?;
    let relay_signer = wallet_service.relay_signer(&config).await?;
    let source_account = source_signer.account;
    let relay_account = relay_signer.as_ref().map(|runtime| runtime.account);
    let source_signer_address = source_account.address;
    let relay_signer_address = relay_account.map(|account| account.address);
    let recipient = config.recipient.resolve(source_signer_address);

    let providers = provider_service.bridge_providers(&config, source_signer.signer, relay_signer);
    let bridge = provider_service.bridge(&config, &providers, recipient);
    let contracts = BridgeContracts::from_bridge(&bridge)?;
    let intent = BridgeIntent::new(
        &config,
        source_account,
        recipient,
        relay_account,
        provider_validation,
        contracts,
    );
    reporter.report_intent(&intent);

    if config.dry_run {
        reporter.report_dry_run_complete();
        return Ok(());
    }

    approval_service.confirm(&intent, config.confirmation)?;
    reporter.report_workflow_start();

    let runtime = CctpBridgeRuntime::new(bridge, providers.source, providers.destination);
    let mut workflow = BridgeWorkflow::new(
        BridgeWorkflowConfig::from(&config),
        runtime,
        source_signer_address,
        recipient,
        relay_signer_address,
    );
    let outcome = workflow.run().await?;
    reporter.report_outcome(&outcome);
    Ok(())
}

trait ConfigService {
    fn bridge_config(&self, args: BridgeArgs) -> Result<BridgeConfig>;
}

trait EnvSource {
    fn get(&self, key: &str) -> Option<String>;
}

#[derive(Clone, Copy, Debug, Default)]
struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|value| !value.is_empty())
    }
}

#[derive(Clone, Debug)]
struct CliConfigService<E = ProcessEnv> {
    env: E,
}

impl Default for CliConfigService<ProcessEnv> {
    fn default() -> Self {
        Self { env: ProcessEnv }
    }
}

#[cfg(test)]
impl<E> CliConfigService<E> {
    const fn new(env: E) -> Self {
        Self { env }
    }
}

impl<E> ConfigService for CliConfigService<E>
where
    E: EnvSource,
{
    fn bridge_config(&self, args: BridgeArgs) -> Result<BridgeConfig> {
        let file = BridgeConfigFile::read_optional(args.config.as_deref())?;

        let from = args.from.or(file.from).unwrap_or(ChainArg::Ethereum);
        let to = args.to.or(file.to).unwrap_or(ChainArg::HyperEvm);
        let route = RouteConfig::new(from, to)?;

        let amount = args
            .amount
            .or(file.amount)
            .ok_or_else(|| eyre!("missing amount; set --amount or amount in the config file"))?;

        let ethereum_rpc = args
            .ethereum_rpc
            .or_else(|| self.env.get(ETHEREUM_RPC_ENV))
            .or(file.ethereum_rpc)
            .ok_or_else(|| {
                eyre!(
                    "missing Ethereum RPC URL; set --ethereum-rpc, {ETHEREUM_RPC_ENV}, or ethereum_rpc in the config file"
                )
            })?;
        let hyperevm_rpc = args
            .hyperevm_rpc
            .or_else(|| self.env.get(HYPEREVM_RPC_ENV))
            .or(file.hyperevm_rpc)
            .ok_or_else(|| {
                eyre!(
                    "missing HyperEVM RPC URL; set --hyperevm-rpc, {HYPEREVM_RPC_ENV}, or hyperevm_rpc in the config file"
                )
            })?;

        let wallet = args.wallet.or(file.wallet).unwrap_or(WalletKind::Trezor);
        let trezor_account = args.trezor_account.or(file.trezor_account).unwrap_or(0);
        let self_relay = args.self_relay.or(file.self_relay).unwrap_or(false);
        let fast = args.fast.or(file.fast).unwrap_or(false);
        let max_fee_usdc = if !fast && args.fast == Some(false) {
            args.max_fee_usdc
        } else {
            args.max_fee_usdc.or(file.max_fee_usdc)
        };
        let dry_run = args.dry_run.or(file.dry_run).unwrap_or(false);
        let receive_polling = ReceivePolling::new(
            args.receive_attempts.or(file.receive_attempts),
            args.receive_interval_secs.or(file.receive_interval_secs),
        )?;

        let source_wallet = WalletConfig::from_kind(wallet, trezor_account);
        source_wallet.validate()?;
        let relay_wallet = RelayWalletConfig::new(
            self_relay,
            wallet,
            args.relay_trezor_account.or(file.relay_trezor_account),
            trezor_account,
        );
        relay_wallet.validate()?;

        Ok(BridgeConfig {
            route,
            amount: UsdcAmount::parse_decimal(&amount)?,
            rpc: RpcEndpoints::parse(ethereum_rpc, hyperevm_rpc)?,
            source_wallet,
            relay_wallet,
            recipient: RecipientConfig::from(args.recipient.or(file.recipient)),
            usdc: args.usdc.or(file.usdc).unwrap_or(MAINNET_USDC),
            transfer_mode: transfer_mode(fast, max_fee_usdc.as_deref())?,
            relay: RelayMode::from_self_relay(self_relay),
            receive_polling,
            dry_run,
            confirmation: ConfirmationPolicy::from_yes(args.yes),
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BridgeConfigFile {
    from: Option<ChainArg>,
    to: Option<ChainArg>,
    amount: Option<String>,
    recipient: Option<Address>,
    ethereum_rpc: Option<String>,
    hyperevm_rpc: Option<String>,
    wallet: Option<WalletKind>,
    trezor_account: Option<u32>,
    relay_trezor_account: Option<u32>,
    usdc: Option<Address>,
    fast: Option<bool>,
    max_fee_usdc: Option<String>,
    self_relay: Option<bool>,
    receive_attempts: Option<u32>,
    receive_interval_secs: Option<u64>,
    dry_run: Option<bool>,
}

impl BridgeConfigFile {
    fn read_optional(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };

        let contents = fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read config file {}", path.display()))?;
        toml::from_str(&contents)
            .wrap_err_with(|| format!("failed to parse config file {}", path.display()))
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
    confirmation: ConfirmationPolicy,
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

    const fn source_label(&self) -> &'static str {
        self.source_label
    }

    const fn destination_label(&self) -> &'static str {
        self.destination_label
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
enum ConfirmationPolicy {
    RequireInteractive,
    SkipPrompt,
}

impl ConfirmationPolicy {
    const fn from_yes(yes: bool) -> Self {
        if yes {
            Self::SkipPrompt
        } else {
            Self::RequireInteractive
        }
    }
}

trait IntentApprovalService {
    fn confirm(&self, intent: &BridgeIntent, policy: ConfirmationPolicy) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Default)]
struct TerminalIntentApprovalService;

impl IntentApprovalService for TerminalIntentApprovalService {
    fn confirm(&self, intent: &BridgeIntent, policy: ConfirmationPolicy) -> Result<()> {
        match policy {
            ConfirmationPolicy::SkipPrompt => {
                println!("Confirmation skipped by --yes.");
                Ok(())
            }
            ConfirmationPolicy::RequireInteractive => {
                print!(
                    "Type CONFIRM to sign and submit this bridge intent for {} USDC: ",
                    intent.amount
                );
                io::stdout()
                    .flush()
                    .wrap_err("failed to flush confirmation prompt")?;

                let mut input = String::new();
                io::stdin()
                    .read_line(&mut input)
                    .wrap_err("failed to read confirmation input")?;
                validate_confirmation_input(&input)
            }
        }
    }
}

fn validate_confirmation_input(input: &str) -> Result<()> {
    if input.trim() == "CONFIRM" {
        Ok(())
    } else {
        bail!("bridge intent was not confirmed")
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WalletRole {
    SourceBurn,
    DestinationRelay,
}

impl std::fmt::Display for WalletRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceBurn => f.write_str("source burn signer"),
            Self::DestinationRelay => f.write_str("destination relay signer"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WalletDerivationPath {
    TrezorLive { account: u32 },
}

impl WalletDerivationPath {
    const fn trezor_live(account: u32) -> Self {
        Self::TrezorLive { account }
    }
}

impl std::fmt::Display for WalletDerivationPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TrezorLive { account } => write!(f, "m/44'/60'/{account}'/0/0"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WalletAccount {
    role: WalletRole,
    wallet: WalletConfig,
    derivation_path: WalletDerivationPath,
    chain_label: &'static str,
    chain_id: u64,
    address: Address,
}

impl WalletConfig {
    const fn from_kind(kind: WalletKind, trezor_account: u32) -> Self {
        match kind {
            WalletKind::Trezor => Self::Trezor {
                account: trezor_account,
            },
        }
    }

    fn validate(self) -> Result<()> {
        self.trezor_account_index().map(|_| ())
    }

    fn account_info(
        self,
        role: WalletRole,
        chain_label: &'static str,
        chain_id: u64,
        address: Address,
    ) -> WalletAccount {
        WalletAccount {
            role,
            wallet: self,
            derivation_path: self.derivation_path(),
            chain_label,
            chain_id,
            address,
        }
    }

    fn trezor_account_index(self) -> Result<usize> {
        match self {
            Self::Trezor { account } => {
                usize::try_from(account).wrap_err("Trezor account index is too large")
            }
        }
    }

    const fn derivation_path(self) -> WalletDerivationPath {
        match self {
            Self::Trezor { account } => WalletDerivationPath::trezor_live(account),
        }
    }

    async fn trezor_signer(self, chain_id: u64) -> Result<TrezorSigner> {
        match self {
            Self::Trezor { .. } => {
                let account_index = self.trezor_account_index()?;
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

    fn validate(self) -> Result<()> {
        match self.wallet() {
            Some(wallet) => wallet.validate(),
            None => Ok(()),
        }
    }
}

struct RelaySignerRuntime {
    signer: TrezorSigner,
    account: WalletAccount,
}

struct SourceSignerRuntime {
    signer: TrezorSigner,
    account: WalletAccount,
}

#[derive(Clone, Copy, Debug, Default)]
struct TrezorWalletService;

#[async_trait(?Send)]
trait WalletService {
    async fn source_signer(&self, config: &BridgeConfig) -> Result<SourceSignerRuntime>;

    async fn relay_signer(&self, config: &BridgeConfig) -> Result<Option<RelaySignerRuntime>>;
}

#[async_trait(?Send)]
impl WalletService for TrezorWalletService {
    async fn source_signer(&self, config: &BridgeConfig) -> Result<SourceSignerRuntime> {
        let signer = config
            .source_wallet
            .trezor_signer(config.route.source_chain_id())
            .await
            .wrap_err("failed to initialize Trezor signer for Ethereum mainnet")?;
        let address = signer
            .get_address()
            .await
            .wrap_err("failed to read Ethereum address from Trezor")?;
        let account = config.source_wallet.account_info(
            WalletRole::SourceBurn,
            config.route.source_label(),
            config.route.source_chain_id(),
            address,
        );

        Ok(SourceSignerRuntime { signer, account })
    }

    async fn relay_signer(&self, config: &BridgeConfig) -> Result<Option<RelaySignerRuntime>> {
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
        let account = wallet.account_info(
            WalletRole::DestinationRelay,
            config.route.destination_label(),
            config.route.destination_chain_id(),
            address,
        );

        Ok(Some(RelaySignerRuntime { signer, account }))
    }
}

#[derive(Clone, Debug)]
struct BridgeProviders {
    source: DynProvider,
    destination: DynProvider,
}

#[derive(Clone, Copy, Debug, Default)]
struct AlloyProviderService;

impl AlloyProviderService {
    fn read_only_providers(self, config: &BridgeConfig) -> BridgeProviders {
        BridgeProviders {
            source: ProviderBuilder::new()
                .connect_http(config.rpc.source.clone())
                .erased(),
            destination: ProviderBuilder::new()
                .connect_http(config.rpc.destination.clone())
                .erased(),
        }
    }

    fn bridge_providers(
        self,
        config: &BridgeConfig,
        source_signer: TrezorSigner,
        relay_signer: Option<RelaySignerRuntime>,
    ) -> BridgeProviders {
        let source = ProviderBuilder::new()
            .wallet(source_signer)
            .connect_http(config.rpc.source.clone())
            .erased();
        let destination = match relay_signer {
            Some(runtime) => ProviderBuilder::new()
                .wallet(runtime.signer)
                .connect_http(config.rpc.destination.clone())
                .erased(),
            None => ProviderBuilder::new()
                .connect_http(config.rpc.destination.clone())
                .erased(),
        };

        BridgeProviders {
            source,
            destination,
        }
    }

    fn bridge(
        self,
        config: &BridgeConfig,
        providers: &BridgeProviders,
        recipient: Address,
    ) -> CctpV2Bridge<DynProvider> {
        CctpV2Bridge::builder()
            .source_chain(config.route.source_chain())
            .destination_chain(config.route.destination_chain())
            .source_provider(providers.source.clone())
            .destination_provider(providers.destination.clone())
            .recipient(recipient)
            .transfer_mode(config.transfer_mode.clone())
            .build()
    }
}

#[async_trait(?Send)]
trait ProviderValidationService {
    async fn validate(
        &self,
        config: &BridgeConfig,
        providers: &BridgeProviders,
    ) -> Result<ProviderValidation>;
}

#[derive(Clone, Copy, Debug, Default)]
struct AlloyProviderValidationService;

#[async_trait(?Send)]
impl ProviderValidationService for AlloyProviderValidationService {
    async fn validate(
        &self,
        config: &BridgeConfig,
        providers: &BridgeProviders,
    ) -> Result<ProviderValidation> {
        let source_chain_id = providers
            .source
            .get_chain_id()
            .await
            .wrap_err("failed to read source RPC chain ID")?;
        let destination_chain_id = providers
            .destination
            .get_chain_id()
            .await
            .wrap_err("failed to read destination RPC chain ID")?;

        ProviderValidation::new(config.route, source_chain_id, destination_chain_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProviderValidation {
    source: ProviderChainCheck,
    destination: ProviderChainCheck,
}

impl ProviderValidation {
    fn new(
        route: RouteConfig,
        source_actual_chain_id: u64,
        destination_actual_chain_id: u64,
    ) -> Result<Self> {
        Ok(Self {
            source: ProviderChainCheck::validate(
                route,
                ProviderEndpointRole::Source,
                route.source_label(),
                route.source_chain_id(),
                source_actual_chain_id,
            )?,
            destination: ProviderChainCheck::validate(
                route,
                ProviderEndpointRole::Destination,
                route.destination_label(),
                route.destination_chain_id(),
                destination_actual_chain_id,
            )?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ProviderChainCheck {
    role: ProviderEndpointRole,
    chain_label: &'static str,
    expected_chain_id: u64,
    actual_chain_id: u64,
}

impl ProviderChainCheck {
    fn validate(
        route: RouteConfig,
        role: ProviderEndpointRole,
        chain_label: &'static str,
        expected_chain_id: u64,
        actual_chain_id: u64,
    ) -> Result<Self> {
        if actual_chain_id != expected_chain_id {
            bail!(
                "{} chain ID mismatch for route {route}: expected {expected_chain_id} ({chain_label}), got {actual_chain_id}",
                role.error_label()
            );
        }

        Ok(Self {
            role,
            chain_label,
            expected_chain_id,
            actual_chain_id,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProviderEndpointRole {
    Source,
    Destination,
}

impl ProviderEndpointRole {
    const fn error_label(self) -> &'static str {
        match self {
            Self::Source => "source RPC",
            Self::Destination => "destination RPC",
        }
    }

    const fn report_label(self) -> &'static str {
        match self {
            Self::Source => "Source RPC",
            Self::Destination => "Destination RPC",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BridgeContracts {
    token_messenger: Address,
    message_transmitter: Address,
    destination_domain: DomainId,
}

impl BridgeContracts {
    fn from_bridge<P>(bridge: &CctpV2Bridge<P>) -> Result<Self>
    where
        P: Provider + Clone,
    {
        Ok(Self {
            token_messenger: bridge.token_messenger_v2_contract()?,
            message_transmitter: bridge.message_transmitter_v2_contract()?,
            destination_domain: bridge.destination_domain_id()?,
        })
    }
}

#[derive(Clone, Debug)]
struct BridgeIntent {
    route: RouteConfig,
    source_account: WalletAccount,
    recipient: Address,
    usdc: Address,
    amount: UsdcAmount,
    transfer_mode: TransferMode,
    relay: RelayMode,
    relay_account: Option<WalletAccount>,
    provider_validation: ProviderValidation,
    contracts: BridgeContracts,
}

impl BridgeIntent {
    fn new(
        config: &BridgeConfig,
        source_account: WalletAccount,
        recipient: Address,
        relay_account: Option<WalletAccount>,
        provider_validation: ProviderValidation,
        contracts: BridgeContracts,
    ) -> Self {
        Self {
            route: config.route,
            source_account,
            recipient,
            usdc: config.usdc,
            amount: config.amount,
            transfer_mode: config.transfer_mode.clone(),
            relay: config.relay,
            relay_account,
            provider_validation,
            contracts,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct HumanReporter;

impl HumanReporter {
    fn report_intent(self, intent: &BridgeIntent) {
        println!("Bridge intent");
        println!("Route: {}", intent.route);
        self.report_provider_check(&intent.provider_validation.source);
        self.report_provider_check(&intent.provider_validation.destination);
        self.report_wallet_account("Source", &intent.source_account);
        println!("Recipient: {}", intent.recipient);
        println!("USDC: {}", intent.usdc);
        println!("Amount: {} USDC", intent.amount);
        self.report_transfer_mode(&intent.transfer_mode);
        println!("Relay: {}", intent.relay);
        match intent.relay_account {
            Some(account) => self.report_wallet_account("Relay", &account),
            None => println!("Destination provider: read-only"),
        }
        println!(
            "TokenMessengerV2 approval spender: {}",
            intent.contracts.token_messenger
        );
        println!(
            "MessageTransmitterV2 destination contract: {}",
            intent.contracts.message_transmitter
        );
        println!(
            "Destination domain: {}",
            intent.contracts.destination_domain
        );
    }

    fn report_provider_check(self, check: &ProviderChainCheck) {
        println!(
            "{} verified: {} (chain id {})",
            check.role.report_label(),
            check.chain_label,
            check.actual_chain_id
        );
    }

    fn report_wallet_account(self, label: &str, account: &WalletAccount) {
        println!("{label} role: {}", account.role);
        println!("{label} wallet: {}", account.wallet);
        println!("{label} derivation: {}", account.derivation_path);
        println!(
            "{label} chain: {} (chain id {})",
            account.chain_label, account.chain_id
        );
        println!("{label} address: {}", account.address);
    }

    fn report_transfer_mode(self, mode: &TransferMode) {
        println!("Mode: {}", mode_label(mode));
        if mode.is_fast() {
            println!(
                "Fast fee cap: {} USDC",
                UsdcAmount::from_atomic(mode.max_fee())
            );
        }
    }

    fn report_dry_run_complete(self) {
        println!("Dry run complete. No transactions sent.");
    }

    fn report_workflow_start(self) {
        println!("Starting bridge workflow.");
    }

    fn report_outcome(self, outcome: &BridgeOutcome) {
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
    for _ in 1..=max_attempts {
        let receipt = provider
            .get_transaction_receipt(tx_hash)
            .await
            .wrap_err_with(|| format!("failed to poll {label} transaction receipt"))?;

        if receipt.is_some() {
            return Ok(());
        }

        sleep(interval).await;
    }

    bail!("{label} transaction {tx_hash} was not confirmed before timeout")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        collections::HashMap,
        sync::atomic::{AtomicU64, Ordering},
    };

    static CONFIG_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone, Debug, Default)]
    struct TestEnv(HashMap<String, String>);

    impl EnvSource for TestEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    fn config_service(env: &[(&str, &str)]) -> CliConfigService<TestEnv> {
        CliConfigService::new(TestEnv(
            env.iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        ))
    }

    fn empty_service() -> CliConfigService<TestEnv> {
        config_service(&[])
    }

    fn write_config(contents: &str) -> PathBuf {
        let count = CONFIG_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("cctp-config-{}-{count}.toml", std::process::id()));
        std::fs::write(&path, contents).expect("write config");
        path
    }

    fn empty_args() -> BridgeArgs {
        BridgeArgs {
            config: None,
            from: None,
            to: None,
            amount: None,
            recipient: None,
            ethereum_rpc: None,
            hyperevm_rpc: None,
            wallet: None,
            trezor_account: None,
            relay_trezor_account: None,
            usdc: None,
            fast: None,
            max_fee_usdc: None,
            self_relay: None,
            receive_attempts: None,
            receive_interval_secs: None,
            dry_run: None,
            yes: false,
        }
    }

    fn sample_args() -> BridgeArgs {
        BridgeArgs {
            from: Some(ChainArg::Ethereum),
            to: Some(ChainArg::HyperEvm),
            amount: Some("1.25".to_owned()),
            ethereum_rpc: Some("https://ethereum.example".to_owned()),
            hyperevm_rpc: Some("https://hyperevm.example".to_owned()),
            wallet: Some(WalletKind::Trezor),
            trezor_account: Some(0),
            ..empty_args()
        }
    }

    #[test]
    fn config_service_builds_bridge_config() {
        let config = empty_service()
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
        assert_eq!(config.confirmation, ConfirmationPolicy::RequireInteractive);
    }

    #[test]
    fn config_service_rejects_unsupported_route() {
        let mut args = sample_args();
        args.to = Some(ChainArg::Ethereum);

        assert!(empty_service().bridge_config(args).is_err());
    }

    #[test]
    fn config_service_requires_fast_fee_for_fast_mode() {
        let mut args = sample_args();
        args.fast = Some(true);

        assert!(empty_service().bridge_config(args).is_err());
    }

    #[test]
    fn config_service_parses_fast_fee() {
        let mut args = sample_args();
        args.fast = Some(true);
        args.max_fee_usdc = Some("0.01".to_owned());

        let config = empty_service().bridge_config(args).expect("valid config");
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
        args.self_relay = Some(true);

        let config = empty_service().bridge_config(args).expect("valid config");
        assert_eq!(config.relay, RelayMode::SelfRelay);
        assert_eq!(
            config.relay_wallet,
            RelayWalletConfig::Trezor { account: 0 }
        );
    }

    #[test]
    fn config_service_accepts_distinct_self_relay_account() {
        let mut args = sample_args();
        args.self_relay = Some(true);
        args.relay_trezor_account = Some(2);

        let config = empty_service().bridge_config(args).expect("valid config");
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

        let config = empty_service().bridge_config(args).expect("valid config");
        assert_eq!(config.relay, RelayMode::WaitForRelayer);
        assert_eq!(config.relay_wallet, RelayWalletConfig::None);
    }

    #[test]
    fn wallet_config_describes_trezor_derivation_and_chain_binding() {
        let wallet = WalletConfig::Trezor { account: 3 };
        let address = address!("0000000000000000000000000000000000000003");

        let account = wallet.account_info(WalletRole::SourceBurn, "Ethereum mainnet", 1, address);

        wallet.validate().expect("wallet config is valid");
        assert_eq!(account.role, WalletRole::SourceBurn);
        assert_eq!(account.wallet, wallet);
        assert_eq!(
            account.derivation_path,
            WalletDerivationPath::TrezorLive { account: 3 }
        );
        assert_eq!(account.derivation_path.to_string(), "m/44'/60'/3'/0/0");
        assert_eq!(account.chain_label, "Ethereum mainnet");
        assert_eq!(account.chain_id, 1);
        assert_eq!(account.address, address);
    }

    #[test]
    fn relay_wallet_config_validates_without_device() {
        let relay_wallet = RelayWalletConfig::new(true, WalletKind::Trezor, Some(2), 0);

        relay_wallet.validate().expect("relay wallet is valid");
        assert_eq!(
            relay_wallet.wallet().expect("relay wallet exists"),
            WalletConfig::Trezor { account: 2 }
        );
        assert!(RelayWalletConfig::None.validate().is_ok());
    }

    #[test]
    fn config_service_reads_config_file() {
        let path = write_config(
            r#"
amount = "2.5"
ethereum_rpc = "https://file.ethereum.example"
hyperevm_rpc = "https://file.hyperevm.example"
recipient = "0x0000000000000000000000000000000000000007"
usdc = "0x0000000000000000000000000000000000000008"
trezor_account = 4
self_relay = true
relay_trezor_account = 5
receive_attempts = 3
receive_interval_secs = 7
"#,
        );
        let mut args = empty_args();
        args.config = Some(path);

        let config = empty_service().bridge_config(args).expect("valid config");

        assert_eq!(config.route.source_chain(), NamedChain::Mainnet);
        assert_eq!(config.route.destination_chain(), NamedChain::Hyperliquid);
        assert_eq!(config.amount.atomic(), U256::from(2_500_000u64));
        assert_eq!(
            config.recipient,
            RecipientConfig::Address(address!("0000000000000000000000000000000000000007"))
        );
        assert_eq!(
            config.usdc,
            address!("0000000000000000000000000000000000000008")
        );
        assert_eq!(config.rpc.source.as_str(), "https://file.ethereum.example/");
        assert_eq!(
            config.rpc.destination.as_str(),
            "https://file.hyperevm.example/"
        );
        assert_eq!(config.source_wallet, WalletConfig::Trezor { account: 4 });
        assert_eq!(
            config.relay_wallet,
            RelayWalletConfig::Trezor { account: 5 }
        );
        assert_eq!(config.relay, RelayMode::SelfRelay);
        assert_eq!(
            config.receive_polling,
            ReceivePolling {
                attempts: Some(3),
                interval_secs: Some(7)
            }
        );
    }

    #[test]
    fn config_service_applies_cli_env_file_default_precedence() {
        let path = write_config(
            r#"
amount = "1"
ethereum_rpc = "https://file.ethereum.example"
hyperevm_rpc = "https://file.hyperevm.example"
trezor_account = 4
dry_run = true
"#,
        );
        let mut args = empty_args();
        args.config = Some(path);
        args.amount = Some("3".to_owned());
        args.ethereum_rpc = Some("https://cli.ethereum.example".to_owned());
        args.trezor_account = Some(9);

        let config = config_service(&[(HYPEREVM_RPC_ENV, "https://env.hyperevm.example")])
            .bridge_config(args)
            .expect("valid config");

        assert_eq!(config.amount.atomic(), U256::from(3_000_000u64));
        assert_eq!(config.rpc.source.as_str(), "https://cli.ethereum.example/");
        assert_eq!(
            config.rpc.destination.as_str(),
            "https://env.hyperevm.example/"
        );
        assert_eq!(config.source_wallet, WalletConfig::Trezor { account: 9 });
        assert_eq!(config.relay_wallet, RelayWalletConfig::None);
        assert_eq!(config.relay, RelayMode::WaitForRelayer);
        assert!(config.dry_run);
    }

    #[test]
    fn config_service_cli_false_overrides_file_true() {
        let path = write_config(
            r#"
amount = "1"
ethereum_rpc = "https://file.ethereum.example"
hyperevm_rpc = "https://file.hyperevm.example"
fast = true
max_fee_usdc = "0.01"
self_relay = true
dry_run = true
"#,
        );
        let mut args = empty_args();
        args.config = Some(path);
        args.fast = Some(false);
        args.self_relay = Some(false);
        args.dry_run = Some(false);

        let config = empty_service().bridge_config(args).expect("valid config");

        assert_eq!(config.transfer_mode, TransferMode::Standard);
        assert_eq!(config.relay, RelayMode::WaitForRelayer);
        assert_eq!(config.relay_wallet, RelayWalletConfig::None);
        assert!(!config.dry_run);
    }

    #[test]
    fn config_service_keeps_confirmation_skip_cli_only() {
        let mut args = sample_args();
        args.yes = true;

        let config = empty_service().bridge_config(args).expect("valid config");

        assert_eq!(config.confirmation, ConfirmationPolicy::SkipPrompt);
    }

    #[test]
    fn config_service_uses_env_rpc_over_file() {
        let path = write_config(
            r#"
amount = "1"
ethereum_rpc = "https://file.ethereum.example"
hyperevm_rpc = "https://file.hyperevm.example"
"#,
        );
        let mut args = empty_args();
        args.config = Some(path);

        let config = config_service(&[
            (ETHEREUM_RPC_ENV, "https://env.ethereum.example"),
            (HYPEREVM_RPC_ENV, "https://env.hyperevm.example"),
        ])
        .bridge_config(args)
        .expect("valid config");

        assert_eq!(config.rpc.source.as_str(), "https://env.ethereum.example/");
        assert_eq!(
            config.rpc.destination.as_str(),
            "https://env.hyperevm.example/"
        );
    }

    #[test]
    fn config_service_rejects_invalid_config_file() {
        let path = write_config("unknown = true\n");
        let mut args = sample_args();
        args.config = Some(path);

        assert!(empty_service().bridge_config(args).is_err());
    }

    #[test]
    fn config_service_rejects_missing_required_values() {
        let error = empty_service()
            .bridge_config(empty_args())
            .expect_err("missing amount is invalid");

        assert!(
            error.to_string().contains("missing amount"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn provider_validation_accepts_expected_chain_ids() {
        let route = RouteConfig::new(ChainArg::Ethereum, ChainArg::HyperEvm).expect("valid route");

        let validation =
            ProviderValidation::new(route, route.source_chain_id(), route.destination_chain_id())
                .expect("chain IDs match");

        assert_eq!(
            validation.source,
            ProviderChainCheck {
                role: ProviderEndpointRole::Source,
                chain_label: "Ethereum mainnet",
                expected_chain_id: route.source_chain_id(),
                actual_chain_id: route.source_chain_id()
            }
        );
        assert_eq!(
            validation.destination,
            ProviderChainCheck {
                role: ProviderEndpointRole::Destination,
                chain_label: "HyperEVM",
                expected_chain_id: route.destination_chain_id(),
                actual_chain_id: route.destination_chain_id()
            }
        );
    }

    #[test]
    fn provider_validation_rejects_source_chain_mismatch_with_route_context() {
        let route = RouteConfig::new(ChainArg::Ethereum, ChainArg::HyperEvm).expect("valid route");

        let error = ProviderValidation::new(route, 31_337, route.destination_chain_id())
            .expect_err("source mismatch is invalid");

        let message = error.to_string();
        assert!(
            message.contains("source RPC"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("Ethereum mainnet -> HyperEVM"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("expected 1"),
            "unexpected error: {message}"
        );
        assert!(message.contains("got 31337"), "unexpected error: {message}");
    }

    #[test]
    fn provider_validation_rejects_destination_chain_mismatch_with_route_context() {
        let route = RouteConfig::new(ChainArg::Ethereum, ChainArg::HyperEvm).expect("valid route");

        let error = ProviderValidation::new(route, route.source_chain_id(), 31_337)
            .expect_err("destination mismatch is invalid");

        let message = error.to_string();
        assert!(
            message.contains("destination RPC"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("Ethereum mainnet -> HyperEVM"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains(&format!("expected {}", route.destination_chain_id())),
            "unexpected error: {message}"
        );
        assert!(message.contains("got 31337"), "unexpected error: {message}");
    }

    #[test]
    fn bridge_intent_captures_contracts_provider_checks_and_relay_account() {
        let mut args = sample_args();
        args.self_relay = Some(true);
        args.relay_trezor_account = Some(2);
        let config = empty_service().bridge_config(args).expect("valid config");
        let source_account = config.source_wallet.account_info(
            WalletRole::SourceBurn,
            config.route.source_label(),
            config.route.source_chain_id(),
            source_sender(),
        );
        let relay_account = config
            .relay_wallet
            .wallet()
            .expect("relay wallet")
            .account_info(
                WalletRole::DestinationRelay,
                config.route.destination_label(),
                config.route.destination_chain_id(),
                address!("0000000000000000000000000000000000000004"),
            );
        let provider_validation = ProviderValidation::new(
            config.route,
            config.route.source_chain_id(),
            config.route.destination_chain_id(),
        )
        .expect("chain IDs match");
        let contracts = BridgeContracts {
            token_messenger: address!("0000000000000000000000000000000000000010"),
            message_transmitter: address!("0000000000000000000000000000000000000020"),
            destination_domain: DomainId::HyperEvm,
        };

        let intent = BridgeIntent::new(
            &config,
            source_account,
            recipient(),
            Some(relay_account),
            provider_validation,
            contracts,
        );

        assert_eq!(intent.route, config.route);
        assert_eq!(intent.source_account, source_account);
        assert_eq!(intent.recipient, recipient());
        assert_eq!(
            intent.amount,
            UsdcAmount::from_atomic(U256::from(1_250_000u64))
        );
        assert_eq!(intent.relay, RelayMode::SelfRelay);
        assert_eq!(intent.relay_account, Some(relay_account));
        assert_eq!(intent.provider_validation, provider_validation);
        assert_eq!(intent.contracts, contracts);
    }

    #[test]
    fn confirmation_input_requires_exact_confirm_token() {
        validate_confirmation_input("CONFIRM\n").expect("CONFIRM is accepted");

        let error = validate_confirmation_input("confirm\n").expect_err("lowercase is rejected");
        assert!(
            error.to_string().contains("not confirmed"),
            "unexpected error: {error}"
        );
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
