// ============================================================================
// VYDEX ESCROW PROGRAM (Anchor / Solana)
// ============================================================================
// Архитектура: один изолированный PDA-vault на каждую сделку.
// Деньги (USDC) лежат в token account, authority которого — PDA эскроу.
// Приватного ключа к vault не существует; релиз возможен только по логике
// программы: approve покупателя / таймаут / решение арбитра (multisig).
//
// Статусы: Funded -> Delivered -> Released
//                 \-> Disputed -> Released / Refunded / Split
//                 \-> Refunded (если исполнитель не сдал работу в срок)
//
// ВАЖНО ДЛЯ ВНЕДРЕНИЯ (Claude Code): см. README-CLAUDE-CODE.md
// ============================================================================

use anchor_lang::prelude::*;
use anchor_spl::token::{self, Mint, Token, TokenAccount, Transfer, CloseAccount};

declare_id!("Vydx111111111111111111111111111111111111111"); // заменить после `anchor keys sync`

// --------------------------------------------------------------------------
// Константы
// --------------------------------------------------------------------------
pub const CONFIG_SEED: &[u8] = b"config";
pub const ESCROW_SEED: &[u8] = b"escrow";
pub const VAULT_SEED: &[u8] = b"vault";

pub const MAX_FEE_BPS: u16 = 1_000; // максимум 10% — защита от rug через update_config
pub const MIN_REVIEW_PERIOD: i64 = 60 * 60;          // 1 час (для тестов/мелких сделок)
pub const MAX_REVIEW_PERIOD: i64 = 30 * 24 * 60 * 60; // 30 дней
pub const BPS_DENOMINATOR: u64 = 10_000;

#[program]
pub mod vydex_escrow {
    use super::*;

    // ------------------------------------------------------------------
    // 1. Инициализация глобального конфига (вызывается один раз админом)
    // ------------------------------------------------------------------
    pub fn initialize_config(
        ctx: Context<InitializeConfig>,
        fee_bps: u16,
        default_review_period: i64,
        delivery_deadline: i64,
        max_deal_amount: u64, // ЛИМИТ на сделку до аудита, напр. 200_000_000 = 200 USDC
    ) -> Result<()> {
        require!(fee_bps <= MAX_FEE_BPS, EscrowError::FeeTooHigh);
        require!(max_deal_amount > 0, EscrowError::ZeroAmount);
        require!(
            (MIN_REVIEW_PERIOD..=MAX_REVIEW_PERIOD).contains(&default_review_period),
            EscrowError::InvalidReviewPeriod
        );

        let config = &mut ctx.accounts.config;
        config.admin = ctx.accounts.admin.key();       // ДОЛЖЕН быть Squads multisig
        config.arbiter = ctx.accounts.arbiter.key();   // ДОЛЖЕН быть Squads multisig
        config.usdc_mint = ctx.accounts.usdc_mint.key();
        config.fee_vault = ctx.accounts.fee_vault.key();
        config.fee_bps = fee_bps;
        config.default_review_period = default_review_period;
        config.delivery_deadline = delivery_deadline;
        config.max_deal_amount = max_deal_amount;
        config.paused = false;
        config.bump = ctx.bumps.config;
        Ok(())
    }

    // ------------------------------------------------------------------
    // 2. Обновление конфига (только admin-multisig)
    // ------------------------------------------------------------------
    pub fn update_config(
        ctx: Context<UpdateConfig>,
        fee_bps: Option<u16>,
        default_review_period: Option<i64>,
        arbiter: Option<Pubkey>,
        paused: Option<bool>,
        max_deal_amount: Option<u64>, // после аудита лимит поднимается admin-multisig'ом
    ) -> Result<()> {
        let config = &mut ctx.accounts.config;
        if let Some(fee) = fee_bps {
            require!(fee <= MAX_FEE_BPS, EscrowError::FeeTooHigh);
            config.fee_bps = fee;
        }
        if let Some(period) = default_review_period {
            require!(
                (MIN_REVIEW_PERIOD..=MAX_REVIEW_PERIOD).contains(&period),
                EscrowError::InvalidReviewPeriod
            );
            config.default_review_period = period;
        }
        if let Some(a) = arbiter {
            config.arbiter = a;
        }
        if let Some(m) = max_deal_amount {
            require!(m > 0, EscrowError::ZeroAmount);
            config.max_deal_amount = m;
        }
        if let Some(p) = paused {
            config.paused = p; // экстренная пауза: блокирует СОЗДАНИЕ новых эскроу,
                               // но НЕ блокирует release/refund существующих —
                               // пользователи всегда могут забрать свои деньги
        }
        Ok(())
    }

