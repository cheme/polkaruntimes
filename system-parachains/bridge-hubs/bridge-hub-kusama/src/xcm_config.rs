// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

use super::{
	bridge_to_polkadot_config::{
		DeliveryRewardInBalance, RequiredStakeForStakeAndSlash, ToBridgeHubPolkadotHaulBlobExporter,
	},
	AccountId, AllPalletsWithSystem, Balances, ParachainInfo, ParachainSystem, PolkadotXcm,
	PriceForParentDelivery, Runtime, RuntimeCall, RuntimeEvent, RuntimeOrigin, WeightToFee,
	XcmpQueue,
};
use frame_support::{
	match_types, parameter_types,
	traits::{ConstU32, Contains, Equals, Everything, Nothing},
};
use frame_system::EnsureRoot;
use kusama_runtime_constants::system_parachain;
use pallet_xcm::XcmPassthrough;
use parachains_common::{
	impls::ToStakingPot,
	xcm_config::{ConcreteAssetFromSystem, RelayOrOtherSystemParachains},
};
use polkadot_parachain_primitives::primitives::Sibling;
use sp_runtime::traits::AccountIdConversion;
use system_parachains_constants::TREASURY_PALLET_ID;
use xcm::latest::prelude::*;
use xcm_builder::{
	AccountId32Aliases, AllowExplicitUnpaidExecutionFrom, AllowKnownQueryResponses,
	AllowSubscriptionsFrom, AllowTopLevelPaidExecutionFrom, CurrencyAdapter,
	DenyReserveTransferToRelayChain, DenyThenTry, DescribeAllTerminal, DescribeFamily,
	EnsureXcmOrigin, HashedDescription, IsConcrete, ParentAsSuperuser, ParentIsPreset,
	RelayChainAsNative, SiblingParachainAsNative, SiblingParachainConvertsVia,
	SignedAccountId32AsNative, SignedToAccountId32, SovereignSignedViaLocation, TakeWeightCredit,
	TrailingSetTopicAsId, UsingComponents, WeightInfoBounds, WithComputedOrigin, WithUniqueTopic,
	XcmFeesToAccount,
};
use xcm_executor::{traits::WithOriginFilter, XcmExecutor};

parameter_types! {
	pub const KsmRelayLocation: MultiLocation = MultiLocation::parent();
	pub const RelayNetwork: NetworkId = NetworkId::Kusama;
	pub RelayChainOrigin: RuntimeOrigin = cumulus_pallet_xcm::Origin::Relay.into();
	pub UniversalLocation: InteriorMultiLocation =
		X2(GlobalConsensus(RelayNetwork::get()), Parachain(ParachainInfo::parachain_id().into()));
	pub const MaxInstructions: u32 = 100;
	pub const MaxAssetsIntoHolding: u32 = 64;
	pub const GovernanceLocation: MultiLocation = MultiLocation::parent();
	pub const FellowshipLocation: MultiLocation = MultiLocation::parent();
	pub RelayTreasuryLocation: MultiLocation = (Parent, PalletInstance(kusama_runtime_constants::TREASURY_PALLET_ID)).into();
	pub TreasuryAccount: Option<AccountId> = Some(TREASURY_PALLET_ID.into_account_truncating());
}

/// Type for specifying how a `MultiLocation` can be converted into an `AccountId`. This is used
/// when determining ownership of accounts for asset transacting and when attempting to use XCM
/// `Transact` in order to determine the dispatch Origin.
pub type LocationToAccountId = (
	// The parent (Relay-chain) origin converts to the parent `AccountId`.
	ParentIsPreset<AccountId>,
	// Sibling parachain origins convert to AccountId via the `ParaId::into`.
	SiblingParachainConvertsVia<Sibling, AccountId>,
	// Straight up local `AccountId32` origins just alias directly to `AccountId`.
	AccountId32Aliases<RelayNetwork, AccountId>,
	// Foreign locations alias into accounts according to a hash of their standard description.
	HashedDescription<AccountId, DescribeFamily<DescribeAllTerminal>>,
);

