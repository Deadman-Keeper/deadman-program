//! Release fees paid in SKR from a plan's fee reserve, converted at the
//! operator-set SKR rates of SOL and USDC.

use {
    anchor_lang::{
        prelude::Pubkey,
        solana_program::{
            clock::Clock,
            instruction::{AccountMeta, Instruction},
            system_program,
        },
        AccountDeserialize, InstructionData, ToAccountMetas,
    },
    anchor_spl::associated_token::get_associated_token_address_with_program_id,
    deadman::{
        AmountMode, Rail, RuleInput, SkrRates, Vault, VestingInput, CONFIG_SEED, SKR_RATES_SEED,
        VAULT_SEED,
    },
    litesvm::LiteSVM,
    litesvm_token::{
        get_spl_account,
        spl_token::state::{Account as SplAccount, Mint as SplMint},
        CreateAssociatedTokenAccountIdempotent, CreateMint, MintTo, TOKEN_ID,
    },
    solana_keypair::Keypair,
    solana_message::{Message, VersionedMessage},
    solana_signer::Signer,
    solana_transaction::versioned::VersionedTransaction,
};

const SOL: u64 = 1_000_000_000;
const SKR: u64 = 1_000_000;
const DAY: i64 = 86_400;

/// SOL at $150, USDC at $1 and SKR at $0.02: SKR base units per whole SOL
/// and per whole USDC.
const SKR_PER_SOL: u64 = 7_500 * SKR;
const SKR_PER_USDC: u64 = 50 * SKR;

fn rates_pda() -> Pubkey {
    Pubkey::find_program_address(&[SKR_RATES_SEED], &deadman::id()).0
}

fn config_pda() -> Pubkey {
    Pubkey::find_program_address(&[CONFIG_SEED], &deadman::id()).0
}

fn ata(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    get_associated_token_address_with_program_id(owner, mint, &TOKEN_ID)
}

/// Independent model of the SKR fee: `bps` of `gross` base units of an
/// asset with `asset_decimals`, at `rate` SKR base units per whole unit,
/// rounded up.
fn model_fee(gross: u64, rate: u64, asset_decimals: u32, bps: u64) -> u64 {
    let num = u128::from(gross) * u128::from(rate) * u128::from(bps);
    let den = 10_000u128 * 10u128.pow(asset_decimals);
    u64::try_from(num.div_ceil(den)).unwrap()
}

fn tier(beneficiary: &Pubkey, mint: Option<Pubkey>, mode: AmountMode, amount: u64) -> RuleInput {
    RuleInput {
        beneficiary: *beneficiary,
        rail: Rail::Solana,
        after_secs: 10 * DAY,
        mint,
        mode,
        amount,
    }
}

fn all_sol(beneficiary: &Pubkey) -> Vec<RuleInput> {
    vec![tier(beneficiary, None, AmountMode::Percent, 10_000)]
}

struct Env {
    svm: LiteSVM,
    admin: Keypair,
    treasury: Keypair,
    owner: Keypair,
    guard: Keypair,
    keeper: Keypair,
    rater: Keypair,
    skr: Pubkey,
    plan: u16,
}

impl Env {
    /// Config with an SKR mint and SOL's SKR rate set (no USDC mint yet),
    /// and the treasury's SKR ATA created.
    fn new() -> Self {
        let mut env = Self::without_rates();
        env.set_rates(&Pubkey::default(), SKR_PER_SOL, SKR_PER_USDC)
            .unwrap();
        env
    }

    /// Like [`Env::new`] before the admin has set any SKR rate.
    fn without_rates() -> Self {
        let mut svm = LiteSVM::new();
        let bytes = include_bytes!(concat!(
            env!("CARGO_TARGET_TMPDIR"),
            "/../deploy/deadman.so"
        ));
        svm.add_program(deadman::id(), bytes).unwrap();
        let [admin, treasury, owner, guard, keeper, rater] =
            std::array::from_fn(|_| Keypair::new());
        for k in [&admin, &treasury, &owner, &guard, &keeper, &rater] {
            svm.airdrop(&k.pubkey(), 100 * SOL).unwrap();
        }
        let pd = Pubkey::find_program_address(
            &[deadman::id().as_ref()],
            &anchor_lang::solana_program::bpf_loader_upgradeable::ID,
        )
        .0;
        let mut acc = svm.get_account(&pd).unwrap();
        acc.data[12] = 1;
        acc.data[13..45].copy_from_slice(admin.pubkey().as_ref());
        svm.set_account(pd, acc).unwrap();
        let skr = CreateMint::new(&mut svm, &admin)
            .decimals(6)
            .send()
            .unwrap();
        let mut env = Self {
            svm,
            admin,
            treasury,
            owner,
            guard,
            keeper,
            rater,
            skr,
            plan: 0,
        };
        env.set_time(1_800_000_000);

        let admin = env.admin.insecure_clone();
        let ix = Instruction::new_with_bytes(
            deadman::id(),
            &deadman::instruction::InitConfig { skr_mint: skr }.data(),
            deadman::accounts::InitConfig {
                admin: admin.pubkey(),
                config: config_pda(),
                treasury: env.treasury.pubkey(),
                program: deadman::id(),
                program_data: pd,
                system_program: system_program::ID,
            }
            .to_account_metas(None),
        );
        env.send(ix, &[&admin]).unwrap();
        let t = env.treasury.pubkey();
        env.create_ata(&t, &skr);
        env
    }

    fn vault_addr(&self) -> Pubkey {
        Pubkey::find_program_address(
            &[
                VAULT_SEED,
                self.owner.pubkey().as_ref(),
                &self.plan.to_le_bytes(),
            ],
            &deadman::id(),
        )
        .0
    }

    fn now(&self) -> i64 {
        self.svm.get_sysvar::<Clock>().unix_timestamp
    }

    fn set_time(&mut self, ts: i64) {
        let mut clock = self.svm.get_sysvar::<Clock>();
        clock.unix_timestamp = ts;
        self.svm.set_sysvar(&clock);
    }

    fn advance(&mut self, secs: i64) {
        let t = self.now() + secs;
        self.set_time(t);
    }

