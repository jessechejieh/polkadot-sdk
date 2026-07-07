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

//! Tests for `pallet-cron`.
#![cfg(test)]

use crate::{
	mock::*, Agenda, Error, Event, HoldReason, Schedule, ServiceWeightUsed, TaskStatus, Tasks,
};
use frame_support::{
	assert_noop, assert_ok,
	traits::{fungible::InspectHold, Task as TaskTrait},
	weights::Weight,
};

fn remark() -> Box<RuntimeCall> {
	Box::new(RuntimeCall::System(frame_system::Call::remark { remark: vec![42] }))
}

fn noop() -> Box<RuntimeCall> {
	Box::new(RuntimeCall::Noop(crate::mock::noop::Call::work {}))
}

fn schedule_remark(schedule: Schedule<u64>, prepay: u64) {
	assert_ok!(Cron::schedule(RuntimeOrigin::signed(1), remark(), schedule, None, prepay));
}

fn run_task(task_id: u64) {
	let task = crate::pallet::Task::<Test>::ExecuteScheduledCall { task_id };
	assert!(task.is_valid());
	assert_ok!(task.run());
}

fn task_valid(task_id: u64) -> bool {
	crate::pallet::Task::<Test>::ExecuteScheduledCall { task_id }.is_valid()
}

fn prepay_held(who: AccountId) -> Balance {
	Balances::balance_on_hold(&HoldReason::Prepay.into(), &who)
}

fn deposit_held(who: AccountId) -> Balance {
	Balances::balance_on_hold(&HoldReason::StorageDeposit.into(), &who)
}

#[test]
fn schedule_works() {
	new_test_ext().execute_with(|| {
		schedule_remark(Schedule::OneTime { at: 200 }, 50);

		let task = Tasks::<Test>::get(0).unwrap();
		assert_eq!(task.scheduler, 1);
		assert_eq!(task.next_run, 200);
		assert_eq!(task.prepaid, 50);
		assert_eq!(task.executions_remaining, Some(1));
		assert_eq!(task.status, TaskStatus::Active);
		assert!(Agenda::<Test>::contains_key(200 / 60, 0));
		assert_eq!(prepay_held(1), 50);
		assert!(deposit_held(1) > 0);
		assert!(!Cron::is_due(0));
	});
}

#[test]
fn one_time_executes_and_completes() {
	new_test_ext().execute_with(|| {
		schedule_remark(Schedule::OneTime { at: 200 }, 50);

		// Due only once `next_run` is reached, not at bucket start.
		set_time(199);
		assert!(!crate::pallet::Task::<Test>::ExecuteScheduledCall { task_id: 0 }.is_valid());

		set_time(200);
		assert_eq!(Cron::due_tasks().collect::<Vec<_>>(), vec![0]);
		run_task(0);

		System::assert_has_event(Event::Dispatched { task_id: 0, result: Ok(()) }.into());
		System::assert_has_event(Event::Completed { task_id: 0 }.into());
		assert!(Tasks::<Test>::get(0).is_none());
		assert_eq!(prepay_held(1), 0);
		assert_eq!(deposit_held(1), 0);
		// Unused prepay refunded, only the execution charge burnt.
		assert_eq!(Balances::free_balance(1), 1000 - FLAT_FEE);
	});
}

#[test]
fn recurring_reschedules_then_completes() {
	new_test_ext().execute_with(|| {
		schedule_remark(
			Schedule::Recurring { start_at: 200, interval: 100, max_executions: Some(2) },
			2 * FLAT_FEE,
		);

		set_time(200);
		run_task(0);
		let task = Tasks::<Test>::get(0).unwrap();
		assert_eq!(task.next_run, 300);
		assert_eq!(task.prepaid, FLAT_FEE);
		assert_eq!(task.executions_remaining, Some(1));
		assert!(Agenda::<Test>::contains_key(300 / 60, 0));

		set_time(300);
		run_task(0);
		assert!(Tasks::<Test>::get(0).is_none());
		assert_eq!(prepay_held(1), 0);
		assert_eq!(deposit_held(1), 0);
		assert_eq!(Balances::free_balance(1), 1000 - 2 * FLAT_FEE);
	});
}