/// Means for transacting the native currency on this chain.
pub type CurrencyTransactor = CurrencyAdapter<
	// Use this currency:
	Balances,
	// Use this currency when it is a fungible asset matching the given location or name:
	IsConcrete<KsmRelayLocation>,
	// Do a simple punn to convert an AccountId32 MultiLocation into a native chain account ID:
	LocationToAccountId,
	// Our chain's account ID type (we can't get away without mentioning it explicitly):
	AccountId,
	// We don't track any teleports of `Balances`.
	(),
>;

/// This is the type we use to convert an (incoming) XCM origin into a local `Origin` instance,
/// ready for dispatching a transaction with Xcm's `Transact`. There is an `OriginKind` which can
/// biases the kind of local `Origin` it will become.
pub type XcmOriginToTransactDispatchOrigin = (
	// Sovereign account converter; this attempts to derive an `AccountId` from the origin location
	// using `LocationToAccountId` and then turn that into the usual `Signed` origin. Useful for
	// foreign chains who want to have a local sovereign account on this chain which they control.
	SovereignSignedViaLocation<LocationToAccountId, RuntimeOrigin>,
	// Native converter for Relay-chain (Parent) location; will converts to a `Relay` origin when
	// recognized.
	RelayChainAsNative<RelayChainOrigin, RuntimeOrigin>,
	// Native converter for sibling Parachains; will convert to a `SiblingPara` origin when
	// recognized.
	SiblingParachainAsNative<cumulus_pallet_xcm::Origin, RuntimeOrigin>,
	// Superuser converter for the Relay-chain (Parent) location. This will allow it to issue a
	// transaction from the Root origin.
	ParentAsSuperuser<RuntimeOrigin>,
	// Native signed account converter; this just converts an `AccountId32` origin into a normal
	// `RuntimeOrigin::Signed` origin of the same 32-byte value.
	SignedAccountId32AsNative<RelayNetwork, RuntimeOrigin>,
	// Xcm origins can be represented natively under the Xcm pallet's Xcm origin.
	XcmPassthrough<RuntimeOrigin>,
);

match_types! {
	pub type ParentOrParentsPlurality: impl Contains<MultiLocation> = {
		MultiLocation { parents: 1, interior: Here } |
		MultiLocation { parents: 1, interior: X1(Plurality { .. }) }
	};
	pub type ParentOrSiblings: impl Contains<MultiLocation> = {
		MultiLocation { parents: 1, interior: Here } |
		MultiLocation { parents: 1, interior: X1(_) }
	};
}
/// A call filter for the XCM Transact instruction. This is a temporary measure until we properly
/// account for proof size weights.
///
/// Calls that are allowed through this filter must:
/// 1. Have a fixed weight;
/// 2. Cannot lead to another call being made;
/// 3. Have a defined proof size weight, e.g. no unbounded vecs in call parameters.
pub struct SafeCallFilter;
impl Contains<RuntimeCall> for SafeCallFilter {
	fn contains(call: &RuntimeCall) -> bool {
		#[cfg(feature = "runtime-benchmarks")]
		{
			if matches!(call, RuntimeCall::System(frame_system::Call::remark_with_event { .. })) {
				return true
			}
		}

		// Allow to change dedicated storage items (called by governance-like)
		match call {
			RuntimeCall::System(frame_system::Call::set_storage { items })
				if items.iter().all(|(k, _)| {
					k.eq(&DeliveryRewardInBalance::key()) ||
						k.eq(&RequiredStakeForStakeAndSlash::key())
				}) =>
				return true,
			_ => (),
		};

		matches!(
			call,
			RuntimeCall::PolkadotXcm(
				pallet_xcm::Call::force_xcm_version { .. } |
					pallet_xcm::Call::force_default_xcm_version { .. }
			) | RuntimeCall::System(
				frame_system::Call::set_heap_pages { .. } |
					frame_system::Call::set_code { .. } |
					frame_system::Call::set_code_without_checks { .. } |
					frame_system::Call::kill_prefix { .. },
			) | RuntimeCall::ParachainSystem(..) |
				RuntimeCall::Timestamp(..) |
				RuntimeCall::Balances(..) |
				RuntimeCall::CollatorSelection(
					pallet_collator_selection::Call::set_desired_candidates { .. } |
						pallet_collator_selection::Call::set_candidacy_bond { .. } |
						pallet_collator_selection::Call::register_as_candidate { .. } |
						pallet_collator_selection::Call::leave_intent { .. } |
						pallet_collator_selection::Call::set_invulnerables { .. } |
						pallet_collator_selection::Call::add_invulnerable { .. } |
						pallet_collator_selection::Call::remove_invulnerable { .. },
				) | RuntimeCall::Session(pallet_session::Call::purge_keys { .. }) |
				RuntimeCall::XcmpQueue(..) |
				RuntimeCall::DmpQueue(..) |
				RuntimeCall::BridgePolkadotGrandpa(pallet_bridge_grandpa::Call::<
					Runtime,
					crate::bridge_to_polkadot_config::BridgeGrandpaPolkadotInstance,
				>::initialize { .. }) |
				RuntimeCall::BridgePolkadotGrandpa(pallet_bridge_grandpa::Call::<
					Runtime,
					crate::bridge_to_polkadot_config::BridgeGrandpaPolkadotInstance,
				>::set_operating_mode { .. }) |
				RuntimeCall::BridgePolkadotParachains(pallet_bridge_parachains::Call::<
					Runtime,
					crate::bridge_to_polkadot_config::BridgeParachainPolkadotInstance,
				>::set_operating_mode { .. }) |
				RuntimeCall::BridgePolkadotMessages(pallet_bridge_messages::Call::<
					Runtime,
					crate::bridge_to_polkadot_config::WithBridgeHubPolkadotMessagesInstance,
				>::set_operating_mode { .. })
		)
	}
}

