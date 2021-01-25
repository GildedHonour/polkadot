// Copyright 2019-2020 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

//! Parathread and parachains leasing system. Allows para IDs to be claimed, the code and data to be initialized and
//! parachain slots (i.e. continuous scheduling) to be leased. Also allows for parachains and parathreads to be
//! swapped.
//!
//! This doesn't handle the mechanics of determining which para ID actually ends up with a parachain lease. This
//! must handled by a separately, through the trait interface that this pallet provides or the root dispatchables.

use sp_std::{prelude::*, mem::swap, convert::TryInto};
use sp_runtime::traits::{
	CheckedSub, StaticLookup, Zero, One, CheckedConversion, Hash, AccountIdConversion,
};
use parity_scale_codec::{Encode, Decode, Codec};
use frame_support::{
	decl_module, decl_storage, decl_event, decl_error, ensure, dispatch::DispatchResult,
	traits::{Currency, ReservableCurrency, WithdrawReasons, ExistenceRequirement, Get, Randomness},
	weights::{DispatchClass, Weight},
};
use primitives::v1::{
	Id as ParaId, ValidationCode, HeadData,
};
use frame_system::{ensure_signed, ensure_root};
use crate::slot_range::{SlotRange, SLOT_RANGE_COUNT};
use crate::traits::{Leaser, LeaseError, Registrar, SwapAux};

type BalanceOf<T> = <<T as Config>::Currency as Currency<<T as frame_system::Config>::AccountId>>::Balance;

/// The module's configuration trait.
pub trait Config: frame_system::Config {
	/// The overarching event type.
	type Event: From<Event<Self>> + Into<<Self as frame_system::Config>::Event>;

	/// The currency type used for bidding.
	type Currency: ReservableCurrency<Self::AccountId>;

	/// The amount required for a basic parathread deposit.
	type ParaDeposit: Get<BalanceOf<Self>>;

	/// The parachain registrar type.
	type Registrar: Registrar<Self::AccountId>;

	/// The number of blocks over which a single period lasts.
	type LeasePeriod: Get<Self::BlockNumber>;
}

type LeasePeriodOf<T> = <T as frame_system::Config>::BlockNumber;

// This module's storage items.
decl_storage! {
	trait Store for Module<T: Config> as Slots {
		/// Amount held on deposit for each para and the original depositor.
		///
		/// The given account ID is responsible for registering the code and initial head data, but may only do
		/// so if it isn't yet registered. (After that, it's up to governance to do so.)
		pub Paras: map hasher(twox_64_concat) ParaId => Option<(T::AccountId, BalanceOf<T>)>;

		/// Amounts held on deposit for each (possibly future) leased parachain.
		///
		/// The actual amount locked on its behalf by any account at any time is the maximum of the second values
		/// of the items in this list whose first value is the account.
		///
		/// The first item in the list is the amount locked for the current Lease Period. Following
		/// items are for the subsequent lease periods.
		///
		/// The default value (an empty list) implies that the parachain no longer exists (or never
		/// existed) as far as this module is concerned.
		///
		/// If a parachain doesn't exist *yet* but is scheduled to exist in the future, then it
		/// will be left-padded with one or more `None`s to denote the fact that nothing is held on
		/// deposit for the non-existent chain currently, but is held at some point in the future.
		///
		/// It is illegal for a `None` value to trail in the list.
		pub Leases: map hasher(twox_64_concat) ParaId => Vec<Option<(T::AccountId, BalanceOf<T>)>>;

		/// The ordered set of Para IDs that are full parachains currently.
		pub CurrentChains: Vec<ParaId>;
	}
}

decl_event!(
	pub enum Event<T> where
		AccountId = <T as frame_system::Config>::AccountId,
		LeasePeriod = LeasePeriodOf<T>,
		ParaId = ParaId,
		Balance = BalanceOf<T>,
	{
		/// A new [lease_period] is beginning.
		NewLeasePeriod(LeasePeriod),
		/// An existing parachain won the right to continue.
		/// First balance is the extra amount reseved. Second is the total amount reserved.
		/// \[parachain_id, leaser, period_begin, period_count, extra_reseved, total_amount\]
		Leased(ParaId, AccountId, LeasePeriod, LeasePeriod, Balance, Balance),
		/// A para ID value has been claimed.
		Claimed(ParaId),
	}
);