    // ------------------------------------------------------------------
    // 3. Создание и пополнение эскроу (атомарно, вызывает покупатель)
    //    instant_delivery = true для покупки готового репозитория
    //    (товар "сдан" в момент оплаты, таймер проверки стартует сразу)
    // ------------------------------------------------------------------
    pub fn create_and_fund(
        ctx: Context<CreateAndFund>,
        order_id: u64,
        amount: u64,
        instant_delivery: bool,
    ) -> Result<()> {
        require!(!ctx.accounts.config.paused, EscrowError::ProtocolPaused);
        require!(amount > 0, EscrowError::ZeroAmount);
        require!(
            amount <= ctx.accounts.config.max_deal_amount,
            EscrowError::DealTooLarge
        );
        require_keys_neq!(
            ctx.accounts.buyer.key(),
            ctx.accounts.seller.key(),
            EscrowError::SelfDeal
        );

        let now = Clock::get()?.unix_timestamp;
        let escrow = &mut ctx.accounts.escrow;

        escrow.order_id = order_id;
        escrow.buyer = ctx.accounts.buyer.key();
        escrow.seller = ctx.accounts.seller.key();
        escrow.amount = amount;
        escrow.tip = 0;
        escrow.created_at = now;
        escrow.review_period = ctx.accounts.config.default_review_period;
        escrow.bump = ctx.bumps.escrow;

        if instant_delivery {
            escrow.status = EscrowStatus::Delivered;
            escrow.delivered_at = now;
        } else {
            escrow.status = EscrowStatus::Funded;
            escrow.delivered_at = 0;
        }

        // Перевод USDC покупателя в vault
        token::transfer(
            CpiContext::new(
                ctx.accounts.token_program.key(),
                Transfer {
                    from: ctx.accounts.buyer_token.to_account_info(),
                    to: ctx.accounts.vault.to_account_info(),
                    authority: ctx.accounts.buyer.to_account_info(),
                },
            ),
            amount,
        )?;

        emit!(EscrowCreated {
            escrow: escrow.key(),
            order_id,
            buyer: escrow.buyer,
            seller: escrow.seller,
            amount,
            instant_delivery,
        });
        Ok(())
    }

    // ------------------------------------------------------------------
    // 4. Исполнитель отмечает работу сданной (для заказов)
    // ------------------------------------------------------------------
    pub fn mark_delivered(ctx: Context<MarkDelivered>) -> Result<()> {
        let escrow = &mut ctx.accounts.escrow;
        require!(escrow.status == EscrowStatus::Funded, EscrowError::InvalidStatus);

        escrow.status = EscrowStatus::Delivered;
        escrow.delivered_at = Clock::get()?.unix_timestamp;

        emit!(Delivered { escrow: escrow.key(), delivered_at: escrow.delivered_at });
        Ok(())
    }

