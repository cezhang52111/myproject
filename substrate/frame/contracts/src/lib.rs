// Copyright 2018-2020 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate. If not, see <http://www.gnu.org/licenses/>.

//! # Contract Module
//!
//! The Contract module provides functionality for the runtime to deploy and execute WebAssembly smart-contracts.
//!
//! - [`contract::Trait`](./trait.Trait.html)
//! - [`Call`](./enum.Call.html)
//!
//! ## Overview
//!
//! This module extends accounts based on the `Currency` trait to have smart-contract functionality. It can
//! be used with other modules that implement accounts based on `Currency`. These "smart-contract accounts"
//! have the ability to instantiate smart-contracts and make calls to other contract and non-contract accounts.
//!
//! The smart-contract code is stored once in a `code_cache`, and later retrievable via its `code_hash`.
//! This means that multiple smart-contracts can be instantiated from the same `code_cache`, without replicating
//! the code each time.
//!
//! When a smart-contract is called, its associated code is retrieved via the code hash and gets executed.
//! This call can alter the storage entries of the smart-contract account, instantiate new smart-contracts,
//! or call other smart-contracts.
//!
//! Finally, when an account is reaped, its associated code and storage of the smart-contract account
//! will also be deleted.
//!
//! ### Gas
//!
//! Senders must specify a gas limit with every call, as all instructions invoked by the smart-contract require gas.
//! Unused gas is refunded after the call, regardless of the execution outcome.
//!
//! If the gas limit is reached, then all calls and state changes (including balance transfers) are only
//! reverted at the current call's contract level. For example, if contract A calls B and B runs out of gas mid-call,
//! then all of B's calls are reverted. Assuming correct error handling by contract A, A's other calls and state
//! changes still persist.
//!
//! ### Notable Scenarios
//!
//! Contract call failures are not always cascading. When failures occur in a sub-call, they do not "bubble up",
//! and the call will only revert at the specific contract level. For example, if contract A calls contract B, and B
//! fails, A can decide how to handle that failure, either proceeding or reverting A's changes.
//!
//! ## Interface
//!
//! ### Dispatchable functions
//!
//! * `put_code` - Stores the given binary Wasm code into the chain's storage and returns its `code_hash`.
//! * `instantiate` - Deploys a new contract from the given `code_hash`, optionally transferring some balance.
//! This instantiates a new smart contract account and calls its contract deploy handler to
//! initialize the contract.
//! * `call` - Makes a call to an account, optionally transferring some balance.
//!
//! ### Signed Extensions
//!
//! The contracts module defines the following extension:
//!
//!   - [`CheckBlockGasLimit`]: Ensures that the transaction does not exceeds the block gas limit.
//!
//! The signed extension needs to be added as signed extra to the transaction type to be used in the
//! runtime.
//!
//! ## Usage
//!
//! The Contract module is a work in progress. The following examples show how this Contract module
//! can be used to instantiate and call contracts.
//!
//! * [`ink`](https://github.com/paritytech/ink) is
//! an [`eDSL`](https://wiki.haskell.org/Embedded_domain_specific_language) that enables writing
//! WebAssembly based smart contracts in the Rust programming language. This is a work in progress.
//!
//! ## Related Modules
//!
//! * [Balances](../pallet_balances/index.html)

#![cfg_attr(not(feature = "std"), no_std)]

#[macro_use]
pub mod gas;

pub mod account_db;
pub mod exec;
pub mod wasm;
pub mod rent;

//#[cfg(test)]
mod tests;

use crate::exec::ExecutionContext;
use crate::account_db::{AccountDb, DirectAccountDb};
use crate::wasm::{WasmLoader, WasmVm};
use crate::wasm::prepare::prepare_contract;
use crate::wasm::{runtime, WasmExecutable};
use wabt;
use hex_literal::hex;
use assert_matches::assert_matches;
use sp_runtime::DispatchError;

pub use crate::gas::{Gas, GasMeter};
pub use crate::exec::{Ext, StorageKey, ExecResult, ExecReturnValue, ExecError, StatusCode, STATUS_SUCCESS};

use crate::tests::{Test};
use crate::tests::Call as TestCall;
use std::collections::HashMap;
use std::cell::RefCell;
use sp_core::H256;
#[cfg(feature = "std")]
use serde::{Serialize, Deserialize};
use sp_core::crypto::UncheckedFrom;
use sp_std::{prelude::*, marker::PhantomData, fmt::Debug};
use codec::{Codec, Encode, Decode};
use sp_io::hashing::blake2_256;
use sp_runtime::{
	traits::{Hash, StaticLookup, Zero, MaybeSerializeDeserialize, Member, SignedExtension},
	transaction_validity::{
		ValidTransaction, InvalidTransaction, TransactionValidity, TransactionValidityError,
	},
	RuntimeDebug,
};
use frame_support::dispatch::{DispatchResult, Dispatchable};
use frame_support::{
	Parameter, decl_module, decl_event, decl_storage, decl_error, storage::child,
	parameter_types, IsSubType,
	weights::DispatchInfo,
};
use frame_support::traits::{OnFreeBalanceZero, OnUnbalanced, Currency, Get, Time, Randomness};
use frame_system::{self as system, ensure_signed, RawOrigin, ensure_root};
use sp_core::storage::well_known_keys::CHILD_STORAGE_KEY_PREFIX;

pub type CodeHash<T> = <T as frame_system::Trait>::Hash;
pub type TrieId = Vec<u8>;

/// A function that generates an `AccountId` for a contract upon instantiation.
pub trait ContractAddressFor<CodeHash, AccountId> {
	fn contract_address_for(code_hash: &CodeHash, data: &[u8], origin: &AccountId) -> AccountId;
}

/// A function that returns the fee for dispatching a `Call`.
pub trait ComputeDispatchFee<Call, Balance> {
	fn compute_dispatch_fee(call: &Call) -> Balance;
}

/// Information for managing an acocunt and its sub trie abstraction.
/// This is the required info to cache for an account
#[derive(Encode, Decode, RuntimeDebug)]
pub enum ContractInfo<T: Trait> {
	Alive(AliveContractInfo<T>),
	Tombstone(TombstoneContractInfo<T>),
}

impl<T: Trait> ContractInfo<T> {
	/// If contract is alive then return some alive info
	pub fn get_alive(self) -> Option<AliveContractInfo<T>> {
		if let ContractInfo::Alive(alive) = self {
			Some(alive)
		} else {
			None
		}
	}
	/// If contract is alive then return some reference to alive info
	pub fn as_alive(&self) -> Option<&AliveContractInfo<T>> {
		if let ContractInfo::Alive(ref alive) = self {
			Some(alive)
		} else {
			None
		}
	}
	/// If contract is alive then return some mutable reference to alive info
	pub fn as_alive_mut(&mut self) -> Option<&mut AliveContractInfo<T>> {
		if let ContractInfo::Alive(ref mut alive) = self {
			Some(alive)
		} else {
			None
		}
	}

