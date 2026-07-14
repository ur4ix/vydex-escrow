import * as anchor from "@coral-xyz/anchor";
import { Program } from "@coral-xyz/anchor";
import { PublicKey, SystemProgram } from "@solana/web3.js";
import { VydexEscrow } from "../target/types/vydex_escrow";
import { CONFIG, PLACEHOLDER } from "./config";

// One-time global config initialization. Run AFTER the program is deployed, from
// the owner's wallet:
//   ANCHOR_PROVIDER_URL=<rpc> ANCHOR_WALLET=<owner-keypair.json> \
//     yarn ts-node scripts/initialize-config.ts
//
// This does NOT deploy the program (see DEPLOY.md) and does not custody funds.
async function main() {
  const provider = anchor.AnchorProvider.env();
  anchor.setProvider(provider);
  const program = anchor.workspace.VydexEscrow as Program<VydexEscrow>;

  // Refuse to run against unset placeholders — admin/arbiter must be real
  // Squads multisig vaults, fee_vault a real platform token account.
  for (const [name, key] of Object.entries({ admin: CONFIG.admin, arbiter: CONFIG.arbiter, feeVault: CONFIG.feeVault })) {
    if (key.toBase58() === PLACEHOLDER) {
      throw new Error(`CONFIG.${name} is still the placeholder — set the real address in scripts/config.ts first`);
    }
  }

  const [configPda] = PublicKey.findProgramAddressSync([Buffer.from("config")], program.programId);
  console.log("program id :", program.programId.toBase58());
  console.log("config PDA :", configPda.toBase58());
  console.log("admin      :", CONFIG.admin.toBase58());
  console.log("arbiter    :", CONFIG.arbiter.toBase58());
  console.log("fee_vault  :", CONFIG.feeVault.toBase58());
  console.log("fee_bps    :", CONFIG.feeBps);
  console.log("review     :", CONFIG.defaultReviewPeriod, "s");
  console.log("deadline   :", CONFIG.deliveryDeadline, "s");
  console.log("max_deal   :", CONFIG.maxDealAmount, "(USDC base units)");

  const sig = await program.methods
    .initializeConfig(
      CONFIG.feeBps,
      new anchor.BN(CONFIG.defaultReviewPeriod),
      new anchor.BN(CONFIG.deliveryDeadline),
      new anchor.BN(CONFIG.maxDealAmount),
    )
    .accountsPartial({
      config: configPda,
      admin: CONFIG.admin,
      arbiter: CONFIG.arbiter,
      usdcMint: CONFIG.usdcMint,
      feeVault: CONFIG.feeVault,
      payer: provider.wallet.publicKey,
      systemProgram: SystemProgram.programId,
    })
    .rpc();

  console.log("\ninitialize_config tx:", sig);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