    // ------------------------------------------------------------------
    // 5. Апрув покупателем + опциональные чаевые -> немедленная выплата
    //    tip_amount переводится с кошелька покупателя отдельным transfer,
    //    комиссия платформы с чаевых НЕ берётся
    // ------------------------------------------------------------------
    pub fn approve_and_release(ctx: Context<ApproveAndRelease>, tip_amount: u64) -> Result<()> {
        require!(
            ctx.accounts.escrow.status == EscrowStatus::Delivered,
            EscrowError::InvalidStatus
        );

        // Чаевые: buyer -> seller напрямую (не через vault)
        if tip_amount > 0 {
            token::transfer(
                CpiContext::new(
                    ctx.accounts.token_program.key(),
                    Transfer {
                        from: ctx.accounts.buyer_token.to_account_info(),
                        to: ctx.accounts.seller_token.to_account_info(),
                        authority: ctx.accounts.buyer.to_account_info(),
                    },
                ),
                tip_amount,
            )?;
            ctx.accounts.escrow.tip = tip_amount;
        }

        payout_to_seller(
            &ctx.accounts.escrow,
            &ctx.accounts.vault,
            &ctx.accounts.seller_token,
            &ctx.accounts.fee_vault,
            &ctx.accounts.config,
            &ctx.accounts.token_program,
            &ctx.accounts.buyer.to_account_info(), // rent от закрытия vault -> покупателю
        )?;

        let escrow = &mut ctx.accounts.escrow;
        escrow.status = EscrowStatus::Released;

        emit!(Released {
            escrow: escrow.key(),
            reason: ReleaseReason::BuyerApproved,
            tip: tip_amount,
        });
        Ok(())
    }

    // ------------------------------------------------------------------
    // 6. Релиз по таймауту (permissionless — может вызвать кто угодно,
    //    например ваш backend-crank или сам продавец)
    // ------------------------------------------------------------------
    pub fn release_after_timeout(ctx: Context<ReleaseAfterTimeout>) -> Result<()> {
        let escrow = &ctx.accounts.escrow;
        require!(escrow.status == EscrowStatus::Delivered, EscrowError::InvalidStatus);

        let now = Clock::get()?.unix_timestamp;
        let deadline = escrow
            .delivered_at
            .checked_add(escrow.review_period)
            .ok_or(EscrowError::MathOverflow)?;
        require!(now >= deadline, EscrowError::ReviewPeriodNotOver);

        payout_to_seller(
            &ctx.accounts.escrow,
            &ctx.accounts.vault,
            &ctx.accounts.seller_token,
            &ctx.accounts.fee_vault,
            &ctx.accounts.config,
            &ctx.accounts.token_program,
            &ctx.accounts.buyer.to_account_info(),
        )?;

        let escrow = &mut ctx.accounts.escrow;
        escrow.status = EscrowStatus::Released;

        emit!(Released {
            escrow: escrow.key(),
            reason: ReleaseReason::Timeout,
            tip: 0,
        });
        Ok(())
    }

    // ------------------------------------------------------------------
    // 7. Возврат покупателю, если исполнитель НЕ сдал работу в срок
    // ------------------------------------------------------------------
    pub fn refund_undelivered(ctx: Context<RefundUndelivered>) -> Result<()> {
        let escrow = &ctx.accounts.escrow;
        require!(escrow.status == EscrowStatus::Funded, EscrowError::InvalidStatus);

        let now = Clock::get()?.unix_timestamp;
        let deadline = escrow
            .created_at
            .checked_add(ctx.accounts.config.delivery_deadline)
            .ok_or(EscrowError::MathOverflow)?;
        require!(now >= deadline, EscrowError::DeliveryDeadlineNotOver);

        refund_to_buyer(
            &ctx.accounts.escrow,
            &ctx.accounts.vault,
            &ctx.accounts.buyer_token,
            &ctx.accounts.token_program,
            &ctx.accounts.buyer.to_account_info(),
        )?;

        let escrow = &mut ctx.accounts.escrow;
        escrow.status = EscrowStatus::Refunded;

        emit!(Refunded { escrow: escrow.key(), reason: RefundReason::NotDelivered });
        Ok(())
    }

    // ------------------------------------------------------------------
    // 8. Открыть диспут (buyer или seller, только в статусе Delivered,
    //    до истечения review_period)
    // ------------------------------------------------------------------
    pub fn open_dispute(ctx: Context<OpenDispute>) -> Result<()> {
        let escrow = &mut ctx.accounts.escrow;
        require!(escrow.status == EscrowStatus::Delivered, EscrowError::InvalidStatus);

        let caller = ctx.accounts.party.key();
        require!(
            caller == escrow.buyer || caller == escrow.seller,
            EscrowError::Unauthorized
        );

        let now = Clock::get()?.unix_timestamp;
        let deadline = escrow
            .delivered_at
            .checked_add(escrow.review_period)
            .ok_or(EscrowError::MathOverflow)?;
        require!(now < deadline, EscrowError::TooLateForDispute);

        escrow.status = EscrowStatus::Disputed;
        emit!(DisputeOpened { escrow: escrow.key(), opened_by: caller });
        Ok(())
    }