	/// If contract is tombstone then return some tombstone info
	pub fn get_tombstone(self) -> Option<TombstoneContractInfo<T>> {
		if let ContractInfo::Tombstone(tombstone) = self {
			Some(tombstone)
		} else {
			None
		}
	}
	/// If contract is tombstone then return some reference to tombstone info
	pub fn as_tombstone(&self) -> Option<&TombstoneContractInfo<T>> {
		if let ContractInfo::Tombstone(ref tombstone) = self {
			Some(tombstone)
		} else {
			None
		}
	}
	/// If contract is tombstone then return some mutable reference to tombstone info
	pub fn as_tombstone_mut(&mut self) -> Option<&mut TombstoneContractInfo<T>> {
		if let ContractInfo::Tombstone(ref mut tombstone) = self {
			Some(tombstone)
		} else {
			None
		}
	}
}

pub type AliveContractInfo<T> =
	RawAliveContractInfo<CodeHash<T>, BalanceOf<T>, <T as frame_system::Trait>::BlockNumber>;

/// Information for managing an account and its sub trie abstraction.
/// This is the required info to cache for an account.
#[derive(Encode, Decode, Clone, PartialEq, Eq, RuntimeDebug)]
pub struct RawAliveContractInfo<CodeHash, Balance, BlockNumber> {
	/// Unique ID for the subtree encoded as a bytes vector.
	pub trie_id: TrieId,
	/// The size of stored value in octet.
	pub storage_size: u32,
	/// The code associated with a given account.
	pub code_hash: CodeHash,
	/// Pay rent at most up to this value.
	pub rent_allowance: Balance,
	/// Last block rent has been payed.
	pub deduct_block: BlockNumber,
	/// Last block child storage has been written.
	pub last_write: Option<BlockNumber>,
}

impl<CodeHash, Balance, BlockNumber> RawAliveContractInfo<CodeHash, Balance, BlockNumber> {
	/// Associated child trie unique id is built from the hash part of the trie id.
	pub fn child_trie_unique_id(&self) -> child::ChildInfo {
		trie_unique_id(&self.trie_id[..])
	}
}

/// Associated child trie unique id is built from the hash part of the trie id.
pub(crate) fn trie_unique_id(trie_id: &[u8]) -> child::ChildInfo {
	let start = CHILD_STORAGE_KEY_PREFIX.len() + b"default:".len();
	child::ChildInfo::new_default(&trie_id[start ..])
}

pub type TombstoneContractInfo<T> =
	RawTombstoneContractInfo<<T as frame_system::Trait>::Hash, <T as frame_system::Trait>::Hashing>;

#[derive(Encode, Decode, PartialEq, Eq, RuntimeDebug)]
pub struct RawTombstoneContractInfo<H, Hasher>(H, PhantomData<Hasher>);

impl<H, Hasher> RawTombstoneContractInfo<H, Hasher>
where
	H: Member + MaybeSerializeDeserialize+ Debug
		+ AsRef<[u8]> + AsMut<[u8]> + Copy + Default
		+ sp_std::hash::Hash + Codec,
	Hasher: Hash<Output=H>,
{
	fn new(storage_root: &[u8], code_hash: H) -> Self {
		let mut buf = Vec::new();
		storage_root.using_encoded(|encoded| buf.extend_from_slice(encoded));
		buf.extend_from_slice(code_hash.as_ref());
		RawTombstoneContractInfo(Hasher::hash(&buf[..]), PhantomData)
	}
}

/// Get a trie id (trie id must be unique and collision resistant depending upon its context).
/// Note that it is different than encode because trie id should be collision resistant
/// (being a proper unique identifier).
pub trait TrieIdGenerator<AccountId> {
	/// Get a trie id for an account, using reference to parent account trie id to ensure
	/// uniqueness of trie id.
	///
	/// The implementation must ensure every new trie id is unique: two consecutive calls with the
	/// same parameter needs to return different trie id values.
	///
	/// Also, the implementation is responsible for ensuring that `TrieId` starts with
	/// `:child_storage:`.
	/// TODO: We want to change this, see https://github.com/paritytech/substrate/issues/2325
	fn trie_id(account_id: &AccountId) -> TrieId;
}

/// Get trie id from `account_id`.
pub struct TrieIdFromParentCounter<T: Trait>(PhantomData<T>);

/// This generator uses inner counter for account id and applies the hash over `AccountId +
/// accountid_counter`.
impl<T: Trait> TrieIdGenerator<T::AccountId> for TrieIdFromParentCounter<T>
where
	T::AccountId: AsRef<[u8]>
{
	fn trie_id(account_id: &T::AccountId) -> TrieId {
		// Note that skipping a value due to error is not an issue here.
		// We only need uniqueness, not sequence.
		let new_seed = AccountCounter::mutate(|v| {
			*v = v.wrapping_add(1);
			*v
		});

		let mut buf = Vec::new();
		buf.extend_from_slice(account_id.as_ref());
		buf.extend_from_slice(&new_seed.to_le_bytes()[..]);

		// TODO: see https://github.com/paritytech/substrate/issues/2325
		CHILD_STORAGE_KEY_PREFIX.iter()
			.chain(b"default:")
			.chain(T::Hashing::hash(&buf[..]).as_ref().iter())
			.cloned()
			.collect()
	}
}

pub type BalanceOf<T> = <<T as Trait>::Currency as Currency<<T as frame_system::Trait>::AccountId>>::Balance;
pub type NegativeImbalanceOf<T> =
	<<T as Trait>::Currency as Currency<<T as frame_system::Trait>::AccountId>>::NegativeImbalance;

parameter_types! {
	/// A reasonable default value for [`Trait::SignedClaimedHandicap`].
	pub const DefaultSignedClaimHandicap: u32 = 2;
	/// A reasonable default value for [`Trait::TombstoneDeposit`].
	pub const DefaultTombstoneDeposit: u32 = 16;
	/// A reasonable default value for [`Trait::StorageSizeOffset`].
	pub const DefaultStorageSizeOffset: u32 = 8;
	/// A reasonable default value for [`Trait::RentByteFee`].
	pub const DefaultRentByteFee: u32 = 4;
	/// A reasonable default value for [`Trait::RentDepositOffset`].
	pub const DefaultRentDepositOffset: u32 = 1000;
	/// A reasonable default value for [`Trait::SurchargeReward`].
	pub const DefaultSurchargeReward: u32 = 150;
	/// A reasonable default value for [`Trait::TransferFee`].
	pub const DefaultTransferFee: u32 = 0;
	/// A reasonable default value for [`Trait::InstantiationFee`].
	pub const DefaultInstantiationFee: u32 = 0;
	/// A reasonable default value for [`Trait::TransactionBaseFee`].
	pub const DefaultTransactionBaseFee: u32 = 0;
	/// A reasonable default value for [`Trait::TransactionByteFee`].
	pub const DefaultTransactionByteFee: u32 = 0;
	/// A reasonable default value for [`Trait::ContractFee`].
	pub const DefaultContractFee: u32 = 21;
	/// A reasonable default value for [`Trait::CallBaseFee`].
	pub const DefaultCallBaseFee: u32 = 1000;
	/// A reasonable default value for [`Trait::InstantiateBaseFee`].
	pub const DefaultInstantiateBaseFee: u32 = 1000;
	/// A reasonable default value for [`Trait::MaxDepth`].
	pub const DefaultMaxDepth: u32 = 32;
	/// A reasonable default value for [`Trait::MaxValueSize`].
	pub const DefaultMaxValueSize: u32 = 16_384;
	/// A reasonable default value for [`Trait::BlockGasLimit`].
	pub const DefaultBlockGasLimit: u32 = 10_000_000;
}

