// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#![cfg(test)]
mod mock;

pub(crate) const LOG_TARGET: &str = "tests::e2e-epm";

use frame_support::{assert_err, assert_noop, assert_ok};
use mock::*;
use pallet_timestamp::Now;
use sp_core::Get;
use sp_runtime::Perbill;

use crate::mock::RuntimeOrigin;

use pallet_election_provider_multi_phase::CurrentPhase;

// syntactic sugar for logging.
#[macro_export]
macro_rules! log {
	($level:tt, $patter:expr $(, $values:expr)* $(,)?) => {
		log::$level!(
			target: crate::LOG_TARGET,
			concat!("🛠️  ", $patter)  $(, $values)*
		)
	};
}

fn log_current_time() {
	log!(
		trace,
		"block: {:?}, session: {:?}, era: {:?}, EPM phase: {:?} ts: {:?}",
		System::block_number(),
		Session::current_index(),
		pallet_staking::CurrentEra::<Runtime>::get(),
		CurrentPhase::<Runtime>::get(),
		Now::<Runtime>::get()
	);
}

#[test]
fn block_progression_works() {
	let (ext, pool_state, _) = ExtBuilder::default().build_offchainify();

	execute_with(ext, || {
		assert_eq!(active_era(), 0);
		assert_eq!(Session::current_index(), 0);
		assert!(CurrentPhase::<Runtime>::get().is_off());

		assert!(start_next_active_era(pool_state.clone()).is_ok());
		assert_eq!(active_era(), 1);
		assert_eq!(Session::current_index(), <SessionsPerEra as Get<u32>>::get());

		assert!(CurrentPhase::<Runtime>::get().is_off());

		roll_to_epm_signed();
		assert!(CurrentPhase::<Runtime>::get().is_signed());
	});

	let (ext, pool_state, _) = ExtBuilder::default().build_offchainify();

	execute_with(ext, || {
		assert_eq!(active_era(), 0);
		assert_eq!(Session::current_index(), 0);
		assert!(CurrentPhase::<Runtime>::get().is_off());

		assert!(start_next_active_era_delayed_solution(pool_state).is_ok());
		// if the solution is delayed, EPM will end up in emergency mode..
		assert!(CurrentPhase::<Runtime>::get().is_emergency());
		// .. era won't progress..
		assert_eq!(active_era(), 0);
		// .. but session does.
		assert_eq!(Session::current_index(), 2);
	})
}

#[test]
fn offchainify_works() {
	use pallet_election_provider_multi_phase::QueuedSolution;

	let staking_builder = StakingExtBuilder::default();
	let epm_builder = EpmExtBuilder::default();
	let (ext, pool_state, _) = ExtBuilder::default()
		.epm(epm_builder)
		.staking(staking_builder)
		.build_offchainify();

	execute_with(ext, || {
		// test ocw progression and solution queue if submission when unsigned phase submission is
		// not delayed.
		for _ in 0..100 {
			roll_one(pool_state.clone(), false);
			let current_phase = CurrentPhase::<Runtime>::get();

			assert!(
				match QueuedSolution::<Runtime>::get() {
					Some(_) => current_phase.is_unsigned(),
					None => !current_phase.is_unsigned(),
				},
				"solution must be queued *only* in unsigned phase"
			);
		}

		// test ocw solution queue if submission in unsigned phase is delayed.
		for _ in 0..100 {
			roll_one(pool_state.clone(), true);
			assert_eq!(
				QueuedSolution::<Runtime>::get(),
				None,
				"solution must never be submitted and stored since it is delayed"
			);
		}
	})
}