    // ------------------------------------------------------------------
    // 9. Решение диспута арбитром (Squads multisig).
    //    seller_bps: доля продавца в bps (0 = полный возврат покупателю,
    //    10000 = полная выплата продавцу, между — сплит).
    //    Комиссия платформы берётся только с доли продавца.
    // ------------------------------------------------------------------
    pub fn resolve_dispute(ctx: Context<ResolveDispute>, seller_bps: u16) -> Result<()> {
        require!(
            ctx.accounts.escrow.status == EscrowStatus::Disputed,
            EscrowError::InvalidStatus
        );
        require!(seller_bps as u64 <= BPS_DENOMINATOR, EscrowError::InvalidBps);

        let escrow_info = ctx.accounts.escrow.to_account_info();
        let escrow = &ctx.accounts.escrow;

        let seller_gross = (escrow.amount as u128)
            .checked_mul(seller_bps as u128)
            .ok_or(EscrowError::MathOverflow)?
            .checked_div(BPS_DENOMINATOR as u128)
            .ok_or(EscrowError::MathOverflow)? as u64;
        let buyer_share = escrow
            .amount
            .checked_sub(seller_gross)
            .ok_or(EscrowError::MathOverflow)?;

        let fee = (seller_gross as u128)
            .checked_mul(ctx.accounts.config.fee_bps as u128)
            .ok_or(EscrowError::MathOverflow)?
            .checked_div(BPS_DENOMINATOR as u128)
            .ok_or(EscrowError::MathOverflow)? as u64;
        let seller_net = seller_gross.checked_sub(fee).ok_or(EscrowError::MathOverflow)?;

        let order_id_bytes = escrow.order_id.to_le_bytes();
        let seeds: &[&[u8]] = &[
            ESCROW_SEED,
            order_id_bytes.as_ref(),
            escrow.buyer.as_ref(),
            escrow.seller.as_ref(),
            &[escrow.bump],
        ];
        let signer_seeds = &[seeds];

        if seller_net > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.key(),
                    Transfer {
                        from: ctx.accounts.vault.to_account_info(),
                        to: ctx.accounts.seller_token.to_account_info(),
                        authority: escrow_info.clone(),
                    },
                    signer_seeds,
                ),
                seller_net,
            )?;
        }
        if fee > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.key(),
                    Transfer {
                        from: ctx.accounts.vault.to_account_info(),
                        to: ctx.accounts.fee_vault.to_account_info(),
                        authority: escrow_info.clone(),
                    },
                    signer_seeds,
                ),
                fee,
            )?;
        }
        if buyer_share > 0 {
            token::transfer(
                CpiContext::new_with_signer(
                    ctx.accounts.token_program.key(),
                    Transfer {
                        from: ctx.accounts.vault.to_account_info(),
                        to: ctx.accounts.buyer_token.to_account_info(),
                        authority: escrow_info.clone(),
                    },
                    signer_seeds,
                ),
                buyer_share,
            )?;
        }

        // Закрыть vault, rent -> покупателю (он платил за создание)
        token::close_account(CpiContext::new_with_signer(
            ctx.accounts.token_program.key(),
            CloseAccount {
                account: ctx.accounts.vault.to_account_info(),
                destination: ctx.accounts.buyer.to_account_info(),
                authority: escrow_info,
            },
            signer_seeds,
        ))?;

        let escrow = &mut ctx.accounts.escrow;
        escrow.status = if seller_bps == 0 {
            EscrowStatus::Refunded
        } else {
            EscrowStatus::Released
        };

        emit!(DisputeResolved { escrow: escrow.key(), seller_bps });
        Ok(())
    }
}

