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

//! # Cron Pallet (POC)
//!
//! Permissionless scheduling of `RuntimeCall`s at future timestamps, one-time or recurring.
//! POC for <https://github.com/paritytech/polkadot-sdk/issues/9966>.
//!
//! Execution is driven by the FRAME task system: the offchain worker enumerates due tasks and
//! submits them as unsigned `frame_system::do_task` transactions, which are only accepted from
//! local/in-block sources.
//!
//! Cost model:
//! - Storage is paid via [`Consideration`], returned on completion or cancellation.
//! - Execution is prepaid into a held balance. Each run burns the charge for the call's weight at
//!   execution-time prices via `Config::ExecutionCharge`. An underfunded task is paused and resumes
//!   via `top_up`.
//!
//! Timing is bucketed (`Config::Bucket`): a task runs in the first block at or after `next_run`
//! with spare capacity. Execution in a specific block, or at a position within one, cannot be
//! bought.
//!
//! Block space: scheduled calls share a per-block weight budget
//! (`Config::MaxServiceWeightPerBlock`) tracked in `ServiceWeightUsed` and reset in `on_finalize`.
//! A due call runs only if it fits the remaining budget, else it waits for a later block. A task
//! may set a `grace` window after `next_run`; a run that misses it is skipped, not executed late.
//!
//! Call filters are checked at scheduling time and again at execution time, since filters are
//! not stored with the call and may change in between.

#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

use alloc::boxed::Box;
use codec::Decode;
use frame_support::{
	dispatch::{GetDispatchInfo, PostDispatchInfo},
	pallet_prelude::*,
	traits::{
		fungible::{Inspect, MutateHold},
		tokens::{Fortitude, Precision},
		Consideration, Contains, Footprint,
	},
};
use frame_system::{offchain::CreateBare, pallet_prelude::*, RawOrigin};
use sp_runtime::traits::{
	AtLeast32Bit, Convert, Dispatchable, One, Saturating, UniqueSaturatedInto, Zero,
};

pub use pallet::*;
// Disambiguate from the `Task` trait in `pallet_prelude`.
pub use pallet::Task;

pub mod mock;
pub mod tests;

#[cfg(feature = "experimental")]
const LOG_TARGET: &str = "pallet-cron";

/// Max due tasks submitted per offchain worker invocation.
#[cfg(feature = "experimental")]
const MAX_TASKS_PER_OCW: usize = 32;

pub type BalanceOf<T> =
	<<T as Config>::Currency as Inspect<<T as frame_system::Config>::AccountId>>::Balance;
pub type MomentOf<T> = <T as pallet_timestamp::Config>::Moment;
pub type CallOf<T> = <T as Config>::RuntimeCall;

/// When and how often a task runs.
#[derive(
	Clone,
	Copy,
	Encode,
	Decode,
	DecodeWithMemTracking,
	Eq,
	PartialEq,
	Debug,
	TypeInfo,
	MaxEncodedLen,
)]
pub enum Schedule<Moment> {
	OneTime { at: Moment },
	Recurring { start_at: Moment, interval: Moment, max_executions: Option<u32> },
}

impl<Moment: AtLeast32Bit + Copy> Schedule<Moment> {
	/// The first run strictly after `now`, anchored at the schedule start so recurring tasks
	/// don't drift when an execution is delayed. `None` if no run remains.
	pub fn run_after(&self, now: Moment) -> Option<Moment> {
		match *self {
			Schedule::OneTime { at } => (at > now).then_some(at),
			Schedule::Recurring { start_at, .. } if start_at > now => Some(start_at),
			Schedule::Recurring { start_at, interval, .. } => {
				if interval.is_zero() {
					return None;
				}
				let periods = (now - start_at) / interval + One::one();
				Some(start_at.saturating_add(periods.saturating_mul(interval)))
			},
		}
	}
}

#[derive(
	Clone,
	Copy,
	Encode,
	Decode,
	DecodeWithMemTracking,
	Eq,
	PartialEq,
	Debug,
	TypeInfo,
	MaxEncodedLen,
)]
pub enum TaskStatus {
	Active,
	/// Prepaid funds ran out; resumes on `top_up`.
	Paused,
}