pub type Barrier = TrailingSetTopicAsId<
	DenyThenTry<
		DenyReserveTransferToRelayChain,
		(
			// Allow local users to buy weight credit.
			TakeWeightCredit,
			// Expected responses are OK.
			AllowKnownQueryResponses<PolkadotXcm>,
			WithComputedOrigin<
				(
					// If the message is one that immediately attempts to pay for execution, then
					// allow it.
					AllowTopLevelPaidExecutionFrom<Everything>,
					// Parent and its pluralities (i.e. governance bodies) and relay treasury get
					// free execution.
					AllowExplicitUnpaidExecutionFrom<(
						ParentOrParentsPlurality,
						Equals<RelayTreasuryLocation>,
					)>,
					// Subscriptions for version tracking are OK.
					AllowSubscriptionsFrom<ParentOrSiblings>,
				),
				UniversalLocation,
				ConstU32<8>,
			>,
		),
	>,
>;

match_types! {
	pub type SystemParachains: impl Contains<MultiLocation> = {
		MultiLocation {
			parents: 1,
			interior: X1(Parachain(
				system_parachain::ASSET_HUB_ID |
				system_parachain::ENCOINTER_ID
			)),
		}
	};
}

/// Locations that will not be charged fees in the executor,
/// either execution or delivery.
/// We only waive fees for system functions, which these locations represent.
pub type WaivedLocations =
	(RelayOrOtherSystemParachains<SystemParachains, Runtime>, Equals<RelayTreasuryLocation>);

/// Cases where a remote origin is accepted as trusted Teleporter for a given asset:
/// - KSM with the parent Relay Chain and sibling parachains.
pub type TrustedTeleporters = ConcreteAssetFromSystem<KsmRelayLocation>;