// --------------------------------------------------------------------------
// Внутренние хелперы выплат (общая логика approve / timeout)
// --------------------------------------------------------------------------
fn payout_to_seller<'info>(
    escrow: &Account<'info, Escrow>,
    vault: &Account<'info, TokenAccount>,
    seller_token: &Account<'info, TokenAccount>,
    fee_vault: &Account<'info, TokenAccount>,
    config: &Account<'info, Config>,
    token_program: &Program<'info, Token>,
    rent_destination: &AccountInfo<'info>,
) -> Result<()> {
    let fee = (escrow.amount as u128)
        .checked_mul(config.fee_bps as u128)
        .ok_or(EscrowError::MathOverflow)?
        .checked_div(BPS_DENOMINATOR as u128)
        .ok_or(EscrowError::MathOverflow)? as u64;
    let seller_net = escrow.amount.checked_sub(fee).ok_or(EscrowError::MathOverflow)?;

    let order_id_bytes = escrow.order_id.to_le_bytes();
    let seeds: &[&[u8]] = &[
        ESCROW_SEED,
        order_id_bytes.as_ref(),
        escrow.buyer.as_ref(),
        escrow.seller.as_ref(),
        &[escrow.bump],
    ];
    let signer_seeds = &[seeds];

    token::transfer(
        CpiContext::new_with_signer(
            token_program.key(),
            Transfer {
                from: vault.to_account_info(),
                to: seller_token.to_account_info(),
                authority: escrow.to_account_info(),
            },
            signer_seeds,
        ),
        seller_net,
    )?;

    if fee > 0 {
        token::transfer(
            CpiContext::new_with_signer(
                token_program.key(),
                Transfer {
                    from: vault.to_account_info(),
                    to: fee_vault.to_account_info(),
                    authority: escrow.to_account_info(),
                },
                signer_seeds,
            ),
            fee,
        )?;
    }

    token::close_account(CpiContext::new_with_signer(
        token_program.key(),
        CloseAccount {
            account: vault.to_account_info(),
            destination: rent_destination.clone(),
            authority: escrow.to_account_info(),
        },
        signer_seeds,
    ))?;
    Ok(())
}

fn refund_to_buyer<'info>(
    escrow: &Account<'info, Escrow>,
    vault: &Account<'info, TokenAccount>,
    buyer_token: &Account<'info, TokenAccount>,
    token_program: &Program<'info, Token>,
    rent_destination: &AccountInfo<'info>,
) -> Result<()> {
    let order_id_bytes = escrow.order_id.to_le_bytes();
    let seeds: &[&[u8]] = &[
        ESCROW_SEED,
        order_id_bytes.as_ref(),
        escrow.buyer.as_ref(),
        escrow.seller.as_ref(),
        &[escrow.bump],
    ];
    let signer_seeds = &[seeds];

    token::transfer(
        CpiContext::new_with_signer(
            token_program.key(),
            Transfer {
                from: vault.to_account_info(),
                to: buyer_token.to_account_info(),
                authority: escrow.to_account_info(),
            },
            signer_seeds,
        ),
        escrow.amount,
    )?;

    token::close_account(CpiContext::new_with_signer(
        token_program.key(),
        CloseAccount {
            account: vault.to_account_info(),
            destination: rent_destination.clone(),
            authority: escrow.to_account_info(),
        },
        signer_seeds,
    ))?;
    Ok(())
}

// --------------------------------------------------------------------------
// Accounts (контексты инструкций) — здесь живёт БОЛЬШАЯ часть безопасности
// --------------------------------------------------------------------------

