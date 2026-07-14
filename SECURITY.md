# Security Policy

Vydex Escrow custodies user funds (USDC) on Solana mainnet. We take reports seriously.

## Reporting a vulnerability

**Do not open a public issue for security bugs.** Instead:

- Email: `security@vydex.dev`
- Or Telegram: `@vydex_security` <!-- TODO: owner to confirm the real contact -->

Please include: a description, affected instruction(s)/accounts, and a proof-of-concept (a failing test against this repo is ideal). We aim to acknowledge within 72 hours.

## Scope

In scope:

- The Anchor program in `programs/vydex-escrow/` — anything that lets funds leave a vault other than by the intended paths (buyer approval, timeout release, undelivered refund, arbiter dispute resolution), or that bypasses the account constraints.
- The crank / indexer backend where a flaw could cause fund loss or wrong on-chain calls.

Out of scope:

- The web frontend UX, rate limits, and off-chain caches (the on-chain state is the source of truth).
- Issues requiring the admin or arbiter **multisig** keys — those are trusted, Squads-controlled roles.

## Risk model (pre-audit)

An external audit is deferred. Until then, exposure is intentionally bounded:

- **Per-deal cap** `max_deal_amount` (launch: **200 USDC**). A worst-case exploit is limited to a few small deals rather than the full TVL.
- **Full attacking test suite** in `tests/` — treated as the audit substitute; all scenarios must pass in CI.
- **Trusted roles are multisigs.** `admin` (config) and `arbiter` (dispute resolution) are Squads multisig vaults; program upgrade authority is held by the multisig.
- **No admin drain path.** Funds can leave a vault only via the intended instructions; there is no admin withdrawal.

The cap is raised only after an external audit, by the admin multisig.

## Disclosure

Coordinated disclosure. We will confirm the issue, ship a fix (or pause new-escrow creation via `update_config` while funds remain withdrawable), and credit the reporter unless they prefer to remain anonymous.
