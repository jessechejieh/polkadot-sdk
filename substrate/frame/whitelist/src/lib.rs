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

//! # Whitelist Pallet
//!
//! - [`Config`]
//! - [`Call`]
//!
//! ## Overview
//!
//! Allow some configurable origin: [`Config::WhitelistOrigin`] to whitelist some hash of a call,
//! and allow another configurable origin: [`Config::DispatchWhitelistedOrigin`] to dispatch them
//! with the root origin.
//!
//! In the meantime the call corresponding to the hash must have been submitted to the pre-image
//! handler [`pallet::Config::Preimages`].

#![cfg_attr(not(feature = "std"), no_std)]

#[cfg(feature = "runtime-benchmarks")]
mod benchmarking;
#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;
pub mod weights;
pub use weights::WeightInfo;

extern crate alloc;

use alloc::{boxed::Box, vec::Vec};
use codec::{DecodeLimit, Encode, FullCodec};
use frame::{
	prelude::*,
	traits::{QueryPreimage, StorePreimage},
};
use scale_info::TypeInfo;
use sp_runtime::traits::BlockNumberProvider;

pub use pallet::*;

pub type BlockNumberFor<T> =
	<<T as Config>::BlockNumberProvider as BlockNumberProvider>::BlockNumber;

/// Source of the call data for dispatch.
#[derive(
	Encode, Decode, DecodeWithMemTracking, Clone, PartialEq, Eq, Debug, TypeInfo, MaxEncodedLen,
)]
enum CallSource<T: Config> {
	/// Fetch and decode from preimage storage (used by `dispatch_whitelisted_call`).
	Preimage { encoded_len: u32, weight_witness: Weight },
	/// Call provided directly (used by `dispatch_whitelisted_call_with_preimage`).
	Direct { call: <T as Config>::RuntimeCall, encoded_len: u32 },
}

#[frame::pallet]
pub mod pallet {
	use super::*;

	#[pallet::config]
	pub trait Config: frame_system::Config {
		/// The overarching event type.
		#[allow(deprecated)]
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// The overarching call type.
		type RuntimeCall: IsType<<Self as frame_system::Config>::RuntimeCall>
			+ Dispatchable<RuntimeOrigin = Self::RuntimeOrigin, PostInfo = PostDispatchInfo>
			+ GetDispatchInfo
			+ FullCodec
			+ TypeInfo
			+ From<frame_system::Call<Self>>
			+ Parameter;

		/// Required origin for whitelisting a call.
		type WhitelistOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// Required origin for dispatching whitelisted call with root origin.
		type DispatchWhitelistedOrigin: EnsureOrigin<Self::RuntimeOrigin>;

		/// The handler of pre-images.
		type Preimages: QueryPreimage<H = Self::Hashing> + StorePreimage;

        /// The number of blocks after which a deferred dispatch expires.
		type DeferredDispatchExpiration: Get<BlockNumberFor<Self>>;

		/// Provider for the block number. Normally this is the `frame_system` pallet.
        type BlockNumberProvider: BlockNumberProvider;

		/// The weight information for this pallet.
		type WeightInfo: WeightInfo;
	}

