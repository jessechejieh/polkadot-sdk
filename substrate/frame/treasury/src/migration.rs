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
use alloc::collections::BTreeSet;
use alloc::vec::Vec;
use core::marker::PhantomData;
use frame_support::{defensive, traits::OnRuntimeUpgrade};

/// The log target for this pallet.
const LOG_TARGET: &str = "runtime::treasury";

pub mod cleanup_proposals {
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
pub mod migrate_to_ordered_payouts {
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

			// Collect all pending/failed spends that haven't expired.
			let mut spends_vec: Vec<(T::AssetKind, SpendIndex, BlockNumberFor<T, I>)> = Vec::new();

			for (index, spend) in Spends::<T, I>::iter() {
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

				// Set the payout queue
				PayoutQueue::<T, I>::insert(&asset_kind, queue);
			}

			log::info!(
				target: LOG_TARGET,
				"Migration complete: processed {} spends across {} asset kinds",
				total_spends_processed,
				total_assets_processed,
			);

			let reads = total_spends_processed as u64 + 1; // Spends reads + block number read
			let writes = total_assets_processed as u64 * 2; // PayoutQueue + NextPayout per asset
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

				// Verify NextPayout is NOT in queue (they are separate storage items in Solution
				// 1.2)
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
}

pub use cleanup_proposals::Migration as CleanupProposalsMigration;
pub use migrate_to_ordered_payouts::MigrateToOrderedPayouts;
