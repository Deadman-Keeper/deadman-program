use anchor_lang::prelude::*;

use crate::state::Rail;

#[event]
pub struct VaultCreated {
    pub vault: Pubkey,
    pub owner: Pubkey,
    pub plan_id: u16,
    pub rules: u8,
}

#[event]
pub struct PolicyUpdated {
    pub vault: Pubkey,
    pub rules: u8,
}

#[event]
pub struct Pulsed {
    pub vault: Pubkey,
    pub by: Pubkey,
    pub streak: u32,
    pub at: i64,
}

#[event]
pub struct RuleSkipped {
    pub vault: Pubkey,
    pub index: u8,
    pub reserved: u64,
    pub by: Pubkey,
}

#[event]
pub struct VestingRevoked {
    pub vault: Pubkey,
    pub at: i64,
}

#[event]
pub struct LockedDown {
    pub vault: Pubkey,
    pub by: Pubkey,
    pub until: i64,
}

#[event]
pub struct Unlocked {
    pub vault: Pubkey,
    pub at: i64,
}

#[event]
pub struct RuleExecuted {
    pub vault: Pubkey,
    pub index: u8,
    pub beneficiary: Pubkey,
    pub rail: Rail,
    pub mint: Option<Pubkey>,
    pub amount: u64,
    pub fee: u64,
    pub by: Pubkey,
}

#[event]
pub struct LegacyVaultRecovered {
    pub vault: Pubkey,
    pub owner: Pubkey,
    pub plan_id: u16,
    pub data_len: u32,
    pub lamports: u64,
}

/// The burned share of an SKR payout fee.
#[event]
pub struct FeeBurned {
    pub vault: Pubkey,
    pub mint: Pubkey,
    pub amount: u64,
}

/// A release fee paid in SKR from the plan's fee reserve instead of in the
/// payout asset; `burned` of `fee` was burned, the rest went to the
/// treasury.
#[event]
pub struct SkrFeePaid {
    pub vault: Pubkey,
    pub index: u8,
    pub fee: u64,
    pub burned: u64,
}

/// The SKR rates were set by the admin or updated by the rate authority
/// (`by`).
#[event]
pub struct SkrRatesSet {
    pub by: Pubkey,
    pub authority: Pubkey,
    pub usdc_mint: Pubkey,
    pub skr_per_sol: u64,
    pub skr_per_usdc: u64,
}

#[event]
pub struct AdminProposed {
    pub admin: Pubkey,
    pub pending_admin: Pubkey,
}

#[event]
pub struct AdminChanged {
    pub old_admin: Pubkey,
    pub new_admin: Pubkey,
}