#[test]
fn delayed_recurring_run_does_not_drift() {
	new_test_ext().execute_with(|| {
		schedule_remark(
			Schedule::Recurring { start_at: 200, interval: 100, max_executions: None },
			10 * FLAT_FEE,
		);

		// Executed late at 250, next run stays on the 200 + k * 100 grid.
		set_time(250);
		run_task(0);
		assert_eq!(Tasks::<Test>::get(0).unwrap().next_run, 300);
	});
}

#[test]
fn pauses_when_underfunded_and_top_up_resumes() {
	new_test_ext().execute_with(|| {
		schedule_remark(
			Schedule::Recurring { start_at: 200, interval: 100, max_executions: None },
			FLAT_FEE,
		);

		set_time(200);
		run_task(0);
		assert_eq!(Tasks::<Test>::get(0).unwrap().prepaid, 0);

		set_time(300);
		run_task(0);
		System::assert_has_event(Event::Paused { task_id: 0 }.into());
		let task = Tasks::<Test>::get(0).unwrap();
		assert_eq!(task.status, TaskStatus::Paused);
		assert!(!Agenda::<Test>::contains_key(300 / 60, 0));
		assert!(!Cron::is_due(0));

		assert_ok!(Cron::top_up(RuntimeOrigin::signed(1), 0, 3 * FLAT_FEE));
		let task = Tasks::<Test>::get(0).unwrap();
		assert_eq!(task.status, TaskStatus::Active);
		assert!(Agenda::<Test>::contains_key(300 / 60, 0));

		run_task(0);
		let task = Tasks::<Test>::get(0).unwrap();
		assert_eq!(task.prepaid, 2 * FLAT_FEE);
		assert_eq!(task.next_run, 400);
	});
}

#[test]
fn cancel_refunds_and_cleans_up() {
	new_test_ext().execute_with(|| {
		schedule_remark(Schedule::OneTime { at: 200 }, 50);

		assert_noop!(Cron::cancel(RuntimeOrigin::signed(2), 0), Error::<Test>::NotScheduler);
		assert_ok!(Cron::cancel(RuntimeOrigin::signed(1), 0));

		assert!(Tasks::<Test>::get(0).is_none());
		assert!(!Agenda::<Test>::contains_key(200 / 60, 0));
		assert_eq!(prepay_held(1), 0);
		assert_eq!(deposit_held(1), 0);
		assert_eq!(Balances::free_balance(1), 1000);
	});
}

#[test]
fn filtered_call_rejected_at_schedule() {
	new_test_ext().execute_with(|| {
		let call = Box::new(RuntimeCall::System(frame_system::Call::set_code { code: vec![] }));
		assert_noop!(
			Cron::schedule(RuntimeOrigin::signed(1), call, Schedule::OneTime { at: 200 }, None, 0),
			Error::<Test>::Filtered
		);
	});
}

#[test]
fn filter_rechecked_at_execution() {
	new_test_ext().execute_with(|| {
		schedule_remark(Schedule::OneTime { at: 200 }, 50);

		ScheduleAllowed::set(false);
		set_time(200);
		run_task(0);

		System::assert_has_event(
			Event::Dispatched {
				task_id: 0,
				result: Err(frame_system::Error::<Test>::CallFiltered.into()),
			}
			.into(),
		);
	});
}

