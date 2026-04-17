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

//! Treasury pallet migrations.

use super::*;
use alloc::{collections::BTreeSet, vec::Vec};
use core::marker::PhantomData;
use frame_support::{defensive, traits::OnRuntimeUpgrade};

/// The log target for this pallet.
const LOG_TARGET: &str = "runtime::treasury";

mod cleanup_proposals {
	use super::*;

	/// Migration to cleanup unapproved proposals to return the bonds back to the proposers.
	/// Proposals can no longer be created and the `Proposal` storage item will be removed in the
	/// future.
	///
	/// `UnreserveWeight` returns `Weight` of `unreserve_balance` operation which is perfomed during
	/// this migration.
	pub struct Migration<T, I, UnreserveWeight>(PhantomData<(T, I, UnreserveWeight)>);

	impl<T: Config<I>, I: 'static, UnreserveWeight: Get<Weight>> OnRuntimeUpgrade
		for Migration<T, I, UnreserveWeight>
	{
		fn on_runtime_upgrade() -> frame_support::weights::Weight {
			let mut approval_index = BTreeSet::new();
			#[allow(deprecated)]
			for approval in Approvals::<T, I>::get().iter() {
				approval_index.insert(*approval);
			}

			let mut proposals_processed = 0;
			#[allow(deprecated)]
			for (proposal_index, p) in Proposals::<T, I>::iter() {
				if !approval_index.contains(&proposal_index) {
					let err_amount = T::Currency::unreserve(&p.proposer, p.bond);
					if err_amount.is_zero() {
						Proposals::<T, I>::remove(proposal_index);
						log::info!(
							target: LOG_TARGET,
							"Released bond amount of {:?} to proposer {:?}",
							p.bond,
							p.proposer,
						);
					} else {
						defensive!(
							"err_amount is non zero for proposal {:?}",
							(proposal_index, err_amount)
						);
						Proposals::<T, I>::mutate_extant(proposal_index, |proposal| {
							proposal.value = err_amount;
						});
						log::info!(
							target: LOG_TARGET,
							"Released partial bond amount of {:?} to proposer {:?}",
							p.bond - err_amount,
							p.proposer,
						);
					}
					proposals_processed += 1;
				}
			}

			log::info!(
				target: LOG_TARGET,
				"Migration for pallet-treasury finished, released {} proposal bonds.",
				proposals_processed,
			);

			// calculate and return migration weights
			let approvals_read = 1;
			T::DbWeight::get().reads_writes(
				proposals_processed as u64 + approvals_read,
				proposals_processed as u64,
			) + UnreserveWeight::get() * proposals_processed
		}

		#[cfg(feature = "try-runtime")]
		fn pre_upgrade() -> Result<Vec<u8>, sp_runtime::TryRuntimeError> {
			let value = (
				Proposals::<T, I>::iter_values().count() as u32,
				Approvals::<T, I>::get().len() as u32,
			);
			log::info!(
				target: LOG_TARGET,
				"Proposals and Approvals count {:?}",
				value,
			);
			Ok(value.encode())
		}

		#[cfg(feature = "try-runtime")]
		fn post_upgrade(state: Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
			let (old_proposals_count, old_approvals_count) =
				<(u32, u32)>::decode(&mut &state[..]).expect("Known good");
			let new_proposals_count = Proposals::<T, I>::iter_values().count() as u32;
			let new_approvals_count = Approvals::<T, I>::get().len() as u32;

			log::info!(
				target: LOG_TARGET,
				"Proposals and Approvals count {:?}",
				(new_proposals_count, new_approvals_count),
			);

			ensure!(
				new_proposals_count <= old_proposals_count,
				"Proposals after migration should be less or equal to old proposals"
			);
			ensure!(
				new_approvals_count == old_approvals_count,
				"Approvals after migration should remain the same"
			);
			Ok(())
		}
	}
}