#[derive(Accounts)]
pub struct InitializeConfig<'info> {
    #[account(
        init,
        payer = payer,
        space = 8 + Config::INIT_SPACE,
        seeds = [CONFIG_SEED],
        bump
    )]
    pub config: Account<'info, Config>,

    /// CHECK: Squads multisig — админ протокола. Не подписывает init,
    /// но записывается как единственный, кто может менять конфиг.
    pub admin: UncheckedAccount<'info>,

    /// CHECK: Squads multisig — арбитр диспутов
    pub arbiter: UncheckedAccount<'info>,

    pub usdc_mint: Account<'info, Mint>,

    #[account(token::mint = usdc_mint)]
    pub fee_vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub payer: Signer<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct UpdateConfig<'info> {
    #[account(
        mut,
        seeds = [CONFIG_SEED],
        bump = config.bump,
        has_one = admin @ EscrowError::Unauthorized
    )]
    pub config: Account<'info, Config>,
    pub admin: Signer<'info>,
}

#[derive(Accounts)]
#[instruction(order_id: u64)]
pub struct CreateAndFund<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(
        init, // НЕ init_if_needed — защита от re-initialization атаки
        payer = buyer,
        space = 8 + Escrow::INIT_SPACE,
        seeds = [ESCROW_SEED, order_id.to_le_bytes().as_ref(), buyer.key().as_ref(), seller.key().as_ref()],
        bump
    )]
    pub escrow: Account<'info, Escrow>,

    #[account(
        init,
        payer = buyer,
        seeds = [VAULT_SEED, escrow.key().as_ref()],
        bump,
        token::mint = usdc_mint,      // защита от подмены mint (fake token attack)
        token::authority = escrow      // владелец vault — только PDA эскроу
    )]
    pub vault: Account<'info, TokenAccount>,

    #[account(mut)]
    pub buyer: Signer<'info>,

    /// CHECK: продавец — только адрес для записи, подпись не нужна
    pub seller: UncheckedAccount<'info>,

    #[account(
        mut,
        token::mint = usdc_mint,
        token::authority = buyer
    )]
    pub buyer_token: Account<'info, TokenAccount>,

    #[account(address = config.usdc_mint @ EscrowError::WrongMint)]
    pub usdc_mint: Account<'info, Mint>,

    pub token_program: Program<'info, Token>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct MarkDelivered<'info> {
    #[account(
        mut,
        seeds = [ESCROW_SEED, escrow.order_id.to_le_bytes().as_ref(), escrow.buyer.as_ref(), escrow.seller.as_ref()],
        bump = escrow.bump,
        has_one = seller @ EscrowError::Unauthorized
    )]
    pub escrow: Account<'info, Escrow>,
    pub seller: Signer<'info>,
}

