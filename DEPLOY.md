# Mainnet deploy — owner-signed

The program is deployed and the config initialized **by the owner, from the owner's wallet**. This repo only provides the recipe and scripts; the AI/agent never signs a mainnet transaction.

> Prerequisite: CI is green (`anchor build`, full test suite, clippy, `cargo audit`).

## 0. One-time preparation

- **Squads multisigs** (Solana): create two Squads vaults — one for `admin`, one for `arbiter`. Copy their **vault addresses**.
- **Fee vault**: create a USDC token account you control to receive fees (an ATA of your platform wallet for the mainnet USDC mint). Copy its address.
- Fill `scripts/config.ts` — replace the three `PLACEHOLDER` addresses (`admin`, `arbiter`, `feeVault`). `usdcMint` and the launch params are already set.
- A deploy wallet with enough SOL (program deploy rent is a few SOL).

## 1. Build

```bash
anchor keys sync          # writes the real program id into lib.rs + Anchor.toml
anchor build
```

Note the program id (`anchor keys list`). Set it in `Anchor.toml` under `[programs.mainnet]` if you keep a mainnet entry.

## 2. Deploy the program (owner-signed)

```bash
solana config set --url https://api.mainnet-beta.solana.com
solana config set --keypair <owner-deploy-keypair.json>

anchor deploy --provider.cluster mainnet
# or: solana program deploy target/deploy/vydex_escrow.so --program-id target/deploy/vydex_escrow-keypair.json
```

Keep `target/deploy/vydex_escrow-keypair.json` safe — it is the program's upgrade keypair until authority is transferred (step 4).

## 3. Initialize the global config (owner-signed)

```bash
ANCHOR_PROVIDER_URL=https://api.mainnet-beta.solana.com \
ANCHOR_WALLET=<owner-deploy-keypair.json> \
  yarn ts-node scripts/initialize-config.ts
```

The script prints every value and refuses to run while any placeholder remains. It writes the `config` PDA (admin, arbiter, USDC mint, fee vault, fee 5%, review 3d, deadline 14d, cap 200 USDC).

## 4. Hand upgrade authority to the Squads multisig

Do NOT leave upgrade authority on a single hot keypair.

```bash
solana program set-upgrade-authority <PROGRAM_ID> \
  --new-upgrade-authority <ADMIN_SQUADS_VAULT> \
  --skip-new-upgrade-authority-signer-check
```

From then on, program upgrades and `update_config` both require the multisig.

## 5. Post-launch

- Start the backend crank + Helius webhook indexer (see `backend/`).
- Wire Telegram alerts on `DisputeResolved` and `update_config`.
- Raise `max_deal_amount` (via the admin multisig `update_config`) only after an external audit.

## Rollback / emergency

- **Pause new deals**: `update_config(paused=true)` from the admin multisig. This blocks new escrows but never blocks release/refund — users can always withdraw.
- There is no admin drain path; funds move only by buyer approval, timeout, undelivered refund, or arbiter dispute resolution.
