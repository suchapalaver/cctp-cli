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
