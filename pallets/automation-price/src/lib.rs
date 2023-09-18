// This file is part of OAK Blockchain.

// Copyright (C) 2022 OAK Network
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! # Automation time pallet
//!
//! DISCLAIMER: This pallet is still in it's early stages. At this point
//! we only support scheduling two tasks per hour, and sending an on-chain
//! with a custom message.
//!
//! This pallet allows a user to schedule tasks. Tasks can scheduled for any whole hour in the future.
//! In order to run tasks this pallet consumes up to a certain amount of weight during `on_initialize`.
//!
//! The pallet supports the following tasks:
//! * On-chain events with custom text
//!

#![cfg_attr(not(feature = "std"), no_std)]
pub use pallet::*;

pub mod weights;

pub mod types;
pub use types::*;

mod fees;

#[cfg(test)]
mod mock;
#[cfg(test)]
mod tests;

pub use fees::*;

use codec::Decode;
use core::convert::{TryFrom, TryInto};
use cumulus_pallet_xcm::Origin as CumulusOrigin;
use cumulus_primitives_core::InteriorMultiLocation;

use cumulus_primitives_core::ParaId;
use frame_support::{
	dispatch::{GetDispatchInfo, PostDispatchInfo},
	pallet_prelude::*,
	sp_runtime::traits::{CheckedSub, Hash},
	storage::{
		with_transaction,
		TransactionOutcome::{Commit, Rollback},
	},
	traits::{Contains, Currency, ExistenceRequirement, IsSubType, OriginTrait},
	transactional,
	weights::constants::WEIGHT_REF_TIME_PER_SECOND,
	BoundedVec,
};
use frame_system::{pallet_prelude::*, Config as SystemConfig};
use orml_traits::{FixedConversionRateProvider, MultiCurrency};
use pallet_timestamp::{self as timestamp};
use scale_info::{prelude::format, TypeInfo};
use sp_runtime::{
	traits::{Convert, SaturatedConversion, Saturating},
	Perbill,
};
use sp_std::{boxed::Box, collections::btree_map::BTreeMap, vec, vec::Vec};

pub use pallet_xcmp_handler::InstructionSequence;
pub use weights::WeightInfo;

use pallet_xcmp_handler::XcmpTransactor;
use xcm::{latest::prelude::*, VersionedMultiLocation};

#[frame_support::pallet]
pub mod pallet {
	use super::*;

	pub type AccountOf<T> = <T as frame_system::Config>::AccountId;
	pub type BalanceOf<T> =
		<<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;
	pub type MultiBalanceOf<T> = <<T as Config>::MultiCurrency as MultiCurrency<
		<T as frame_system::Config>::AccountId,
	>>::Balance;
	pub type ActionOf<T> = Action<AccountOf<T>, BalanceOf<T>>;

	pub type MultiCurrencyId<T> = <<T as Config>::MultiCurrency as MultiCurrency<
		<T as frame_system::Config>::AccountId,
	>>::CurrencyId;

	type UnixTime = u64;
	pub type TaskId = Vec<u8>;

	// TODO: Cleanup before merge
	type ChainName = Vec<u8>;
	type Exchange = Vec<u8>;

	type AssetName = Vec<u8>;
	type AssetPair = (AssetName, AssetName);
	type AssetPrice = u128;

	/// The struct that stores all information needed for a task.
	#[derive(Debug, Eq, Encode, Decode, TypeInfo, Clone)]
	#[scale_info(skip_type_params(T))]
	pub struct Task<T: Config> {
		// origin data from the account schedule the tasks
		pub owner_id: AccountOf<T>,

		// generated data
		pub task_id: TaskId,

		// user input data
		pub chain: ChainName,
		pub exchange: Exchange,
		pub asset_pair: AssetPair,

		// TODO: Maybe expose enum?
		pub trigger_function: Vec<u8>,
		pub trigger_params: Vec<u128>,
		pub action: ActionOf<T>,
	}

	/// Needed for assert_eq to compare Tasks in tests due to BoundedVec.
	impl<T: Config> PartialEq for Task<T> {
		fn eq(&self, other: &Self) -> bool {
			// TODO: correct this
			self.owner_id == other.owner_id &&
				self.task_id == other.task_id &&
				self.asset_pair == other.asset_pair &&
				self.trigger_function == other.trigger_function &&
				self.trigger_params == other.trigger_params
		}
	}