	#[pallet::pallet]
	pub struct Pallet<T>(_);

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		CallWhitelisted { call_hash: T::Hash },
		WhitelistedCallRemoved { call_hash: T::Hash },
		WhitelistedCallDispatched { call_hash: T::Hash, result: DispatchResultWithPostInfo },
		DispatchDeferred { call_hash: T::Hash },
		DeferredDispatchRemoved { call_hash: T::Hash },
		DeferredDispatchExecuted { call_hash: T::Hash, who: T::AccountId },
	}

	#[pallet::error]
	pub enum Error<T> {
		/// The preimage of the call hash could not be loaded.
		UnavailablePreImage,
		/// The call could not be decoded.
		UndecodableCall,
		/// The weight of the decoded call was higher than the witness.
		InvalidCallWeightWitness,
		/// The call was not whitelisted.
		CallIsNotWhitelisted,
		/// The call was already whitelisted; No-Op.
		CallAlreadyWhitelisted,
		/// No deferred dispatch entry exists for this call hash.
		DeferredDispatchNotFound,
		/// The deferred dispatch entry has not yet expired.
		DeferredDispatchNotExpired,
		/// The dispatch has been defered
		AlreadyDeferred,
		/// The deferred dispatch has expired.
		DeferredDispatchExpired,
	}

	#[pallet::storage]
	pub type WhitelistedCall<T: Config> = StorageMap<_, Twox64Concat, T::Hash, (), OptionQuery>;

	#[pallet::storage]
	pub type DeferredDispatch<T: Config> =
		StorageMap<_, Twox64Concat, T::Hash, DeferredEntry<T>, OptionQuery>;

	#[derive(Encode, Decode, TypeInfo, MaxEncodedLen)]
	#[scale_info(skip_type_params(T))]
	pub struct DeferredEntry<T: Config> {
		/// Block number when this deferred dispatch expires
		pub expire_at: BlockNumberFor<T>,
		/// Encoded length of the call (for weight calculation)
		pub call_encoded_len: u32,
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		#[pallet::call_index(0)]
		#[pallet::weight(T::WeightInfo::whitelist_call())]
		pub fn whitelist_call(origin: OriginFor<T>, call_hash: T::Hash) -> DispatchResult {
			T::WhitelistOrigin::ensure_origin(origin)?;

			ensure!(
				!WhitelistedCall::<T>::contains_key(call_hash),
				Error::<T>::CallAlreadyWhitelisted,
			);

			WhitelistedCall::<T>::insert(call_hash, ());
			T::Preimages::request(&call_hash);

			Self::deposit_event(Event::<T>::CallWhitelisted { call_hash });
			Ok(())
		}

		#[pallet::call_index(1)]
		#[pallet::weight(T::WeightInfo::remove_whitelisted_call())]
		pub fn remove_whitelisted_call(origin: OriginFor<T>, call_hash: T::Hash) -> DispatchResult {
			T::WhitelistOrigin::ensure_origin(origin)?;

			WhitelistedCall::<T>::take(call_hash).ok_or(Error::<T>::CallIsNotWhitelisted)?;

			T::Preimages::unrequest(&call_hash);

			Self::deposit_event(Event::<T>::WhitelistedCallRemoved { call_hash });

			Ok(())
		}

		#[pallet::call_index(2)]
		#[pallet::weight(
            T::WeightInfo::dispatch_whitelisted_call(*call_encoded_len)
                .saturating_add(*call_weight_witness)
        )]
		pub fn dispatch_whitelisted_call(
			origin: OriginFor<T>,
			call_hash: T::Hash,
			call_encoded_len: u32,
			call_weight_witness: Weight,
		) -> DispatchResultWithPostInfo {
			let relayer = match T::DispatchWhitelistedOrigin::try_origin(origin) {
				Ok(_) => {
					if !WhitelistedCall::<T>::contains_key(call_hash) {
						return Self::defer_dispatch(call_hash, None, call_encoded_len);
					}
					None
				},
				Err(dispatch_origin) => {
					let who = ensure_signed(dispatch_origin)?;

					let deferred_dispatch = DeferredDispatch::<T>::get(call_hash)
						.ok_or(Error::<T>::DeferredDispatchNotFound)?;

					ensure!(
						T::BlockNumberProvider::current_block_number() <
							deferred_dispatch.expire_at,
						Error::<T>::DeferredDispatchExpired
					);

					ensure!(
						WhitelistedCall::<T>::contains_key(call_hash),
						Error::<T>::CallIsNotWhitelisted
					);

					Some(who)
				},
			};

			Self::clean_and_dispatch(
				call_hash,
				CallSource::Preimage {
					encoded_len: call_encoded_len,
					weight_witness: call_weight_witness,
				},
				relayer,
			)
		}

		#[pallet::call_index(3)]
		#[pallet::weight({
            let call_weight = call.get_dispatch_info().call_weight;
            let call_len = call.encoded_size() as u32;
            T::WeightInfo::dispatch_whitelisted_call_with_preimage(call_len)
                .saturating_add(call_weight)
        })]
		pub fn dispatch_whitelisted_call_with_preimage(
			origin: OriginFor<T>,
			call: Box<<T as Config>::RuntimeCall>,
		) -> DispatchResultWithPostInfo {
			let call_hash = T::Hashing::hash_of(&call).into();
			let call_len = call.encoded_size() as u32;

			let relayer = match T::DispatchWhitelistedOrigin::try_origin(origin) {
				Ok(_) => {
					if !WhitelistedCall::<T>::contains_key(call_hash) {
						return Self::defer_dispatch(call_hash, Some(call.encode()), call_len);
					}
					None
				},
				Err(dispatch_origin) => {
					let who = ensure_signed(dispatch_origin)?;

					let deferred_dispatch = DeferredDispatch::<T>::get(call_hash)
						.ok_or(Error::<T>::DeferredDispatchNotFound)?;

					ensure!(
						T::BlockNumberProvider::current_block_number() <
							deferred_dispatch.expire_at,
						Error::<T>::DeferredDispatchExpired
					);

					let _ = T::Preimages::fetch(&call_hash, Some(call_len))
						.map_err(|_| Error::<T>::UnavailablePreImage)?;

					ensure!(
						WhitelistedCall::<T>::contains_key(call_hash),
						Error::<T>::CallIsNotWhitelisted
					);
					Some(who)
				},
			};

			Self::clean_and_dispatch(
				call_hash,
				CallSource::Direct { call: *call, encoded_len: call_len },
				relayer,
			)
		}

		#[pallet::call_index(4)]
		#[pallet::weight(100)]
		pub fn remove_deferred_dispatch(
			origin: OriginFor<T>,
			call_hash: T::Hash,
		) -> DispatchResultWithPostInfo {
			ensure_signed(origin)?;

			let deferred_entry = DeferredDispatch::<T>::get(call_hash)
				.ok_or(Error::<T>::DeferredDispatchNotFound)?;

			let now = T::BlockNumberProvider::current_block_number();

			ensure!(now >= deferred_entry.expire_at, Error::<T>::DeferredDispatchNotExpired);

			DeferredDispatch::<T>::remove(call_hash);

			Self::deposit_event(Event::<T>::DeferredDispatchRemoved { call_hash });

			Ok(Pays::No.into())
		}
	}
}