pub trait Trait: frame_system::Trait {
	type Currency: Currency<Self::AccountId>;
	type Time: Time;
	type Randomness: Randomness<Self::Hash>;

	/// The outer call dispatch type.
	type Call: Parameter + Dispatchable<Origin=<Self as frame_system::Trait>::Origin> + IsSubType<Module<Self>, Self>;

	/// The overarching event type.
	type Event: From<Event<Self>> + Into<<Self as frame_system::Trait>::Event>;

	/// A function type to get the contract address given the instantiator.
	type DetermineContractAddress: ContractAddressFor<CodeHash<Self>, Self::AccountId>;

	/// A function type that computes the fee for dispatching the given `Call`.
	///
	/// It is recommended (though not required) for this function to return a fee that would be
	/// taken by the Executive module for regular dispatch.
	type ComputeDispatchFee: ComputeDispatchFee<<Self as Trait>::Call, BalanceOf<Self>>;

	/// trie id generator
	type TrieIdGenerator: TrieIdGenerator<Self::AccountId>;

	/// Handler for the unbalanced reduction when making a gas payment.
	type GasPayment: OnUnbalanced<NegativeImbalanceOf<Self>>;

	/// Handler for rent payments.
	type RentPayment: OnUnbalanced<NegativeImbalanceOf<Self>>;

	/// Number of block delay an extrinsic claim surcharge has.
	///
	/// When claim surcharge is called by an extrinsic the rent is checked
	/// for current_block - delay
	type SignedClaimHandicap: Get<Self::BlockNumber>;

	/// The minimum amount required to generate a tombstone.
	type TombstoneDeposit: Get<BalanceOf<Self>>;

	/// Size of a contract at the time of instantiation. This is a simple way to ensure
	/// that empty contracts eventually gets deleted.
	type StorageSizeOffset: Get<u32>;

	/// Price of a byte of storage per one block interval. Should be greater than 0.
	type RentByteFee: Get<BalanceOf<Self>>;

	/// The amount of funds a contract should deposit in order to offset
	/// the cost of one byte.
	///
	/// Let's suppose the deposit is 1,000 BU (balance units)/byte and the rent is 1 BU/byte/day,
	/// then a contract with 1,000,000 BU that uses 1,000 bytes of storage would pay no rent.
	/// But if the balance reduced to 500,000 BU and the storage stayed the same at 1,000,
	/// then it would pay 500 BU/day.
	type RentDepositOffset: Get<BalanceOf<Self>>;

	/// Reward that is received by the party whose touch has led
	/// to removal of a contract.
	type SurchargeReward: Get<BalanceOf<Self>>;

	/// The fee required to make a transfer.
	type TransferFee: Get<BalanceOf<Self>>;

	/// The fee required to create an account.
	type CreationFee: Get<BalanceOf<Self>>;

	/// The fee to be paid for making a transaction; the base.
	type TransactionBaseFee: Get<BalanceOf<Self>>;

	/// The fee to be paid for making a transaction; the per-byte portion.
	type TransactionByteFee: Get<BalanceOf<Self>>;

	/// The fee required to instantiate a contract instance.
	type ContractFee: Get<BalanceOf<Self>>;

	/// The base fee charged for calling into a contract.
	type CallBaseFee: Get<Gas>;

	/// The base fee charged for instantiating a contract.
	type InstantiateBaseFee: Get<Gas>;

	/// The maximum nesting level of a call/instantiate stack.
	type MaxDepth: Get<u32>;

	/// The maximum size of a storage value in bytes.
	type MaxValueSize: Get<u32>;

	/// The maximum amount of gas that could be expended per block.
	type BlockGasLimit: Get<Gas>;
}

/// Simple contract address determiner.
///
/// Address calculated from the code (of the constructor), input data to the constructor,
/// and the account id that requested the account creation.
///
/// Formula: `blake2_256(blake2_256(code) + blake2_256(data) + origin)`
pub struct SimpleAddressDeterminator<T: Trait>(PhantomData<T>);
impl<T: Trait> ContractAddressFor<CodeHash<T>, T::AccountId> for SimpleAddressDeterminator<T>
where
	T::AccountId: UncheckedFrom<T::Hash> + AsRef<[u8]>
{
	fn contract_address_for(code_hash: &CodeHash<T>, data: &[u8], origin: &T::AccountId) -> T::AccountId {
		let data_hash = T::Hashing::hash(data);

		let mut buf = Vec::new();
		buf.extend_from_slice(code_hash.as_ref());
		buf.extend_from_slice(data_hash.as_ref());
		buf.extend_from_slice(origin.as_ref());

		UncheckedFrom::unchecked_from(T::Hashing::hash(&buf[..]))
	}
}

/// The default dispatch fee computor computes the fee in the same way that
/// the implementation of `ChargeTransactionPayment` for the Balances module does. Note that this only takes a fixed
/// fee based on size. Unlike the balances module, weight-fee is applied.
pub struct DefaultDispatchFeeComputor<T: Trait>(PhantomData<T>);
impl<T: Trait> ComputeDispatchFee<<T as Trait>::Call, BalanceOf<T>> for DefaultDispatchFeeComputor<T> {
	fn compute_dispatch_fee(call: &<T as Trait>::Call) -> BalanceOf<T> {
		let encoded_len = call.using_encoded(|encoded| encoded.len() as u32);
		let base_fee = T::TransactionBaseFee::get();
		let byte_fee = T::TransactionByteFee::get();
		base_fee + byte_fee * encoded_len.into()
	}
}

decl_error! {
	/// Error for the contracts module.
	pub enum Error for Module<T: Trait> {
		/// A new schedule must have a greater version than the current one.
		InvalidScheduleVersion,
		/// An origin must be signed or inherent and auxiliary sender only provided on inherent.
		InvalidSurchargeClaim,
		/// Cannot restore from nonexisting or tombstone contract.
		InvalidSourceContract,
		/// Cannot restore to nonexisting or alive contract.
		InvalidDestinationContract,
		/// Tombstones don't match.
		InvalidTombstone,
		/// An origin TrieId written in the current block.
		InvalidContractOrigin
	}
}