	impl<T: Config> Task<T> {
		pub fn create_event_task(
			owner_id: AccountOf<T>,
			chain: ChainName,
			exchange: Exchange,
			task_id: Vec<u8>,
			asset_pair: AssetPair,
			recipient: AccountOf<T>,
			amount: BalanceOf<T>,
		) -> Task<T> {
			// TODO: remove dead code, use new method
			let action = Action::NativeTransfer { sender: owner_id.clone(), recipient, amount };
			Task::<T> {
				owner_id,
				task_id,
				chain,
				exchange,
				asset_pair,
				trigger_function: vec![1],
				trigger_params: vec![1],
				action,
			}
		}
	}

	#[pallet::config]
	pub trait Config: frame_system::Config + pallet_timestamp::Config {
		type RuntimeEvent: From<Event<Self>> + IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Weight information for the extrinsics in this module.
		type WeightInfo: WeightInfo;

		/// The maximum number of tasks that can be scheduled for a time slot.
		#[pallet::constant]
		type MaxTasksPerSlot: Get<u32>;

		/// The maximum weight per block.
		#[pallet::constant]
		type MaxBlockWeight: Get<u64>;

		/// The maximum percentage of weight per block used for scheduled tasks.
		#[pallet::constant]
		type MaxWeightPercentage: Get<Perbill>;

		#[pallet::constant]
		type ExecutionWeightFee: Get<BalanceOf<Self>>;

		/// The Currency type for interacting with balances
		type Currency: Currency<Self::AccountId>;

		/// The MultiCurrency type for interacting with balances
		type MultiCurrency: MultiCurrency<Self::AccountId>;

		/// The currencyIds that our chain supports.
		type CurrencyId: Parameter
			+ Member
			+ Copy
			+ MaybeSerializeDeserialize
			+ Ord
			+ TypeInfo
			+ MaxEncodedLen
			+ From<MultiCurrencyId<Self>>
			+ Into<MultiCurrencyId<Self>>
			+ From<u32>;

		/// Converts CurrencyId to Multiloc
		type CurrencyIdConvert: Convert<Self::CurrencyId, Option<MultiLocation>>
			+ Convert<MultiLocation, Option<Self::CurrencyId>>;

		/// Handler for fees
		type FeeHandler: HandleFees<Self>;

		//type Origin: From<<Self as SystemConfig>::RuntimeOrigin>
		//	+ Into<Result<CumulusOrigin, <Self as Config>::Origin>>;

		/// Converts between comparable currencies
		type FeeConversionRateProvider: FixedConversionRateProvider;

		/// This chain's Universal Location.
		type UniversalLocation: Get<InteriorMultiLocation>;

		//The paraId of this chain.
		type SelfParaId: Get<ParaId>;

		/// Utility for sending XCM messages
		type XcmpTransactor: XcmpTransactor<Self::AccountId, Self::CurrencyId>;
	}

	#[pallet::pallet]
	#[pallet::without_storage_info]
	#[pallet::generate_store(pub(super) trait Store)]
	pub struct Pallet<T>(_);

	// TODO: Cleanup before merge
	#[derive(Debug, Encode, Decode, TypeInfo)]
	#[scale_info(skip_type_params(T))]
	pub struct RegistryInfo<T: Config> {
		round: u128,
		decimal: u8,
		last_update: u64,
		oracle_providers: Vec<AccountOf<T>>,
	}

	// TODO: Use a ring buffer to also store last n history data effectively
	#[derive(Debug, Encode, Decode, TypeInfo)]
	#[scale_info(skip_type_params(T))]
	pub struct PriceData {
		pub round: u128,
		pub nonce: u128,
		pub amount: u128,
	}

	// AssetRegistry holds information and metadata about the asset we support
	#[pallet::storage]
	#[pallet::getter(fn get_asset_registry_info)]
	pub type AssetRegistry<T: Config> = StorageNMap<
		_,
		(
			NMapKey<Twox64Concat, ChainName>,
			NMapKey<Twox64Concat, Exchange>,
			NMapKey<Twox64Concat, AssetName>,
			NMapKey<Twox64Concat, AssetName>,
		),
		RegistryInfo<T>,
	>;