decl_error! {
	pub enum Error for Module<T: Config> {
		/// The lease period is in the past.
		LeasePeriodInPast,
		/// The origin for this call must be a parachain.
		NotParaOrigin,
		/// The parachain ID is not onboarding.
		ParaNotOnboarding,
		/// The origin for this call must be the origin who registered the parachain.
		InvalidOrigin,
		/// Parachain is already registered.
		AlreadyRegistered,
		/// The code must correspond to the hash.
		InvalidCode,
		/// Deployment data has not been set for this parachain.
		UnsetDeployData,
		/// The bid must overlap all intersecting ranges.
		NonIntersectingRange,
		/// Given code size is too large.
		CodeTooLarge,
		/// Given initial head data is too large.
		HeadDataTooLarge,
		/// The Id given is already in use.
		InUse,
		/// There was an error with the lease.
		LeaseError,
	}
}

decl_module! {
	pub struct Module<T: Config> for enum Call where origin: T::Origin {
		type Error = Error<T>;

		fn deposit_event() = default;

		fn on_initialize(n: T::BlockNumber) -> Weight {
			// If we're beginning a new lease period then handle that.
			let lease_period = T::LeasePeriod::get();
			if (n % lease_period).is_zero() {
				let lease_period_index: LeasePeriodOf<T> = (n / lease_period).into();
				Self::manage_lease_period_start(lease_period_index);
			}

			0
		}

		/// Just a hotwire into the `lease_out` call, in case Root wants to force some lease to happen
		/// independently of any other on-chain mechanism to use it.
		#[weight = 0]
		fn force_lease(origin,
			para: ParaId,
			leaser: T::AccountId,
			amount: BalanceOf<T>,
			period_begin: LeasePeriodOf<T>,
			period_count: LeasePeriodOf<T>,
		) -> DispatchResult {
			ensure_root(origin)?;
			Self::lease_out(para, &leaser, amount, period_begin, period_count)
				.map_err(|_| Error::<T>::LeaseError)?;
			Ok(())
		}
	}
}

impl<T: Config> Module<T> {
	/// A new lease period is beginning. We're at the start of the first block of it.
	///
	/// We need to on-board and off-board parachains as needed. We should also handle reducing/
	/// returning deposits.
	fn manage_lease_period_start(lease_period_index: LeasePeriodOf<T>) {
		Self::deposit_event(RawEvent::NewLeasePeriod(lease_period_index));

		let old_parachains = CurrentChains::get();

		// Figure out what chains need bringing on.
		let mut parachains = Vec::new();
		for (para, mut lease_periods) in Leases::<T>::iter() {
			if lease_periods.is_empty() { continue }
			// ^^ should never be empty since we would have deleted the entry otherwise.

			if lease_periods.len() == 1 {
				// Just one entry, which corresponds to the now-ended lease period.
				//
				// `para` is now just a parathread.
				//
				// Unreserve whatever is left.
				if let Some((who, value)) = &lease_periods[0] {
					T::Currency::unreserve(&who, *value);
				}

				// Remove the now-empty lease list.
				Leases::<T>::remove(para);
			} else {
				// The parachain entry has leased future periods.

				// We need to pop the first deposit entry, which corresponds to the now-
				// ended lease period.
				let maybe_ended_lease = lease_periods.remove(0);

				Leases::<T>::insert(para, &lease_periods);

				// If we *were* active in the last period and so have ended a lease...
				if let Some(ended_lease) = maybe_ended_lease {
					// Then we need to get the new amount that should continue to be held on
					// deposit for the parachain.
					let now_held = Self::deposit_held(para, &ended_lease.0);

					// If this is less than what we were holding for this leaser's now-ended lease, then
					// unreserve it.
					if let Some(rebate) = ended_lease.1.checked_sub(&now_held) {
						T::Currency::unreserve( &ended_lease.0, rebate);
					}
				}

				// If we have an active lease in the new period, then add to the current parachains
				if lease_periods[0].is_some() {
					parachains.push(para);
				}
			}
		}
		parachains.sort();
		CurrentChains::put(&parachains);

		for para in parachains.iter() {
			if old_parachains.binary_search(para).is_err() {
				// incoming.
				let _ = T::Registrar::make_parachain(*para);
			}
		}

		for para in old_parachains.iter() {
			if parachains.binary_search(para).is_err() {
				// outgoing.
				let _ = T::Registrar::make_parathread(*para);
			}
		}
	}
}

impl<T: Config> Leaser for Module<T> {
	type AccountId = T::AccountId;
	type LeasePeriod = T::BlockNumber;
	type Currency = T::Currency;