decl_module! {
	/// Contracts module.
	pub struct Module<T: Trait> for enum Call where origin: <T as frame_system::Trait>::Origin {
		type Error = Error<T>;

		/// Number of block delay an extrinsic claim surcharge has.
		///
		/// When claim surcharge is called by an extrinsic the rent is checked
		/// for current_block - delay
		const SignedClaimHandicap: T::BlockNumber = T::SignedClaimHandicap::get();

		/// The minimum amount required to generate a tombstone.
		const TombstoneDeposit: BalanceOf<T> = T::TombstoneDeposit::get();

		/// Size of a contract at the time of instantiaion. This is a simple way to ensure that
		/// empty contracts eventually gets deleted.
		const StorageSizeOffset: u32 = T::StorageSizeOffset::get();

		/// Price of a byte of storage per one block interval. Should be greater than 0.
		const RentByteFee: BalanceOf<T> = T::RentByteFee::get();

		/// The amount of funds a contract should deposit in order to offset
		/// the cost of one byte.
		///
		/// Let's suppose the deposit is 1,000 BU (balance units)/byte and the rent is 1 BU/byte/day,
		/// then a contract with 1,000,000 BU that uses 1,000 bytes of storage would pay no rent.
		/// But if the balance reduced to 500,000 BU and the storage stayed the same at 1,000,
		/// then it would pay 500 BU/day.
		const RentDepositOffset: BalanceOf<T> = T::RentDepositOffset::get();

		/// Reward that is received by the party whose touch has led
		/// to removal of a contract.
		const SurchargeReward: BalanceOf<T> = T::SurchargeReward::get();

		/// The fee required to make a transfer.
		const TransferFee: BalanceOf<T> = T::TransferFee::get();

		/// The fee required to create an account.
		const CreationFee: BalanceOf<T> = T::CreationFee::get();

		/// The fee to be paid for making a transaction; the base.
		const TransactionBaseFee: BalanceOf<T> = T::TransactionBaseFee::get();

		/// The fee to be paid for making a transaction; the per-byte portion.
		const TransactionByteFee: BalanceOf<T> = T::TransactionByteFee::get();

		/// The fee required to instantiate a contract instance. A reasonable default value
		/// is 21.
		const ContractFee: BalanceOf<T> = T::ContractFee::get();

		/// The base fee charged for calling into a contract. A reasonable default
		/// value is 135.
		const CallBaseFee: Gas = T::CallBaseFee::get();

		/// The base fee charged for instantiating a contract. A reasonable default value
		/// is 175.
		const InstantiateBaseFee: Gas = T::InstantiateBaseFee::get();

		/// The maximum nesting level of a call/instantiate stack. A reasonable default
		/// value is 100.
		const MaxDepth: u32 = T::MaxDepth::get();

		/// The maximum size of a storage value in bytes. A reasonable default is 16 KiB.
		const MaxValueSize: u32 = T::MaxValueSize::get();

		/// The maximum amount of gas that could be expended per block. A reasonable
		/// default value is 10_000_000.
		const BlockGasLimit: Gas = T::BlockGasLimit::get();

		fn deposit_event() = default;

		/// Updates the schedule for metering contracts.
		///
		/// The schedule must have a greater version than the stored schedule.
		pub fn update_schedule(origin, schedule: Schedule) -> DispatchResult {
			ensure_root(origin)?;
			if <Module<T>>::current_schedule().version >= schedule.version {
				Err(Error::<T>::InvalidScheduleVersion)?
			}

			Self::deposit_event(RawEvent::ScheduleUpdated(schedule.version));
			CurrentSchedule::put(schedule);

			Ok(())
		}

		/// Stores the given binary Wasm code into the chain's storage and returns its `codehash`.
		/// You can instantiate contracts only with stored code.
		pub fn put_code(
			origin,
			#[compact] gas_limit: Gas,
			code: Vec<u8>
		) -> DispatchResult {
			let origin = ensure_signed(origin)?;

			let (mut gas_meter, imbalance) = gas::buy_gas::<T>(&origin, gas_limit)?;

			let schedule = <Module<T>>::current_schedule();
			let result = wasm::save_code::<T>(code, &mut gas_meter, &schedule);
			if let Ok(code_hash) = result {
				Self::deposit_event(RawEvent::CodeStored(code_hash));
			}

			gas::refund_unused_gas::<T>(&origin, gas_meter, imbalance);

			result.map(|_| ()).map_err(Into::into)
		}

		/// Makes a call to an account, optionally transferring some balance.
		///
		/// * If the account is a smart-contract account, the associated code will be
		/// executed and any value will be transferred.
		/// * If the account is a regular account, any value will be transferred.
		/// * If no account exists and the call value is not less than `existential_deposit`,
		/// a regular account will be created and any value will be transferred.
		pub fn call(
			origin,
			dest: <T::Lookup as StaticLookup>::Source,
			#[compact] value: BalanceOf<T>,
			#[compact] gas_limit: Gas,
			data: Vec<u8>
		) -> DispatchResult {
			let origin = ensure_signed(origin)?;
			let dest = T::Lookup::lookup(dest)?;

			Self::bare_call(origin, dest, value, gas_limit, data)
				.map(|_| ())
				.map_err(|e| e.reason.into())
		}

		/// Instantiates a new contract from the `codehash` generated by `put_code`, optionally transferring some balance.
		///
		/// Instantiation is executed as follows:
		///
		/// - The destination address is computed based on the sender and hash of the code.
		/// - The smart-contract account is created at the computed address.
		/// - The `ctor_code` is executed in the context of the newly-created account. Buffer returned
		///   after the execution is saved as the `code` of the account. That code will be invoked
		///   upon any call received by this account.
		/// - The contract is initialized.
		pub fn instantiate(
			origin,
			#[compact] endowment: BalanceOf<T>,
			#[compact] gas_limit: Gas,
			code_hash: CodeHash<T>,
			data: Vec<u8>
		) -> DispatchResult {
			let origin = ensure_signed(origin)?;

			Self::execute_wasm(origin, gas_limit, |ctx, gas_meter| {
				ctx.instantiate(endowment, gas_meter, &code_hash, data)
					.map(|(_address, output)| output)
			})
			.map(|_| ())
			.map_err(|e| e.reason.into())
		}

		/// Allows block producers to claim a small reward for evicting a contract. If a block producer
		/// fails to do so, a regular users will be allowed to claim the reward.
		///
		/// If contract is not evicted as a result of this call, no actions are taken and
		/// the sender is not eligible for the reward.
		fn claim_surcharge(origin, dest: T::AccountId, aux_sender: Option<T::AccountId>) {
			let origin = origin.into();
			let (signed, rewarded) = match (origin, aux_sender) {
				(Ok(frame_system::RawOrigin::Signed(account)), None) => {
					(true, account)
				},
				(Ok(frame_system::RawOrigin::None), Some(aux_sender)) => {
					(false, aux_sender)
				},
				_ => Err(Error::<T>::InvalidSurchargeClaim)?,
			};

			// Add some advantage for block producers (who send unsigned extrinsics) by
			// adding a handicap: for signed extrinsics we use a slightly older block number
			// for the eviction check. This can be viewed as if we pushed regular users back in past.
			let handicap = if signed {
				T::SignedClaimHandicap::get()
			} else {
				Zero::zero()
			};

			// If poking the contract has lead to eviction of the contract, give out the rewards.
			if rent::try_evict::<T>(&dest, handicap) == rent::RentOutcome::Evicted {
				T::Currency::deposit_into_existing(&rewarded, T::SurchargeReward::get())?;
			}
		}

		fn on_finalize() {
			GasSpent::kill();
		}
	}
}