    fn send(&mut self, ix: Instruction, signers: &[&Keypair]) -> Result<u64, String> {
        self.svm.expire_blockhash();
        let msg = Message::new_with_blockhash(
            &[ix],
            Some(&signers[0].pubkey()),
            &self.svm.latest_blockhash(),
        );
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), signers).unwrap();
        self.svm
            .send_transaction(tx)
            .map(|m| m.compute_units_consumed)
            .map_err(|e| format!("{:?}\n{}", e.err, e.meta.logs.join("\n")))
    }

    fn vault(&self) -> Vault {
        let acc = self.svm.get_account(&self.vault_addr()).unwrap();
        Vault::try_deserialize(&mut acc.data.as_slice()).unwrap()
    }

    fn lamports(&self, k: &Pubkey) -> u64 {
        self.svm.get_account(k).map(|a| a.lamports).unwrap_or(0)
    }

    fn balance(&self, k: &Pubkey) -> u64 {
        get_spl_account::<SplAccount>(&self.svm, k).unwrap().amount
    }

    fn supply(&self, mint: &Pubkey) -> u64 {
        get_spl_account::<SplMint>(&self.svm, mint).unwrap().supply
    }

    fn withdrawable(&self) -> u64 {
        let acc = self.svm.get_account(&self.vault_addr()).unwrap();
        let rent = self
            .svm
            .minimum_balance_for_rent_exemption(acc.data.len())
            .max(self.vault().rent_paid);
        acc.lamports - rent
    }

    fn create_ata(&mut self, owner: &Pubkey, mint: &Pubkey) -> Pubkey {
        let admin = self.admin.insecure_clone();
        CreateAssociatedTokenAccountIdempotent::new(&mut self.svm, &admin, mint)
            .owner(owner)
            .send()
            .unwrap()
    }

    fn mint_to(&mut self, mint: &Pubkey, to: &Pubkey, amount: u64) {
        let admin = self.admin.insecure_clone();
        MintTo::new(&mut self.svm, &admin, mint, to, amount)
            .send()
            .unwrap();
    }

    /// New 6-decimal mint with vault and treasury ATAs; `vault_amount` in
    /// the vault.
    fn token(&mut self, vault_amount: u64) -> Pubkey {
        let admin = self.admin.insecure_clone();
        let mint = CreateMint::new(&mut self.svm, &admin)
            .decimals(6)
            .send()
            .unwrap();
        let (v, t) = (self.vault_addr(), self.treasury.pubkey());
        let vault_ata = self.create_ata(&v, &mint);
        self.create_ata(&t, &mint);
        self.mint_to(&mint, &vault_ata, vault_amount);
        mint
    }

    fn set_rates_ix(
        &self,
        signer: &Pubkey,
        usdc: &Pubkey,
        skr_per_sol: u64,
        skr_per_usdc: u64,
    ) -> Instruction {
        Instruction::new_with_bytes(
            deadman::id(),
            &deadman::instruction::SetSkrRates {
                authority: self.rater.pubkey(),
                usdc_mint: *usdc,
                skr_per_sol,
                skr_per_usdc,
            }
            .data(),
            deadman::accounts::SetSkrRates {
                admin: *signer,
                config: config_pda(),
                skr_rates: rates_pda(),
                system_program: system_program::ID,
            }
            .to_account_metas(None),
        )
    }

    fn set_rates(
        &mut self,
        usdc: &Pubkey,
        skr_per_sol: u64,
        skr_per_usdc: u64,
    ) -> Result<u64, String> {
        let admin = self.admin.insecure_clone();
        let ix = self.set_rates_ix(&admin.pubkey(), usdc, skr_per_sol, skr_per_usdc);
        self.send(ix, &[&admin])
    }

    fn update_rates_as(
        &mut self,
        signer: &Keypair,
        skr_per_sol: u64,
        skr_per_usdc: u64,
    ) -> Result<u64, String> {
        let ix = Instruction::new_with_bytes(
            deadman::id(),
            &deadman::instruction::UpdateSkrRates {
                skr_per_sol,
                skr_per_usdc,
            }
            .data(),
            deadman::accounts::UpdateSkrRates {
                authority: signer.pubkey(),
                skr_rates: rates_pda(),
            }
            .to_account_metas(None),
        );
        self.send(ix, &[signer])
    }

    fn update_rates(&mut self, skr_per_sol: u64, skr_per_usdc: u64) -> Result<u64, String> {
        let rater = self.rater.insecure_clone();
        self.update_rates_as(&rater, skr_per_sol, skr_per_usdc)
    }

    /// The rate script's refresh: the current rates again, now.
    fn refresh_rates(&mut self) {
        let r = self.rates();
        self.update_rates(r.skr_per_sol, r.skr_per_usdc).unwrap();
    }

    fn rates(&self) -> SkrRates {
        let acc = self.svm.get_account(&rates_pda()).unwrap();
        SkrRates::try_deserialize(&mut acc.data.as_slice()).unwrap()
    }

    fn put(&mut self, address: &Pubkey, data: Vec<u8>, owner: Pubkey) {
        let mut acc = self.svm.get_account(&self.admin.pubkey()).unwrap();
        acc.lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
        acc.data = data;
        acc.owner = owner;
        self.svm.set_account(*address, acc).unwrap();
    }

    fn create_plan(&mut self, rules: Vec<RuleInput>) -> Result<u64, String> {
        let mut ix = Instruction::new_with_bytes(
            deadman::id(),
            &deadman::instruction::CreatePlan {
                plan_id: self.plan,
                label: "Plan".to_string(),
                guard: self.guard.pubkey(),
                lock_secs: 3 * DAY,
                skip_grace_secs: 30 * DAY,
                rules: rules.clone(),
            }
            .data(),
            deadman::accounts::CreateVault {
                owner: self.owner.pubkey(),
                payer: self.owner.pubkey(),
                vault: self.vault_addr(),
                system_program: system_program::ID,
            }
            .to_account_metas(None),
        );
        let mut mints: Vec<Pubkey> = rules.iter().filter_map(|r| r.mint).collect();
        mints.dedup();
        ix.accounts
            .extend(mints.iter().map(|m| AccountMeta::new_readonly(*m, false)));
        let owner = self.owner.insecure_clone();
        self.send(ix, &[&owner])
    }

    fn update_plan(&mut self, rules: Vec<RuleInput>) -> Result<u64, String> {
        let mut ix = Instruction::new_with_bytes(
            deadman::id(),
            &deadman::instruction::UpdatePlan {
                label: "Plan".to_string(),
                lock_secs: 3 * DAY,
                skip_grace_secs: 30 * DAY,
                rules: rules.clone(),
                guardian: None,
            }
            .data(),
            deadman::accounts::OwnerAction {
                owner: self.owner.pubkey(),
                vault: self.vault_addr(),
            }
            .to_account_metas(None),
        );
        let mut mints: Vec<Pubkey> = rules.iter().filter_map(|r| r.mint).collect();
        mints.dedup();
        ix.accounts
            .extend(mints.iter().map(|m| AccountMeta::new_readonly(*m, false)));
        let owner = self.owner.insecure_clone();
        self.send(ix, &[&owner])
    }

    fn create_vesting(&mut self, schedules: Vec<VestingInput>) -> Result<u64, String> {
        let mut ix = Instruction::new_with_bytes(
            deadman::id(),
            &deadman::instruction::CreateVesting {
                plan_id: self.plan,
                label: "Vesting".to_string(),
                guard: self.guard.pubkey(),
                lock_secs: 3 * DAY,
                start_at: self.now(),
                revocable: false,
                schedules: schedules.clone(),
                period_secs: 0,
            }
            .data(),
            deadman::accounts::CreateVault {
                owner: self.owner.pubkey(),
                payer: self.owner.pubkey(),
                vault: self.vault_addr(),
                system_program: system_program::ID,
            }
            .to_account_metas(None),
        );
        ix.accounts.extend(
            schedules
                .iter()
                .filter_map(|s| s.mint)
                .map(|m| AccountMeta::new_readonly(m, false)),
        );
        let owner = self.owner.insecure_clone();
        self.send(ix, &[&owner])
    }

    fn deposit_sol(&mut self, amount: u64) {
        let ix = anchor_lang::solana_program::system_instruction::transfer(
            &self.owner.pubkey(),
            &self.vault_addr(),
            amount,
        );
        let owner = self.owner.insecure_clone();
        self.send(ix, &[&owner]).unwrap();
    }

    /// Gives the owner `amount` SKR and moves it into the fee reserve.
    fn fund(&mut self, amount: u64) -> Result<u64, String> {
        let (o, v, skr) = (self.owner.pubkey(), self.vault_addr(), self.skr);
        let owner_ata = self.create_ata(&o, &skr);
        self.create_ata(&v, &skr);
        self.mint_to(&skr, &owner_ata, amount);
        self.fund_with(&skr, amount)
    }

    fn fund_with(&mut self, mint: &Pubkey, amount: u64) -> Result<u64, String> {
        let (o, v) = (self.owner.pubkey(), self.vault_addr());
        let ix = Instruction::new_with_bytes(
            deadman::id(),
            &deadman::instruction::FundSkrReserve { amount }.data(),
            deadman::accounts::FundSkrReserve {
                owner: o,
                vault: v,
                config: config_pda(),
                mint: *mint,
                vault_token: ata(&v, mint),
                owner_token: ata(&o, mint),
                token_program: TOKEN_ID,
            }
            .to_account_metas(None),
        );
        let owner = self.owner.insecure_clone();
        self.send(ix, &[&owner])
    }

    fn owner_token_ix<T: InstructionData>(&self, data: T, mint: &Pubkey) -> Instruction {
        let (o, v) = (self.owner.pubkey(), self.vault_addr());
        Instruction::new_with_bytes(
            deadman::id(),
            &data.data(),
            deadman::accounts::WithdrawToken {
                owner: o,
                vault: v,
                mint: *mint,
                vault_token: ata(&v, mint),
                owner_token: ata(&o, mint),
                token_program: TOKEN_ID,
            }
            .to_account_metas(None),
        )
    }

    fn withdraw_reserve(&mut self, amount: u64) -> Result<u64, String> {
        let ix = self.owner_token_ix(
            deadman::instruction::WithdrawSkrReserve { amount },
            &self.skr,
        );
        let owner = self.owner.insecure_clone();
        self.send(ix, &[&owner])
    }

    fn withdraw_token(&mut self, mint: &Pubkey, amount: u64) -> Result<u64, String> {
        let ix = self.owner_token_ix(deadman::instruction::WithdrawToken { amount }, mint);
        let owner = self.owner.insecure_clone();
        self.send(ix, &[&owner])
    }

    /// The SKR fee accounts the program requires for every payout of a plan
    /// with a reserve.
    fn skr_accounts(&self) -> Vec<AccountMeta> {
        vec![
            AccountMeta::new_readonly(rates_pda(), false),
            AccountMeta::new(ata(&self.vault_addr(), &self.skr), false),
            AccountMeta::new(self.skr, false),
            AccountMeta::new(ata(&self.treasury.pubkey(), &self.skr), false),
            AccountMeta::new_readonly(TOKEN_ID, false),
        ]
    }

    fn sol_ix<T: InstructionData>(
        &self,
        data: T,
        beneficiary: &Pubkey,
        extra: Vec<AccountMeta>,
    ) -> Instruction {
        let mut ix = Instruction::new_with_bytes(
            deadman::id(),
            &data.data(),
            deadman::accounts::ExecuteSolRule {
                executor: self.keeper.pubkey(),
                vault: self.vault_addr(),
                config: config_pda(),
                beneficiary: *beneficiary,
                treasury: self.treasury.pubkey(),
            }
            .to_account_metas(None),
        );
        ix.accounts.extend(extra);
        ix
    }

    fn token_ix<T: InstructionData>(
        &mut self,
        data: T,
        beneficiary: &Pubkey,
        mint: &Pubkey,
        extra: Vec<AccountMeta>,
    ) -> Instruction {
        self.create_ata(beneficiary, mint);
        let vault = self.vault_addr();
        let mut ix = Instruction::new_with_bytes(
            deadman::id(),
            &data.data(),
            deadman::accounts::ExecuteTokenRule {
                executor: self.keeper.pubkey(),
                vault,
                config: config_pda(),
                mint: *mint,
                vault_token: ata(&vault, mint),
                beneficiary: *beneficiary,
                beneficiary_token: ata(beneficiary, mint),
                treasury_token: Some(ata(&self.treasury.pubkey(), mint)),
                token_program: TOKEN_ID,
            }
            .to_account_metas(None),
        );
        ix.accounts.extend(extra);
        ix
    }

    fn run(&mut self, ix: Instruction) -> Result<u64, String> {
        let keeper = self.keeper.insecure_clone();
        self.send(ix, &[&keeper])
    }

    fn execute_sol(
        &mut self,
        beneficiary: &Pubkey,
        extra: Vec<AccountMeta>,
    ) -> Result<u64, String> {
        let ix = self.sol_ix(
            deadman::instruction::ExecuteSolRule { index: 0 },
            beneficiary,
            extra,
        );
        self.run(ix)
    }

    fn execute_token(
        &mut self,
        index: u8,
        beneficiary: &Pubkey,
        mint: &Pubkey,
        extra: Vec<AccountMeta>,
    ) -> Result<u64, String> {
        let ix = self.token_ix(
            deadman::instruction::ExecuteTokenRule { index },
            beneficiary,
            mint,
            extra,
        );
        self.run(ix)
    }

    fn treasury_skr(&self) -> u64 {
        self.balance(&ata(&self.treasury.pubkey(), &self.skr))
    }
}