#[test]
fn invalid_schedules_rejected() {
	new_test_ext().execute_with(|| {
		assert_noop!(
			Cron::schedule(
				RuntimeOrigin::signed(1),
				remark(),
				Schedule::OneTime { at: 50 },
				None,
				0
			),
			Error::<Test>::InThePast
		);
		assert_noop!(
			Cron::schedule(
				RuntimeOrigin::signed(1),
				remark(),
				Schedule::Recurring { start_at: 200, interval: 0, max_executions: None },
				None,
				0
			),
			Error::<Test>::ZeroInterval
		);
		assert_noop!(
			Cron::schedule(
				RuntimeOrigin::signed(1),
				remark(),
				Schedule::Recurring { start_at: 200, interval: 100, max_executions: Some(0) },
				None,
				0
			),
			Error::<Test>::ZeroExecutions
		);
	});
}

#[test]
fn scheduled_call_is_distinguishable() {
	new_test_ext().execute_with(|| {
		// Direct call: no scheduled context.
		assert_ok!(Noop::work(RuntimeOrigin::signed(1)));
		assert!(!crate::mock::noop::RanScheduled::<Test>::get());

		// Scheduled call: the target sees the executing task.
		assert_ok!(Cron::schedule(
			RuntimeOrigin::signed(1),
			noop(),
			Schedule::OneTime { at: 200 },
			None,
			FLAT_FEE
		));
		set_time(200);
		run_task(0);
		assert!(crate::mock::noop::RanScheduled::<Test>::get());
		// Context cleared once dispatch returns.
		assert!(Cron::executing_task().is_none());
	});
}

#[test]
fn block_budget_serialises_due_calls() {
	new_test_ext().execute_with(|| {
		// Two calls due at once; the budget fits only one per block.
		assert_ok!(Cron::schedule(
			RuntimeOrigin::signed(1),
			noop(),
			Schedule::OneTime { at: 200 },
			None,
			FLAT_FEE
		));
		assert_ok!(Cron::schedule(
			RuntimeOrigin::signed(1),
			noop(),
			Schedule::OneTime { at: 200 },
			None,
			FLAT_FEE
		));
		set_time(200);
		assert!(Cron::should_service(0));
		assert!(Cron::should_service(1));

		run_task(0);
		// Budget spent; the second call must wait for a later block.
		assert_eq!(ServiceWeightUsed::<Test>::get(), SERVICE_WEIGHT);
		assert!(!Cron::should_service(1));
		assert!(!task_valid(1));

		end_block();
		assert_eq!(ServiceWeightUsed::<Test>::get(), Weight::zero());
		assert!(Cron::should_service(1));
		run_task(1);
		assert!(Tasks::<Test>::get(1).is_none());
	});
}

#[test]
fn one_time_past_grace_is_skipped() {
	new_test_ext().execute_with(|| {
		assert_ok!(Cron::schedule(
			RuntimeOrigin::signed(1),
			remark(),
			Schedule::OneTime { at: 200 },
			Some(10),
			50
		));

		// Serviced past its grace window: skipped, not dispatched.
		set_time(211);
		run_task(0);
		System::assert_has_event(Event::Skipped { task_id: 0 }.into());
		assert!(Tasks::<Test>::get(0).is_none());
		// No charge; prepay fully refunded.
		assert_eq!(Balances::free_balance(1), 1000);
	});
}

#[test]
fn recurring_past_grace_skips_occurrence() {
	new_test_ext().execute_with(|| {
		assert_ok!(Cron::schedule(
			RuntimeOrigin::signed(1),
			remark(),
			Schedule::Recurring { start_at: 200, interval: 100, max_executions: Some(2) },
			Some(10),
			2 * FLAT_FEE
		));

		// First occurrence missed its window; skip to the next slot without charging.
		set_time(215);
		run_task(0);
		System::assert_has_event(Event::Skipped { task_id: 0 }.into());
		let task = Tasks::<Test>::get(0).unwrap();
		assert_eq!(task.next_run, 300);
		assert_eq!(task.prepaid, 2 * FLAT_FEE);
		assert_eq!(task.executions_remaining, Some(2));
	});
}