// NOTE: the Account<..> wrappers here are Box'ed. Deserializing six accounts on
// the 4KB BPF stack overflowed it (`try_accounts` frame ~4480 bytes), which is
// undefined behaviour and in practice corrupted the escrow data. Boxing moves the
// deserialized data to the heap. No constraint is changed by this.
#[derive(Accounts)]
pub struct ApproveAndRelease<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = fee_vault @ EscrowError::WrongFeeVault)]
    pub config: Box<Account<'info, Config>>,

    #[account(
        mut,
        seeds = [ESCROW_SEED, escrow.order_id.to_le_bytes().as_ref(), escrow.buyer.as_ref(), escrow.seller.as_ref()],
        bump = escrow.bump,
        has_one = buyer @ EscrowError::Unauthorized,
        has_one = seller @ EscrowError::Unauthorized
    )]
    pub escrow: Box<Account<'info, Escrow>>,

    #[account(
        mut,
        seeds = [VAULT_SEED, escrow.key().as_ref()],
        bump,
        token::authority = escrow
    )]
    pub vault: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub buyer: Signer<'info>, // релизит ТОЛЬКО покупатель

    /// CHECK: адрес продавца из escrow (has_one)
    pub seller: UncheckedAccount<'info>,

    #[account(
        mut,
        token::mint = config.usdc_mint,
        token::authority = buyer
    )]
    pub buyer_token: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        token::mint = config.usdc_mint,
        token::authority = seller // деньги могут уйти ТОЛЬКО на счёт продавца
    )]
    pub seller_token: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub fee_vault: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct ReleaseAfterTimeout<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump, has_one = fee_vault @ EscrowError::WrongFeeVault)]
    pub config: Account<'info, Config>,

    #[account(
        mut,
        seeds = [ESCROW_SEED, escrow.order_id.to_le_bytes().as_ref(), escrow.buyer.as_ref(), escrow.seller.as_ref()],
        bump = escrow.bump,
        has_one = buyer @ EscrowError::Unauthorized,
        has_one = seller @ EscrowError::Unauthorized
    )]
    pub escrow: Account<'info, Escrow>,

    #[account(
        mut,
        seeds = [VAULT_SEED, escrow.key().as_ref()],
        bump,
        token::authority = escrow
    )]
    pub vault: Account<'info, TokenAccount>,

    /// CHECK: rent от vault возвращается покупателю
    #[account(mut)]
    pub buyer: UncheckedAccount<'info>,

    /// CHECK: адрес продавца из escrow (has_one)
    pub seller: UncheckedAccount<'info>,

    #[account(
        mut,
        token::mint = config.usdc_mint,
        token::authority = seller
    )]
    pub seller_token: Account<'info, TokenAccount>,

    #[account(mut)]
    pub fee_vault: Account<'info, TokenAccount>,

    // crank: кто угодно может вызвать после дедлайна, подпись не требуется
    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct RefundUndelivered<'info> {
    #[account(seeds = [CONFIG_SEED], bump = config.bump)]
    pub config: Account<'info, Config>,

    #[account(
        mut,
        seeds = [ESCROW_SEED, escrow.order_id.to_le_bytes().as_ref(), escrow.buyer.as_ref(), escrow.seller.as_ref()],
        bump = escrow.bump,
        has_one = buyer @ EscrowError::Unauthorized
    )]
    pub escrow: Account<'info, Escrow>,

    #[account(
        mut,
        seeds = [VAULT_SEED, escrow.key().as_ref()],
        bump,
        token::authority = escrow
    )]
    pub vault: Account<'info, TokenAccount>,

    /// CHECK: rent-получатель; сам возврат идёт на buyer_token
    #[account(mut)]
    pub buyer: UncheckedAccount<'info>,

    #[account(
        mut,
        token::mint = config.usdc_mint,
        token::authority = buyer
    )]
    pub buyer_token: Account<'info, TokenAccount>,

    pub token_program: Program<'info, Token>,
}

#[derive(Accounts)]
pub struct OpenDispute<'info> {
    #[account(
        mut,
        seeds = [ESCROW_SEED, escrow.order_id.to_le_bytes().as_ref(), escrow.buyer.as_ref(), escrow.seller.as_ref()],
        bump = escrow.bump
    )]
    pub escrow: Account<'info, Escrow>,
    pub party: Signer<'info>, // проверка buyer/seller — в теле инструкции
}

#[derive(Accounts)]
pub struct ResolveDispute<'info> {
    #[account(
        seeds = [CONFIG_SEED],
        bump = config.bump,
        has_one = arbiter @ EscrowError::Unauthorized,
        has_one = fee_vault @ EscrowError::WrongFeeVault
    )]
    pub config: Box<Account<'info, Config>>,

    pub arbiter: Signer<'info>, // Squads multisig подписывает через свой vault-PDA

    #[account(
        mut,
        seeds = [ESCROW_SEED, escrow.order_id.to_le_bytes().as_ref(), escrow.buyer.as_ref(), escrow.seller.as_ref()],
        bump = escrow.bump,
        has_one = buyer @ EscrowError::Unauthorized,
        has_one = seller @ EscrowError::Unauthorized
    )]
    pub escrow: Box<Account<'info, Escrow>>,

    #[account(
        mut,
        seeds = [VAULT_SEED, escrow.key().as_ref()],
        bump,
        token::authority = escrow
    )]
    pub vault: Box<Account<'info, TokenAccount>>,

    /// CHECK: rent-получатель + получатель buyer_share
    #[account(mut)]
    pub buyer: UncheckedAccount<'info>,

    /// CHECK: адрес продавца из escrow (has_one)
    pub seller: UncheckedAccount<'info>,

    #[account(
        mut,
        token::mint = config.usdc_mint,
        token::authority = buyer
    )]
    pub buyer_token: Box<Account<'info, TokenAccount>>,

    #[account(
        mut,
        token::mint = config.usdc_mint,
        token::authority = seller
    )]
    pub seller_token: Box<Account<'info, TokenAccount>>,

    #[account(mut)]
    pub fee_vault: Box<Account<'info, TokenAccount>>,

    pub token_program: Program<'info, Token>,
}