/// A plan paying all its SOL to a fresh beneficiary, `sol` deposited and
/// `reserve` SKR in the fee reserve, with the tier due and the rates fresh.
fn sol_plan(sol: u64, reserve: u64) -> (Env, Pubkey) {
    let mut env = Env::new();
    let b = Keypair::new().pubkey();
    env.create_plan(all_sol(&b)).unwrap();
    env.deposit_sol(sol);
    if reserve > 0 {
        env.fund(reserve).unwrap();
    }
    env.advance(10 * DAY + 1);
    env.refresh_rates();
    (env, b)
}

/// Asserts the SOL payout fell back to the 2% fee in SOL and left the
/// reserve untouched.
fn assert_sol_fallback(env: &Env, b: &Pubkey, gross: u64, reserve: u64, treasury_before: u64) {
    let fee = gross * 200 / 10_000;
    assert_eq!(env.lamports(b), gross - fee);
    assert_eq!(env.lamports(&env.treasury.pubkey()), treasury_before + fee);
    assert_eq!(env.vault().skr_fee_reserve, reserve);
    assert_eq!(env.treasury_skr(), 0);
}

/// A plan with a USDC tier and `reserve` SKR, the tier due, USDC registered
/// and the rates fresh. Returns the beneficiary and the USDC mint.
fn usdc_plan(usdc_in_vault: u64, reserve: u64) -> (Env, Pubkey, Pubkey) {
    let mut env = Env::new();
    let b = Keypair::new().pubkey();
    env.create_plan(all_sol(&Keypair::new().pubkey())).unwrap();
    let usdc = env.token(usdc_in_vault);
    env.update_plan(vec![tier(&b, Some(usdc), AmountMode::Percent, 10_000)])
        .unwrap();
    env.set_rates(&usdc, SKR_PER_SOL, SKR_PER_USDC).unwrap();
    env.fund(reserve).unwrap();
    env.advance(10 * DAY + 1);
    env.refresh_rates();
    (env, b, usdc)
}

