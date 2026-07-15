import * as anchor from "@coral-xyz/anchor";
import { Program, BN } from "@coral-xyz/anchor";
import { PublicKey, Keypair, SystemProgram, Transaction } from "@solana/web3.js";
import {
  TOKEN_PROGRAM_ID,
  MINT_SIZE,
  AccountLayout,
  createInitializeMint2Instruction,
  getAssociatedTokenAddressSync,
  createAssociatedTokenAccountInstruction,
  createMintToInstruction,
} from "@solana/spl-token";
import { startAnchor, Clock, ProgramTestContext } from "solana-bankrun";
import { BankrunProvider } from "anchor-bankrun";
import { assert } from "chai";
import { VydexEscrow } from "../target/types/vydex_escrow";
import idl from "../target/idl/vydex_escrow.json";

// Launch parameters (see README). review_period uses the program minimum so the
// config is valid; timeout scenarios advance the bankrun clock instead of waiting.
const FEE_BPS = 500; // 5%
const REVIEW_PERIOD = 3600; // 1h (MIN_REVIEW_PERIOD)
const DELIVERY_DEADLINE = 1_209_600; // 14 days
const MAX_DEAL = 200_000_000; // 200 USDC
const DECIMALS = 6;

describe("vydex-escrow", () => {
  let context: ProgramTestContext;
  let provider: BankrunProvider;
  let program: Program<VydexEscrow>;
  let payer: Keypair; // bankrun's funded payer (tx fee payer)

  let mint: PublicKey;
  let mintAuthority: Keypair;
  let wrongMint: PublicKey; // a different mint, for fake-token attacks
  let platform: Keypair; // owns the fee vault
  let feeVault: PublicKey;

  let admin: Keypair;
  let arbiter: Keypair;
  let configPda: PublicKey;

  let orderCounter = 0;
  const nextOrderId = () => new BN(++orderCounter);

  // ---- helpers -------------------------------------------------------------
  async function send(ixs: anchor.web3.TransactionInstruction[], signers: Keypair[] = []) {
    const tx = new Transaction();
    ixs.forEach((ix) => tx.add(ix));
    return provider.sendAndConfirm(tx, signers);
  }

  async function fundSol(pubkey: PublicKey, sol: number) {
    await send([
      SystemProgram.transfer({
        fromPubkey: payer.publicKey,
        toPubkey: pubkey,
        lamports: sol * anchor.web3.LAMPORTS_PER_SOL,
      }),
    ]);
  }

  async function createMintFor(authority: Keypair): Promise<PublicKey> {
    const m = Keypair.generate();
    const rent = await context.banksClient.getRent();
    const lamports = Number(rent.minimumBalance(BigInt(MINT_SIZE)));
    await send(
      [
        SystemProgram.createAccount({
          fromPubkey: payer.publicKey,
          newAccountPubkey: m.publicKey,
          space: MINT_SIZE,
          lamports,
          programId: TOKEN_PROGRAM_ID,
        }),
        createInitializeMint2Instruction(m.publicKey, DECIMALS, authority.publicKey, null),
      ],
      [m],
    );
    return m.publicKey;
  }

  async function ata(m: PublicKey, owner: PublicKey): Promise<PublicKey> {
    const address = getAssociatedTokenAddressSync(m, owner, true);
    await send([createAssociatedTokenAccountInstruction(payer.publicKey, address, owner, m)]);
    return address;
  }

  async function ataWithBalance(m: PublicKey, owner: PublicKey, amount: number, auth: Keypair): Promise<PublicKey> {
    const address = await ata(m, owner);
    await send([createMintToInstruction(m, address, auth.publicKey, amount)], [auth]);
    return address;
  }

  async function balance(tokenAccount: PublicKey): Promise<bigint> {
    const acc = await context.banksClient.getAccount(tokenAccount);
    if (!acc) return 0n;
    return AccountLayout.decode(Buffer.from(acc.data)).amount;
  }

  async function exists(pubkey: PublicKey): Promise<boolean> {
    return (await context.banksClient.getAccount(pubkey)) !== null;
  }

  const escrowPda = (orderId: BN, buyer: PublicKey, seller: PublicKey) =>
    PublicKey.findProgramAddressSync(
      [Buffer.from("escrow"), orderId.toArrayLike(Buffer, "le", 8), buyer.toBuffer(), seller.toBuffer()],
      program.programId,
    )[0];

  const vaultPda = (escrow: PublicKey) =>
    PublicKey.findProgramAddressSync([Buffer.from("vault"), escrow.toBuffer()], program.programId)[0];

  async function warpBy(seconds: number) {
    const clock = await context.banksClient.getClock();
    context.setClock(
      new Clock(
        clock.slot + 1n,
        clock.epochStartTimestamp,
        clock.epoch,
        clock.leaderScheduleEpoch,
        clock.unixTimestamp + BigInt(seconds),
      ),
    );
  }

  function expectError(e: any, name: string) {
    const code = e?.error?.errorCode?.code;
    const blob = `${code ?? ""} ${String(e)} ${JSON.stringify(e?.logs ?? e?.transactionLogs ?? "")}`;
    assert.include(blob, name, `expected error "${name}", got: ${blob}`);
  }

  // A fresh, funded buyer/seller pair with token accounts for `mint`.
  async function newParties(fund = 300_000_000) {
    const buyer = Keypair.generate();
    const seller = Keypair.generate();
    await fundSol(buyer.publicKey, 5);
    await fundSol(seller.publicKey, 1);
    const buyerToken = await ataWithBalance(mint, buyer.publicKey, fund, mintAuthority);
    const sellerToken = await ata(mint, seller.publicKey);
    return { buyer, seller, buyerToken, sellerToken };
  }

  // create_and_fund for a fresh deal; returns the pdas + accounts.
  async function createDeal(opts: {
    buyer: Keypair;
    seller: Keypair;
    buyerToken: PublicKey;
    amount: number;
    instant: boolean;
  }) {
    const orderId = nextOrderId();
    const escrow = escrowPda(orderId, opts.buyer.publicKey, opts.seller.publicKey);
    const vault = vaultPda(escrow);
    await program.methods
      .createAndFund(orderId, new BN(opts.amount), opts.instant)
      .accountsPartial({
        config: configPda,
        escrow,
        vault,
        buyer: opts.buyer.publicKey,
        seller: opts.seller.publicKey,
        buyerToken: opts.buyerToken,
        usdcMint: mint,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      })
      .signers([opts.buyer])
      .rpc();
    return { orderId, escrow, vault };
  }

  // ---- setup ---------------------------------------------------------------
  before(async () => {
    context = await startAnchor("", [], []);
    provider = new BankrunProvider(context);
    anchor.setProvider(provider);
    program = new Program<VydexEscrow>(idl as VydexEscrow, provider);
    payer = context.payer;

    mintAuthority = Keypair.generate();
    platform = Keypair.generate();
    admin = Keypair.generate();
    arbiter = Keypair.generate();
    await fundSol(admin.publicKey, 1);
    await fundSol(arbiter.publicKey, 1);

    mint = await createMintFor(mintAuthority);
    wrongMint = await createMintFor(mintAuthority);
    feeVault = await ata(mint, platform.publicKey);

    [configPda] = PublicKey.findProgramAddressSync([Buffer.from("config")], program.programId);

    await program.methods
      .initializeConfig(FEE_BPS, new BN(REVIEW_PERIOD), new BN(DELIVERY_DEADLINE), new BN(MAX_DEAL))
      .accountsPartial({
        config: configPda,
        admin: admin.publicKey,
        arbiter: arbiter.publicKey,
        usdcMint: mint,
        feeVault,
        payer: payer.publicKey,
        systemProgram: SystemProgram.programId,
      })
      .rpc();
  });

  // ======================================================================
  // HAPPY PATHS
  // ======================================================================
  describe("happy paths", () => {
    it("repo purchase: create_and_fund(instant) → approve_and_release WITHOUT tip", async () => {
      const p = await newParties();
      const amount = 50_000_000;
      const { escrow, vault } = await createDeal({ ...p, amount, instant: true });

      // TEMP DEBUG: compare raw on-chain bytes vs the client's decoded view.
      const rawAcc = await context.banksClient.getAccount(escrow);
      const hex = Buffer.from(rawAcc!.data).toString("hex");
      const dbg = await program.account.escrow.fetch(escrow);
      console.log("DBG escrow:", escrow.toBase58());
      console.log("DBG len:", rawAcc!.data.length, "raw:", hex);
      console.log("DBG fetch status:", JSON.stringify(dbg.status), "deliveredAt:", dbg.deliveredAt.toString());

      const feeBefore = await balance(feeVault);
      await program.methods
        .approveAndRelease(new BN(0))
        .accountsPartial({
          config: configPda,
          escrow,
          vault,
          buyer: p.buyer.publicKey,
          seller: p.seller.publicKey,
          buyerToken: p.buyerToken,
          sellerToken: p.sellerToken,
          feeVault,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([p.buyer])
        .rpc();

      const fee = Math.floor((amount * FEE_BPS) / 10_000);
      assert.equal(await balance(p.sellerToken), BigInt(amount - fee));
      assert.equal((await balance(feeVault)) - feeBefore, BigInt(fee));
      assert.isFalse(await exists(vault), "vault must be closed");
      const acc = await program.account.escrow.fetch(escrow);
      assert.deepEqual(acc.status, { released: {} });
    });

    it("repo purchase: approve_and_release WITH tip (no fee on tip)", async () => {
      const p = await newParties();
      const amount = 40_000_000;
      const tip = 3_000_000;
      const { escrow, vault } = await createDeal({ ...p, amount, instant: true });

      await program.methods
        .approveAndRelease(new BN(tip))
        .accountsPartial({
          config: configPda,
          escrow,
          vault,
          buyer: p.buyer.publicKey,
          seller: p.seller.publicKey,
          buyerToken: p.buyerToken,
          sellerToken: p.sellerToken,
          feeVault,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([p.buyer])
        .rpc();

      const fee = Math.floor((amount * FEE_BPS) / 10_000);
      // seller gets (amount - fee) from vault + full tip directly
      assert.equal(await balance(p.sellerToken), BigInt(amount - fee + tip));
    });

    it("order: create_and_fund(false) → mark_delivered → approve_and_release", async () => {
      const p = await newParties();
      const amount = 60_000_000;
      const { escrow, vault } = await createDeal({ ...p, amount, instant: false });

      let acc = await program.account.escrow.fetch(escrow);
      assert.deepEqual(acc.status, { funded: {} });

      await program.methods
        .markDelivered()
        .accountsPartial({ escrow, seller: p.seller.publicKey })
        .signers([p.seller])
        .rpc();
      acc = await program.account.escrow.fetch(escrow);
      assert.deepEqual(acc.status, { delivered: {} });

      await program.methods
        .approveAndRelease(new BN(0))
        .accountsPartial({
          config: configPda,
          escrow,
          vault,
          buyer: p.buyer.publicKey,
          seller: p.seller.publicKey,
          buyerToken: p.buyerToken,
          sellerToken: p.sellerToken,
          feeVault,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([p.buyer])
        .rpc();

      const fee = Math.floor((amount * FEE_BPS) / 10_000);
      assert.equal(await balance(p.sellerToken), BigInt(amount - fee));
    });

    it("release_after_timeout after review_period (clock warp)", async () => {
      const p = await newParties();
      const amount = 30_000_000;
      const { escrow, vault } = await createDeal({ ...p, amount, instant: true });

      await warpBy(REVIEW_PERIOD + 10);
      // permissionless: signed by an unrelated crank wallet
      await program.methods
        .releaseAfterTimeout()
        .accountsPartial({
          config: configPda,
          escrow,
          vault,
          buyer: p.buyer.publicKey,
          seller: p.seller.publicKey,
          sellerToken: p.sellerToken,
          feeVault,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .rpc();

      const fee = Math.floor((amount * FEE_BPS) / 10_000);
      assert.equal(await balance(p.sellerToken), BigInt(amount - fee));
      assert.isFalse(await exists(vault));
    });

    it("refund_undelivered after delivery_deadline (clock warp)", async () => {
      const p = await newParties();
      const amount = 25_000_000;
      const { escrow, vault } = await createDeal({ ...p, amount, instant: false });
      const buyerBefore = await balance(p.buyerToken);

      await warpBy(DELIVERY_DEADLINE + 10);
      await program.methods
        .refundUndelivered()
        .accountsPartial({
          config: configPda,
          escrow,
          vault,
          buyer: p.buyer.publicKey,
          buyerToken: p.buyerToken,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .rpc();

      assert.equal((await balance(p.buyerToken)) - buyerBefore, BigInt(amount));
      assert.isFalse(await exists(vault));
      const acc = await program.account.escrow.fetch(escrow);
      assert.deepEqual(acc.status, { refunded: {} });
    });

    for (const sellerBps of [0, 10_000, 5_000]) {
      it(`open_dispute → resolve_dispute with seller_bps=${sellerBps} (amounts + fee exact)`, async () => {
        const p = await newParties();
        const amount = 80_000_000;
        const { escrow, vault } = await createDeal({ ...p, amount, instant: true });
        const buyerBefore = await balance(p.buyerToken);
        const feeBefore = await balance(feeVault);

        await program.methods
          .openDispute()
          .accountsPartial({ escrow, party: p.buyer.publicKey })
          .signers([p.buyer])
          .rpc();

        await program.methods
          .resolveDispute(sellerBps)
          .accountsPartial({
            config: configPda,
            arbiter: arbiter.publicKey,
            escrow,
            vault,
            buyer: p.buyer.publicKey,
            seller: p.seller.publicKey,
            buyerToken: p.buyerToken,
            sellerToken: p.sellerToken,
            feeVault,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([arbiter])
          .rpc();

        const sellerGross = Math.floor((amount * sellerBps) / 10_000);
        const buyerShare = amount - sellerGross;
        const fee = Math.floor((sellerGross * FEE_BPS) / 10_000);
        const sellerNet = sellerGross - fee;

        assert.equal(await balance(p.sellerToken), BigInt(sellerNet), "seller net");
        assert.equal((await balance(p.buyerToken)) - buyerBefore, BigInt(buyerShare), "buyer share");
        assert.equal((await balance(feeVault)) - feeBefore, BigInt(fee), "fee");
        // INVARIANT: every unit of escrow.amount is accounted for
        assert.equal(sellerNet + fee + buyerShare, amount, "sum == amount");
        assert.isFalse(await exists(vault), "vault closed");
      });
    }
  });

  // ======================================================================
  // ATTACKS — must all fail with the right error
  // ======================================================================
  describe("attacks", () => {
    it("stranger cannot approve_and_release", async () => {
      const p = await newParties();
      const { escrow, vault } = await createDeal({ ...p, amount: 20_000_000, instant: true });
      const attacker = Keypair.generate();
      await fundSol(attacker.publicKey, 1);
      const attackerToken = await ata(mint, attacker.publicKey);
      try {
        await program.methods
          .approveAndRelease(new BN(0))
          .accountsPartial({
            config: configPda,
            escrow,
            vault,
            buyer: attacker.publicKey, // not escrow.buyer
            seller: p.seller.publicKey,
            buyerToken: attackerToken,
            sellerToken: p.sellerToken,
            feeVault,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([attacker])
          .rpc();
        assert.fail("should have thrown");
      } catch (e) {
        expectError(e, "Unauthorized");
      }
    });

    it("stranger cannot mark_delivered", async () => {
      const p = await newParties();
      const { escrow } = await createDeal({ ...p, amount: 20_000_000, instant: false });
      const attacker = Keypair.generate();
      await fundSol(attacker.publicKey, 1);
      try {
        await program.methods
          .markDelivered()
          .accountsPartial({ escrow, seller: attacker.publicKey })
          .signers([attacker])
          .rpc();
        assert.fail("should have thrown");
      } catch (e) {
        expectError(e, "Unauthorized");
      }
    });

    it("stranger cannot resolve_dispute", async () => {
      const p = await newParties();
      const { escrow, vault } = await createDeal({ ...p, amount: 20_000_000, instant: true });
      await program.methods
        .openDispute()
        .accountsPartial({ escrow, party: p.buyer.publicKey })
        .signers([p.buyer])
        .rpc();
      const attacker = Keypair.generate();
      await fundSol(attacker.publicKey, 1);
      try {
        await program.methods
          .resolveDispute(10_000)
          .accountsPartial({
            config: configPda,
            arbiter: attacker.publicKey, // not the config arbiter
            escrow,
            vault,
            buyer: p.buyer.publicKey,
            seller: p.seller.publicKey,
            buyerToken: p.buyerToken,
            sellerToken: p.sellerToken,
            feeVault,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([attacker])
          .rpc();
        assert.fail("should have thrown");
      } catch (e) {
        expectError(e, "Unauthorized");
      }
    });

    it("seller_token whose authority != seller is rejected", async () => {
      const p = await newParties();
      const { escrow, vault } = await createDeal({ ...p, amount: 20_000_000, instant: true });
      const evil = Keypair.generate();
      const evilToken = await ata(mint, evil.publicKey); // owned by attacker, not seller
      try {
        await program.methods
          .approveAndRelease(new BN(0))
          .accountsPartial({
            config: configPda,
            escrow,
            vault,
            buyer: p.buyer.publicKey,
            seller: p.seller.publicKey,
            buyerToken: p.buyerToken,
            sellerToken: evilToken,
            feeVault,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([p.buyer])
          .rpc();
        assert.fail("should have thrown");
      } catch (e) {
        expectError(e, "ConstraintTokenOwner");
      }
    });

    it("wrong mint (fake token) is rejected at create_and_fund", async () => {
      const buyer = Keypair.generate();
      const seller = Keypair.generate();
      await fundSol(buyer.publicKey, 5);
      const buyerWrongToken = await ataWithBalance(wrongMint, buyer.publicKey, 50_000_000, mintAuthority);
      const orderId = nextOrderId();
      const escrow = escrowPda(orderId, buyer.publicKey, seller.publicKey);
      const vault = vaultPda(escrow);
      try {
        await program.methods
          .createAndFund(orderId, new BN(10_000_000), true)
          .accountsPartial({
            config: configPda,
            escrow,
            vault,
            buyer: buyer.publicKey,
            seller: seller.publicKey,
            buyerToken: buyerWrongToken,
            usdcMint: wrongMint, // not config.usdc_mint
            tokenProgram: TOKEN_PROGRAM_ID,
            systemProgram: SystemProgram.programId,
          })
          .signers([buyer])
          .rpc();
        assert.fail("should have thrown");
      } catch (e) {
        expectError(e, "WrongMint");
      }
    });

    it("release_after_timeout BEFORE the deadline is rejected", async () => {
      const p = await newParties();
      const { escrow, vault } = await createDeal({ ...p, amount: 20_000_000, instant: true });
      try {
        await program.methods
          .releaseAfterTimeout()
          .accountsPartial({
            config: configPda,
            escrow,
            vault,
            buyer: p.buyer.publicKey,
            seller: p.seller.publicKey,
            sellerToken: p.sellerToken,
            feeVault,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .rpc();
        assert.fail("should have thrown");
      } catch (e) {
        expectError(e, "ReviewPeriodNotOver");
      }
    });

    it("open_dispute after the review period is rejected; and re-open is rejected", async () => {
      const p = await newParties();
      const { escrow } = await createDeal({ ...p, amount: 20_000_000, instant: true });

      // too late
      await warpBy(REVIEW_PERIOD + 10);
      try {
        await program.methods
          .openDispute()
          .accountsPartial({ escrow, party: p.buyer.publicKey })
          .signers([p.buyer])
          .rpc();
        assert.fail("should have thrown (too late)");
      } catch (e) {
        expectError(e, "TooLateForDispute");
      }

      // fresh deal, open once, then re-open must fail (status no longer Delivered)
      const p2 = await newParties();
      const d2 = await createDeal({ ...p2, amount: 20_000_000, instant: true });
      await program.methods
        .openDispute()
        .accountsPartial({ escrow: d2.escrow, party: p2.buyer.publicKey })
        .signers([p2.buyer])
        .rpc();
      try {
        await program.methods
          .openDispute()
          .accountsPartial({ escrow: d2.escrow, party: p2.seller.publicKey })
          .signers([p2.seller])
          .rpc();
        assert.fail("should have thrown (already disputed)");
      } catch (e) {
        expectError(e, "InvalidStatus");
      }
    });

    it("re-init: create_and_fund twice with same order_id+buyer+seller is rejected", async () => {
      const p = await newParties();
      const orderId = nextOrderId();
      const escrow = escrowPda(orderId, p.buyer.publicKey, p.seller.publicKey);
      const vault = vaultPda(escrow);
      const accounts = {
        config: configPda,
        escrow,
        vault,
        buyer: p.buyer.publicKey,
        seller: p.seller.publicKey,
        buyerToken: p.buyerToken,
        usdcMint: mint,
        tokenProgram: TOKEN_PROGRAM_ID,
        systemProgram: SystemProgram.programId,
      };
      await program.methods.createAndFund(orderId, new BN(10_000_000), true).accountsPartial(accounts).signers([p.buyer]).rpc();
      try {
        await program.methods.createAndFund(orderId, new BN(10_000_000), true).accountsPartial(accounts).signers([p.buyer]).rpc();
        assert.fail("should have thrown (already in use)");
      } catch (e) {
        // Anchor `init` on an existing account fails ("already in use").
        expectError(e, "already in use");
      }
    });

    it("double release: approve after Released is rejected", async () => {
      const p = await newParties();
      const { escrow, vault } = await createDeal({ ...p, amount: 20_000_000, instant: true });
      const doRelease = () =>
        program.methods
          .approveAndRelease(new BN(0))
          .accountsPartial({
            config: configPda,
            escrow,
            vault,
            buyer: p.buyer.publicKey,
            seller: p.seller.publicKey,
            buyerToken: p.buyerToken,
            sellerToken: p.sellerToken,
            feeVault,
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([p.buyer])
          .rpc();
      await doRelease();
      try {
        await doRelease();
        assert.fail("should have thrown");
      } catch (e) {
        // vault is closed after release → account no longer initialized
        expectError(e, "AccountNotInitialized");
      }
    });

    it("dispute on a Released deal is rejected (status guard)", async () => {
      const p = await newParties();
      const { escrow, vault } = await createDeal({ ...p, amount: 20_000_000, instant: true });
      await program.methods
        .approveAndRelease(new BN(0))
        .accountsPartial({
          config: configPda,
          escrow,
          vault,
          buyer: p.buyer.publicKey,
          seller: p.seller.publicKey,
          buyerToken: p.buyerToken,
          sellerToken: p.sellerToken,
          feeVault,
          tokenProgram: TOKEN_PROGRAM_ID,
        })
        .signers([p.buyer])
        .rpc();
      try {
        await program.methods
          .openDispute()
          .accountsPartial({ escrow, party: p.buyer.publicKey })
          .signers([p.buyer])
          .rpc();
        assert.fail("should have thrown");
      } catch (e) {
        expectError(e, "InvalidStatus");
      }
    });

    it("buyer == seller is rejected (SelfDeal)", async () => {
      const buyer = Keypair.generate();
      await fundSol(buyer.publicKey, 5);
      const buyerToken = await ataWithBalance(mint, buyer.publicKey, 50_000_000, mintAuthority);
      const orderId = nextOrderId();
      const escrow = escrowPda(orderId, buyer.publicKey, buyer.publicKey);
      const vault = vaultPda(escrow);
      try {
        await program.methods
          .createAndFund(orderId, new BN(10_000_000), true)
          .accountsPartial({
            config: configPda,
            escrow,
            vault,
            buyer: buyer.publicKey,
            seller: buyer.publicKey,
            buyerToken,
            usdcMint: mint,
            tokenProgram: TOKEN_PROGRAM_ID,
            systemProgram: SystemProgram.programId,
          })
          .signers([buyer])
          .rpc();
        assert.fail("should have thrown");
      } catch (e) {
        expectError(e, "SelfDeal");
      }
    });

    it("update_config by a non-admin is rejected", async () => {
      const attacker = Keypair.generate();
      await fundSol(attacker.publicKey, 1);
      try {
        await program.methods
          .updateConfig(600, null, null, null, null)
          .accountsPartial({ config: configPda, admin: attacker.publicKey })
          .signers([attacker])
          .rpc();
        assert.fail("should have thrown");
      } catch (e) {
        expectError(e, "Unauthorized");
      }
    });

    it("update_config with fee_bps > MAX is rejected", async () => {
      try {
        await program.methods
          .updateConfig(1_001, null, null, null, null)
          .accountsPartial({ config: configPda, admin: admin.publicKey })
          .signers([admin])
          .rpc();
        assert.fail("should have thrown");
      } catch (e) {
        expectError(e, "FeeTooHigh");
      }
    });

    it("substituting the fee_vault for a stranger account is rejected", async () => {
      const p = await newParties();
      const { escrow, vault } = await createDeal({ ...p, amount: 20_000_000, instant: true });
      const strangerFeeVault = await ata(mint, Keypair.generate().publicKey);
      try {
        await program.methods
          .approveAndRelease(new BN(0))
          .accountsPartial({
            config: configPda,
            escrow,
            vault,
            buyer: p.buyer.publicKey,
            seller: p.seller.publicKey,
            buyerToken: p.buyerToken,
            sellerToken: p.sellerToken,
            feeVault: strangerFeeVault, // not config.fee_vault
            tokenProgram: TOKEN_PROGRAM_ID,
          })
          .signers([p.buyer])
          .rpc();
        assert.fail("should have thrown");
      } catch (e) {
        expectError(e, "WrongFeeVault");
      }
    });

    it("create_and_fund above max_deal_amount is rejected (DealTooLarge)", async () => {
      const p = await newParties(MAX_DEAL + 100_000_000);
      const orderId = nextOrderId();
      const escrow = escrowPda(orderId, p.buyer.publicKey, p.seller.publicKey);
      const vault = vaultPda(escrow);
      try {
        await program.methods
          .createAndFund(orderId, new BN(MAX_DEAL + 1), true)
          .accountsPartial({
            config: configPda,
            escrow,
            vault,
            buyer: p.buyer.publicKey,
            seller: p.seller.publicKey,
            buyerToken: p.buyerToken,
            usdcMint: mint,
            tokenProgram: TOKEN_PROGRAM_ID,
            systemProgram: SystemProgram.programId,
          })
          .signers([p.buyer])
          .rpc();
        assert.fail("should have thrown");
      } catch (e) {
        expectError(e, "DealTooLarge");
      }
    });
  });
});