// --------------------------------------------------------------------------
// State
// --------------------------------------------------------------------------

#[account]
#[derive(InitSpace)]
pub struct Config {
    pub admin: Pubkey,                 // Squads multisig
    pub arbiter: Pubkey,               // Squads multisig
    pub usdc_mint: Pubkey,
    pub fee_vault: Pubkey,
    pub fee_bps: u16,                  // комиссия платформы, напр. 500 = 5%
    pub default_review_period: i64,    // секунды, напр. 259200 = 3 дня
    pub delivery_deadline: i64,        // срок сдачи работы до авто-возврата
    pub max_deal_amount: u64,          // потолок суммы сделки (риск-лимит до аудита)
    pub paused: bool,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Escrow {
    pub order_id: u64,
    pub buyer: Pubkey,
    pub seller: Pubkey,
    pub amount: u64,          // в минимальных единицах USDC (6 decimals)
    pub tip: u64,
    pub created_at: i64,
    pub delivered_at: i64,
    pub review_period: i64,   // фиксируется при создании — update_config не влияет на живые сделки
    pub status: EscrowStatus,
    pub bump: u8,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace)]
pub enum EscrowStatus {
    Funded,
    Delivered,
    Disputed,
    Released,
    Refunded,
}

// --------------------------------------------------------------------------
// Events (для индексации на бэкенде через Helius webhooks / geyser)
// --------------------------------------------------------------------------

#[event]
pub struct EscrowCreated {
    pub escrow: Pubkey,
    pub order_id: u64,
    pub buyer: Pubkey,
    pub seller: Pubkey,
    pub amount: u64,
    pub instant_delivery: bool,
}

#[event]
pub struct Delivered {
    pub escrow: Pubkey,
    pub delivered_at: i64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub enum ReleaseReason {
    BuyerApproved,
    Timeout,
}

#[event]
pub struct Released {
    pub escrow: Pubkey,
    pub reason: ReleaseReason,
    pub tip: u64,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy)]
pub enum RefundReason {
    NotDelivered,
}

#[event]
pub struct Refunded {
    pub escrow: Pubkey,
    pub reason: RefundReason,
}

#[event]
pub struct DisputeOpened {
    pub escrow: Pubkey,
    pub opened_by: Pubkey,
}

#[event]
pub struct DisputeResolved {
    pub escrow: Pubkey,
    pub seller_bps: u16,
}

// --------------------------------------------------------------------------
// Errors
// --------------------------------------------------------------------------

#[error_code]
pub enum EscrowError {
    #[msg("Fee exceeds maximum allowed")]
    FeeTooHigh,
    #[msg("Invalid review period")]
    InvalidReviewPeriod,
    #[msg("Protocol is paused")]
    ProtocolPaused,
    #[msg("Amount must be greater than zero")]
    ZeroAmount,
    #[msg("Buyer and seller cannot be the same wallet")]
    SelfDeal,
    #[msg("Invalid escrow status for this action")]
    InvalidStatus,
    #[msg("Unauthorized")]
    Unauthorized,
    #[msg("Review period is not over yet")]
    ReviewPeriodNotOver,
    #[msg("Delivery deadline is not over yet")]
    DeliveryDeadlineNotOver,
    #[msg("Too late to open a dispute")]
    TooLateForDispute,
    #[msg("Wrong token mint")]
    WrongMint,
    #[msg("Wrong fee vault")]
    WrongFeeVault,
    #[msg("Invalid basis points value")]
    InvalidBps,
    #[msg("Math overflow")]
    MathOverflow,
    #[msg("Deal amount exceeds current platform limit")]
    DealTooLarge,
}