#[test]
fn set_skr_rates_is_admin_only_and_resets_the_reference() {
    let mut env = Env::new();
    let usdc = Pubkey::new_unique();
    env.advance(DAY);
    env.set_rates(&usdc, 9 * SKR, 11 * SKR).unwrap();
    let r = env.rates();
    assert_eq!(
        (r.authority, r.usdc_mint, r.skr_per_sol, r.skr_per_usdc),
        (env.rater.pubkey(), usdc, 9 * SKR, 11 * SKR)
    );
    assert_eq!((r.ref_skr_per_sol, r.ref_skr_per_usdc), (9 * SKR, 11 * SKR));
    assert_eq!((r.updated_at, r.ref_at), (env.now(), env.now()));

    // The admin is unbounded: a 100x move and disabling both are fine.
    env.set_rates(&usdc, 900 * SKR, 0).unwrap();
    assert_eq!(env.rates().skr_per_sol, 900 * SKR);

    let intruder = Keypair::new();
    env.svm.airdrop(&intruder.pubkey(), SOL).unwrap();
    let ix = env.set_rates_ix(&intruder.pubkey(), &usdc, 1, 1);
    let err = env.send(ix, &[&intruder]).unwrap_err();
    assert!(err.contains("Unauthorized"), "{err}");
    // The rate authority cannot use the admin's instruction either.
    let rater = env.rater.insecure_clone();
    let ix = env.set_rates_ix(&rater.pubkey(), &usdc, 1, 1);
    let err = env.send(ix, &[&rater]).unwrap_err();
    assert!(err.contains("Unauthorized"), "{err}");

    // A rate authority is required.
    let admin = env.admin.insecure_clone();
    let mut ix = env.set_rates_ix(&admin.pubkey(), &usdc, 1, 1);
    ix.data = deadman::instruction::SetSkrRates {
        authority: Pubkey::default(),
        usdc_mint: usdc,
        skr_per_sol: 1,
        skr_per_usdc: 1,
    }
    .data();
    let err = env.send(ix, &[&admin]).unwrap_err();
    assert!(err.contains("InvalidConfig"), "{err}");
}

#[test]
fn the_rate_authority_moves_rates_at_most_twenty_percent_a_day() {
    let mut env = Env::new();
    let (sol, usdc) = (SKR_PER_SOL, SKR_PER_USDC);

    // Exactly ±20% of the reference, on either rate, is allowed...
    env.update_rates(sol * 12 / 10, usdc * 8 / 10).unwrap();
    env.update_rates(sol * 8 / 10, usdc * 12 / 10).unwrap();
    // ...one base unit further is not, and neither is stepping from the
    // last value instead of the day's reference.
    for (s, u) in [
        (sol * 12 / 10 + 1, usdc),
        (sol, usdc * 8 / 10 - 1),
        (sol * 8 / 10 - 1, usdc),
    ] {
        let err = env.update_rates(s, u).unwrap_err();
        assert!(err.contains("RateChangeTooLarge"), "{s} {u}: {err}");
    }
    env.update_rates(sol * 12 / 10, usdc).unwrap();
    let err = env.update_rates(sol * 144 / 100, usdc).unwrap_err();
    assert!(err.contains("RateChangeTooLarge"), "{err}");
    let r = env.rates();
    assert_eq!((r.skr_per_sol, r.updated_at), (sol * 12 / 10, env.now()));
    assert_eq!(r.ref_skr_per_sol, sol);

    // A day later the reference rolls forward to the current rates.
    env.advance(DAY);
    env.update_rates(sol * 144 / 100, usdc).unwrap();
    let r = env.rates();
    assert_eq!((r.ref_skr_per_sol, r.ref_at), (sol * 12 / 10, env.now()));

    // Only the rate authority may update; the admin uses set_skr_rates.
    let admin = env.admin.insecure_clone();
    let err = env.update_rates_as(&admin, sol, usdc).unwrap_err();
    assert!(err.contains("Unauthorized"), "{err}");
}