/// The possible errors that can happen querying the storage of a contract.
pub enum GetStorageError {
	/// The given address doesn't point on a contract.
	ContractDoesntExist,
	/// The specified contract is a tombstone and thus cannot have any storage.
	IsTombstone,
}

/// Public APIs provided by the contracts module.
impl<T: Trait> Module<T> {
	/// Perform a call to a specified contract.
	///
	/// This function is similar to `Self::call`, but doesn't perform any address lookups and better
	/// suitable for calling directly from Rust.
	pub fn bare_call(
		origin: T::AccountId,
		dest: T::AccountId,
		value: BalanceOf<T>,
		gas_limit: Gas,
		input_data: Vec<u8>,
	) -> ExecResult {
		Self::execute_wasm(origin, gas_limit, |ctx, gas_meter| {
			ctx.call(dest, value, gas_meter, input_data)
		})
	}

	/// Query storage of a specified contract under a specified key.
	pub fn get_storage(
		address: T::AccountId,
		key: [u8; 32],
	) -> sp_std::result::Result<Option<Vec<u8>>, GetStorageError> {
		let contract_info = <ContractInfoOf<T>>::get(&address)
			.ok_or(GetStorageError::ContractDoesntExist)?
			.get_alive()
			.ok_or(GetStorageError::IsTombstone)?;

		let maybe_value = AccountDb::<T>::get_storage(
			&DirectAccountDb,
			&address,
			Some(&contract_info.trie_id),
			&key,
		);
		Ok(maybe_value)
	}
}

impl<T: Trait> Module<T> {
	fn execute_wasm(
		origin: T::AccountId,
		gas_limit: Gas,
		func: impl FnOnce(&mut ExecutionContext<T, WasmVm, WasmLoader>, &mut GasMeter<T>) -> ExecResult
	) -> ExecResult {
		// Pay for the gas upfront.
		//
		// NOTE: it is very important to avoid any state changes before
		// paying for the gas.
		let (mut gas_meter, imbalance) =
			try_or_exec_error!(
				gas::buy_gas::<T>(&origin, gas_limit),
				// We don't have a spare buffer here in the first place, so create a new empty one.
				Vec::new()
			);

		let cfg = Config::preload();
		let vm = WasmVm::new(&cfg.schedule);
		let loader = WasmLoader::new(&cfg.schedule);
		let mut ctx = ExecutionContext::top_level(origin.clone(), &cfg, &vm, &loader);

		let result = func(&mut ctx, &mut gas_meter);

		if result.as_ref().map(|output| output.is_success()).unwrap_or(false) {
			// Commit all changes that made it thus far into the persistent storage.
			DirectAccountDb.commit(ctx.overlay.into_change_set());
		}

		// Refund cost of the unused gas.
		//
		// NOTE: This should go after the commit to the storage, since the storage changes
		// can alter the balance of the caller.
		gas::refund_unused_gas::<T>(&origin, gas_meter, imbalance);

		// Execute deferred actions.
		ctx.deferred.into_iter().for_each(|deferred| {
			use self::exec::DeferredAction::*;
			match deferred {
				DepositEvent {
					topics,
					event,
				} => <frame_system::Module<T>>::deposit_event_indexed(
					&*topics,
					<T as Trait>::Event::from(event).into(),
				),
				DispatchRuntimeCall {
					origin: who,
					call,
				} => {
					let result = call.dispatch(RawOrigin::Signed(who.clone()).into());
					Self::deposit_event(RawEvent::Dispatched(who, result.is_ok()));
				}
				RestoreTo {
					donor,
					dest,
					code_hash,
					rent_allowance,
					delta,
				} => {
					let _result = Self::restore_to(donor, dest, code_hash, rent_allowance, delta);
				}
			}
		});

		result
	}

	fn restore_to(
		origin: T::AccountId,
		dest: T::AccountId,
		code_hash: CodeHash<T>,
		rent_allowance: BalanceOf<T>,
		delta: Vec<exec::StorageKey>
	) -> DispatchResult {
		let mut origin_contract = <ContractInfoOf<T>>::get(&origin)
			.and_then(|c| c.get_alive())
			.ok_or(Error::<T>::InvalidSourceContract)?;

		let current_block = <frame_system::Module<T>>::block_number();

		if origin_contract.last_write == Some(current_block) {
			Err(Error::<T>::InvalidContractOrigin)?
		}

		let dest_tombstone = <ContractInfoOf<T>>::get(&dest)
			.and_then(|c| c.get_tombstone())
			.ok_or(Error::<T>::InvalidDestinationContract)?;

		let last_write = if !delta.is_empty() {
			Some(current_block)
		} else {
			origin_contract.last_write
		};

		let key_values_taken = delta.iter()
			.filter_map(|key| {
				child::get_raw(
					&origin_contract.trie_id,
					origin_contract.child_trie_unique_id(),
					&blake2_256(key),
				).map(|value| {
					child::kill(
						&origin_contract.trie_id,
						origin_contract.child_trie_unique_id(),
						&blake2_256(key),
					);

					(key, value)
				})
			})
			.collect::<Vec<_>>();

		let tombstone = <TombstoneContractInfo<T>>::new(
			// This operation is cheap enough because last_write (delta not included)
			// is not this block as it has been checked earlier.
			&child::child_root(
				&origin_contract.trie_id,
			)[..],
			code_hash,
		);

		if tombstone != dest_tombstone {
			for (key, value) in key_values_taken {
				child::put_raw(
					&origin_contract.trie_id,
					origin_contract.child_trie_unique_id(),
					&blake2_256(key),
					&value,
				);
			}

			return Err(Error::<T>::InvalidTombstone.into());
		}

		origin_contract.storage_size -= key_values_taken.iter()
			.map(|(_, value)| value.len() as u32)
			.sum::<u32>();

		<ContractInfoOf<T>>::remove(&origin);
		<ContractInfoOf<T>>::insert(&dest, ContractInfo::Alive(RawAliveContractInfo {
			trie_id: origin_contract.trie_id,
			storage_size: origin_contract.storage_size,
			code_hash,
			rent_allowance,
			deduct_block: current_block,
			last_write,
		}));

		let origin_free_balance = T::Currency::free_balance(&origin);
		T::Currency::make_free_balance_be(&origin, <BalanceOf<T>>::zero());
		T::Currency::deposit_creating(&dest, origin_free_balance);

		Ok(())
	}
}

