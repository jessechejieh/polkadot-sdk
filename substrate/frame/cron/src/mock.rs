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

//! Mock runtime for `pallet-cron` tests.
#![cfg(test)]

use crate as pallet_cron;
use frame_support::{
	derive_impl, parameter_types,
	traits::{fungible::HoldConsideration, ConstU32, ConstU64, Contains, Footprint},
	weights::Weight,
};
use sp_runtime::{traits::Convert, BuildStorage};

pub type AccountId = u64;
pub type Balance = u64;

/// Weight of the `noop::work` call, used to size the per-block service budget.
pub const SERVICE_WEIGHT: Weight = Weight::from_parts(2_000_000_000, 0);

/// Minimal pallet with one weighted no-op call, so scheduled calls have a real weight.
#[frame_support::pallet]
pub mod noop {
	use frame_support::pallet_prelude::*;
	use frame_system::pallet_prelude::*;

	#[pallet::config]
	pub trait Config: frame_system::Config + crate::Config {}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	/// Set by `work` to whether it ran inside a scheduled dispatch.
	#[pallet::storage]
	pub type RanScheduled<T> = StorageValue<_, bool, ValueQuery>;

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		#[pallet::call_index(0)]
		#[pallet::weight(super::SERVICE_WEIGHT)]
		pub fn work(origin: OriginFor<T>) -> DispatchResult {
			ensure_signed(origin)?;
			RanScheduled::<T>::put(crate::Pallet::<T>::executing_task().is_some());
			Ok(())
		}
	}
}

impl noop::Config for Test {}

type Block = frame_system::mocking::MockBlock<Test>;
frame_support::construct_runtime!(
	pub enum Test {
		System: frame_system,
		Timestamp: pallet_timestamp,
		Balances: pallet_balances,
		Noop: noop,
		Cron: pallet_cron,
	}
);

pub type Extrinsic = sp_runtime::testing::TestXt<RuntimeCall, ()>;

#[derive_impl(frame_system::config_preludes::TestDefaultConfig)]
impl frame_system::Config for Test {
	type Block = Block;
	type AccountData = pallet_balances::AccountData<Balance>;
}

impl pallet_timestamp::Config for Test {
	type Moment = u64;
	type OnTimestampSet = ();
	type MinimumPeriod = ConstU64<1>;
	type WeightInfo = ();
}

#[derive_impl(pallet_balances::config_preludes::TestDefaultConfig)]
impl pallet_balances::Config for Test {
	type AccountStore = System;
}

impl<LocalCall> frame_system::offchain::CreateTransactionBase<LocalCall> for Test
where
	RuntimeCall: From<LocalCall>,
{
	type RuntimeCall = RuntimeCall;
	type Extrinsic = Extrinsic;
}

impl<LocalCall> frame_system::offchain::CreateBare<LocalCall> for Test
where
	RuntimeCall: From<LocalCall>,
{
	fn create_bare(call: Self::RuntimeCall) -> Self::Extrinsic {
		Extrinsic::new_bare(call)
	}
}

parameter_types! {
	pub const CronDepositReason: RuntimeHoldReason =
		RuntimeHoldReason::Cron(pallet_cron::HoldReason::StorageDeposit);
	pub static ScheduleAllowed: bool = true;
}

pub struct Deposit;
impl Convert<Footprint, Balance> for Deposit {
	fn convert(fp: Footprint) -> Balance {
		fp.count + fp.size
	}
}

/// Flat charge per execution.
pub const FLAT_FEE: Balance = 10;

pub struct FlatFee;
impl Convert<(Weight, u32), Balance> for FlatFee {
	fn convert(_: (Weight, u32)) -> Balance {
		FLAT_FEE
	}
}

parameter_types! {
	// Budget for exactly one `noop::work` per block.
	pub MaxService: Weight = SERVICE_WEIGHT;
}

pub struct Filter;
impl Contains<RuntimeCall> for Filter {
	fn contains(call: &RuntimeCall) -> bool {
		ScheduleAllowed::get() &&
			!matches!(call, RuntimeCall::System(frame_system::Call::set_code { .. }))
	}
}

impl pallet_cron::Config for Test {
	type RuntimeCall = RuntimeCall;
	type RuntimeTask = RuntimeTask;
	type Currency = Balances;
	type RuntimeHoldReason = RuntimeHoldReason;
	type Consideration = HoldConsideration<AccountId, Balances, CronDepositReason, Deposit>;
	type ExecutionCharge = FlatFee;
	type ScheduleFilter = Filter;
	type MaxTasksPerAccount = ConstU32<8>;
	type MaxCallLen = ConstU32<1024>;
	type Bucket = ConstU64<60>;
	type MaxServiceWeightPerBlock = MaxService;
}

pub fn set_time(now: u64) {
	pallet_timestamp::Now::<Test>::put(now);
}

/// Run `on_finalize` for the current block, then advance to the next: resets the service budget.
pub fn end_block() {
	use frame_support::traits::Hooks;
	Cron::on_finalize(System::block_number());
	System::set_block_number(System::block_number() + 1);
}

pub fn new_test_ext() -> sp_io::TestExternalities {
	let mut t = frame_system::GenesisConfig::<Test>::default().build_storage().unwrap();
	pallet_balances::GenesisConfig::<Test> {
		balances: vec![(1, 1000), (2, 1000)],
		..Default::default()
	}
	.assimilate_storage(&mut t)
	.unwrap();
	let mut ext: sp_io::TestExternalities = t.into();
	ext.execute_with(|| {
		System::set_block_number(1);
		set_time(100);
	});
	ext
}