	// PriceRegistry holds price only information for the asset we support
	#[pallet::storage]
	#[pallet::getter(fn get_asset_price_data)]
	pub type PriceRegistry<T> = StorageNMap<
		_,
		(
			NMapKey<Twox64Concat, ChainName>,
			NMapKey<Twox64Concat, Exchange>,
			NMapKey<Twox64Concat, AssetName>,
			NMapKey<Twox64Concat, AssetName>,
		),
		PriceData,
	>;

	// TODO: move these to a trigger model
	// TODO: handle task expiration
	#[pallet::storage]
	#[pallet::getter(fn get_sorted_tasks_index)]
	pub type SortedTasksIndex<T> = StorageNMap<
		_,
		(
			NMapKey<Twox64Concat, ChainName>,
			NMapKey<Twox64Concat, Exchange>,
			NMapKey<Twox64Concat, AssetName>,
			NMapKey<Twox64Concat, AssetName>,
		),
		BTreeMap<TaskId, u128>,
	>;



	#[pallet::storage]
	#[pallet::getter(fn get_scheduled_tasks)]
	pub type ScheduledTasks<T: Config> = StorageNMap<
		_,
		(NMapKey<Twox64Concat, AssetName>, NMapKey<Twox64Concat, Vec<u8>>),
		BoundedVec<T::Hash, T::MaxTasksPerSlot>,
	>;

	#[pallet::storage]
	#[pallet::getter(fn get_scheduled_asset_period_reset)]
	pub type ScheduledAssetDeletion<T: Config> =
		StorageMap<_, Twox64Concat, UnixTime, Vec<AssetName>>;

	// Tasks hold all active task, look up through (Owner, TaskId)
	#[pallet::storage]
	#[pallet::getter(fn get_task)]
	pub type Tasks<T: Config> = StorageMap<_, Twox64Concat, TaskId, Task<T>>;

	#[pallet::storage]
	#[pallet::getter(fn get_account_task)]
	pub type AccountTasks<T: Config> = StorageMap<_, Twox64Concat, AccountOf<T>, Vec<TaskId>>;

	#[pallet::storage]
	#[pallet::getter(fn get_task_queue)]
	pub type TaskQueue<T: Config> = StorageValue<_, Vec<(AssetName, T::Hash)>, ValueQuery>;

	#[pallet::storage]
	#[pallet::getter(fn is_shutdown)]
	pub type Shutdown<T: Config> = StorageValue<_, bool, ValueQuery>;

	#[pallet::error]
	pub enum Error<T> {
		/// The provided_id cannot be empty
		EmptyProvidedId,
		/// Time must end in a whole hour.
		InvalidTime,
		/// Duplicate task
		DuplicateTask,
		/// Non existent asset
		AssetNotSupported,
		AssetNotInitialized,
		/// Asset already supported
		AssetAlreadySupported,
		AssetAlreadyInitialized,
		/// Asset cannot be updated by this account
		InvalidAssetSudo,
		OracleNotAuthorized,
		/// Asset must be in triggerable range.
		AssetNotInTriggerableRange,
		/// Block Time not set
		BlockTimeNotSet,
		/// Invalid Expiration Window for new asset
		InvalidAssetExpirationWindow,
		/// Maximum tasks reached for the slot
		MaxTasksReached,
		/// Failed to insert task
		TaskInsertionFailure,
		/// Insufficient Balance
		InsufficientBalance,
		/// Restrictions on Liquidity in Account
		LiquidityRestrictions,
		/// Too Many Assets Created
		AssetLimitReached,
		BadVersion,
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config> {
		/// Schedule task success.
		TaskScheduled {
			who: AccountOf<T>,
			task_id: TaskId,
		},
		Notify {
			message: Vec<u8>,
		},
		TaskNotFound {
			task_id: TaskId,
		},
		AssetCreated {
			chain: ChainName,
			exchange: Exchange,
			asset1: AssetName,
			asset2: AssetName,
			decimal: u8,
		},
		AssetUpdated {
			asset: AssetName,
		},
		AssetDeleted {
			asset: AssetName,
		},
		AssetPeriodReset {
			asset: AssetName,
		},
		/// Successfully transferred funds
		SuccessfullyTransferredFunds {
			task_id: TaskId,
		},
		/// Transfer Failed
		TransferFailed {
			task_id: TaskId,
			error: DispatchError,
		},
	}

