# cctp-cli

Small Trezor-backed CLI for bridging USDC with
[`cctp-rs`](https://crates.io/crates/cctp-rs).

The first supported route is Ethereum mainnet to HyperEVM. The CLI uses Alloy's
Trezor signer support and defaults to waiting for any permissionless relayer to
complete the destination mint.

## Usage

```sh
export ETHEREUM_RPC_URL="https://..."
export HYPEREVM_RPC_URL="https://..."

cargo run -- bridge \
  --amount 10.25 \
  --recipient 0x0000000000000000000000000000000000000000
```

By default this sends standard-finality CCTP v2 transactions. To request fast
finality, provide an explicit fee cap:

```sh
cargo run -- bridge \
  --amount 10.25 \
  --fast \
  --max-fee-usdc 0.01
```

By default the CLI waits for any relayer to complete the mint on HyperEVM. It
uses a read-only HyperEVM provider and does not initialize a destination signer
or require HyperEVM gas.

To self-relay, add `--self-relay`; the relay account must hold HyperEVM gas.
The relay signer defaults to `--trezor-account`, but can be selected
independently with `--relay-trezor-account`.

Use `--dry-run` to print route and Trezor account details without sending
transactions.

## Configuration

Configuration is treated as a service boundary. Raw CLI/env input is resolved
once into a validated `BridgeConfig`; execution code consumes that immutable
config instead of reading flags or environment variables directly.

Precedence is:

1. CLI flags.
2. Environment variables for RPC URLs: `ETHEREUM_RPC_URL` and
   `HYPEREVM_RPC_URL`.
3. TOML config file passed with `--config`.
4. Built-in defaults for route, wallet, account, relay mode, and transfer mode.

`amount`, `ethereum_rpc`, and `hyperevm_rpc` must be supplied by CLI, env, or
config file. Example:

```toml
amount = "10.25"
ethereum_rpc = "https://..."
hyperevm_rpc = "https://..."
recipient = "0x0000000000000000000000000000000000000000"
trezor_account = 0
fast = false
self_relay = false
dry_run = false
```

Run with:

```sh
cargo run -- bridge --config cctp.toml --amount 25
```

Domain primitives are shared with `cctp-rs` where they belong. The CLI uses
`CctpV2Route` for route validation and `UsdcAmount` for six-decimal USDC amount
parsing. Wallet backends, RPC endpoints, dry-run behavior, and relay policy stay
in the CLI because they are application concerns.