decl_event! {
	pub enum Event<T>
	where
		Balance = BalanceOf<T>,
		<T as frame_system::Trait>::AccountId,
		<T as frame_system::Trait>::Hash
	{
		/// Transfer happened `from` to `to` with given `value` as part of a `call` or `instantiate`.
		Transfer(AccountId, AccountId, Balance),

		/// Contract deployed by address at the specified address.
		Instantiated(AccountId, AccountId),

		/// Code with the specified hash has been stored.
		CodeStored(Hash),

		/// Triggered when the current schedule is updated.
		ScheduleUpdated(u32),

		/// A call was dispatched from the given account. The bool signals whether it was
		/// successful execution or not.
		Dispatched(AccountId, bool),

		/// An event from contract of account.
		Contract(AccountId, Vec<u8>),
	}
}

decl_storage! {
	trait Store for Module<T: Trait> as Contract {
		/// Gas spent so far in this block.
		GasSpent get(fn gas_spent): Gas;
		/// Current cost schedule for contracts.
		CurrentSchedule get(fn current_schedule) config(): Schedule = Schedule::default();
		/// A mapping from an original code hash to the original code, untouched by instrumentation.
		pub PristineCode: map CodeHash<T> => Option<Vec<u8>>;
		/// A mapping between an original code hash and instrumented wasm code, ready for execution.
		pub CodeStorage: map CodeHash<T> => Option<wasm::PrefabWasmModule>;
		/// The subtrie counter.
		pub AccountCounter: u64 = 0;
		/// The code associated with a given account.
		pub ContractInfoOf: map T::AccountId => Option<ContractInfo<T>>;
		/// The price of one unit of gas.
		GasPrice get(fn gas_price) config(): BalanceOf<T> = 1.into();
	}
}

impl<T: Trait> OnFreeBalanceZero<T::AccountId> for Module<T> {
	fn on_free_balance_zero(who: &T::AccountId) {
		if let Some(ContractInfo::Alive(info)) = <ContractInfoOf<T>>::take(who) {
			child::kill_storage(&info.trie_id, info.child_trie_unique_id());
		}
	}
}

/// In-memory cache of configuration values.
///
/// We assume that these values can't be changed in the
/// course of transaction execution.
pub struct Config<T: Trait> {
	pub schedule: Schedule,
	pub existential_deposit: BalanceOf<T>,
	pub max_depth: u32,
	pub max_value_size: u32,
	pub contract_account_instantiate_fee: BalanceOf<T>,
	pub account_create_fee: BalanceOf<T>,
	pub transfer_fee: BalanceOf<T>,
}

impl<T: Trait> Config<T> {
	fn preload() -> Config<T> {
		Config {
			schedule: <Module<T>>::current_schedule(),
			existential_deposit: T::Currency::minimum_balance(),
			max_depth: T::MaxDepth::get(),
			max_value_size: T::MaxValueSize::get(),
			contract_account_instantiate_fee: T::ContractFee::get(),
			account_create_fee: T::CreationFee::get(),
			transfer_fee: T::TransferFee::get(),
		}
	}
}

/// Definition of the cost schedule and other parameterizations for wasm vm.
#[cfg_attr(feature = "std", derive(Serialize, Deserialize))]
#[derive(Clone, Encode, Decode, PartialEq, Eq, RuntimeDebug)]
pub struct Schedule {
	/// Version of the schedule.
	pub version: u32,

	/// Cost of putting a byte of code into storage.
	pub put_code_per_byte_cost: Gas,

	/// Gas cost of a growing memory by single page.
	pub grow_mem_cost: Gas,

	/// Gas cost of a regular operation.
	pub regular_op_cost: Gas,

	/// Gas cost per one byte returned.
	pub return_data_per_byte_cost: Gas,

	/// Gas cost to deposit an event; the per-byte portion.
	pub event_data_per_byte_cost: Gas,

	/// Gas cost to deposit an event; the cost per topic.
	pub event_per_topic_cost: Gas,

	/// Gas cost to deposit an event; the base.
	pub event_base_cost: Gas,

	/// Base gas cost to call into a contract.
	pub call_base_cost: Gas,

	/// Base gas cost to instantiate a contract.
	pub instantiate_base_cost: Gas,

	/// Gas cost per one byte read from the sandbox memory.
	pub sandbox_data_read_cost: Gas,

	/// Gas cost per one byte written to the sandbox memory.
	pub sandbox_data_write_cost: Gas,

	/// The maximum number of topics supported by an event.
	pub max_event_topics: u32,

	/// Maximum allowed stack height.
	///
	/// See https://wiki.parity.io/WebAssembly-StackHeight to find out
	/// how the stack frame cost is calculated.
	pub max_stack_height: u32,

	/// Maximum number of memory pages allowed for a contract.
	pub max_memory_pages: u32,

	/// Maximum allowed size of a declared table.
	pub max_table_size: u32,

	/// Whether the `ext_println` function is allowed to be used contracts.
	/// MUST only be enabled for `dev` chains, NOT for production chains
	pub enable_println: bool,

	/// The maximum length of a subject used for PRNG generation.
	pub max_subject_len: u32,
}

impl Default for Schedule {
	fn default() -> Schedule {
		Schedule {
			version: 0,
			put_code_per_byte_cost: 1,
			grow_mem_cost: 1,
			regular_op_cost: 1,
			return_data_per_byte_cost: 1,
			event_data_per_byte_cost: 1,
			event_per_topic_cost: 1,
			event_base_cost: 1,
			call_base_cost: 135,
			instantiate_base_cost: 175,
			sandbox_data_read_cost: 1,
			sandbox_data_write_cost: 1,
			max_event_topics: 4,
			max_stack_height: 64 * 1024,
			max_memory_pages: 16,
			max_table_size: 16 * 1024,
			enable_println: false,
			max_subject_len: 32,
		}
	}
}

/// `SignedExtension` that checks if a transaction would exhausts the block gas limit.
#[derive(Encode, Decode, Clone, Eq, PartialEq)]
pub struct CheckBlockGasLimit<T: Trait + Send + Sync>(PhantomData<T>);

impl<T: Trait + Send + Sync> Default for CheckBlockGasLimit<T> {
	fn default() -> Self {
		Self(PhantomData)
	}
}

impl<T: Trait + Send + Sync> sp_std::fmt::Debug for CheckBlockGasLimit<T> {
	#[cfg(feature = "std")]
	fn fmt(&self, f: &mut sp_std::fmt::Formatter) -> sp_std::fmt::Result {
		write!(f, "CheckBlockGasLimit")
	}

	#[cfg(not(feature = "std"))]
	fn fmt(&self, _: &mut sp_std::fmt::Formatter) -> sp_std::fmt::Result {
		Ok(())
	}
}

impl<T: Trait + Send + Sync> SignedExtension for CheckBlockGasLimit<T> {
	type AccountId = T::AccountId;
	type Call = <T as Trait>::Call;
	type AdditionalSigned = ();
	type DispatchInfo = DispatchInfo;
	type Pre = ();

	fn additional_signed(&self) -> sp_std::result::Result<(), TransactionValidityError> { Ok(()) }