	fn lease_out(
		para: ParaId,
		leaser: &Self::AccountId,
		amount: <Self::Currency as Currency<Self::AccountId>>::Balance,
		period_begin: Self::LeasePeriod,
		period_count: Self::LeasePeriod,
	) -> Result<(), LeaseError> {
		// Finally, we update the deposit held so it is `amount` for the new lease period
		// indices that were won in the auction.
		let offset = period_begin
			.checked_sub(&Self::lease_period_index())
			.and_then(|x| x.checked_into::<usize>())
			.ok_or(LeaseError::AlreadyEnded)?;

		// offset is the amount into the `Deposits` items list that our lease begins. `period_count`
		// is the number of items that it lasts for.

		// The lease period index range (begin, end) that newly belongs to this parachain
		// ID. We need to ensure that it features in `Deposits` to prevent it from being
		// reaped too early (any managed parachain whose `Deposits` set runs low will be
		// removed).
		Leases::<T>::try_mutate(para, |d| {
			// Left-pad with `None`s as necessary.
			if d.len() < offset {
				d.resize_with(offset, || { None });
			}
			let period_count_usize = period_count.checked_into::<usize>()
				.ok_or(LeaseError::AlreadyEnded)?;
			// Then place the deposit values for as long as the chain should exist.
			for i in offset .. (offset + period_count_usize) {
				if d.len() > i {
					// Already exists but it's `None`. That means a later slot was already leased.
					// No problem.
					if d[i] == None {
						d[i] = Some((leaser.clone(), amount));
					} else {
						// The chain tried to lease the same period twice. This might be a griefing
						// attempt.
						//
						// We bail, not giving any lease and leave it for governance to sort out.
						return Err(LeaseError::AlreadyLeased);
					}
				} else if d.len() == i {
					// Doesn't exist. This is usual.
					d.push(Some((leaser.clone(), amount)));
				} else {
					// earlier resize means it must be >= i; qed
					// defensive code though since we really don't want to panic here.
				}
			}

			// Figure out whether we already have some funds of `leaser` held in reserve for `para_id`.
			//  If so, then we can deduct those from the amount that we need to reserve.
			let maybe_additional = amount.checked_sub(&Self::deposit_held(para, &leaser));
			if let Some(ref additional) = maybe_additional {
				T::Currency::reserve(&leaser, *additional)
					.map_err(|_| LeaseError::ReserveFailed)?;
			}

			let reserved = maybe_additional.unwrap_or_default();
			Self::deposit_event(
				RawEvent::Leased(para, leaser.clone(), period_begin, period_count, reserved, amount)
			);

			Ok(())
		})
	}

	fn deposit_held(para: ParaId, leaser: &Self::AccountId) -> <Self::Currency as Currency<Self::AccountId>>::Balance {
		Leases::<T>::get(para)
			.into_iter()
			.map(|lease| {
				match lease {
					Some((who, amount)) => {
						if &who == leaser { amount } else { Zero::zero() }
					},
					None => Zero::zero(),
				}
			})
			.max()
			.unwrap_or_else(Zero::zero)
	}

	fn lease_period() -> Self::LeasePeriod {
		T::LeasePeriod::get()
	}

	fn lease_period_index() -> Self::LeasePeriod {
		(<frame_system::Module<T>>::block_number() / T::LeasePeriod::get()).into()
	}
}

/// tests for this module
#[cfg(test)]
mod tests {
	use super::*;
	use std::{collections::HashMap, cell::RefCell};

	use sp_core::H256;
	use sp_runtime::traits::{BlakeTwo256, Hash, IdentityLookup};
	use frame_support::{
		impl_outer_origin, parameter_types, assert_ok, assert_noop,
		traits::{OnInitialize, OnFinalize}
	};
	use pallet_balances;
	use primitives::v1::{BlockNumber, Header, Id as ParaId};

	impl_outer_origin! {
		pub enum Origin for Test {}
	}

	// For testing the module, we construct most of a mock runtime. This means
	// first constructing a configuration type (`Test`) which `impl`s each of the
	// configuration traits of modules we want to use.
	#[derive(Clone, Eq, PartialEq)]
	pub struct Test;
	parameter_types! {
		pub const BlockHashCount: u32 = 250;
	}
	impl frame_system::Config for Test {
		type BaseCallFilter = ();
		type BlockWeights = ();
		type BlockLength = ();
		type DbWeight = ();
		type Origin = Origin;
		type Call = ();
		type Index = u64;
		type BlockNumber = BlockNumber;
		type Hash = H256;
		type Hashing = BlakeTwo256;
		type AccountId = u64;
		type Lookup = IdentityLookup<Self::AccountId>;
		type Header = Header;
		type Event = ();
		type BlockHashCount = BlockHashCount;
		type Version = ();
		type PalletInfo = ();
		type AccountData = pallet_balances::AccountData<u64>;
		type OnNewAccount = ();
		type OnKilledAccount = ();
		type SystemWeightInfo = ();
		type SS58Prefix = ();
	}

	parameter_types! {
		pub const ExistentialDeposit: u64 = 1;
	}

	impl pallet_balances::Config for Test {
		type Balance = u64;
		type Event = ();
		type DustRemoval = ();
		type ExistentialDeposit = ExistentialDeposit;
		type AccountStore = System;
		type MaxLocks = ();
		type WeightInfo = ();
	}

