# Vydex Escrow

On-chain escrow for [vydex.dev](https://vydex.dev) — a marketplace for repositories and custom-dev orders. Payments settle in **USDC on Solana**.

One **isolated PDA vault per deal**: the buyer's USDC is locked in a token account whose authority is the escrow PDA. There is no private key to the vault — funds can only move by program logic (buyer approval, timeout, or an arbiter multisig decision).

```
Funded ──▶ Delivered ──▶ Released
   │            │
   │            ├──▶ Disputed ──▶ Released / Refunded / Split (arbiter)
   │
   └──▶ Refunded  (seller missed the delivery deadline)
```

## Why this is safe without a paid audit (yet)

An external audit is deferred. Risk is bounded instead by:

- **`max_deal_amount` cap** (launch: 200 USDC). A worst-case exploit touches a few small deals, not the whole TVL. Raised later by the admin multisig once audited.
- **A full attacking test suite** (see `tests/`) — every scenario in the checklist, including auth bypass, mint substitution, fake fee-vault, re-init, double-release, and timing attacks. These are treated as the audit substitute; they must all pass.
- **Public source** — this repo is open. Report issues per [SECURITY.md](SECURITY.md).
- **Free tooling in CI** — `anchor build`, the full test suite, `cargo clippy -D warnings`, and `cargo audit` run on every push.

## Security model (the load-bearing constraints)

The `#[derive(Accounts)]` contexts in [`programs/vydex-escrow/src/lib.rs`](programs/vydex-escrow/src/lib.rs) are where most of the security lives. **Do not weaken them.** In particular:

- **Vault authority = escrow PDA.** `token::authority = escrow` on the vault; every payout is a CPI signed by the escrow PDA seeds. No wallet can move vault funds.
- **`init`, never `init_if_needed`.** The escrow PDA is seeded by `(order_id, buyer, seller)`; `init` makes a repeated `create_and_fund` for the same triple fail — a re-initialization guard.
- **Mint is pinned.** The vault and every user token account are constrained to `config.usdc_mint`, blocking fake-token attacks.
- **Payout destinations are constrained.** `seller_token.authority == seller`, `buyer_token.authority == buyer`, and `fee_vault` is checked against `config.fee_vault` (`has_one`). Funds can only reach the real parties.
- **`fee_bps` is capped** at `MAX_FEE_BPS` (10%) so `update_config` can never rug via fees.
- **`review_period` is frozen at creation** — changing the config default does not affect live deals.
- **Pause blocks creation only.** `paused` stops new escrows but never blocks release/refund — users can always retrieve their funds.
- **No admin withdrawal path exists.** The only way funds leave a vault other than approve/timeout/refund is `resolve_dispute`, callable solely by the arbiter multisig on a `Disputed` deal.

## Instructions

| Instruction | Who | Effect |
|---|---|---|
| `initialize_config` | deployer | one-time global config (admin, arbiter, USDC mint, fee vault, fees, periods, cap) |
| `update_config` | admin multisig | tune fee/period/arbiter/pause/cap (within hard caps) |
| `create_and_fund` | buyer | create the deal + lock USDC; `instant_delivery` for ready repos |
| `mark_delivered` | seller | mark an order delivered (starts the review timer) |
| `approve_and_release` | buyer | accept + optional tip → pay the seller now (fee taken; not on tip) |
| `release_after_timeout` | anyone (crank) | pay the seller after the review period |
| `refund_undelivered` | anyone (crank) | refund the buyer if the delivery deadline passed while `Funded` |
| `open_dispute` | buyer or seller | dispute a `Delivered` deal before the review period ends |
| `resolve_dispute` | arbiter multisig | split funds `seller_bps` / rest to buyer (fee on the seller share only) |

## Build & test

Everything runs on a Linux Anchor toolchain. Easiest is the prebuilt image (also used in CI):

```bash
docker run --rm -v "$PWD":/w -w /w backpackapp/build:v0.30.1 \
  bash -lc "yarn install && anchor keys sync && anchor build && anchor test"
```

Native (Linux/macOS/WSL) with Rust + Solana + Anchor 0.30.1:

```bash
anchor keys sync   # replaces the placeholder program id in lib.rs + Anchor.toml
anchor build
anchor test        # spins up a local validator and runs the full suite
cargo clippy --all-targets -- -D warnings
cargo audit
```

## Deploy (owner-signed)

Deployment and the config initialization are performed **by the owner from their own wallet** — this repo only provides the scripts (`scripts/`). See [`scripts/README.md`](scripts/README.md). Launch parameters:

| Param | Value |
|---|---|
| `fee_bps` | `500` (5%) |
| `default_review_period` | `259200` (3 days) |
| `delivery_deadline` | `1209600` (14 days) |
| `max_deal_amount` | `200000000` (200 USDC) |
| USDC mint (mainnet) | `EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v` |
| `admin` / `arbiter` | Squads multisig vaults — **provided by the owner** (placeholders in the deploy config) |

After deploy, the program upgrade authority is transferred to the Squads multisig (see the deploy guide).

## License

MIT.