#[test]
/// Inspired by the Kusama incident of 8th Dec 2022 and its resolution through the governance
/// fallback.
///
/// Mass slash of validators shouldn't disable more than 1/3 of them (the byzantine threshold). Also
/// no new era should be forced which could lead to EPM entering emergency mode.
fn mass_slash_doesnt_enter_emergency_phase() {
	let epm_builder = EpmExtBuilder::default().disable_emergency_throttling();
	let staking_builder = StakingExtBuilder::default().validator_count(7);
	let (mut ext, _, _) = ExtBuilder::default()
		.epm(epm_builder)
		.staking(staking_builder)
		.build_offchainify();

	ext.execute_with(|| {
		assert_eq!(pallet_staking::ForceEra::<Runtime>::get(), pallet_staking::Forcing::NotForcing);

		let active_set_size_before_slash = Session::validators().len();

		// Slash more than 1/3 of the active validators
		let mut slashed = slash_half_the_active_set();

		let active_set_size_after_slash = Session::validators().len();

		// active set should stay the same before and after the slash
		assert_eq!(active_set_size_before_slash, active_set_size_after_slash);

		// Slashed validators are disabled up to a limit
		slashed.truncate(
			pallet_staking::UpToLimitDisablingStrategy::<SLASHING_DISABLING_FACTOR>::disable_limit(
				active_set_size_after_slash,
			),
		);

		// Find the indices of the disabled validators
		let active_set = Session::validators();
		let expected_disabled = slashed
			.into_iter()
			.map(|d| active_set.iter().position(|a| *a == d).unwrap() as u32)
			.collect::<Vec<_>>();

		assert_eq!(pallet_staking::ForceEra::<Runtime>::get(), pallet_staking::Forcing::NotForcing);
		assert_eq!(Session::disabled_validators(), expected_disabled);
	});
}

#[test]
/// Continuously slash 10% of the active validators per era.
///
/// Since the `OffendingValidatorsThreshold` is only checked per era staking does not force a new
/// era even as the number of active validators is decreasing across eras. When processing a new
/// slash, staking calculates the offending threshold based on the length of the current list of
/// active validators. Thus, slashing a percentage of the current validators that is lower than
/// `OffendingValidatorsThreshold` will never force a new era. However, as the slashes progress, if
/// the subsequent elections do not meet the minimum election untrusted score, the election will
/// fail and enter in emergency mode.
fn continuous_slashes_below_offending_threshold() {
	let staking_builder = StakingExtBuilder::default().validator_count(10);
	let epm_builder = EpmExtBuilder::default().disable_emergency_throttling();

	let (ext, pool_state, _) = ExtBuilder::default()
		.epm(epm_builder)
		.staking(staking_builder)
		.build_offchainify();

	execute_with(ext, || {
		assert_eq!(Session::validators().len(), 10);
		let mut active_validator_set = Session::validators();

		roll_to_epm_signed();

		// set a minimum election score.
		assert!(set_minimum_election_score(500, 1000, 500).is_ok());

		// slash 10% of the active validators and progress era until the minimum trusted score
		// is reached.
		while active_validator_set.len() > 0 {
			let slashed = slash_percentage(Perbill::from_percent(10));
			assert_eq!(slashed.len(), 1);

			// break loop when era does not progress; EPM is in emergency phase as election
			// failed due to election minimum score.
			if start_next_active_era(pool_state.clone()).is_err() {
				assert!(CurrentPhase::<Runtime>::get().is_emergency());
				break;
			}

			active_validator_set = Session::validators();

			log!(
				trace,
				"slashed 10% of active validators ({:?}). After slash: {:?}",
				slashed,
				active_validator_set
			);
		}
	});
}