/// Migration to initialize the payout queue for existing spends (Solution 1.2).
///
/// This migration identifies all pending/failed spends that have not yet expired,
/// groups them by asset kind, sorts them by valid_from (FIFO order),
/// and initializes the PayoutQueue and NextPayout for each asset kind.
mod migrate_to_ordered_payouts {
	use super::*;

	/// Migration to initialize the payout queue for existing spends.
	pub struct MigrateToOrderedPayouts<T, I = ()>(PhantomData<(T, I)>);

	impl<T: Config<I>, I: 'static> OnRuntimeUpgrade for MigrateToOrderedPayouts<T, I> {
		fn on_runtime_upgrade() -> Weight {
			log::info!(
				target: LOG_TARGET,
				"Running migration to initialize ordered payouts",
			);

			let now = T::BlockNumberProvider::current_block_number();
			let mut total_spends_read: u64 = 0;

			// Collect all pending/failed spends that haven't expired.
			let mut spends_vec: Vec<(T::AssetKind, SpendIndex, BlockNumberFor<T, I>)> = Vec::new();

			for (index, spend) in Spends::<T, I>::iter() {
				total_spends_read += 1;
				match spend.status {
					PaymentState::Pending | PaymentState::Failed => {
						// Only include spends that haven't expired
						if spend.expire_at > now {
							spends_vec.push((spend.asset_kind, index, spend.valid_from));
						} else {
							log::debug!(
								target: LOG_TARGET,
								"Skipping expired spend {} (expire_at: {:?}, now: {:?})",
								index,
								spend.expire_at,
								now,
							);
						}
					},
					PaymentState::Attempted { .. } => {
						log::debug!(
							target: LOG_TARGET,
							"Skipping attempted spend {}",
							index,
						);
					},
				}
			}

			// Group by encoded AssetKind
			let mut spends_by_asset: BTreeMap<
				Vec<u8>,
				(T::AssetKind, Vec<(SpendIndex, BlockNumberFor<T, I>)>),
			> = BTreeMap::new();

			for (asset_kind, index, valid_from) in spends_vec {
				let key = asset_kind.encode();
				spends_by_asset
					.entry(key)
					.or_insert_with(|| (asset_kind, Vec::new()))
					.1
					.push((index, valid_from));
			}

			let mut total_spends_processed = 0u32;
			let mut total_assets_processed = 0u32;

			// Process each AssetKind
			for (_, (asset_kind, mut spends)) in spends_by_asset {
				// Sort by valid_from, then by index for deterministic ordering (consensus safety)
				spends.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));

				let spend_count = spends.len() as u32;
				total_spends_processed += spend_count;
				total_assets_processed += 1;

				log::info!(
					target: LOG_TARGET,
					"Processing asset kind with {} spends",
					spend_count,
				);

				// Build the payout queue (bounded by MaxQueuedSpends)
				let mut queue =
					BoundedVec::<(SpendIndex, BlockNumberFor<T, I>), T::MaxQueuedSpends>::default();
				let mut next_payout_set = false;

				for (index, valid_from) in spends {
					if !next_payout_set {
						// First spend becomes NextPayout
						let expire_at = now.saturating_add(T::OrderExpirationPeriod::get());
						NextPayout::<T, I>::insert(&asset_kind, (index, expire_at));
						next_payout_set = true;
						log::debug!(
							target: LOG_TARGET,
							"Set NextPayout for asset to {} with expiration at {:?}",
							index,
							expire_at,
						);
					} else if queue.len() < T::MaxQueuedSpends::get() as usize {
						// Add to queue
						if let Err(_) = queue.try_push((index, valid_from)) {
							log::warn!(
								target: LOG_TARGET,
								"Failed to push spend {} to queue (queue full)",
								index
							);
						}
					} else {
						log::warn!(
							target: LOG_TARGET,
							"Payout queue is full, skipping spend {}",
							index
						);
					}
				}