pub struct XcmConfig;
impl xcm_executor::Config for XcmConfig {
	type RuntimeCall = RuntimeCall;
	type XcmSender = XcmRouter;
	type AssetTransactor = CurrencyTransactor;
	type OriginConverter = XcmOriginToTransactDispatchOrigin;
	// BridgeHub does not recognize a reserve location for any asset. Users must teleport KSM
	// where allowed (e.g. with the Relay Chain).
	type IsReserve = ();
	type IsTeleporter = TrustedTeleporters;
	type UniversalLocation = UniversalLocation;
	type Barrier = Barrier;
	type Weigher = WeightInfoBounds<
		crate::weights::xcm::BridgeHubKusamaXcmWeight<RuntimeCall>,
		RuntimeCall,
		MaxInstructions,
	>;
	type Trader =
		UsingComponents<WeightToFee, KsmRelayLocation, AccountId, Balances, ToStakingPot<Runtime>>;
	type ResponseHandler = PolkadotXcm;
	type AssetTrap = PolkadotXcm;
	type AssetClaims = PolkadotXcm;
	type SubscriptionService = PolkadotXcm;
	type PalletInstancesInfo = AllPalletsWithSystem;
	type MaxAssetsIntoHolding = MaxAssetsIntoHolding;
	type AssetLocker = ();
	type AssetExchanger = ();
	type FeeManager = XcmFeesToAccount<Self, WaivedLocations, AccountId, TreasuryAccount>;
	type MessageExporter = ToBridgeHubPolkadotHaulBlobExporter;
	type UniversalAliases = Nothing;
	type CallDispatcher = WithOriginFilter<SafeCallFilter>;
	type SafeCallFilter = SafeCallFilter;
	type Aliasers = Nothing;
}

/// Converts a local signed origin into an XCM multilocation.
/// Forms the basis for local origins sending/executing XCMs.
pub type LocalOriginToLocation = SignedToAccountId32<RuntimeOrigin, AccountId, RelayNetwork>;

/// The means for routing XCM messages which are not for local execution into the right message
/// queues.
pub type XcmRouter = WithUniqueTopic<(
	// Two routers - use UMP to communicate with the relay chain:
	cumulus_primitives_utility::ParentAsUmp<ParachainSystem, PolkadotXcm, PriceForParentDelivery>,
	// ..and XCMP to communicate with the sibling chains.
	XcmpQueue,
)>;

#[cfg(feature = "runtime-benchmarks")]
parameter_types! {
	pub ReachableDest: Option<MultiLocation> = Some(Parent.into());
}

impl pallet_xcm::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	// We want to disallow users sending (arbitrary) XCMs from this chain.
	type SendXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, ()>;
	type XcmRouter = XcmRouter;
	// We support local origins dispatching XCM executions in principle...
	type ExecuteXcmOrigin = EnsureXcmOrigin<RuntimeOrigin, LocalOriginToLocation>;
	// ... but disallow generic XCM execution. As a result only teleports are allowed.
	type XcmExecuteFilter = Nothing;
	type XcmExecutor = XcmExecutor<XcmConfig>;
	type XcmTeleportFilter = Everything;
	type XcmReserveTransferFilter = Nothing; // This parachain is not meant as a reserve location.
	type Weigher = WeightInfoBounds<
		crate::weights::xcm::BridgeHubKusamaXcmWeight<RuntimeCall>,
		RuntimeCall,
		MaxInstructions,
	>;
	type UniversalLocation = UniversalLocation;
	type RuntimeOrigin = RuntimeOrigin;
	type RuntimeCall = RuntimeCall;
	const VERSION_DISCOVERY_QUEUE_SIZE: u32 = 100;
	type AdvertisedXcmVersion = pallet_xcm::CurrentXcmVersion;
	type Currency = Balances;
	type CurrencyMatcher = ();
	type TrustedLockers = ();
	type SovereignAccountOf = LocationToAccountId;
	type MaxLockers = ConstU32<8>;
	type WeightInfo = crate::weights::pallet_xcm::WeightInfo<Runtime>;
	#[cfg(feature = "runtime-benchmarks")]
	type ReachableDest = ReachableDest;
	type AdminOrigin = EnsureRoot<AccountId>;
	type MaxRemoteLockConsumers = ConstU32<0>;
	type RemoteLockConsumerIdentifier = ();
}

impl cumulus_pallet_xcm::Config for Runtime {
	type RuntimeEvent = RuntimeEvent;
	type XcmExecutor = XcmExecutor<XcmConfig>;
}