	#[pallet::hooks]
	impl<T: Config> Hooks<BlockNumberFor<T>> for Pallet<T> {
		fn on_initialize(_: T::BlockNumber) -> Weight {
			if Self::is_shutdown() {
				return T::DbWeight::get().reads(1u64)
			}

			let max_weight: Weight = Weight::from_ref_time(
				T::MaxWeightPercentage::get().mul_floor(T::MaxBlockWeight::get()),
			);
			Self::trigger_tasks(max_weight)
		}
	}

	#[pallet::call]
	impl<T: Config> Pallet<T> {
		/// Initialize an asset
		///
		/// Add a new asset
		///
		/// # Parameters
		/// * `asset`: asset type
		/// * `target_price`: baseline price of the asset
		/// * `upper_bound`: TBD - highest executable percentage increase for asset
		/// * `lower_bound`: TBD - highest executable percentage decrease for asset
		/// * `asset_owner`: owner of the asset
		/// * `expiration_period`: how frequently the tasks for an asset should expire
		///
		/// # Errors
		#[pallet::call_index(1)]
		#[pallet::weight(<T as Config>::WeightInfo::initialize_asset_extrinsic())]
		#[transactional]
		pub fn initialize_asset(
			origin: OriginFor<T>,
			chain: Vec<u8>,
			exchange: Vec<u8>,
			asset1: AssetName,
			asset2: AssetName,
			decimal: u8,
			asset_owners: Vec<AccountOf<T>>,
		) -> DispatchResult {
			// TODO: needs fees if opened up to non-sudo
			ensure_root(origin)?;
			Self::create_new_asset(chain, exchange, asset1, asset2, decimal, asset_owners)?;

			Ok(().into())
		}

		/// Post asset update
		///
		/// Update the asset price
		///
		/// # Parameters
		/// * `asset`: asset type
		/// * `value`: value of asset
		///
		/// # Errors
		#[pallet::call_index(2)]
		#[pallet::weight(<T as Config>::WeightInfo::asset_price_update_extrinsic())]
		#[transactional]
		pub fn update_asset_prices(
			origin: OriginFor<T>,
			chains: Vec<ChainName>,
			exchanges: Vec<Exchange>,
			assets1: Vec<AssetName>,
			assets2: Vec<AssetName>,
			prices: Vec<AssetPrice>,
			submitted_at: Vec<u128>,
			rounds: Vec<u128>,
		) -> DispatchResult {
			let who = ensure_signed(origin)?;
			// TODO: ensure length are all same
			for (index, price) in prices.clone().iter().enumerate() {
				let index: usize = index.try_into().unwrap();

				let chain = chains[index].clone();
				let exchange = exchanges[index].clone();
				let asset1 = assets1[index].clone();
				let asset2 = assets2[index].clone();
				let round = rounds[index].clone();

				let key = (&chain, &exchange, &asset1, &asset2);

				if !AssetRegistry::<T>::contains_key(&key) {
					// TODO: emit error and continue update the rest instead, temporary do this to
					// keep going, will update later
					Err(Error::<T>::AssetNotInitialized)?
				}

				if let Some(asset_registry) = Self::get_asset_registry_info(key) {
					let allow_wallets: Vec<AccountOf<T>> = asset_registry.oracle_providers;
					if !allow_wallets.contains(&who) {
						Err(Error::<T>::OracleNotAuthorized)?
					}

					// TODO: Add round and nonce check logic
					PriceRegistry::<T>::insert(
						&key,
						PriceData {
							round,
							// TODO: remove hard code
							nonce: 1,
							amount: *price,
						},
					);
				}
			}
			Ok(().into())
		}

		/// Delete an asset
		///
		/// # Parameters
		/// * `asset`: asset type
		/// * `directions`: number of directions of data input. (up, down, ?)
		///
		/// # Errors
		#[pallet::call_index(3)]
		#[pallet::weight(<T as Config>::WeightInfo::delete_asset_extrinsic())]
		#[transactional]
		pub fn delete_asset(
            origin: OriginFor<T>,
            chain: ChainName,
			exchange: Exchange,
			asset1: AssetName,
			asset2: AssetName
        ) -> DispatchResult {
			// TODO: needs fees if opened up to non-sudo
			ensure_root(origin)?;

            let key = (chain, exchange, &asset1, &asset2);

            // TODO: handle delete
			if let Some(_asset_target_price) = Self::get_asset_registry_info(key) {
				//Self::delete_asset_tasks(asset.clone());
				Self::deposit_event(Event::AssetDeleted { asset: asset1 });
			} else {
				Err(Error::<T>::AssetNotSupported)?
			}
			Ok(())
		}