impl<T: Config> Pallet<T> {
	/// Defer the dispatch of a whitelisted call to a future block.
    ///
    /// This function stores the call hash for later execution by any signed origin
    /// before the expiration block. If a preimage is provided, it is uploaded to
    /// the preimages pallet for retrieval during the actual dispatch.
    fn defer_dispatch(
		call_hash: T::Hash,
		preimage: Option<Vec<u8>>,
		call_encoded_len: u32,
	) -> DispatchResultWithPostInfo {
		let now = T::BlockNumberProvider::current_block_number();

		let expire_at = now.saturating_add(T::DeferredDispatchExpiration::get());

		ensure!(!DeferredDispatch::<T>::contains_key(call_hash), Error::<T>::AlreadyDeferred);

		if let Some(ref preimage_data) = preimage {
			let _ = T::Preimages::note(preimage_data.into());
		}

		DeferredDispatch::<T>::insert(call_hash, DeferredEntry { expire_at, call_encoded_len });

		Self::deposit_event(Event::<T>::DispatchDeferred { call_hash });

		Ok(Some(match preimage {
			Some(_) => T::WeightInfo::dispatch_whitelisted_call_with_preimage(call_encoded_len),
			None => T::WeightInfo::dispatch_whitelisted_call(call_encoded_len),
		})
		.into())
	}

	/// Clean whitelisting/preimage, dispatch call, and handle weight calculation.
	///
	/// Returns the `DispatchResultWithPostInfo` with the actual weight including overhead.
	fn clean_and_dispatch(
		call_hash: T::Hash,
		source: CallSource<T>,
		relayer: Option<T::AccountId>,
	) -> DispatchResultWithPostInfo {
		let (call, weight_overhead) = match source {
			CallSource::Preimage { encoded_len, weight_witness } => {
				let call_data = T::Preimages::fetch(&call_hash, Some(encoded_len))
					.map_err(|_| Error::<T>::UnavailablePreImage)?;

				let call = <T as Config>::RuntimeCall::decode_all_with_depth_limit(
					frame::deps::frame_support::MAX_EXTRINSIC_DEPTH,
					&mut &call_data[..],
				)
				.map_err(|_| Error::<T>::UndecodableCall)?;

				ensure!(
					call.get_dispatch_info().call_weight.all_lte(weight_witness),
					Error::<T>::InvalidCallWeightWitness
				);

				(call, T::WeightInfo::dispatch_whitelisted_call(encoded_len))
			},
			CallSource::Direct { call, encoded_len } => {
				(call, T::WeightInfo::dispatch_whitelisted_call_with_preimage(encoded_len))
			},
		};

		WhitelistedCall::<T>::remove(call_hash);
		T::Preimages::unrequest(&call_hash);
		DeferredDispatch::<T>::remove(call_hash);

		let result = call.dispatch(frame_system::Origin::<T>::Root.into());

		let call_actual_weight = match result {
			Ok(call_post_info) => call_post_info.actual_weight,
			Err(call_err) => call_err.post_info.actual_weight,
		};

		Self::deposit_event(Event::<T>::WhitelistedCallDispatched { call_hash, result });

		if let Some(who) = relayer {
			if result.is_ok() {
				Self::deposit_event(Event::<T>::DeferredDispatchExecuted { call_hash, who });
			}
		}

		let actual_weight = call_actual_weight.map(|w| w.saturating_add(weight_overhead));
		Ok(actual_weight.into())
	}
}