#[test]
fn a_rate_disabled_across_a_window_needs_the_admin_to_come_back() {
    let mut env = Env::new();
    // Disabling is always allowed, and re-enabling within the same window.
    env.update_rates(0, SKR_PER_USDC).unwrap();
    env.update_rates(SKR_PER_SOL, SKR_PER_USDC).unwrap();
    env.update_rates(0, SKR_PER_USDC).unwrap();
    env.advance(DAY);
    // The reference is now 0, so the authority cannot enable it again.
    let err = env.update_rates(SKR_PER_SOL, SKR_PER_USDC).unwrap_err();
    assert!(err.contains("RateChangeTooLarge"), "{err}");
    env.set_rates(&Pubkey::default(), SKR_PER_SOL, SKR_PER_USDC)
        .unwrap();
    env.update_rates(SKR_PER_SOL, SKR_PER_USDC).unwrap();
}

#[test]
fn sol_payout_pays_the_fee_in_skr_and_burns_ten_percent() {
    let (mut env, b) = sol_plan(10 * SOL, 10_000 * SKR);
    let gross = env.withdrawable();
    let treasury_sol = env.lamports(&env.treasury.pubkey());
    let supply = env.supply(&env.skr);
    let extra = env.skr_accounts();
    let cu = env.execute_sol(&b, extra).unwrap();
    println!("execute_sol_rule with SKR fee: {cu} CU");

    // $1,500 payout: 1.5% = $22.50 = 1,125 SKR at $0.02.
    let fee = model_fee(gross, SKR_PER_SOL, 9, 150);
    assert_eq!(fee, 1_125 * SKR);
    let burned = fee / 10;
    assert_eq!(env.lamports(&b), gross, "beneficiary gets 100%");
    assert_eq!(env.lamports(&env.treasury.pubkey()), treasury_sol);
    assert_eq!(env.treasury_skr(), fee - burned);
    assert_eq!(env.supply(&env.skr), supply - burned);
    let v = env.vault();
    assert_eq!(v.skr_fee_reserve, 10_000 * SKR - fee);
    assert_eq!(v.rules[0].paid, gross);
    assert_eq!(
        env.balance(&ata(&env.vault_addr(), &env.skr)),
        10_000 * SKR - fee
    );
}

#[test]
fn usdc_payout_pays_the_fee_in_skr() {
    let (mut env, b, usdc) = usdc_plan(1_000 * SKR, 1_000 * SKR);
    let extra = env.skr_accounts();
    let cu = env.execute_token(0, &b, &usdc, extra).unwrap();
    println!("execute_token_rule with SKR fee: {cu} CU");

    // $1,000 payout: $15 = 750 SKR.
    let fee = model_fee(1_000 * SKR, SKR_PER_USDC, 6, 150);
    assert_eq!(fee, 750 * SKR);
    assert_eq!(env.balance(&ata(&b, &usdc)), 1_000 * SKR);
    assert_eq!(env.balance(&ata(&env.treasury.pubkey(), &usdc)), 0);
    assert_eq!(env.treasury_skr(), fee - fee / 10);
    assert_eq!(env.vault().skr_fee_reserve, 1_000 * SKR - fee);
}

#[test]
fn the_fee_rounds_up_in_the_protocol_favour() {
    // 1 SOL + 7 lamports: the exact fee is 112.5000007875 SKR.
    let (mut env, b) = sol_plan(SOL + 7, 200 * SKR);
    let gross = env.withdrawable();
    assert_eq!(gross, SOL + 7);
    let extra = env.skr_accounts();
    env.execute_sol(&b, extra).unwrap();
    let fee = 200 * SKR - env.vault().skr_fee_reserve;
    assert_eq!(fee, 112_500_001);
    assert_eq!(fee, model_fee(gross, SKR_PER_SOL, 9, 150));
}

#[test]
fn unusable_rates_fall_back_to_the_asset_fee() {
    type Spoil = fn(&mut Env);
    let cases: [(&str, Spoil); 3] = [
        ("older than a day", |e| e.advance(DAY + 1)),
        ("SOL disabled", |e| {
            e.update_rates(0, SKR_PER_USDC).unwrap();
        }),
        ("SOL disabled by the admin", |e| {
            e.set_rates(&Pubkey::default(), 0, SKR_PER_USDC).unwrap();
        }),
    ];
    for (name, spoil) in cases {
        let (mut env, b) = sol_plan(10 * SOL, 10_000 * SKR);
        spoil(&mut env);
        let gross = env.withdrawable();
        let treasury = env.lamports(&env.treasury.pubkey());
        let extra = env.skr_accounts();
        env.execute_sol(&b, extra)
            .unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_sol_fallback(&env, &b, gross, 10_000 * SKR, treasury);
    }
}

#[test]
fn rates_exactly_a_day_old_are_still_usable() {
    let (mut env, b) = sol_plan(10 * SOL, 10_000 * SKR);
    env.advance(DAY);
    let gross = env.withdrawable();
    let extra = env.skr_accounts();
    env.execute_sol(&b, extra).unwrap();
    assert_eq!(env.lamports(&b), gross);
    assert!(env.vault().skr_fee_reserve < 10_000 * SKR);
}

#[test]
fn before_any_rate_is_set_the_empty_rates_address_falls_back() {
    let mut env = Env::without_rates();
    let b = Keypair::new().pubkey();
    env.create_plan(all_sol(&b)).unwrap();
    env.deposit_sol(10 * SOL);
    env.fund(10_000 * SKR).unwrap();
    env.advance(10 * DAY + 1);
    assert!(env.svm.get_account(&rates_pda()).is_none());
    let gross = env.withdrawable();
    let treasury = env.lamports(&env.treasury.pubkey());
    let extra = env.skr_accounts();
    env.execute_sol(&b, extra).unwrap();
    assert_sol_fallback(&env, &b, gross, 10_000 * SKR, treasury);
}