#[test]
/// Active ledger balance may fall below ED if account chills before unbounding.
///
/// Unbonding call fails if the remaining ledger's stash balance falls below the existential
/// deposit. However, if the stash is chilled before unbonding, the ledger's active balance may
/// be below ED. In that case, only the stash (or root) can kill the ledger entry by calling
/// `withdraw_unbonded` after the bonding period has passed.
///
/// Related to <https://github.com/paritytech/substrate/issues/14246>.
fn ledger_consistency_active_balance_below_ed() {
	use pallet_staking::{Error, Event};

	let (ext, pool_state, _) =
		ExtBuilder::default().staking(StakingExtBuilder::default()).build_offchainify();

	execute_with(ext, || {
		assert_eq!(Staking::ledger(11.into()).unwrap().active, 1000);

		// unbonding total of active stake fails because the active ledger balance would fall
		// below the `MinNominatorBond`.
		assert_noop!(
			Staking::unbond(RuntimeOrigin::signed(11), 1000),
			Error::<Runtime>::InsufficientBond
		);

		// however, chilling works as expected.
		assert_ok!(Staking::chill(RuntimeOrigin::signed(11)));

		// now unbonding the full active balance works, since remainder of the active balance is
		// not enforced to be below `MinNominatorBond` if the stash has been chilled.
		assert_ok!(Staking::unbond(RuntimeOrigin::signed(11), 1000));

		// the active balance of the ledger entry is 0, while total balance is 1000 until
		// `withdraw_unbonded` is called.
		assert_eq!(Staking::ledger(11.into()).unwrap().active, 0);
		assert_eq!(Staking::ledger(11.into()).unwrap().total, 1000);

		// trying to withdraw the unbonded balance won't work yet because not enough bonding
		// eras have passed.
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0));
		assert_eq!(Staking::ledger(11.into()).unwrap().total, 1000);

		// tries to reap stash after chilling, which fails since the stash total balance is
		// above ED.
		assert_err!(
			Staking::reap_stash(RuntimeOrigin::signed(11), 21, 0),
			Error::<Runtime>::FundedTarget,
		);

		// check the events so far: 1x Chilled and 1x Unbounded
		assert_eq!(
			staking_events(),
			[Event::Chilled { stash: 11 }, Event::Unbonded { stash: 11, amount: 1000 }]
		);

		// after advancing `BondingDuration` eras, the `withdraw_unbonded` will unlock the
		// chunks and the ledger entry will be cleared, since the ledger active balance is 0.
		advance_eras(
			<Runtime as pallet_staking::Config>::BondingDuration::get() as usize,
			pool_state,
		);
		assert_ok!(Staking::withdraw_unbonded(RuntimeOrigin::signed(11), 0));
		assert!(Staking::ledger(11.into()).is_err());
	});
}

