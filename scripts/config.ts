import { PublicKey } from "@solana/web3.js";

// ── Deploy configuration ────────────────────────────────────────────────────
// Fill the PLACEHOLDER addresses before running the mainnet deploy.
// `admin` and `arbiter` MUST be Squads multisig vault addresses (owner-provided).
// The all-ones value below is a deliberate placeholder — initialize-config.ts
// refuses to run until every placeholder is replaced.

export const PLACEHOLDER = "11111111111111111111111111111111";

export const CLUSTER = process.env.ANCHOR_PROVIDER_URL ?? "https://api.mainnet-beta.solana.com";

export const CONFIG = {
  // Squads multisig vault — protocol admin (only role that can update config).
  admin: new PublicKey(PLACEHOLDER), // TODO: owner to provide
  // Squads multisig vault — dispute arbiter (only role that can resolve disputes).
  arbiter: new PublicKey(PLACEHOLDER), // TODO: owner to provide
  // Platform USDC token account that receives fees (create/own it beforehand).
  feeVault: new PublicKey(PLACEHOLDER), // TODO: owner to provide

  // USDC mainnet mint — do not change for mainnet.
  usdcMint: new PublicKey("EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"),

  // Launch parameters.
  feeBps: 500, // 5%
  defaultReviewPeriod: 259_200, // 3 days
  deliveryDeadline: 1_209_600, // 14 days
  maxDealAmount: 200_000_000, // 200 USDC — risk cap; raised later via the admin multisig
};