		// TODO: correct weight
		#[pallet::call_index(4)]
		#[pallet::weight(<T as Config>::WeightInfo::delete_asset_extrinsic())]
		#[transactional]
		pub fn schedule_xcmp_task(
			origin: OriginFor<T>,
			chain: ChainName,
			exchange: Exchange,
			asset1: AssetName,
			asset2: AssetName,
			expired_at: u128,
			trigger_function: Vec<u8>,
			trigger_param: Vec<u128>,
			destination: Box<VersionedMultiLocation>,
			schedule_fee: Box<VersionedMultiLocation>,
			execution_fee: Box<AssetPayment>,
			encoded_call: Vec<u8>,
			encoded_call_weight: Weight,
			overall_weight: Weight,
		) -> DispatchResult {
			// Step 1:
			//   Build Task and put it into the task registry
			// Step 2:
			//   Put task id on the index
			// TODO: the value to be inserted into the BTree should come from a function that
			// extract value from param
			//
			// TODO: HANDLE FEE to see user can pay fee
			let who = ensure_signed(origin)?;
			let task_id = Self::generate_task_id();

			let destination =
				MultiLocation::try_from(*destination).map_err(|()| Error::<T>::BadVersion)?;
			let schedule_fee =
				MultiLocation::try_from(*schedule_fee).map_err(|()| Error::<T>::BadVersion)?;

			let action = Action::XCMP {
				destination,
				schedule_fee,
				execution_fee: *execution_fee,
				encoded_call,
				encoded_call_weight,
				overall_weight,
				schedule_as: None,
				instruction_sequence: InstructionSequence::PayThroughSovereignAccount,
			};

			let task: Task<T> = Task::<T> {
				owner_id: who.clone(),
				task_id: task_id.clone(),
				chain,
				exchange,
				asset_pair: (asset1, asset2),
				trigger_function,
				trigger_params: trigger_param,
				action,
			};

			Self::validate_and_schedule_task(task)?;
			// TODO withdraw fee
			//T::FeeHandler::withdraw_fee(&who, fee).map_err(|_| Error::<T>::InsufficientBalance)?;
			Ok(())
		}
	}

	impl<T: Config> Pallet<T> {
		pub fn generate_task_id() -> TaskId {
			let current_block_number =
				match TryInto::<u64>::try_into(<frame_system::Pallet<T>>::block_number()).ok() {
					Some(i) => i,
					None => 0,
				};

			let tx_id = match <frame_system::Pallet<T>>::extrinsic_index() {
				Some(i) => i,
				None => 0,
			};

			let evt_index = <frame_system::Pallet<T>>::event_count();

			format!("{:}-{:}-{:}", current_block_number, tx_id, evt_index)
				.as_bytes()
				.to_vec()
		}

		/// Trigger tasks for the block time.
		///
		/// Complete as many tasks as possible given the maximum weight.
		pub fn trigger_tasks(max_weight: Weight) -> Weight {
			let mut weight_left: Weight = max_weight;
			let check_time_and_deletion_weight = T::DbWeight::get().reads(2u64);
			if weight_left.ref_time() < check_time_and_deletion_weight.ref_time() {
				return weight_left
			}

			// remove assets as necessary
			//let current_time_slot = match Self::get_current_time_slot() {
			//	Ok(time_slot) => time_slot,
			//	Err(_) => return weight_left,
			//};
			//if let Some(scheduled_deletion_assets) =
			//	Self::get_scheduled_asset_period_reset(current_time_slot)
			//{
			//	// delete assets' tasks
			//	let asset_reset_weight = <T as Config>::WeightInfo::reset_asset(
			//		scheduled_deletion_assets.len().saturated_into(),
			//	);
			//	if weight_left.ref_time() < asset_reset_weight.ref_time() {
			//		return weight_left
			//	}
			//	// TODO: this assumes that all assets that need to be reset in a period can all be done successfully in a block.
			//	// 			 in the future, we need to make sure to be able to break out of for loop if out of weight and continue
			//	//       in the next block. Right now, we will not run out of weight - we will simply not execute anything if
			//	//       not all of the asset resets can be run at once. this may cause the asset reset triggers to not go off,
			//	//       but at least it should not brick the chain.
			//	for asset in scheduled_deletion_assets {
			//		if let Some(last_asset_price) = Self::get_asset_price(asset.clone()) {
			//			AssetBaselinePrices::<T>::insert(asset.clone(), last_asset_price);
			//			Self::delete_asset_tasks(asset.clone());
			//			Self::update_asset_reset(asset.clone(), current_time_slot);
			//			Self::deposit_event(Event::AssetPeriodReset { asset });
			//		};
			//	}
			//	ScheduledAssetDeletion::<T>::remove(current_time_slot);
			//	weight_left = weight_left - asset_reset_weight;
			//}

			//// run as many scheduled tasks as we can
			//let task_queue = Self::get_task_queue();
			//weight_left = weight_left
			//	.saturating_sub(T::DbWeight::get().reads(1u64))
			//	// For measuring the TaskQueue::<T>::put(tasks_left);
			//	.saturating_sub(T::DbWeight::get().writes(1u64));
			//if task_queue.len() > 0 {
			//	let (tasks_left, new_weight_left) = Self::run_tasks(task_queue, weight_left);
			//	weight_left = new_weight_left;
			//	TaskQueue::<T>::put(tasks_left);
			//}
			weight_left
		}