	fn validate(
		&self,
		_: &Self::AccountId,
		call: &Self::Call,
		_: Self::DispatchInfo,
		_: usize,
	) -> TransactionValidity {
		let call = match call.is_sub_type() {
			Some(call) => call,
			None => return Ok(ValidTransaction::default()),
		};

		match call {
			Call::claim_surcharge(_, _) | Call::update_schedule(_) =>
				Ok(ValidTransaction::default()),
			Call::put_code(gas_limit, _)
				| Call::call(_, _, gas_limit, _)
				| Call::instantiate(_, gas_limit, _, _)
			=> {
				// Check if the specified amount of gas is available in the current block.
				// This cannot underflow since `gas_spent` is never greater than `T::BlockGasLimit`.
				let gas_available = T::BlockGasLimit::get() - <Module<T>>::gas_spent();
				if *gas_limit > gas_available {
					// gas limit reached, revert the transaction and retry again in the future
					InvalidTransaction::ExhaustsResources.into()
				} else {
					Ok(ValidTransaction::default())
				}
			},
			Call::__PhantomItem(_, _)  => unreachable!("Variant is never constructed"),
		}
	}
}



#[derive(Debug, PartialEq, Eq)]
struct DispatchEntry(TestCall);

#[derive(Debug, PartialEq, Eq)]
struct RestoreEntry {
	dest: u64,
	code_hash: H256,
	rent_allowance: u64,
	delta: Vec<StorageKey>,
}

#[derive(Debug, PartialEq, Eq)]
struct InstantiateEntry {
	code_hash: H256,
	endowment: u64,
	data: Vec<u8>,
	gas_left: u64,
}

#[derive(Debug, PartialEq, Eq)]
struct TransferEntry {
	to: u64,
	value: u64,
	data: Vec<u8>,
	gas_left: u64,
}

#[derive(Default)]
pub struct MockExt {
	pub storage: HashMap<StorageKey, Vec<u8>>,
	rent_allowance: u64,
	instantiates: Vec<InstantiateEntry>,
	transfers: Vec<TransferEntry>,
	dispatches: Vec<DispatchEntry>,
	restores: Vec<RestoreEntry>,
	// (topics, data)
	events: Vec<(Vec<H256>, Vec<u8>)>,
	next_account_id: u64,

	/// Runtime storage keys works the following way.
	///
	/// - If the test code requests a value and it doesn't exist in this storage map then a
	///   panic happens.
	/// - If the value does exist it is returned and then removed from the map. So a panic
	///   happens if the same value is requested for the second time.
	///
	/// This behavior is used to prevent mixing up an access to unexpected location and empty
	/// cell.
	runtime_storage_keys: RefCell<HashMap<Vec<u8>, Option<Vec<u8>>>>,
	pub write_key_value: HashMap<StorageKey, Vec<u8>>,
	pub read_key_value: HashMap<StorageKey, Vec<u8>>,
}

impl Ext for MockExt {
	type T = Test;

	fn get_storage(&mut self, key: &StorageKey) -> Option<Vec<u8>> {
		//self.storage.get(key).cloned()
		let v = self.storage.get(key).cloned().unwrap_or(Vec::new());
		self.read_key_value.insert(*key, v);
		self.storage.get(key).cloned()
	}
	fn set_storage(&mut self, key: StorageKey, value: Option<Vec<u8>>)
		-> Result<(), &'static str>
	{
		*self.storage.entry(key).or_insert(Vec::new()) = value.clone().unwrap_or(Vec::new());
		self.write_key_value.insert(key, value.unwrap_or(Vec::new()));
		Ok(())
	}
	fn instantiate(
		&mut self,
		code_hash: &CodeHash<Test>,
		endowment: u64,
		gas_meter: &mut GasMeter<Test>,
		data: Vec<u8>,
	) -> Result<(u64, ExecReturnValue), ExecError> {
		self.instantiates.push(InstantiateEntry {
			code_hash: code_hash.clone(),
			endowment,
			data: data.to_vec(),
			gas_left: gas_meter.gas_left(),
		});
		let address = self.next_account_id;
		self.next_account_id += 1;

		Ok((address, ExecReturnValue { status: STATUS_SUCCESS, data: Vec::new() }))
	}
	fn call(
		&mut self,
		to: &u64,
		value: u64,
		gas_meter: &mut GasMeter<Test>,
		data: Vec<u8>,
	) -> ExecResult {
		self.transfers.push(TransferEntry {
			to: *to,
			value,
			data: data.to_vec(),
			gas_left: gas_meter.gas_left(),
		});
		// Assume for now that it was just a plain transfer.
		// TODO: Add tests for different call outcomes.
		Ok(ExecReturnValue { status: STATUS_SUCCESS, data: Vec::new() })
	}
	fn note_dispatch_call(&mut self, call: TestCall) {
		self.dispatches.push(DispatchEntry(call));
	}
	fn note_restore_to(
		&mut self,
		dest: u64,
		code_hash: H256,
		rent_allowance: u64,
		delta: Vec<StorageKey>,
	) {
		self.restores.push(RestoreEntry {
			dest,
			code_hash,
			rent_allowance,
			delta,
		});
	}
	fn caller(&self) -> &u64 {
		&42
	}
	fn address(&self) -> &u64 {
		&69
	}
	fn balance(&self) -> u64 {
		228
	}
	fn value_transferred(&self) -> u64 {
		1337
	}

	fn now(&self) -> &u64 {
		&1111
	}

	fn minimum_balance(&self) -> u64 {
		666
	}

	fn random(&self, subject: &[u8]) -> H256 {
		H256::from_slice(subject)
	}

	fn deposit_event(&mut self, topics: Vec<H256>, data: Vec<u8>) {
		self.events.push((topics, data))
	}

	fn set_rent_allowance(&mut self, rent_allowance: u64) {
		self.rent_allowance = rent_allowance;
	}

	fn rent_allowance(&self) -> u64 {
		self.rent_allowance
	}

	fn block_number(&self) -> u64 { 121 }

	fn max_value_size(&self) -> u32 { 16_384 }

	fn get_runtime_storage(&self, key: &[u8]) -> Option<Vec<u8>> {
		let opt_value = self.runtime_storage_keys
			.borrow_mut()
			.remove(key);
		opt_value.unwrap_or_else(||
			panic!(
				"{:?} doesn't exist. values that do exist {:?}",
				key,
				self.runtime_storage_keys
			)
		)
	}
}

impl Ext for &mut MockExt {
	type T = <MockExt as Ext>::T;