	thread_local! {
		pub static PARACHAIN_COUNT: RefCell<u32> = RefCell::new(0);
		pub static PARACHAINS:
			RefCell<HashMap<u32, (ValidationCode, HeadData)>> = RefCell::new(HashMap::new());
	}

	const MAX_CODE_SIZE: u32 = 100;
	const MAX_HEAD_DATA_SIZE: u32 = 10;

	pub struct TestParachains;
	impl Registrar<u64> for TestParachains {
		fn head_data_size_allowed(head_data_size: u32) -> bool {
			head_data_size <= MAX_HEAD_DATA_SIZE
		}

		fn code_size_allowed(code_size: u32) -> bool {
			code_size <= MAX_CODE_SIZE
		}

		fn register_para(
			id: ParaId,
			_parachain: bool,
			code: ValidationCode,
			initial_head_data: HeadData,
		) -> DispatchResult {
			PARACHAINS.with(|p| {
				if p.borrow().contains_key(&id.into()) {
					panic!("ID already exists")
				}
				p.borrow_mut().insert(id.into(), (code, initial_head_data));
				Ok(())
			})
		}
		fn deregister_para(id: ParaId) -> DispatchResult {
			PARACHAINS.with(|p| {
				if !p.borrow().contains_key(&id.into()) {
					panic!("ID doesn't exist")
				}
				p.borrow_mut().remove(&id.into());
				Ok(())
			})
		}
	}

	fn reset_count() {
		PARACHAIN_COUNT.with(|p| *p.borrow_mut() = 0);
	}

	fn with_parachains<T>(f: impl FnOnce(&HashMap<u32, (ValidationCode, HeadData)>) -> T) -> T {
		PARACHAINS.with(|p| f(&*p.borrow()))
	}

	parameter_types!{
		pub const LeasePeriod: BlockNumber = 10;
		pub const EndingPeriod: BlockNumber = 3;
	}

	impl Config for Test {
		type Event = ();
		type Currency = Balances;
		type Parachains = TestParachains;
		type LeasePeriod = LeasePeriod;
		type EndingPeriod = EndingPeriod;
		type Randomness = RandomnessCollectiveFlip;
	}

	type System = frame_system::Module<Test>;
	type Balances = pallet_balances::Module<Test>;
	type Slots = Module<Test>;
	type RandomnessCollectiveFlip = pallet_randomness_collective_flip::Module<Test>;

	// This function basically just builds a genesis storage key/value store according to
	// our desired mock up.
	fn new_test_ext() -> sp_io::TestExternalities {
		let mut t = frame_system::GenesisConfig::default().build_storage::<Test>().unwrap();
		pallet_balances::GenesisConfig::<Test>{
			balances: vec![(1, 10), (2, 20), (3, 30), (4, 40), (5, 50), (6, 60)],
		}.assimilate_storage(&mut t).unwrap();
		t.into()
	}

	fn run_to_block(n: BlockNumber) {
		while System::block_number() < n {
			Slots::on_finalize(System::block_number());
			Balances::on_finalize(System::block_number());
			System::on_finalize(System::block_number());
			System::set_block_number(System::block_number() + 1);
			System::on_initialize(System::block_number());
			Balances::on_initialize(System::block_number());
			Slots::on_initialize(System::block_number());
		}
	}

	#[test]
	fn basic_setup_works() {
		new_test_ext().execute_with(|| {
			assert_eq!(Slots::auction_counter(), 0);
			assert_eq!(Slots::deposit_held(&0u32.into()), 0);
			assert_eq!(Slots::is_in_progress(), false);
			assert_eq!(Slots::is_ending(System::block_number()), None);

			run_to_block(10);

			assert_eq!(Slots::auction_counter(), 0);
			assert_eq!(Slots::deposit_held(&0u32.into()), 0);
			assert_eq!(Slots::is_in_progress(), false);
			assert_eq!(Slots::is_ending(System::block_number()), None);
		});
	}