		pub fn create_new_asset(
			chain: ChainName,
			exchange: Exchange,
			asset1: AssetName,
			asset2: AssetName,
			decimal: u8,
			asset_owners: Vec<AccountOf<T>>,
		) -> Result<(), DispatchError> {
			let key = (&chain, &exchange, &asset1, &asset2);

			if AssetRegistry::<T>::contains_key(&key) {
				Err(Error::<T>::AssetAlreadyInitialized)?
			}

			let asset_info = RegistryInfo::<T> {
				decimal,
				round: 0,
				last_update: 0,
				oracle_providers: asset_owners,
			};

			AssetRegistry::<T>::insert(key, asset_info);

			Self::deposit_event(Event::AssetCreated { chain, exchange, asset1, asset2, decimal });
			Ok(())
		}

		pub fn get_current_time_slot() -> Result<UnixTime, Error<T>> {
			let now = <timestamp::Pallet<T>>::get().saturated_into::<UnixTime>();
			if now == 0 {
				Err(Error::<T>::BlockTimeNotSet)?
			}
			let now = now.saturating_div(1000);
			let diff_to_min = now % 60;
			Ok(now.saturating_sub(diff_to_min))
		}

		pub fn delete_asset_tasks(asset: AssetName) {
			// delete scheduled tasks
			let _ = ScheduledTasks::<T>::clear_prefix((asset.clone(),), u32::MAX, None);
			// delete tasks from task queue
			let existing_task_queue: Vec<(AssetName, T::Hash)> = Self::get_task_queue();
			let mut updated_task_queue: Vec<(AssetName, T::Hash)> = vec![];
			for task in existing_task_queue {
				if task.0 != asset {
					updated_task_queue.push(task);
				}
			}
			TaskQueue::<T>::put(updated_task_queue);
		}

		pub fn run_native_transfer_task(
			sender: T::AccountId,
			recipient: T::AccountId,
			amount: BalanceOf<T>,
			task_id: TaskId,
		) -> Weight {
			match T::Currency::transfer(
				&sender,
				&recipient,
				amount,
				ExistenceRequirement::KeepAlive,
			) {
				Ok(_number) => Self::deposit_event(Event::SuccessfullyTransferredFunds { task_id }),
				Err(e) => Self::deposit_event(Event::TransferFailed { task_id, error: e }),
			};

			<T as Config>::WeightInfo::run_native_transfer_task()
		}

