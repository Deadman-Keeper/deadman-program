# Deadman program

The on-chain program behind [Deadman](https://x.com/deadmankeeper), a self-custody safety net for the Solana Seeker. It holds each plan's funds in a vault PDA and enforces who may check in, lock or withdraw, when a release tier falls due, where it pays and what fee it takes.

- **Program ID:** `BonEyRsg5PJLwLyceeRAu2QwgduaHcqvPHoerz9uZwK` (devnet)
- **Framework:** Anchor 1.1.2, Rust 1.89.0 (pinned in `rust-toolchain.toml`)
- **IDL:** [`idl/deadman.json`](idl/deadman.json)

> **Status: unaudited hackathon build, devnet only.** Do not put real funds in it.

## What it does

- **Release plans.** A vault with 1 to 8 ordered rules. Each rule pays a beneficiary a fixed amount or a percentage of one asset (SOL, any classic SPL token, or a classic NFT) once the owner has been silent for its `after_secs`. A check-in (`pulse`) resets every pending rule. Execution is permissionless and the destinations are fixed on-chain.
- **Coercion and loss.** A device guard key may only `pulse` and `lockdown`, never withdraw. A lockdown freezes withdrawals and plan changes until it expires or the owner and the guardian unlock together. It never blocks inheritance.
- **Vesting.** The same vault can release SOL or tokens to up to 8 people in installments, with an optional cliff, revocable or not.
- **Fees.** 2% of a payout, taken only when a rule or an installment is released; withdrawing, closing and revoking are free. A payout in SKR pays 1.5%, and 10% of that fee is burned. A plan can also keep an SKR fee reserve that pays the fee of a SOL or USDC payout in SKR, 1.5% of its value at the operator-set SKR rate, so the beneficiary receives 100%; a rate older than a day, any other token or a short reserve falls back to 2%. Every rate is capped at 5% in the program.

## Layout

```
programs/deadman/src/        the program (instructions/, state.rs, constants.rs, error.rs, events.rs)
programs/deadman/tests/      LiteSVM tests (test_*.rs) and audit regression tests (review_*.rs)
trident-tests/               Trident fuzz harness with invariants
qedgen/                      QEDGen spec
idl/deadman.json             the IDL of the deployed build
```

## Build and test

```bash
anchor build                        # writes target/deploy/deadman.so and target/idl/deadman.json
cargo fmt --all
cargo clippy --all-targets -- -W clippy::all
cargo test                          # the LiteSVM tests load target/deploy/deadman.so, so build first

cd trident-tests && trident fuzz run fuzz_0
```

Check the devnet deployment with `solana program show BonEyRsg5PJLwLyceeRAu2QwgduaHcqvPHoerz9uZwK --url devnet`.

## Security notes

- **Unaudited and not a verifiable build yet.** Internal reviews and fuzzing only.
- **Single-key admin.** The upgrade authority and the Config admin are single keys on devnet. The Config admin rotates in two steps (`propose_admin`, then `accept_admin`). A multisig is planned before mainnet.
- **Fees are read at execution.** The admin can change the rates, up to the 5% cap, and the new rate applies to rules that have not executed yet.
- **SKR rates are set by the operator, not an oracle.** The admin sets the SKR price of SOL and of USDC and a rate key (`set_skr_rates`); the rate key, run by a script that reads Jupiter, refreshes them (`update_skr_rates`), at most 20% a day away from a daily reference. A rate older than 24 hours is not used. A wrong rate can misprice the SKR fee, up to the plan's reserve.

Report security issues privately to the maintainers through GitHub rather than in a public issue.

## History

This repository was split out of the Deadman monorepo on 2026-10-14 and keeps the commit history of its `onchain/` directory, so some commit messages describe app changes made in the same commit.

## License

[Apache License 2.0](LICENSE).