#[test]
fn a_short_reserve_falls_back_and_keeps_the_reserve() {
    // The fee is 1,125 SKR; the reserve holds 1,124.
    let (mut env, b) = sol_plan(10 * SOL, 1_124 * SKR);
    let gross = env.withdrawable();
    let treasury = env.lamports(&env.treasury.pubkey());
    let extra = env.skr_accounts();
    env.execute_sol(&b, extra).unwrap();
    assert_sol_fallback(&env, &b, gross, 1_124 * SKR, treasury);
}

#[test]
fn a_missing_treasury_skr_account_falls_back() {
    let mut env = Env::new();
    let b = Keypair::new().pubkey();
    env.create_plan(all_sol(&b)).unwrap();
    env.deposit_sol(10 * SOL);
    env.fund(10_000 * SKR).unwrap();
    // Close the treasury's SKR ATA (empty), as if never created.
    let t_ata = ata(&env.treasury.pubkey(), &env.skr);
    let mut acc = env.svm.get_account(&t_ata).unwrap();
    acc.data = vec![];
    acc.owner = system_program::ID;
    acc.lamports = 0;
    env.svm.set_account(t_ata, acc).unwrap();
    env.advance(10 * DAY + 1);
    env.refresh_rates();
    let gross = env.withdrawable();
    let treasury = env.lamports(&env.treasury.pubkey());
    let extra = env.skr_accounts();
    env.execute_sol(&b, extra).unwrap();
    let fee = gross * 200 / 10_000;
    assert_eq!(env.lamports(&b), gross - fee);
    assert_eq!(env.lamports(&env.treasury.pubkey()), treasury + fee);
    assert_eq!(env.vault().skr_fee_reserve, 10_000 * SKR);
}

#[test]
fn executors_cannot_substitute_or_omit_the_fee_accounts() {
    let (mut env, b) = sol_plan(10 * SOL, 10_000 * SKR);
    let good = env.skr_accounts();

    let err = env.execute_sol(&b, vec![]).unwrap_err();
    assert!(err.contains("SkrFeeAccountsRequired"), "{err}");
    let err = env.execute_sol(&b, good[..4].to_vec()).unwrap_err();
    assert!(err.contains("SkrFeeAccountsRequired"), "{err}");

    // A copy of the real rates (with a cheaper SKR) at another address,
    // owned by the program.
    let fake = Pubkey::new_unique();
    let mut data = env.svm.get_account(&rates_pda()).unwrap().data;
    data[72..80].copy_from_slice(&1u64.to_le_bytes());
    env.put(&fake, data, deadman::id());
    let swaps: Vec<(usize, Pubkey)> = vec![
        (0, fake),
        // An address that is neither the rates account nor its PDA.
        (0, Keypair::new().pubkey()),
        (1, ata(&env.owner.pubkey(), &env.skr)),
        (2, Pubkey::new_unique()),
        (3, ata(&env.owner.pubkey(), &env.skr)),
        (4, system_program::ID),
    ];
    for (i, key) in swaps {
        let mut extra = good.clone();
        extra[i].pubkey = key;
        let err = env.execute_sol(&b, extra).unwrap_err();
        assert!(err.contains("InvalidSkrFeeAccounts"), "slot {i}: {err}");
    }
    assert_eq!(env.vault().rules[0].executed_at, 0);

    env.execute_sol(&b, good).unwrap();
    assert!(env.vault().skr_fee_reserve < 10_000 * SKR);
}

#[test]
fn other_tokens_pay_two_percent_in_kind() {
    let mut env = Env::new();
    let b = Keypair::new().pubkey();
    env.create_plan(all_sol(&Keypair::new().pubkey())).unwrap();
    let tok = env.token(1_000 * SKR);
    env.update_plan(vec![tier(&b, Some(tok), AmountMode::Percent, 10_000)])
        .unwrap();
    // USDC is registered, but this token is not USDC.
    let usdc = Pubkey::new_unique();
    env.set_rates(&usdc, SKR_PER_SOL, SKR_PER_USDC).unwrap();
    env.fund(500 * SKR).unwrap();
    env.advance(10 * DAY + 1);
    env.refresh_rates();
    // A plan with a reserve still passes the fee accounts on every payout.
    let err = env.execute_token(0, &b, &tok, vec![]).unwrap_err();
    assert!(err.contains("SkrFeeAccountsRequired"), "{err}");
    let extra = env.skr_accounts();
    env.execute_token(0, &b, &tok, extra).unwrap();
    assert_eq!(env.balance(&ata(&b, &tok)), 980 * SKR);
    assert_eq!(env.balance(&ata(&env.treasury.pubkey(), &tok)), 20 * SKR);
    assert_eq!(env.vault().skr_fee_reserve, 500 * SKR);
}

#[test]
fn skr_payouts_stay_in_kind_and_never_touch_the_reserve() {
    let mut env = Env::new();
    let b = Keypair::new().pubkey();
    env.create_plan(all_sol(&Keypair::new().pubkey())).unwrap();
    let skr = env.skr;
    env.update_plan(vec![tier(&b, Some(skr), AmountMode::Percent, 10_000)])
        .unwrap();
    env.fund(300 * SKR).unwrap();
    // Tier funds: 1,000 SKR deposited directly next to the reserve.
    let vault_ata = ata(&env.vault_addr(), &skr);
    env.mint_to(&skr, &vault_ata, 1_000 * SKR);
    let supply = env.supply(&skr);
    env.advance(10 * DAY + 1);
    // No fee accounts needed: the in-kind 1.5% applies.
    env.execute_token(0, &b, &skr, vec![]).unwrap();
    let fee = 1_000 * SKR * 150 / 10_000;
    assert_eq!(env.balance(&ata(&b, &skr)), 1_000 * SKR - fee);
    assert_eq!(env.treasury_skr(), fee - fee / 10);
    assert_eq!(env.supply(&skr), supply - fee / 10);
    assert_eq!(env.balance(&vault_ata), 300 * SKR);
    assert_eq!(env.vault().skr_fee_reserve, 300 * SKR);
}