				PayoutQueue::<T, I>::insert(&asset_kind, queue);
			}

			log::info!(
				target: LOG_TARGET,
				"Migration complete: processed {} spends across {} asset kinds",
				total_spends_processed,
				total_assets_processed,
			);

			let reads = total_spends_read + 1;
			let writes = total_assets_processed as u64 * 2;
			T::DbWeight::get().reads_writes(reads, writes)
		}

		#[cfg(feature = "try-runtime")]
		fn pre_upgrade() -> Result<Vec<u8>, sp_runtime::TryRuntimeError> {
			let pending_spends: Vec<(T::AssetKind, SpendIndex)> = Spends::<T, I>::iter()
				.filter_map(|(index, spend)| match spend.status {
					PaymentState::Pending | PaymentState::Failed => Some((spend.asset_kind, index)),
					_ => None,
				})
				.collect();

			log::info!(
				target: LOG_TARGET,
				"Pre-upgrade: {} pending/failed spends",
				pending_spends.len()
			);

			Ok(pending_spends.encode())
		}

		#[cfg(feature = "try-runtime")]
		fn post_upgrade(state: Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
			let pre_pending_spends: Vec<(T::AssetKind, SpendIndex)> =
				Vec::decode(&mut &state[..]).expect("Known good");

			let mut post_queue_count = 0usize;
			for (_, queue) in PayoutQueue::<T, I>::iter() {
				post_queue_count += queue.len();
			}

			log::info!(
				target: LOG_TARGET,
				"Post-upgrade: {} spends in queues (pre-upgrade had {} pending)",
				post_queue_count,
				pre_pending_spends.len()
			);

			// Verify queue invariants for each asset kind
			for (asset_kind, queue) in PayoutQueue::<T, I>::iter() {
				ensure!(
					queue.len() as u32 <= T::MaxQueuedSpends::get(),
					"Queue length exceeds MaxQueuedSpends"
				);

				// Verify all items in queue are valid pending spends
				for (index, _) in queue.iter() {
					let spend = Spends::<T, I>::get(index)
						.ok_or(sp_runtime::TryRuntimeError::Other("Spend in queue not found"))?;
					ensure!(
						matches!(spend.status, PaymentState::Pending | PaymentState::Failed),
						"Spend in queue has invalid status"
					);
					ensure!(spend.asset_kind == asset_kind, "Spend in queue has wrong asset kind");
				}

				// Verify NextPayout is NOT in queue
				if let Some((next_index, _)) = NextPayout::<T, I>::get(&asset_kind) {
					ensure!(
						!queue.iter().any(|(idx, _)| *idx == next_index),
						"NextPayout should not be in the queue"
					);
				}
			}

			Ok(())
		}
	}

	#[cfg(test)]
	mod tests {
		use super::*;
		use crate::{
			pallet::Spends,
			tests::{ExtBuilder, System, Test},
		};
		use frame_support::traits::OnRuntimeUpgrade;

		#[cfg(feature = "try-runtime")]
		use frame_support::assert_ok;

		/// Helper to directly insert a spend into storage
		fn insert_spend(
			index: SpendIndex,
			asset_kind: u32,
			amount: u64,
			beneficiary: u128,
			valid_from: u64,
			expire_at: u64,
			status: PaymentState<u64>,
		) {
			let spend = crate::SpendStatus {
				asset_kind,
				amount,
				beneficiary,
				valid_from,
				expire_at,
				status,
			};

			crate::pallet::Spends::<Test>::insert(index, spend);

			let current = crate::pallet::SpendCount::<Test>::get();

			if index >= current {
				crate::pallet::SpendCount::<Test>::put(index + 1);
			}
		}

		#[test]
		fn migration_empty_state() {
			ExtBuilder::default().build().execute_with(|| {
				System::set_block_number(100);
				assert_eq!(Spends::<Test>::iter().count(), 0);

				let weight = MigrateToOrderedPayouts::<Test>::on_runtime_upgrade();

				assert!(weight.ref_time() == 0);
				assert!(NextPayout::<Test>::iter().next().is_none());
				assert!(PayoutQueue::<Test>::iter().next().is_none());
			});
		}

		#[test]
		fn migration_skips_attempted_spends() {
			ExtBuilder::default().build().execute_with(|| {
				System::set_block_number(100);

				insert_spend(0, 1, 100, 1000, 50, 200, PaymentState::Attempted { id: 123u64 });

				MigrateToOrderedPayouts::<Test>::on_runtime_upgrade();

				assert!(NextPayout::<Test>::get(1u32).is_none());
				assert_eq!(PayoutQueue::<Test>::get(1u32).len(), 0);
			});
		}

		#[test]
		fn migration_skips_expired_spends() {
			ExtBuilder::default().build().execute_with(|| {
				System::set_block_number(100);

				// expire_at (99) < now (100)
				insert_spend(0, 1, 100, 1000, 50, 99, PaymentState::Pending);

				MigrateToOrderedPayouts::<Test>::on_runtime_upgrade();

				assert!(NextPayout::<Test>::get(1u32).is_none());
			});
		}

		#[test]
		fn migration_groups_by_asset() {
			ExtBuilder::default().build().execute_with(|| {
				System::set_block_number(100);

				// Asset 1: 2 spends
				insert_spend(0, 1, 100, 1000, 50, 200, PaymentState::Pending);
				insert_spend(1, 1, 200, 1001, 60, 200, PaymentState::Pending);
				// Asset 2: 1 spend
				insert_spend(2, 2, 300, 1002, 40, 200, PaymentState::Failed);

				MigrateToOrderedPayouts::<Test>::on_runtime_upgrade();

				// Asset 1: First spend is NextPayout
				let (next_idx, expire_at) = NextPayout::<Test>::get(1u32).unwrap();
				assert_eq!(next_idx, 0);
				assert_eq!(expire_at, 102); // 100 + OrderExpirationPeriod(2)

				// Asset 1 queue: spend 1
				assert_eq!(PayoutQueue::<Test>::get(1u32), vec![(1, 60)]);

				// Asset 2: Spend 2 is NextPayout
				assert_eq!(NextPayout::<Test>::get(2u32).map(|(idx, _)| idx), Some(2));
				assert_eq!(PayoutQueue::<Test>::get(2u32).len(), 0);
			});
		}

		#[test]
		fn migration_sorts_by_valid_from() {
			ExtBuilder::default().build().execute_with(|| {
				System::set_block_number(100);

				// Insert out of order
				insert_spend(0, 1, 100, 1000, 100, 200, PaymentState::Pending); // latest
				insert_spend(1, 1, 100, 1001, 50, 200, PaymentState::Pending); // earliest
				insert_spend(2, 1, 100, 1002, 75, 200, PaymentState::Pending); // middle

				MigrateToOrderedPayouts::<Test>::on_runtime_upgrade();

				// Sorted: 1 (50), 2 (75), 0 (100)
				assert_eq!(NextPayout::<Test>::get(1u32).map(|(idx, _)| idx), Some(1));
				assert_eq!(PayoutQueue::<Test>::get(1u32), vec![(2, 75), (0, 100)]);
			});
		}

		#[test]
		fn migration_tie_breaks_by_index() {
			ExtBuilder::default().build().execute_with(|| {
				System::set_block_number(100);

				// Same valid_from, different indices (inserted out of order)
				insert_spend(5, 1, 100, 1000, 50, 200, PaymentState::Pending);
				insert_spend(3, 1, 100, 1001, 50, 200, PaymentState::Pending);
				insert_spend(4, 1, 100, 1002, 50, 200, PaymentState::Pending);

				MigrateToOrderedPayouts::<Test>::on_runtime_upgrade();

				// Sorted by index: 3, 4, 5
				assert_eq!(NextPayout::<Test>::get(1u32).map(|(idx, _)| idx), Some(3));
				assert_eq!(PayoutQueue::<Test>::get(1u32), vec![(4, 50), (5, 50)]);
			});
		}

		#[test]
		fn migration_respects_max_queue() {
			ExtBuilder::default().build().execute_with(|| {
				System::set_block_number(100);

				// Create 105 spends (MaxQueuedSpends = 100)
				for i in 0..105u32 {
					insert_spend(
						i,
						1,
						100,
						1000 + i as u128,
						50 + i as u64,
						200,
						PaymentState::Pending,
					);
				}

				MigrateToOrderedPayouts::<Test>::on_runtime_upgrade();

				// NextPayout is index 0
				assert_eq!(NextPayout::<Test>::get(1u32).map(|(idx, _)| idx), Some(0));

				// Queue capped at 100
				let queue = PayoutQueue::<Test>::get(1u32);
				assert_eq!(queue.len(), 100);
				assert_eq!(queue[0].0, 1);
				assert_eq!(queue[99].0, 100);
			});
		}

		#[test]
		fn migration_mixed_statuses() {
			ExtBuilder::default().build().execute_with(|| {
				System::set_block_number(100);

				insert_spend(0, 1, 100, 1000, 50, 200, PaymentState::Pending);
				insert_spend(1, 1, 100, 1001, 51, 200, PaymentState::Failed);
				insert_spend(2, 1, 100, 1002, 52, 200, PaymentState::Attempted { id: 1 });
				insert_spend(3, 1, 100, 1003, 53, 99, PaymentState::Pending); // expired

				MigrateToOrderedPayouts::<Test>::on_runtime_upgrade();

				// Only 0 and 1 in queue
				assert_eq!(NextPayout::<Test>::get(1u32).map(|(idx, _)| idx), Some(0));
				assert_eq!(PayoutQueue::<Test>::get(1u32), vec![(1, 51)]);
			});
		}

		#[test]
		#[cfg(feature = "try-runtime")]
		fn pre_upgrade_captures_state() {
			ExtBuilder::default().build().execute_with(|| {
				insert_spend(0, 1, 100, 1000, 50, 200, PaymentState::Pending);
				insert_spend(1, 1, 100, 1001, 51, 200, PaymentState::Failed);
				insert_spend(2, 1, 100, 1002, 52, 200, PaymentState::Attempted { id: 1 });

				let state = MigrateToOrderedPayouts::<Test>::pre_upgrade().unwrap();
				let decoded: Vec<(u32, u32)> = Vec::decode(&mut &state[..]).unwrap();

				assert_eq!(decoded.len(), 2);
				assert!(decoded.contains(&(1, 0)));
				assert!(decoded.contains(&(1, 1)));
			});
		}

		#[test]
		#[cfg(feature = "try-runtime")]
		fn post_upgrade_validates() {
			ExtBuilder::default().build().execute_with(|| {
				let pre_state = {
					let mut spends = Vec::new();
					spends.push((1u32, 0u32));
					spends.push((1u32, 1u32));
					spends.encode()
				};

				System::set_block_number(100);
				insert_spend(0, 1, 100, 1000, 50, 200, PaymentState::Pending);
				insert_spend(1, 1, 100, 1001, 51, 200, PaymentState::Pending);
				MigrateToOrderedPayouts::<Test>::on_runtime_upgrade();

				assert_ok!(MigrateToOrderedPayouts::<Test>::post_upgrade(pre_state));
			});
		}

		#[test]
		#[cfg(feature = "try-runtime")]
		fn post_upgrade_detects_invalid() {
			ExtBuilder::default().build().execute_with(|| {
				System::set_block_number(100);

				// Create invalid state: NextPayout in queue
				insert_spend(0, 1, 100, 1000, 50, 200, PaymentState::Pending);
				NextPayout::<Test>::insert(1u32, (0u32, 102u64));

				let bounded_vec: BoundedVec<(u32, u64), crate::tests::MaxQueuedSpends> =
					vec![(0u32, 50u64)].try_into().unwrap();
				crate::pallet::PayoutQueue::<Test>::insert(1u32, bounded_vec);

				let pre_state = vec![(1u32, 0u32)].encode();

				assert!(MigrateToOrderedPayouts::<Test>::post_upgrade(pre_state).is_err());
			});
		}
	}
}

pub use cleanup_proposals::Migration as CleanupProposalsMigration;
pub use migrate_to_ordered_payouts::MigrateToOrderedPayouts;
