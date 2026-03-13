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

use alloc::boxed::Box;
use codec::{DecodeLimit, Encode, FullCodec};
use frame::{
	prelude::*,
	traits::{QueryPreimage, StorePreimage},
};
use scale_info::TypeInfo;

pub use pallet::*;

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

        type DeferredDispatchExpiration: Get<BlockNumberFor<Self>>;

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
            match T::DispatchWhitelistedOrigin::try_origin(origin) {
        
                Ok(_) => { 
                    if WhitelistedCall::<T>::contains_key(call_hash) {
                        Self::execute_whitelisted_call(call_hash, call_encoded_len, call_weight_witness, None)
                    } else {
                        Self::defer_dispatch(call_hash, call_encoded_len)
                    }
                },
        
                Err(original_origin) => {
                    let caller = ensure_signed(original_origin)?;
                    ensure!(
                        DeferredDispatch::<T>::contains_key(call_hash),
                        Error::<T>::DeferredDispatchNotFound
                    );
                    ensure!(
                        WhitelistedCall::<T>::contains_key(call_hash),
                        Error::<T>::CallIsNotWhitelisted
                    );
                    Self::execute_whitelisted_call(call_hash, call_encoded_len, call_weight_witness, Some(caller))
                }
            }
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

            match T::DispatchWhitelistedOrigin::try_origin(origin) {
                Ok(_) => {
                    if WhitelistedCall::<T>::contains_key(call_hash) {
                        let actual_weight = Self::clean_and_dispatch(call_hash, *call).map(|w| {
                            w.saturating_add(T::WeightInfo::dispatch_whitelisted_call_with_preimage(call_len))
                        });
                        Ok(actual_weight.into())
                    } else {
                        Self::defer_dispatch(call_hash, call_len)
                    }
                }
                Err(original_origin) => {
                    let caller = ensure_signed(original_origin)?;
                    ensure!(
                        DeferredDispatch::<T>::contains_key(call_hash),
                        Error::<T>::DeferredDispatchNotFound
                    );

                    ensure!(
                        WhitelistedCall::<T>::contains_key(call_hash),
                        Error::<T>::CallIsNotWhitelisted
                    );

                    let actual_weight = Self::clean_and_dispatch(call_hash, *call).map(|w| {
                        w.saturating_add(T::WeightInfo::dispatch_whitelisted_call_with_preimage(call_len))
                    });
                    
                    Ok(actual_weight.into())
                }
            }
		}
	}
}

impl<T: Config> Pallet<T> {
	/// Clean whitelisting/preimage and dispatch call.
	///
	/// Return the call actual weight of the dispatched call if there is some.
	fn clean_and_dispatch(call_hash: T::Hash, call: <T as Config>::RuntimeCall) -> Option<Weight> {
		WhitelistedCall::<T>::remove(call_hash);

		T::Preimages::unrequest(&call_hash);

        let _ = DeferredDispatch::<T>::take(call_hash);

		let result = call.dispatch(frame_system::Origin::<T>::Root.into());

		let call_actual_weight = match result {
			Ok(call_post_info) => call_post_info.actual_weight,
			Err(call_err) => call_err.post_info.actual_weight,
		};

		// TODO: Need to add conditional event
        Self::deposit_event(Event::<T>::WhitelistedCallDispatched { call_hash, result });

        call_actual_weight
	}

    fn defer_dispatch(
        call_hash: T::Hash,
        preimage: Option<Vec<u8>>,
        call_encoded_len: u32,
    ) -> DispatchResultWithPostInfo {
        let now = frame_system::Pallet::<T>::block_number();
        let expire_at = now.saturating_add(T::DeferredDispatchExpiration::get());

        DeferredDispatch::<T>::insert(call_hash, DeferredEntry {
            expire_at,
            call_encoded_len,
        });

        Self::deposit_event(Event::<T>::DispatchDeferred { call_hash });

        if preimage.is_some() {
            let weight = T::WeightInfo::dispatch_whitelisted_call_with_preimage(call_encoded_len);
            Ok(Some(weight).into())
        } else {
            let weight = T::WeightInfo::dispatch_whitelisted_call(call_encoded_len);
            Ok(Some(weight).into()
        }
    }

    fn execute_whitelisted_call(
        call_hash: T::Hash,
        call_encoded_len: u32,
        call_weight_witness: Weight,
        caller: Option<T::AccountId>,
    ) -> DispatchResultWithPostInfo {
    
        let call_data = T::Preimages::fetch(&call_hash, Some(call_encoded_len))
            .map_err(|_| Error::<T>::UnavailablePreImage)?;

        let call = <T as Config>::RuntimeCall::decode_all_with_depth_limit(
            frame::deps::frame_support::MAX_EXTRINSIC_DEPTH,
            &mut &call_data[..],
        ).map_err(|_| Error::<T>::UndecodableCall)?;
    
        ensure!(
            call.get_dispatch_info().call_weight.all_lte(call_weight_witness),
            Error::<T>::InvalidCallWeightWitness
        );
 
        let actual_weight = Self::clean_and_dispatch(call_hash, call).map(|w| {
            w.saturating_add(T::WeightInfo::dispatch_whitelisted_call(call_encoded_len))
        });
    
        match caller { 
            Some(account_id) => { 
                Self::deposit_event(Event::<T>::DeferredDispatchExecuted {
                    call_hash,
                    who: account_id,
                });
            },
            None => {
                Self::deposit_event(Event::<T>::WhitelistedCallDispatched {
                    call_hash,
                    result: Ok(actual_weight.into())
                });
            }
        }
        Ok(actual_weight.into())
    }
}