#[test]
fn nft_payouts_stay_fee_free_without_fee_accounts() {
    let mut env = Env::new();
    let b = Keypair::new().pubkey();
    env.create_plan(all_sol(&Keypair::new().pubkey())).unwrap();
    let admin = env.admin.insecure_clone();
    let nft = CreateMint::new(&mut env.svm, &admin)
        .decimals(0)
        .send()
        .unwrap();
    let v = env.vault_addr();
    let vault_nft = env.create_ata(&v, &nft);
    let t = env.treasury.pubkey();
    env.create_ata(&t, &nft);
    env.mint_to(&nft, &vault_nft, 1);
    env.update_plan(vec![tier(&b, Some(nft), AmountMode::Fixed, 1)])
        .unwrap();
    env.fund(100 * SKR).unwrap();
    env.advance(10 * DAY + 1);
    env.execute_token(0, &b, &nft, vec![]).unwrap();
    assert_eq!(env.balance(&ata(&b, &nft)), 1);
    assert_eq!(env.vault().skr_fee_reserve, 100 * SKR);
}

#[test]
fn the_reserve_is_kept_apart_from_tier_funds() {
    let mut env = Env::new();
    let b = Keypair::new().pubkey();
    env.create_plan(all_sol(&Keypair::new().pubkey())).unwrap();
    let skr = env.skr;
    env.update_plan(vec![tier(&b, Some(skr), AmountMode::Percent, 5_000)])
        .unwrap();
    env.fund(300 * SKR).unwrap();
    let vault_ata = ata(&env.vault_addr(), &skr);
    env.mint_to(&skr, &vault_ata, 100 * SKR);

    // Tier withdrawals only see the 100 deposited SKR.
    let err = env.withdraw_token(&skr, 101 * SKR).unwrap_err();
    assert!(err.contains("FundsCommitted"), "{err}");
    env.withdraw_token(&skr, 40 * SKR).unwrap();
    assert_eq!(env.vault().skr_fee_reserve, 300 * SKR);

    // The reserve comes back without a fee, but never more than it holds.
    let err = env.withdraw_reserve(301 * SKR).unwrap_err();
    assert!(err.contains("InsufficientFunds"), "{err}");
    let before = env.balance(&ata(&env.owner.pubkey(), &skr));
    env.withdraw_reserve(100 * SKR).unwrap();
    assert_eq!(
        env.balance(&ata(&env.owner.pubkey(), &skr)),
        before + 100 * SKR
    );
    assert_eq!(env.vault().skr_fee_reserve, 200 * SKR);
    assert_eq!(env.balance(&vault_ata), 260 * SKR);

    // A due tier pays 50% of the 60 tier SKR, not of the reserve.
    env.advance(10 * DAY + 1);
    env.execute_token(0, &b, &skr, vec![]).unwrap();
    let gross = 30 * SKR;
    assert_eq!(env.balance(&ata(&b, &skr)), gross - gross * 150 / 10_000);
    assert_eq!(env.vault().skr_fee_reserve, 200 * SKR);
}

#[test]
fn skipping_reserves_tier_funds_only() {
    let mut env = Env::new();
    let b = Keypair::new().pubkey();
    env.create_plan(all_sol(&Keypair::new().pubkey())).unwrap();
    let skr = env.skr;
    env.update_plan(vec![tier(&b, Some(skr), AmountMode::Percent, 10_000)])
        .unwrap();
    env.fund(300 * SKR).unwrap();
    let vault_ata = ata(&env.vault_addr(), &skr);
    env.mint_to(&skr, &vault_ata, 100 * SKR);
    env.advance(41 * DAY);
    let ix = Instruction::new_with_bytes(
        deadman::id(),
        &deadman::instruction::SkipRule { index: 0 }.data(),
        deadman::accounts::SkipRule {
            caller: env.keeper.pubkey(),
            vault: env.vault_addr(),
            vault_token: Some(vault_ata),
        }
        .to_account_metas(None),
    );
    env.run(ix).unwrap();
    assert_eq!(env.vault().rules[0].reserved, 100 * SKR);
}

#[test]
fn reserve_funding_and_withdrawal_rules() {
    let mut env = Env::new();
    env.create_plan(all_sol(&Keypair::new().pubkey())).unwrap();

    // Only the config's SKR mint can fund the reserve.
    let other = env.token(0);
    let (o, v) = (env.owner.pubkey(), env.vault_addr());
    let owner_other = env.create_ata(&o, &other);
    env.mint_to(&other, &owner_other, 10 * SKR);
    let err = env.fund_with(&other, SKR).unwrap_err();
    assert!(err.contains("WrongAsset"), "{err}");
    let err = env
        .send(
            env.owner_token_ix(
                deadman::instruction::WithdrawSkrReserve { amount: 1 },
                &other,
            ),
            &[&env.owner.insecure_clone()],
        )
        .unwrap_err();
    assert!(err.contains("WrongAsset"), "{err}");

    env.fund(50 * SKR).unwrap();
    env.fund_with(&env.skr.clone(), 0).unwrap();
    let vault = env.vault();
    assert_eq!(
        (vault.skr_fee_reserve, vault.skr_reserve_mint),
        (50 * SKR, env.skr)
    );
    assert_eq!(env.balance(&ata(&v, &env.skr)), 50 * SKR);

    // Funding works during lockdown; withdrawing does not.
    let guard = env.guard.insecure_clone();
    let ix = Instruction::new_with_bytes(
        deadman::id(),
        &deadman::instruction::Lockdown {}.data(),
        deadman::accounts::Lockdown {
            signer: guard.pubkey(),
            vault: v,
        }
        .to_account_metas(None),
    );
    env.send(ix, &[&guard]).unwrap();
    let skr = env.skr;
    let owner_skr = ata(&o, &skr);
    env.mint_to(&skr, &owner_skr, 5 * SKR);
    env.fund_with(&skr, 5 * SKR).unwrap();
    let err = env.withdraw_reserve(SKR).unwrap_err();
    assert!(err.contains("VaultLocked"), "{err}");
    env.advance(3 * DAY + 1);
    env.withdraw_reserve(SKR).unwrap();
    assert_eq!(env.vault().skr_fee_reserve, 54 * SKR);
}

