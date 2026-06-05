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

// A pallet using `#[pallet::tasks_experimental]` automatically gets the
// `frame_system::Config<RuntimeTask: From<Task<Self>>>` supertrait bound added to its `Config`.
//
// `construct_runtime!` only generates `From<Pallet::Task>` for the `RuntimeTask` aggregate for
// pallets it includes. `pallet_not_in_runtime` below is *not* part of the runtime, so the aggregate
// `RuntimeTask` does not implement `From<pallet_not_in_runtime::Task<Runtime>>`. Implementing its
// `Config` for `Runtime` must therefore fail the auto-added supertrait bound.

use frame_support::derive_impl;

#[frame_support::pallet(dev_mode)]
pub mod pallet_in_runtime {
	use frame_support::pallet_prelude::DispatchResult;

	#[pallet::config]
	pub trait Config: frame_system::Config {}

	#[pallet::pallet]
	pub struct Pallet<T>(core::marker::PhantomData<T>);

	#[pallet::tasks_experimental]
	impl<T: Config> Pallet<T> {
		#[pallet::task_index(0)]
		#[pallet::task_condition(|i, j| i == 0u32 && j == 2u64)]
		#[pallet::task_list(vec![(0u32, 2u64), (2u32, 4u64)].iter())]
		#[pallet::task_weight(0.into())]
		fn foo(_i: u32, _j: u64) -> DispatchResult {
			Ok(())
		}
	}
}

#[frame_support::pallet(dev_mode)]
pub mod pallet_not_in_runtime {
	use frame_support::pallet_prelude::DispatchResult;

	#[pallet::config]
	pub trait Config: frame_system::Config {}

	#[pallet::pallet]
	pub struct Pallet<T>(core::marker::PhantomData<T>);

	#[pallet::tasks_experimental]
	impl<T: Config> Pallet<T> {
		#[pallet::task_index(0)]
		#[pallet::task_condition(|i, j| i == 0u32 && j == 2u64)]
		#[pallet::task_list(vec![(0u32, 2u64), (2u32, 4u64)].iter())]
		#[pallet::task_weight(0.into())]
		fn foo(_i: u32, _j: u64) -> DispatchResult {
			Ok(())
		}
	}
}

type Block = frame_system::mocking::MockBlock<Runtime>;

frame_support::construct_runtime!(
	pub enum Runtime {
		System: frame_system,
		PalletInRuntime: pallet_in_runtime,
	}
);

#[derive_impl(frame_system::config_preludes::TestDefaultConfig as frame_system::DefaultConfig)]
impl frame_system::Config for Runtime {
	type Block = Block;
}

impl pallet_in_runtime::Config for Runtime {}

// `RuntimeTask` does not implement `From<pallet_not_in_runtime::Task<Runtime>>`, so the auto-added
// supertrait bound on `pallet_not_in_runtime::Config` is not satisfied.
impl pallet_not_in_runtime::Config for Runtime {}

fn main() {}