	fn get_storage(&mut self, key: &[u8; 32]) -> Option<Vec<u8>> {
		(**self).get_storage(key)
	}
	fn set_storage(&mut self, key: [u8; 32], value: Option<Vec<u8>>)
		-> Result<(), &'static str>
	{
		(**self).set_storage(key, value)
	}
	fn instantiate(
		&mut self,
		code: &CodeHash<Test>,
		value: u64,
		gas_meter: &mut GasMeter<Test>,
		input_data: Vec<u8>,
	) -> Result<(u64, ExecReturnValue), ExecError> {
		(**self).instantiate(code, value, gas_meter, input_data)
	}
	fn call(
		&mut self,
		to: &u64,
		value: u64,
		gas_meter: &mut GasMeter<Test>,
		input_data: Vec<u8>,
	) -> ExecResult {
		(**self).call(to, value, gas_meter, input_data)
	}
	fn note_dispatch_call(&mut self, call: TestCall) {
		(**self).note_dispatch_call(call)
	}
	fn note_restore_to(
		&mut self,
		dest: u64,
		code_hash: H256,
		rent_allowance: u64,
		delta: Vec<StorageKey>,
	) {
		(**self).note_restore_to(
			dest,
			code_hash,
			rent_allowance,
			delta,
		)
	}
	fn caller(&self) -> &u64 {
		(**self).caller()
	}
	fn address(&self) -> &u64 {
		(**self).address()
	}
	fn balance(&self) -> u64 {
		(**self).balance()
	}
	fn value_transferred(&self) -> u64 {
		(**self).value_transferred()
	}
	fn now(&self) -> &u64 {
		(**self).now()
	}
	fn minimum_balance(&self) -> u64 {
		(**self).minimum_balance()
	}
	fn random(&self, subject: &[u8]) -> H256 {
		(**self).random(subject)
	}
	fn deposit_event(&mut self, topics: Vec<H256>, data: Vec<u8>) {
		(**self).deposit_event(topics, data)
	}
	fn set_rent_allowance(&mut self, rent_allowance: u64) {
		(**self).set_rent_allowance(rent_allowance)
	}
	fn rent_allowance(&self) -> u64 {
		(**self).rent_allowance()
	}
	fn block_number(&self) -> u64 {
		(**self).block_number()
	}
	fn max_value_size(&self) -> u32 {
		(**self).max_value_size()
	}
	fn get_runtime_storage(&self, key: &[u8]) -> Option<Vec<u8>> {
		(**self).get_runtime_storage(key)
	}
}

pub fn execute<E: Ext>(
	wat: &str,
	input_data: Vec<u8>,
	ext: &mut E,
	gas_meter: &mut GasMeter<E::T>,
) -> ExecResult {
	use crate::exec::Vm;

	let wasm = wabt::wat2wasm(wat).unwrap();
	let schedule = crate::Schedule::default();
	let prefab_module =
		prepare_contract::<runtime::Env>(&wasm, &schedule).unwrap();

	let exec = WasmExecutable {
		// Use a "call" convention.
		entrypoint_name: "call",
		prefab_module,
	};

	let cfg = Default::default();
	let vm = WasmVm::new(&cfg);

	vm.execute(&exec, ext, input_data, gas_meter)
}

const CODE_TRANSFER: &str = r#"
(module
	;; ext_call(
	;;    callee_ptr: u32,
	;;    callee_len: u32,
	;;    gas: u64,
	;;    value_ptr: u32,
	;;    value_len: u32,
	;;    input_data_ptr: u32,
	;;    input_data_len: u32
	;;) -> u32
	(import "env" "ext_call" (func $ext_call (param i32 i32 i64 i32 i32 i32 i32) (result i32)))
	(import "env" "memory" (memory 1 1))
	(func (export "call")
		(drop
			(call $ext_call
				(i32.const 4)  ;; Pointer to "callee" address.
				(i32.const 8)  ;; Length of "callee" address.
				(i64.const 0)  ;; How much gas to devote for the execution. 0 = all.
				(i32.const 12) ;; Pointer to the buffer with value to transfer
				(i32.const 8)  ;; Length of the buffer with value to transfer.
				(i32.const 20) ;; Pointer to input data buffer address
				(i32.const 4)  ;; Length of input data buffer
			)
		)
	)
	(func (export "deploy"))

	;; Destination AccountId to transfer the funds.
	;; Represented by u64 (8 bytes long) in little endian.
	(data (i32.const 4) "\09\00\00\00\00\00\00\00")
	;; Amount of value to transfer.
	;; Represented by u64 (8 bytes long) in little endian.
	(data (i32.const 12) "\06\00\00\00\00\00\00\00")

	(data (i32.const 20) "\01\02\03\04")
)
"#;

	#[test]
	fn mycontract_transfer() {
		let mut mock_ext = MockExt::default();
		let _ = execute(
			CODE_TRANSFER,
			vec![],
			&mut mock_ext,
			&mut GasMeter::with_limit(50_000, 1),
		).unwrap();

		assert_eq!(
			&mock_ext.transfers,
			&[TransferEntry {
				to: 9,
				value: 6,
				data: vec![1, 2, 3, 4],
				gas_left: 49971,
			}]
		);
	}
	const CODE_GET_STORAGE: &str = r#"
(module
	(import "env" "ext_get_storage" (func $ext_get_storage (param i32) (result i32)))
	(import "env" "ext_scratch_size" (func $ext_scratch_size (result i32)))
	(import "env" "ext_scratch_read" (func $ext_scratch_read (param i32 i32 i32)))
	(import "env" "ext_return" (func $ext_return (param i32 i32)))
	(import "env" "memory" (memory 1 1))

	(func $assert (param i32)
		(block $ok
			(br_if $ok
				(get_local 0)
			)
			(unreachable)
		)
	)

	(func (export "call")
		(local $buf_size i32)


		;; Load a storage value into the scratch buf.
		(call $assert
			(i32.eq
				(call $ext_get_storage
					(i32.const 4)		;; The pointer to the storage key to fetch
				)

				;; Return value 0 means that the value is found and there were
				;; no errors.
				(i32.const 0)
			)
		)

		;; Find out the size of the scratch buffer
		(set_local $buf_size
			(call $ext_scratch_size)
		)

		;; Copy scratch buffer into this contract memory.
		(call $ext_scratch_read
			(i32.const 36)		;; The pointer where to store the scratch buffer contents,
								;; 36 = 4 + 32
			(i32.const 0)		;; Offset from the start of the scratch buffer.
			(get_local			;; Count of bytes to copy.
				$buf_size
			)
		)

		;; Return the contents of the buffer
		(call $ext_return
			(i32.const 36)
			(get_local $buf_size)
		)

		;; env:ext_return doesn't return, so this is effectively unreachable.
		(unreachable)
	)

	(func (export "deploy"))

	(data (i32.const 4)
		"\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11"
		"\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11\11"
	)
)
"#;

	#[test]
	fn myget_storage_puts_data_into_scratch_buf() {
		let mut mock_ext = MockExt::default();
		mock_ext
			.storage
			.insert([0x11; 32], [0x22; 32].to_vec());

		let output = execute(
			CODE_GET_STORAGE,
			vec![],
			&mut mock_ext,
			&mut GasMeter::with_limit(50_000, 1),
		).unwrap();
		println!("get storage puts data into scratch" );
		let wkv = mock_ext.write_key_value;
		for (k, v) in &wkv {
			println!("write k: {:?}, v: {:?}", k, v);
		}

		let rkv = mock_ext.read_key_value;
		for (k, v) in &rkv {
			println!("read k: {:?}, v: {:?}", k, v);
		}

		assert_eq!(output, ExecReturnValue { status: STATUS_SUCCESS, data: [0x22; 32].to_vec() });
	}