#[test]
fn closing_requires_the_reserve_withdrawn() {
    let mut env = Env::new();
    env.create_plan(all_sol(&Keypair::new().pubkey())).unwrap();
    env.fund(20 * SKR).unwrap();
    let close = |env: &mut Env| {
        let ix = Instruction::new_with_bytes(
            deadman::id(),
            &deadman::instruction::CloseVault {}.data(),
            deadman::accounts::CloseVault {
                owner: env.owner.pubkey(),
                vault: env.vault_addr(),
                rent_payer: env.owner.pubkey(),
            }
            .to_account_metas(None),
        );
        let owner = env.owner.insecure_clone();
        env.send(ix, &[&owner])
    };
    let err = close(&mut env).unwrap_err();
    assert!(err.contains("FeeReserveNotEmpty"), "{err}");
    env.withdraw_reserve(20 * SKR).unwrap();
    close(&mut env).unwrap();
    assert!(env
        .svm
        .get_account(&env.vault_addr())
        .is_none_or(|a| a.lamports == 0));
}

#[test]
fn an_skr_mint_change_never_strands_the_reserve() {
    let (mut env, b) = sol_plan(10 * SOL, 100 * SKR);
    let old = env.skr;
    let admin = env.admin.insecure_clone();
    let new_skr = CreateMint::new(&mut env.svm, &admin)
        .decimals(6)
        .send()
        .unwrap();
    let ix = Instruction::new_with_bytes(
        deadman::id(),
        &deadman::instruction::SetConfig {
            fee_bps_public: 200,
            fee_bps_private: 200,
            skr_mint: new_skr,
            fee_bps_skr: 150,
            skr_burn_bps: 1_000,
        }
        .data(),
        deadman::accounts::SetConfig {
            admin: admin.pubkey(),
            config: config_pda(),
            treasury: env.treasury.pubkey(),
            system_program: system_program::ID,
        }
        .to_account_metas(None),
    );
    env.send(ix, &[&admin]).unwrap();

    // The old-mint reserve no longer pays fees or demands fee accounts...
    let gross = env.withdrawable();
    let treasury = env.lamports(&env.treasury.pubkey());
    env.execute_sol(&b, vec![]).unwrap();
    assert_eq!(env.lamports(&b), gross - gross * 200 / 10_000);
    assert_eq!(
        env.lamports(&env.treasury.pubkey()),
        treasury + gross * 200 / 10_000
    );
    // ...and the owner can still take it back in the old mint.
    env.skr = old;
    env.withdraw_reserve(100 * SKR).unwrap();
    assert_eq!(env.vault().skr_fee_reserve, 0);
}

#[test]
fn vesting_releases_pay_the_fee_in_skr() {
    let mut env = Env::new();
    let (b, c) = (Keypair::new().pubkey(), Keypair::new().pubkey());
    // The vault must exist before its token accounts.
    let usdc_admin = env.admin.insecure_clone();
    let usdc = CreateMint::new(&mut env.svm, &usdc_admin)
        .decimals(6)
        .send()
        .unwrap();
    let schedule = |beneficiary: Pubkey, mint: Option<Pubkey>, total: u64| VestingInput {
        beneficiary,
        rail: Rail::Solana,
        mint,
        total,
        cliff_secs: 0,
        duration_secs: 100 * DAY,
    };
    env.create_vesting(vec![
        schedule(b, None, 10 * SOL),
        schedule(c, Some(usdc), 1_000 * SKR),
    ])
    .unwrap();
    env.deposit_sol(10 * SOL);
    let (v, t) = (env.vault_addr(), env.treasury.pubkey());
    let vault_usdc = env.create_ata(&v, &usdc);
    env.create_ata(&t, &usdc);
    env.mint_to(&usdc, &vault_usdc, 1_000 * SKR);
    env.set_rates(&usdc, SKR_PER_SOL, SKR_PER_USDC).unwrap();
    env.fund(10_000 * SKR).unwrap();
    env.advance(50 * DAY);
    env.refresh_rates();

    let extra = env.skr_accounts();
    let ix = env.sol_ix(
        deadman::instruction::ReleaseVestedSol { index: 0 },
        &b,
        extra,
    );
    env.run(ix).unwrap();
    assert_eq!(env.lamports(&b), 5 * SOL);
    let sol_fee = model_fee(5 * SOL, SKR_PER_SOL, 9, 150);
    assert_eq!(env.vault().skr_fee_reserve, 10_000 * SKR - sol_fee);

    let extra = env.skr_accounts();
    let ix = env.token_ix(
        deadman::instruction::ReleaseVestedToken { index: 1 },
        &c,
        &usdc,
        extra,
    );
    env.run(ix).unwrap();
    assert_eq!(env.balance(&ata(&c, &usdc)), 500 * SKR);
    let usdc_fee = model_fee(500 * SKR, SKR_PER_USDC, 6, 150);
    assert_eq!(
        env.vault().skr_fee_reserve,
        10_000 * SKR - sol_fee - usdc_fee
    );
    assert_eq!(
        env.treasury_skr(),
        sol_fee - sol_fee / 10 + usdc_fee - usdc_fee / 10
    );
    assert_eq!(env.vault().rules[1].released, 500 * SKR);
}

#[test]
fn plans_without_a_reserve_need_no_fee_accounts() {
    let (mut env, b) = sol_plan(10 * SOL, 0);
    let gross = env.withdrawable();
    let treasury = env.lamports(&env.treasury.pubkey());
    env.execute_sol(&b, vec![]).unwrap();
    assert_sol_fallback(&env, &b, gross, 0, treasury);
}

#[test]
fn quote_math_handles_decimals_rounding_and_overflow() {
    use deadman::skr_fee_quote;
    // 1.5% of 1 SOL at 7,500 SKR per SOL: 112.5 SKR.
    assert_eq!(skr_fee_quote(SOL, SKR_PER_SOL, 9, 150), Some(112_500_000));
    // 0 decimals: whole units.
    assert_eq!(skr_fee_quote(2, 3, 0, 10_000), Some(6));
    // Round up.
    assert_eq!(skr_fee_quote(1, 1, 6, 150), Some(1));
    // Overflow is refused.
    assert_eq!(skr_fee_quote(u64::MAX, u64::MAX, 0, 500), None);
    assert_eq!(skr_fee_quote(u64::MAX, u64::MAX, 9, 500), None);
}
