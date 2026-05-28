# cctp-cli

Small Trezor-backed CLI for bridging USDC with
[`cctp-rs`](../cctp-rs).

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

By default the CLI waits for any relayer to complete the mint on HyperEVM. To
self-relay from the same Trezor account, add `--self-relay`; that account must
hold HyperEVM gas.

Use `--dry-run` to print route and Trezor account details without sending
transactions.

## Configuration

Configuration is treated as a service boundary. Raw CLI/env input is resolved
once into a validated `BridgeConfig`; execution code consumes that immutable
config instead of reading flags or environment variables directly.

Domain primitives are shared with `cctp-rs` where they belong. The CLI uses
`CctpV2Route` for route validation and `UsdcAmount` for six-decimal USDC amount
parsing. Wallet backends, RPC endpoints, dry-run behavior, and relay policy stay
in the CLI because they are application concerns.