#[derive(CloneNoBound, EqNoBound, PartialEqNoBound, DebugNoBound, Encode, Decode, TypeInfo)]
#[scale_info(skip_type_params(T))]
pub struct TaskDetails<T: Config> {
	pub scheduler: T::AccountId,
	/// The encoded call. Decoded and dispatched with `Signed(scheduler)` origin.
	pub call: BoundedVec<u8, T::MaxCallLen>,
	pub schedule: Schedule<MomentOf<T>>,
	pub next_run: MomentOf<T>,
	/// Window after `next_run` in which the run must happen, else it is skipped. `None` never
	/// expires.
	pub grace: Option<MomentOf<T>>,
	/// Storage deposit ticket.
	pub ticket: T::Consideration,
	/// Remaining prepaid execution funds, held under `HoldReason::Prepay`.
	pub prepaid: BalanceOf<T>,
	/// `None` for unlimited recurring tasks.
	pub executions_remaining: Option<u32>,
	pub status: TaskStatus,
}

#[frame_support::pallet(dev_mode)]
pub mod pallet {
	use super::*;

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::config]
	pub trait Config:
		CreateBare<frame_system::Call<Self>> + pallet_timestamp::Config + frame_system::Config
	{
		/// The overarching call type that can be scheduled.
		type RuntimeCall: Parameter
			+ Dispatchable<
				RuntimeOrigin = <Self as frame_system::Config>::RuntimeOrigin,
				PostInfo = PostDispatchInfo,
			> + GetDispatchInfo
			+ From<frame_system::Call<Self>>;

		/// The aggregated task type, for offchain submission of due tasks.
		type RuntimeTask: frame_support::traits::Task
			+ IsType<<Self as frame_system::Config>::RuntimeTask>
			+ From<Task<Self>>;

		type Currency: MutateHold<Self::AccountId, Reason = Self::RuntimeHoldReason>;
		type RuntimeHoldReason: From<HoldReason>;

		/// Storage deposit for a scheduled task.
		type Consideration: Consideration<Self::AccountId, Footprint>;

		/// Per-execution charge for a `(call weight, encoded call length)`. Wire to
		/// `pallet_transaction_payment::compute_fee` so scheduling is never cheaper than the
		/// normal inclusion fee for the same call.
		type ExecutionCharge: Convert<(Weight, u32), BalanceOf<Self>>;

		/// Calls that may be scheduled. Checked at scheduling and again at execution.
		type ScheduleFilter: Contains<<Self as Config>::RuntimeCall>;

		#[pallet::constant]
		type MaxTasksPerAccount: Get<u32>;

		/// Max encoded length of a scheduled call.
		#[pallet::constant]
		type MaxCallLen: Get<u32>;

		/// Width of an agenda time bucket, in timestamp moments.
		#[pallet::constant]
		type Bucket: Get<MomentOf<Self>>;

		/// Per-block weight budget for scheduled calls. A due call runs only if it fits the
		/// remaining budget, else it waits for a later block. Set to a fraction of
		/// `BlockWeights::max_block` to leave room for normal transactions.
		#[pallet::constant]
		type MaxServiceWeightPerBlock: Get<Weight>;
	}

	#[pallet::composite_enum]
	pub enum HoldReason {
		/// Prepaid execution funds of scheduled tasks.
		#[codec(index = 0)]
		Prepay,
		/// Storage deposit of scheduled tasks.
		#[codec(index = 1)]
		StorageDeposit,
	}

	#[pallet::storage]
	pub type Tasks<T: Config> = StorageMap<_, Blake2_128Concat, u64, TaskDetails<T>, OptionQuery>;

	#[pallet::storage]
	pub type NextTaskId<T: Config> = StorageValue<_, u64, ValueQuery>;

	/// Index of active tasks by time bucket of their `next_run`.
	#[pallet::storage]
	pub type Agenda<T: Config> =
		StorageDoubleMap<_, Twox64Concat, u64, Twox64Concat, u64, (), OptionQuery>;

	/// The task currently being dispatched, set only while its call is on the stack. Lets a
	/// target pallet detect scheduled execution without a dedicated origin.
	#[pallet::storage]
	pub type Executing<T: Config> = StorageValue<_, u64, OptionQuery>;

	/// Weight consumed by scheduled calls in the current block. Reset in `on_finalize`.
	#[pallet::storage]
	pub type ServiceWeightUsed<T: Config> = StorageValue<_, Weight, ValueQuery>;

	#[pallet::storage]
	pub type TasksByScheduler<T: Config> = StorageMap<
		_,
		Blake2_128Concat,
		T::AccountId,
		BoundedVec<u64, T::MaxTasksPerAccount>,
		ValueQuery,
	>;

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		Scheduled {
			task_id: u64,
			scheduler: T::AccountId,
			first_run: MomentOf<T>,
		},
		Dispatched {
			task_id: u64,
			result: DispatchResult,
		},
		Completed {
			task_id: u64,
		},
		Cancelled {
			task_id: u64,
		},
		Paused {
			task_id: u64,
		},
		ToppedUp {
			task_id: u64,
			amount: BalanceOf<T>,
		},
		/// A run was skipped because it did not happen within its grace window.
		Skipped {
			task_id: u64,
		},
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The call is not allowed by `ScheduleFilter`.
		Filtered,
		/// The encoded call exceeds `MaxCallLen`.
		CallTooLarge,
		TooManyTasks,
		/// The schedule has no run in the future.
		InThePast,
		ZeroInterval,
		ZeroExecutions,
		NotFound,
		/// Origin is not the task's scheduler.
		NotScheduler,
		NotDue,
		UndecodableCall,
		/// The block's scheduled-call weight budget is exhausted; retried next block.
		BlockFull,
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Schedule `call` for future execution with `Signed(origin)` origin, prepaying
		/// `prepay` towards execution charges.
		pub fn schedule(
			origin: OriginFor<T>,
			call: Box<CallOf<T>>,
			schedule: Schedule<MomentOf<T>>,
			grace: Option<MomentOf<T>>,
			#[pallet::compact] prepay: BalanceOf<T>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			ensure!(T::ScheduleFilter::contains(&call), Error::<T>::Filtered);
			if let Schedule::Recurring { interval, max_executions, .. } = &schedule {
				ensure!(!interval.is_zero(), Error::<T>::ZeroInterval);
				ensure!(*max_executions != Some(0), Error::<T>::ZeroExecutions);
			}
			let now = Self::now();
			let first_run = schedule.run_after(now).ok_or(Error::<T>::InThePast)?;
			let bounded: BoundedVec<u8, T::MaxCallLen> =
				call.encode().try_into().map_err(|_| Error::<T>::CallTooLarge)?;

			let task_id = NextTaskId::<T>::mutate(|id| {
				let this = *id;
				*id = id.saturating_add(1);
				this
			});
			TasksByScheduler::<T>::try_mutate(&who, |ids| {
				ids.try_push(task_id).map_err(|_| Error::<T>::TooManyTasks)
			})?;
			let ticket = T::Consideration::new(&who, Footprint::from_parts(1, bounded.len()))?;
			T::Currency::hold(&HoldReason::Prepay.into(), &who, prepay)?;

			let executions_remaining = match &schedule {
				Schedule::OneTime { .. } => Some(1),
				Schedule::Recurring { max_executions, .. } => *max_executions,
			};
			Tasks::<T>::insert(
				task_id,
				TaskDetails {
					scheduler: who.clone(),
					call: bounded,
					schedule,
					next_run: first_run,
					grace,
					ticket,
					prepaid: prepay,
					executions_remaining,
					status: TaskStatus::Active,
				},
			);
			Agenda::<T>::insert(Self::bucket(first_run), task_id, ());
			Self::deposit_event(Event::Scheduled { task_id, scheduler: who, first_run });
			Ok(())
		}

		/// Cancel a task, releasing remaining prepaid funds and the storage deposit.
		pub fn cancel(origin: OriginFor<T>, #[pallet::compact] task_id: u64) -> DispatchResult {
			let who = ensure_signed(origin)?;
			let task = Tasks::<T>::get(task_id).ok_or(Error::<T>::NotFound)?;
			ensure!(task.scheduler == who, Error::<T>::NotScheduler);
			Self::remove_task(task_id, task)?;
			Self::deposit_event(Event::Cancelled { task_id });
			Ok(())
		}

		/// Add prepaid execution funds. Resumes a paused task.
		pub fn top_up(
			origin: OriginFor<T>,
			#[pallet::compact] task_id: u64,
			#[pallet::compact] amount: BalanceOf<T>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			Tasks::<T>::try_mutate(task_id, |maybe_task| {
				let task = maybe_task.as_mut().ok_or(Error::<T>::NotFound)?;
				ensure!(task.scheduler == who, Error::<T>::NotScheduler);
				T::Currency::hold(&HoldReason::Prepay.into(), &who, amount)?;
				task.prepaid = task.prepaid.saturating_add(amount);
				if task.status == TaskStatus::Paused {
					task.status = TaskStatus::Active;
					Agenda::<T>::insert(Self::bucket(task.next_run), task_id, ());
				}
				Self::deposit_event(Event::ToppedUp { task_id, amount });
				Ok(())
			})
		}
	}

	#[pallet::tasks_experimental]
	impl<T: Config> Pallet<T> {
		#[pallet::task_list(Pallet::<T>::due_tasks())]
		#[pallet::task_condition(|task_id| Pallet::<T>::should_service(task_id))]
		#[pallet::task_weight(Pallet::<T>::execution_weight(task_id))]
		#[pallet::task_index(0)]
		pub fn execute_scheduled_call(task_id: u64) -> DispatchResult {
			Pallet::<T>::do_execute(task_id)
		}
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(_n: BlockNumberFor<T>) -> Weight {
			// Reserve weight for the `on_finalize` reset.
			T::DbWeight::get().writes(1)
		}

		fn on_finalize(_n: BlockNumberFor<T>) {
			ServiceWeightUsed::<T>::kill();
		}

		#[cfg(feature = "experimental")]
		fn offchain_worker(_block_number: BlockNumberFor<T>) {
			use frame_system::offchain::SubmitTransaction;
			for task_id in Self::due_tasks().take(MAX_TASKS_PER_OCW) {
				let task = Task::<T>::ExecuteScheduledCall { task_id };
				let runtime_task = <T as Config>::RuntimeTask::from(task);
				let call = frame_system::Call::<T>::do_task { task: runtime_task.into() };
				let xt = <T as CreateBare<frame_system::Call<T>>>::create_bare(call.into());
				if let Err(e) =
					SubmitTransaction::<T, frame_system::Call<T>>::submit_transaction(xt)
				{
					log::warn!(target: LOG_TARGET, "failed to submit task {task_id}: {e:?}");
				}
			}
		}

		#[cfg(not(feature = "experimental"))]
		fn offchain_worker(_block_number: BlockNumberFor<T>) {}
	}

	impl<T: Config> Pallet<T> {
		fn now() -> MomentOf<T> {
			pallet_timestamp::Now::<T>::get()
		}

		fn bucket(moment: MomentOf<T>) -> u64 {
			let width = T::Bucket::get().max(One::one());
			(moment / width).unique_saturated_into()
		}

		/// Active tasks whose `next_run` has passed.
		pub fn due_tasks() -> impl Iterator<Item = u64> {
			let current = Self::bucket(Self::now());
			Agenda::<T>::iter()
				.filter(move |(bucket, _, _)| *bucket <= current)
				.map(|(_, task_id, _)| task_id)
				.filter(|task_id| Self::is_due(*task_id))
		}

		/// The task whose call is currently being dispatched, if any. A target pallet reads this
		/// to apply scheduled-specific logic (fees, rate limits, audit).
		pub fn executing_task() -> Option<u64> {
			Executing::<T>::get()
		}

		pub fn is_due(task_id: u64) -> bool {
			Tasks::<T>::get(task_id)
				.map(|t| t.status == TaskStatus::Active && t.next_run <= Self::now())
				.unwrap_or(false)
		}

		/// Whether a due task should run in the current block. An expired run is always
		/// serviced since cleanup is cheap, otherwise the call must fit the block's budget.
		pub fn should_service(task_id: u64) -> bool {
			let Some(task) = Tasks::<T>::get(task_id) else { return false };
			let now = Self::now();
			if task.status != TaskStatus::Active || task.next_run > now {
				return false;
			}
			Self::is_expired(&task, now) || Self::fits(Self::call_weight(&task))
		}

		fn is_expired(task: &TaskDetails<T>, now: MomentOf<T>) -> bool {
			task.grace.map_or(false, |g| now > task.next_run.saturating_add(g))
		}

		fn call_weight(task: &TaskDetails<T>) -> Weight {
			CallOf::<T>::decode(&mut &task.call[..])
				.map(|c| c.get_dispatch_info().total_weight())
				.unwrap_or_default()
		}

		fn fits(call_weight: Weight) -> bool {
			ServiceWeightUsed::<T>::get()
				.saturating_add(call_weight)
				.all_lte(T::MaxServiceWeightPerBlock::get())
		}

		/// `do_task` weight: fixed overhead plus the call's weight when it will dispatch.
		pub fn execution_weight(task_id: u64) -> Weight {
			let overhead = T::DbWeight::get().reads_writes(4, 4);
			let Some(task) = Tasks::<T>::get(task_id) else { return overhead };
			if Self::is_expired(&task, Self::now()) {
				return overhead;
			}
			overhead.saturating_add(Self::call_weight(&task))
		}

		fn do_execute(task_id: u64) -> DispatchResult {
			let mut task = Tasks::<T>::get(task_id).ok_or(Error::<T>::NotFound)?;
			let now = Self::now();
			ensure!(task.status == TaskStatus::Active, Error::<T>::NotDue);
			ensure!(task.next_run <= now, Error::<T>::NotDue);

			// Missed its grace window: skip this run without charging or dispatching.
			if Self::is_expired(&task, now) {
				Self::deposit_event(Event::Skipped { task_id });
				return Self::reschedule(task_id, task, now, false);
			}

			// Decoding can only fail if a runtime upgrade changed call encoding.
			let call = CallOf::<T>::decode(&mut &task.call[..])
				.map_err(|_| Error::<T>::UndecodableCall)?;
			let call_weight = call.get_dispatch_info().total_weight();
			// The condition already gated capacity; re-check so a full block leaves the task
			// in the agenda for a later block.
			ensure!(Self::fits(call_weight), Error::<T>::BlockFull);

			let charge = T::ExecutionCharge::convert((call_weight, task.call.len() as u32));
			if task.prepaid < charge {
				// Pause rather than error so the state change persists.
				Agenda::<T>::remove(Self::bucket(task.next_run), task_id);
				task.status = TaskStatus::Paused;
				Tasks::<T>::insert(task_id, task);
				Self::deposit_event(Event::Paused { task_id });
				return Ok(());
			}
			// POC: charges are burnt. Production should route them to the block author or
			// treasury via `OnUnbalanced`.
			T::Currency::burn_held(
				&HoldReason::Prepay.into(),
				&task.scheduler,
				charge,
				Precision::Exact,
				Fortitude::Force,
			)?;
			task.prepaid = task.prepaid.saturating_sub(charge);
			ServiceWeightUsed::<T>::mutate(|w| *w = w.saturating_add(call_weight));

			// Re-check the filter: it may have changed since scheduling. Dispatch also
			// enforces the origin's `BaseCallFilter`.
			Executing::<T>::put(task_id);
			let result = if T::ScheduleFilter::contains(&call) {
				call.dispatch(RawOrigin::Signed(task.scheduler.clone()).into())
					.map(|_| ())
					.map_err(|e| e.error)
			} else {
				Err(frame_system::Error::<T>::CallFiltered.into())
			};
			Executing::<T>::kill();
			Self::deposit_event(Event::Dispatched { task_id, result });
			Self::reschedule(task_id, task, now, true)
		}

		/// Advance a task to its next run or finish it. `consumed` decrements the remaining
		/// execution count for a run that dispatched.
		fn reschedule(
			task_id: u64,
			mut task: TaskDetails<T>,
			now: MomentOf<T>,
			consumed: bool,
		) -> DispatchResult {
			Agenda::<T>::remove(Self::bucket(task.next_run), task_id);
			if consumed {
				task.executions_remaining = task.executions_remaining.map(|n| n.saturating_sub(1));
			}
			let finished = task.executions_remaining == Some(0);
			match task.schedule.run_after(now) {
				Some(next_run) if !finished => {
					task.next_run = next_run;
					Agenda::<T>::insert(Self::bucket(next_run), task_id, ());
					Tasks::<T>::insert(task_id, task);
				},
				_ => {
					Self::remove_task(task_id, task)?;
					Self::deposit_event(Event::Completed { task_id });
				},
			}
			Ok(())
		}

		fn remove_task(task_id: u64, task: TaskDetails<T>) -> DispatchResult {
			let TaskDetails { scheduler, ticket, prepaid, next_run, .. } = task;
			Agenda::<T>::remove(Self::bucket(next_run), task_id);
			T::Currency::release(
				&HoldReason::Prepay.into(),
				&scheduler,
				prepaid,
				Precision::BestEffort,
			)?;
			ticket.drop(&scheduler)?;
			TasksByScheduler::<T>::mutate(&scheduler, |ids| ids.retain(|id| *id != task_id));
			Tasks::<T>::remove(task_id);
			Ok(())
		}
	}
}