#[test]
/// Automatic withdrawal of unlocking funds in staking propagates to the nomination pools and its
/// state correctly.
///
/// The staking pallet may withdraw unlocking funds from a pool's bonded account without a pool
/// member or operator calling explicitly `Call::withdraw*`. This test verifies that the member's
/// are eventually paid and the `TotalValueLocked` is kept in sync in those cases.
fn automatic_unbonding_pools() {
	use pallet_nomination_pools::TotalValueLocked;

	// closure to fetch the staking unlocking chunks of an account.
	let unlocking_chunks_of = |account: AccountId| -> usize {
		Staking::ledger(sp_staking::StakingAccount::Controller(account))
			.unwrap()
			.unlocking
			.len()
	};

	let (ext, pool_state, _) = ExtBuilder::default()
		.pools(PoolsExtBuilder::default().max_unbonding(1))
		.staking(StakingExtBuilder::default().max_unlocking(1).bonding_duration(2))
		.build_offchainify();

	execute_with(ext, || {
		assert_eq!(<Runtime as pallet_staking::Config>::MaxUnlockingChunks::get(), 1);
		assert_eq!(<Runtime as pallet_staking::Config>::BondingDuration::get(), 2);
		assert_eq!(<Runtime as pallet_nomination_pools::Config>::MaxUnbonding::get(), 1);

		// init state of pool members.
		let init_stakeable_balance_2 = pallet_staking::asset::stakeable_balance::<Runtime>(&2);
		let init_stakeable_balance_3 = pallet_staking::asset::stakeable_balance::<Runtime>(&3);

		let pool_bonded_account = Pools::generate_bonded_account(1);

		// creates a pool with 5 bonded, owned by 1.
		assert_ok!(Pools::create(RuntimeOrigin::signed(1), 5, 1, 1, 1));
		assert_eq!(staked_amount_for(pool_bonded_account), 5);

		let init_tvl = TotalValueLocked::<Runtime>::get();

		// 2 joins the pool.
		assert_ok!(Pools::join(RuntimeOrigin::signed(2), 10, 1));
		assert_eq!(staked_amount_for(pool_bonded_account), 15);

		// 3 joins the pool.
		assert_ok!(Pools::join(RuntimeOrigin::signed(3), 10, 1));
		assert_eq!(staked_amount_for(pool_bonded_account), 25);

		assert_eq!(TotalValueLocked::<Runtime>::get(), 25);

		// currently unlocking 0 chunks in the bonded pools ledger.
		assert_eq!(unlocking_chunks_of(pool_bonded_account), 0);

		// unbond 2 from pool.
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(2), 2, 10));

		// amount is still locked in the pool, needs to wait for unbonding period.
		assert_eq!(staked_amount_for(pool_bonded_account), 25);

		// max chunks in the ledger are now filled up (`MaxUnlockingChunks == 1`).
		assert_eq!(unlocking_chunks_of(pool_bonded_account), 1);

		// tries to unbond 3 from pool. it will fail since there are no unlocking chunks left
		// available and the current in the queue haven't been there for more than bonding
		// duration.
		assert_err!(
			Pools::unbond(RuntimeOrigin::signed(3), 3, 10),
			pallet_staking::Error::<Runtime>::NoMoreChunks
		);

		assert_eq!(current_era(), 0);

		// progress over bonding duration.
		for _ in 0..=<Runtime as pallet_staking::Config>::BondingDuration::get() {
			start_next_active_era(pool_state.clone()).unwrap();
		}
		assert_eq!(current_era(), 3);
		System::reset_events();

		let staked_before_withdraw_pool = staked_amount_for(pool_bonded_account);
		assert_eq!(pallet_staking::asset::stakeable_balance::<Runtime>(&pool_bonded_account), 26);

		// now unbonding 3 will work, although the pool's ledger still has the unlocking chunks
		// filled up.
		assert_ok!(Pools::unbond(RuntimeOrigin::signed(3), 3, 10));
		assert_eq!(unlocking_chunks_of(pool_bonded_account), 1);

		assert_eq!(
			staking_events(),
			[
				// auto-withdraw happened as expected to release 2's unbonding funds, but the funds
				// were not transferred to 2 and stay in the pool's transferrable balance instead.
				pallet_staking::Event::Withdrawn { stash: 7939698191839293293, amount: 10 },
				pallet_staking::Event::Unbonded { stash: 7939698191839293293, amount: 10 }
			]
		);

		// balance of the pool remains the same, it hasn't withdraw explicitly from the pool yet.
		assert_eq!(pallet_staking::asset::stakeable_balance::<Runtime>(&pool_bonded_account), 26);
		// but the locked amount in the pool's account decreases due to the auto-withdraw:
		assert_eq!(staked_before_withdraw_pool - 10, staked_amount_for(pool_bonded_account));

		// TVL correctly updated.
		assert_eq!(TotalValueLocked::<Runtime>::get(), 25 - 10);

		// however, note that the withdrawing from the pool still works for 2, the funds are taken
		// from the pool's non staked balance.
		assert_eq!(pallet_staking::asset::stakeable_balance::<Runtime>(&pool_bonded_account), 26);
		assert_eq!(pallet_staking::asset::staked::<Runtime>(&pool_bonded_account), 15);
		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(2), 2, 10));
		assert_eq!(pallet_staking::asset::stakeable_balance::<Runtime>(&pool_bonded_account), 16);

		assert_eq!(pallet_staking::asset::stakeable_balance::<Runtime>(&2), 20);
		assert_eq!(TotalValueLocked::<Runtime>::get(), 15);

		// 3 cannot withdraw yet.
		assert_err!(
			Pools::withdraw_unbonded(RuntimeOrigin::signed(3), 3, 10),
			pallet_nomination_pools::Error::<Runtime>::CannotWithdrawAny
		);

		// progress over bonding duration.
		for _ in 0..=<Runtime as pallet_staking::Config>::BondingDuration::get() {
			start_next_active_era(pool_state.clone()).unwrap();
		}
		assert_eq!(current_era(), 6);
		System::reset_events();

		assert_ok!(Pools::withdraw_unbonded(RuntimeOrigin::signed(3), 3, 10));

		// final conditions are the expected.
		assert_eq!(pallet_staking::asset::stakeable_balance::<Runtime>(&pool_bonded_account), 6); // 5 init bonded + ED
		assert_eq!(
			pallet_staking::asset::stakeable_balance::<Runtime>(&2),
			init_stakeable_balance_2
		);
		assert_eq!(
			pallet_staking::asset::stakeable_balance::<Runtime>(&3),
			init_stakeable_balance_3
		);

		assert_eq!(TotalValueLocked::<Runtime>::get(), init_tvl);
	});
}