		/// Runs as many tasks as the weight allows from the provided vec of task_ids.
		///
		/// Returns a vec with the tasks that were not run and the remaining weight.
		pub fn run_tasks(
			mut task_ids: Vec<TaskId>,
			mut weight_left: Weight,
		) -> (Vec<TaskId>, Weight) {
			let mut consumed_task_index: usize = 0;
			for task_id in task_ids.iter() {
				consumed_task_index.saturating_inc();
				// TODO: Correct this place holder
				let action_weight = match Self::get_task(task_id) {
					None => {
						// TODO: add back signature when insert new task work
						//Self::deposit_event(Event::TaskNotFound { task_id: task_id.clone() });
						<T as Config>::WeightInfo::emit_event()
					},
					Some(task) => {
						let task_action_weight = match task.action.clone() {
							// TODO: Run actual task later to return weight
							// not just return weight for test to pass
							Action::XCMP { .. } => Weight::from_ref_time(1_000_000u64),
							Action::NativeTransfer { sender, recipient, amount } =>
								Self::run_native_transfer_task(
									sender,
									recipient,
									amount,
									task_id.clone(),
								),
						};
						Tasks::<T>::remove(task_id);
						task_action_weight
							.saturating_add(T::DbWeight::get().writes(1u64))
							.saturating_add(T::DbWeight::get().reads(1u64))
					},
				};

				weight_left = weight_left.saturating_sub(action_weight);

				let run_another_task_weight = <T as Config>::WeightInfo::emit_event()
					.saturating_add(T::DbWeight::get().writes(1u64))
					.saturating_add(T::DbWeight::get().reads(1u64));
				if weight_left.ref_time() < run_another_task_weight.ref_time() {
					break
				}
			}

			if consumed_task_index == task_ids.len() {
				(vec![], weight_left)
			} else {
				(task_ids.split_off(consumed_task_index), weight_left)
			}
		}

		/// Schedule task and return it's task_id.
		/// With transaction will protect against a partial success where N of M execution times might be full,
		/// rolling back any successful insertions into the schedule task table.
		pub fn schedule_task(task: &Task<T>) -> Result<TaskId, Error<T>> {
			// TODO: Rewrite this function
			//if let Some(_) = Self::get_task((asset.clone(), task_id.clone())) {
			//	Err(Error::<T>::DuplicateTask)?
			//}
			//if let Some(mut asset_tasks) = Self::get_scheduled_tasks((
			//	asset.clone(),
			//	direction.clone(),
			//	trigger_percentage.clone(),
			//)) {
			//	if let Err(_) = asset_tasks.try_push(task_id.clone()) {
			//		Err(Error::<T>::MaxTasksReached)?
			//	}
			//	<ScheduledTasks<T>>::insert((asset, direction, trigger_percentage), asset_tasks);
			//} else {
			//	let scheduled_tasks: BoundedVec<T::Hash, T::MaxTasksPerSlot> =
			//		vec![task_id.clone()].try_into().unwrap();
			//	<ScheduledTasks<T>>::insert(
			//		(asset, direction, trigger_percentage),
			//		scheduled_tasks,
			//	);
			//}
			Ok(task.task_id.clone())
		}

		/// Validate and schedule task.
		/// This will also charge the execution fee.
		/// TODO: double check atomic
		pub fn validate_and_schedule_task(task: Task<T>) -> Result<(), Error<T>> {
			if task.task_id.is_empty() {
				Err(Error::<T>::EmptyProvidedId)?
			}

			// TODO: correct TaskRegistry to new format
			<Tasks<T>>::insert(task.task_id.clone(), &task);

			if let Some(mut task_index) = Self::get_sorted_tasks_index((
				&task.chain,
				&task.exchange,
				&task.asset_pair.0,
				&task.asset_pair.1,
			)) {
				task_index.insert(task.task_id.clone(), task.trigger_params[0]);
			} else {
				let mut task_index = BTreeMap::<TaskId, u128>::new();
				task_index.insert(task.task_id.clone(), task.trigger_params[0]);

				// TODO: sorted based on trigger_function comparison of the parameter
				// then at the time of trigger we cut off all the left part of the tree
				SortedTasksIndex::<T>::insert(
					(
						task.chain.clone(),
						task.exchange.clone(),
						task.asset_pair.0.clone(),
						task.asset_pair.1.clone(),
					),
					task_index,
				);
			}

			Self::schedule_task(&task)?;

			// TODO: add back signature when insert new task work
			Self::deposit_event(Event::TaskScheduled {
				who: task.owner_id,
				task_id: task.task_id.clone(),
			});
			Ok(())
		}
	}

	impl<T: Config> pallet_valve::Shutdown for Pallet<T> {
		fn is_shutdown() -> bool {
			Self::is_shutdown()
		}
		fn shutdown() {
			Shutdown::<T>::put(true);
		}
		fn restart() {
			Shutdown::<T>::put(false);
		}
	}
}
