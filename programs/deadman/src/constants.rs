use anchor_lang::prelude::*;

pub const CONFIG_SEED: &[u8] = b"config";
pub const VAULT_SEED: &[u8] = b"vault";
pub const SKR_RATES_SEED: &[u8] = b"skr_rates";

pub const MAX_RULES: usize = 8;
/// Plan label length in bytes.
pub const MAX_LABEL_LEN: usize = 32;
pub const BPS_DENOMINATOR: u64 = 10_000;
/// Hard cap on the payout fee for any rail or mint: 5%.
pub const MAX_FEE_BPS: u16 = 500;
/// Release fee on every rail: 2%.
pub const DEFAULT_FEE_BPS: u16 = 200;
/// Release fee for payouts in SKR: 1.5%.
pub const DEFAULT_FEE_BPS_SKR: u16 = 150;
/// Share of an SKR fee that is burned: 10%.
pub const DEFAULT_SKR_BURN_BPS: u16 = 1_000;

pub const SECS_PER_DAY: i64 = 86_400;

/// Bounds on a tier's delay after the last check-in. One minute minimum so
/// the switch can be demoed live.
pub const MIN_RULE_DELAY_SECS: i64 = 60;
pub const MAX_RULE_DELAY_SECS: i64 = 3 * 366 * SECS_PER_DAY;
/// Bounds for the owner-chosen skip grace: a due tier that still cannot pay
/// that long after it fell due may be skipped by anyone, so one broken
/// destination cannot block later tiers forever.
pub const MIN_SKIP_GRACE_SECS: i64 = 60;
pub const MAX_SKIP_GRACE_SECS: i64 = 366 * SECS_PER_DAY;
/// Guard-key check-ins keep a plan alive for at most this long after the
/// owner's last wallet-signed action.
pub const MAX_GUARD_ONLY_SECS: i64 = 365 * SECS_PER_DAY;
pub const MIN_LOCK_SECS: i64 = 60;
pub const MAX_LOCK_SECS: i64 = 30 * SECS_PER_DAY;

/// Vesting schedules: up to this long from start to fully vested, and a
/// start at most this far in the past or future of creation.
pub const MAX_VEST_SECS: i64 = 20 * 366 * SECS_PER_DAY;
pub const MAX_VEST_START_SKEW_SECS: i64 = 366 * SECS_PER_DAY;
/// Shortest installment of a stepped vesting plan (0 = continuous).
pub const MIN_VEST_PERIOD_SECS: i64 = 60;

/// SOL sent along with token payouts on private rails so a fresh claim key
/// can pay to route its funds onward. A Cloak SPL deposit needs about
/// 0.01 SOL on the claim key plus fees; a Zcash (1Click) deposit needs the
/// deposit address's ATA rent plus fees.
pub const CLOAK_GAS_STIPEND: u64 = 12_000_000;
/// What a payout pays whoever runs it, from the plan's gas reserve: the
/// network fee with room for a priority fee.
pub const EXEC_TIP_LAMPORTS: u64 = 10_000;
/// SPL token account size: a token rule's first payout also refunds the
/// executor the rent of the beneficiary's token account.
pub const TOKEN_ACCOUNT_LEN: usize = 165;
/// A vesting release pays its executor only if it releases at least this
/// share of the schedule (1 / MAX_VEST_TIPS) or completes it, so one
/// schedule pays at most about this many tips however often it is called.
pub const MAX_VEST_TIPS: u64 = 100;
pub const ZCASH_GAS_STIPEND: u64 = 3_000_000;

/// SOL's SKR rate is looked up under the wrapped SOL mint.
pub const NATIVE_MINT: Pubkey = pubkey!("So11111111111111111111111111111111111111112");
pub const SOL_DECIMALS: u8 = 9;
/// SKR rates older than this are unusable: the payout pays the fee in its
/// own asset instead.
pub const SKR_RATE_MAX_AGE_SECS: i64 = SECS_PER_DAY;
/// The rate authority may move each rate at most 20% from its reference,
/// which rolls forward to the current rates once per window, so a leaked
/// rate key moves a rate at most 20% a day.
pub const MAX_RATE_STEP_BPS: u64 = 2_000;
pub const RATE_REF_WINDOW_SECS: i64 = SECS_PER_DAY;