	#[test]
	fn can_start_auction() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));

			assert_eq!(Slots::auction_counter(), 1);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), None);
		});
	}

	#[test]
	fn auction_proceeds_correctly() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));

			assert_eq!(Slots::auction_counter(), 1);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), None);

			run_to_block(2);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), None);

			run_to_block(3);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), None);

			run_to_block(4);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), None);

			run_to_block(5);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), None);

			run_to_block(6);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), Some(0));

			run_to_block(7);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), Some(1));

			run_to_block(8);
			assert_eq!(Slots::is_in_progress(), true);
			assert_eq!(Slots::is_ending(System::block_number()), Some(2));

			run_to_block(9);
			assert_eq!(Slots::is_in_progress(), false);
			assert_eq!(Slots::is_ending(System::block_number()), None);
		});
	}

	#[test]
	fn can_win_auction() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 4, 1));
			assert_eq!(Balances::reserved_balance(1), 1);
			assert_eq!(Balances::free_balance(1), 9);

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);
			assert_eq!(Slots::onboarding(ParaId::from(0)),
				Some((1, IncomingParachain::Unset(NewBidder { who: 1, sub: 0 })))
			);
			assert_eq!(Slots::deposit_held(&0.into()), 1);
			assert_eq!(Balances::reserved_balance(1), 0);
			assert_eq!(Balances::free_balance(1), 9);
		});
	}

	#[test]
	fn offboarding_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 4, 1));
			assert_eq!(Balances::free_balance(1), 9);

			run_to_block(9);
			assert_eq!(Slots::deposit_held(&0.into()), 1);
			assert_eq!(Slots::deposits(ParaId::from(0))[0], 0);

			run_to_block(50);
			assert_eq!(Slots::deposit_held(&0.into()), 0);
			assert_eq!(Balances::free_balance(1), 10);
		});
	}

	#[test]
	fn set_offboarding_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 4, 1));

			run_to_block(9);
			assert_eq!(Slots::deposit_held(&0.into()), 1);
			assert_eq!(Slots::deposits(ParaId::from(0))[0], 0);

			run_to_block(49);
			assert_eq!(Slots::deposit_held(&0.into()), 1);
			assert_ok!(Slots::set_offboarding(Origin::signed(ParaId::from(0).into_account()), 10));

			run_to_block(50);
			assert_eq!(Slots::deposit_held(&0.into()), 0);
			assert_eq!(Balances::free_balance(10), 1);
		});
	}

	#[test]
	fn onboarding_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 4, 1));

			run_to_block(9);
			let h = BlakeTwo256::hash(&[42u8][..]);
			assert_ok!(Slots::fix_deploy_data(Origin::signed(1), 0, 0.into(), h, 1, vec![69].into()));
			assert_ok!(Slots::elaborate_deploy_data(Origin::signed(0), 0.into(), vec![42].into()));

			run_to_block(10);
			with_parachains(|p| {
				assert_eq!(p.len(), 1);
				assert_eq!(p[&0], (vec![42].into(), vec![69].into()));
			});
		});
	}

	#[test]
	fn late_onboarding_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 4, 1));

			run_to_block(10);
			with_parachains(|p| {
				assert_eq!(p.len(), 0);
			});

			run_to_block(11);
			let h = BlakeTwo256::hash(&[42u8][..]);
			assert_ok!(Slots::fix_deploy_data(Origin::signed(1), 0, 0.into(), h, 1, vec![69].into()));
			assert_ok!(Slots::elaborate_deploy_data(Origin::signed(0), 0.into(), vec![42].into()));
			with_parachains(|p| {
				assert_eq!(p.len(), 1);
				assert_eq!(p[&0], (vec![42].into(), vec![69].into()));
			});
		});
	}

	#[test]
	fn under_bidding_works() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 4, 5));
			assert_ok!(Slots::bid(Origin::signed(2), 0, 1, 1, 4, 1));
			assert_eq!(Balances::reserved_balance(2), 0);
			assert_eq!(Balances::free_balance(2), 20);
			assert_eq!(
				Slots::winning(0).unwrap()[SlotRange::ZeroThree as u8 as usize],
				Some((Bidder::New(NewBidder{who: 1, sub: 0}), 5))
			);
		});
	}

	#[test]
	fn should_choose_best_combination() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 1, 1));
			assert_ok!(Slots::bid(Origin::signed(2), 0, 1, 2, 3, 1));
			assert_ok!(Slots::bid(Origin::signed(3), 0, 1, 4, 4, 2));
			assert_ok!(Slots::bid(Origin::signed(1), 1, 1, 1, 4, 1));
			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(0)),
				Some((1, IncomingParachain::Unset(NewBidder { who: 1, sub: 0 })))
			);
			assert_eq!(Slots::onboard_queue(2), vec![1.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(1)),
				Some((2, IncomingParachain::Unset(NewBidder { who: 2, sub: 0 })))
			);
			assert_eq!(Slots::onboard_queue(4), vec![2.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(2)),
				Some((4, IncomingParachain::Unset(NewBidder { who: 3, sub: 0 })))
			);
		});
	}

	#[test]
	fn independent_bids_should_fail() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 1, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 2, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 2, 4, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 2, 2, 1));
			assert_noop!(
				Slots::bid(Origin::signed(1), 0, 1, 3, 3, 1),
				Error::<Test>::NonIntersectingRange
			);
		});
	}

	#[test]
	fn multiple_onboards_offboards_should_work() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 1, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 1, 1));
			assert_ok!(Slots::bid(Origin::signed(2), 0, 1, 2, 3, 1));
			assert_ok!(Slots::bid(Origin::signed(3), 0, 1, 4, 4, 1));

			run_to_block(5);
			assert_ok!(Slots::new_auction(Origin::root(), 1, 1));
			assert_ok!(Slots::bid(Origin::signed(4), 1, 2, 1, 2, 1));
			assert_ok!(Slots::bid(Origin::signed(5), 1, 2, 3, 4, 1));

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into(), 3.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(0)),
				Some((1, IncomingParachain::Unset(NewBidder { who: 1, sub: 0 })))
			);
			assert_eq!(
				Slots::onboarding(ParaId::from(3)),
				Some((1, IncomingParachain::Unset(NewBidder { who: 4, sub: 1 })))
			);
			assert_eq!(Slots::onboard_queue(2), vec![1.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(1)),
				Some((2, IncomingParachain::Unset(NewBidder { who: 2, sub: 0 })))
			);
			assert_eq!(Slots::onboard_queue(3), vec![4.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(4)),
				Some((3, IncomingParachain::Unset(NewBidder { who: 5, sub: 1 })))
			);
			assert_eq!(Slots::onboard_queue(4), vec![2.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(2)),
				Some((4, IncomingParachain::Unset(NewBidder { who: 3, sub: 0 })))
			);

			for &(para, sub, acc) in &[(0, 0, 1), (1, 0, 2), (2, 0, 3), (3, 1, 4), (4, 1, 5)] {
				let h = BlakeTwo256::hash(&[acc][..]);
				assert_ok!(Slots::fix_deploy_data(Origin::signed(acc as _), sub, para.into(), h, 1, vec![acc].into()));
				assert_ok!(Slots::elaborate_deploy_data(Origin::signed(0), para.into(), vec![acc].into()));
			}

			run_to_block(10);
			with_parachains(|p| {
				assert_eq!(p.len(), 2);
				assert_eq!(p[&0], (vec![1].into(), vec![1].into()));
				assert_eq!(p[&3], (vec![4].into(), vec![4].into()));
			});
			run_to_block(20);
			with_parachains(|p| {
				assert_eq!(p.len(), 2);
				assert_eq!(p[&1], (vec![2].into(), vec![2].into()));
				assert_eq!(p[&3], (vec![4].into(), vec![4].into()));
			});
			run_to_block(30);
			with_parachains(|p| {
				assert_eq!(p.len(), 2);
				assert_eq!(p[&1], (vec![2].into(), vec![2].into()));
				assert_eq!(p[&4], (vec![5].into(), vec![5].into()));
			});
			run_to_block(40);
			with_parachains(|p| {
				assert_eq!(p.len(), 2);
				assert_eq!(p[&2], (vec![3].into(), vec![3].into()));
				assert_eq!(p[&4], (vec![5].into(), vec![5].into()));
			});
			run_to_block(50);
			with_parachains(|p| {
				assert_eq!(p.len(), 0);
			});
		});
	}

	#[test]
	fn extensions_should_work() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 1, 1));

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);

			run_to_block(10);
			let h = BlakeTwo256::hash(&[1u8][..]);
			assert_ok!(Slots::fix_deploy_data(Origin::signed(1), 0, 0.into(), h, 1, vec![1].into()));
			assert_ok!(Slots::elaborate_deploy_data(Origin::signed(0), 0.into(), vec![1].into()));

			assert_ok!(Slots::new_auction(Origin::root(), 5, 2));
			assert_ok!(Slots::bid_renew(Origin::signed(ParaId::from(0).into_account()), 2, 2, 2, 1));

			with_parachains(|p| {
				assert_eq!(p.len(), 1);
				assert_eq!(p[&0], (vec![1].into(), vec![1].into()));
			});

			run_to_block(20);
			with_parachains(|p| {
				assert_eq!(p.len(), 1);
				assert_eq!(p[&0], (vec![1].into(), vec![1].into()));
			});
			assert_ok!(Slots::new_auction(Origin::root(), 5, 2));
			assert_ok!(Balances::transfer(Origin::signed(1), ParaId::from(0).into_account(), 1));
			assert_ok!(Slots::bid_renew(Origin::signed(ParaId::from(0).into_account()), 3, 3, 3, 2));

			run_to_block(30);
			with_parachains(|p| {
				assert_eq!(p.len(), 1);
				assert_eq!(p[&0], (vec![1].into(), vec![1].into()));
			});

			run_to_block(40);
			with_parachains(|p| {
				assert_eq!(p.len(), 0);
			});
		});
	}

	#[test]
	fn renewal_with_lower_value_should_work() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 1, 5));

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);

			run_to_block(10);
			let h = BlakeTwo256::hash(&[1u8][..]);
			assert_ok!(Slots::fix_deploy_data(Origin::signed(1), 0, 0.into(), h, 1, vec![1].into()));
			assert_ok!(Slots::elaborate_deploy_data(Origin::signed(0), 0.into(), vec![1].into()));

			assert_ok!(Slots::new_auction(Origin::root(), 5, 2));
			assert_ok!(Slots::bid_renew(Origin::signed(ParaId::from(0).into_account()), 2, 2, 2, 3));

			run_to_block(20);
			assert_eq!(Balances::free_balance(&ParaId::from(0u32).into_account()), 2);

			assert_ok!(Slots::new_auction(Origin::root(), 5, 2));
			assert_ok!(Slots::bid_renew(Origin::signed(ParaId::from(0).into_account()), 3, 3, 3, 4));

			run_to_block(30);
			assert_eq!(Balances::free_balance(&ParaId::from(0u32).into_account()), 1);
		});
	}

	#[test]
	fn can_win_incomplete_auction() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 4, 4, 5));

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![]);
			assert_eq!(Slots::onboard_queue(2), vec![]);
			assert_eq!(Slots::onboard_queue(3), vec![]);
			assert_eq!(Slots::onboard_queue(4), vec![0.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(0)),
				Some((4, IncomingParachain::Unset(NewBidder { who: 1, sub: 0 })))
			);
			assert_eq!(Slots::deposit_held(&0.into()), 5);
		});
	}

	#[test]
	fn multiple_bids_work_pre_ending() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));

			for i in 1..6u64 {
				run_to_block(i as _);
				assert_ok!(Slots::bid(Origin::signed(i), 0, 1, 1, 4, i));
				for j in 1..6 {
					assert_eq!(Balances::reserved_balance(j), if j == i { j } else { 0 });
					assert_eq!(Balances::free_balance(j), if j == i { j * 9 } else { j * 10 });
				}
			}

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(0)),
				Some((1, IncomingParachain::Unset(NewBidder { who: 5, sub: 0 })))
			);
			assert_eq!(Slots::deposit_held(&0.into()), 5);
			assert_eq!(Balances::reserved_balance(5), 0);
			assert_eq!(Balances::free_balance(5), 45);
		});
	}

	#[test]
	fn multiple_bids_work_post_ending() {
		new_test_ext().execute_with(|| {
			run_to_block(1);

			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));

			for i in 1..6u64 {
				run_to_block((i + 3) as _);
				assert_ok!(Slots::bid(Origin::signed(i), 0, 1, 1, 4, i));
				for j in 1..6 {
					assert_eq!(Balances::reserved_balance(j), if j == i { j } else { 0 });
					assert_eq!(Balances::free_balance(j), if j == i { j * 9 } else { j * 10 });
				}
			}

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);
			assert_eq!(
				Slots::onboarding(ParaId::from(0)),
				Some((1, IncomingParachain::Unset(NewBidder { who: 3, sub: 0 })))
			);
			assert_eq!(Slots::deposit_held(&0.into()), 3);
			assert_eq!(Balances::reserved_balance(3), 0);
			assert_eq!(Balances::free_balance(3), 27);
		});
	}

	#[test]
	fn incomplete_calculate_winners_works() {
		let winning = [
			None,
			None,
			None,
			None,
			None,
			None,
			None,
			None,
			None,
			Some((Bidder::New(NewBidder{who: 1, sub: 0}), 1)),
		];
		let winners = vec![
			(Some(NewBidder{who: 1, sub: 0}), 0.into(), 1, SlotRange::ThreeThree)
		];

		assert_eq!(Slots::calculate_winners(winning, TestParachains::new_id), winners);
	}

	#[test]
	fn first_incomplete_calculate_winners_works() {
		let winning = [
			Some((Bidder::New(NewBidder{who: 1, sub: 0}), 1)),
			None,
			None,
			None,
			None,
			None,
			None,
			None,
			None,
			None,
		];
		let winners = vec![
			(Some(NewBidder{who: 1, sub: 0}), 0.into(), 1, SlotRange::ZeroZero)
		];

		assert_eq!(Slots::calculate_winners(winning, TestParachains::new_id), winners);
	}

	#[test]
	fn calculate_winners_works() {
		let mut winning = [
			/*0..0*/
			Some((Bidder::New(NewBidder{who: 2, sub: 0}), 2)),
			/*0..1*/
			None,
			/*0..2*/
			None,
			/*0..3*/
			Some((Bidder::New(NewBidder{who: 1, sub: 0}), 1)),
			/*1..1*/
			Some((Bidder::New(NewBidder{who: 3, sub: 0}), 1)),
			/*1..2*/
			None,
			/*1..3*/
			None,
			/*2..2*/
			//Some((Bidder::New(NewBidder{who: 4, sub: 0}), 1)),
			Some((Bidder::New(NewBidder{who: 1, sub: 0}), 53)),
			/*2..3*/
			None,
			/*3..3*/
			Some((Bidder::New(NewBidder{who: 5, sub: 0}), 1)),
		];
		let winners = vec![
			(Some(NewBidder{who: 2,sub: 0}), 0.into(), 2, SlotRange::ZeroZero),
			(Some(NewBidder{who: 3,sub: 0}), 1.into(), 1, SlotRange::OneOne),
			(Some(NewBidder{who: 1,sub: 0}), 2.into(), 53, SlotRange::TwoTwo),
			(Some(NewBidder{who: 5,sub: 0}), 3.into(), 1, SlotRange::ThreeThree)
		];

		assert_eq!(Slots::calculate_winners(winning.clone(), TestParachains::new_id), winners);

		reset_count();
		winning[SlotRange::ZeroThree as u8 as usize] = Some((Bidder::New(NewBidder{who: 1, sub: 0}), 2));
		let winners = vec![
			(Some(NewBidder{who: 2,sub: 0}), 0.into(), 2, SlotRange::ZeroZero),
			(Some(NewBidder{who: 3,sub: 0}), 1.into(), 1, SlotRange::OneOne),
			(Some(NewBidder{who: 1,sub: 0}), 2.into(), 53, SlotRange::TwoTwo),
			(Some(NewBidder{who: 5,sub: 0}), 3.into(), 1, SlotRange::ThreeThree)
		];
		assert_eq!(Slots::calculate_winners(winning.clone(), TestParachains::new_id), winners);

		reset_count();
		winning[SlotRange::ZeroOne as u8 as usize] = Some((Bidder::New(NewBidder{who: 4, sub: 0}), 3));
		let winners = vec![
			(Some(NewBidder{who: 4,sub: 0}), 0.into(), 3, SlotRange::ZeroOne),
			(Some(NewBidder{who: 1,sub: 0}), 1.into(), 53, SlotRange::TwoTwo),
			(Some(NewBidder{who: 5,sub: 0}), 2.into(), 1, SlotRange::ThreeThree)
		];
		assert_eq!(Slots::calculate_winners(winning.clone(), TestParachains::new_id), winners);
	}

	#[test]
	fn deploy_code_too_large() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 1, 5));

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);

			run_to_block(10);

			let code = vec![0u8; (MAX_CODE_SIZE + 1) as _];
			let h = BlakeTwo256::hash(&code[..]);
			assert_eq!(
				Slots::fix_deploy_data(
					Origin::signed(1), 0, 0.into(), h, code.len() as _, vec![1].into(),
				),
				Err(Error::<Test>::CodeTooLarge.into()),
			);
		});
	}

	#[test]
	fn deploy_maximum_ok() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 1, 5));

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);

			run_to_block(10);

			let code = vec![0u8; MAX_CODE_SIZE as _];
			let head_data = vec![1u8; MAX_HEAD_DATA_SIZE as _].into();
			let h = BlakeTwo256::hash(&code[..]);
			assert_ok!(Slots::fix_deploy_data(
				Origin::signed(1), 0, 0.into(), h, code.len() as _, head_data,
			));
		});
	}

	#[test]
	fn deploy_head_data_too_large() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 1, 5));

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);

			run_to_block(10);

			let code = vec![0u8; MAX_CODE_SIZE as _];
			let head_data = vec![1u8; (MAX_HEAD_DATA_SIZE + 1) as _].into();
			let h = BlakeTwo256::hash(&code[..]);
			assert_eq!(
				Slots::fix_deploy_data(
					Origin::signed(1), 0, 0.into(), h, code.len() as _, head_data,
				),
				Err(Error::<Test>::HeadDataTooLarge.into()),
			);
		});
	}

	#[test]
	fn code_size_must_be_correct() {
		new_test_ext().execute_with(|| {
			run_to_block(1);
			assert_ok!(Slots::new_auction(Origin::root(), 5, 1));
			assert_ok!(Slots::bid(Origin::signed(1), 0, 1, 1, 1, 5));

			run_to_block(9);
			assert_eq!(Slots::onboard_queue(1), vec![0.into()]);

			run_to_block(10);

			let code = vec![0u8; MAX_CODE_SIZE as _];
			let head_data = vec![1u8; MAX_HEAD_DATA_SIZE as _].into();
			let h = BlakeTwo256::hash(&code[..]);
			assert_ok!(Slots::fix_deploy_data(
				Origin::signed(1), 0, 0.into(), h, (code.len() - 1) as _, head_data,
			));
			assert!(Slots::elaborate_deploy_data(Origin::signed(0), 0.into(), code.into()).is_err());
		});
	}
}
