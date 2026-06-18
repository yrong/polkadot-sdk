// Copyright (C) Parity Technologies (UK) Ltd.
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

//! A module that is responsible for migration of storage for the configuration pallet.
//! v13 -> v14:
//! - Added `provides_window_size` field (speculative messaging provides window).

use crate::configuration::{self, Config, Pallet};
use alloc::vec::Vec;
use frame_support::{
	migrations::VersionedMigration,
	pallet_prelude::*,
	traits::{Defensive, UncheckedOnRuntimeUpgrade},
};
use frame_system::pallet_prelude::BlockNumberFor;
use polkadot_core_primitives::Balance;
use polkadot_primitives::{
	vstaging::SchedulerParams, ApprovalVotingParams, AsyncBackingParams, ExecutorParams,
	NodeFeatures,
};
use sp_staking::SessionIndex;

type V14HostConfiguration<BlockNumber> = configuration::HostConfiguration<BlockNumber>;

/// The v13 `HostConfiguration`, before the `provides_window_size` field was added.
#[derive(Encode, Decode, Debug, Clone)]
pub struct V13HostConfiguration<BlockNumber> {
	pub max_code_size: u32,
	pub max_head_data_size: u32,
	pub max_upward_queue_count: u32,
	pub max_upward_queue_size: u32,
	pub max_upward_message_size: u32,
	pub max_upward_message_num_per_candidate: u32,
	pub hrmp_max_message_num_per_candidate: u32,
	pub validation_upgrade_cooldown: BlockNumber,
	pub validation_upgrade_delay: BlockNumber,
	pub async_backing_params: AsyncBackingParams,
	pub max_pov_size: u32,
	pub max_downward_message_size: u32,
	pub hrmp_max_parachain_outbound_channels: u32,
	pub hrmp_sender_deposit: Balance,
	pub hrmp_recipient_deposit: Balance,
	pub hrmp_channel_max_capacity: u32,
	pub hrmp_channel_max_total_size: u32,
	pub hrmp_max_parachain_inbound_channels: u32,
	pub hrmp_channel_max_message_size: u32,
	pub executor_params: ExecutorParams,
	pub code_retention_period: BlockNumber,
	pub max_validators: Option<u32>,
	pub dispute_period: SessionIndex,
	pub dispute_post_conclusion_acceptance_period: BlockNumber,
	pub no_show_slots: u32,
	pub n_delay_tranches: u32,
	pub zeroth_delay_tranche_width: u32,
	pub needed_approvals: u32,
	pub relay_vrf_modulo_samples: u32,
	pub pvf_voting_ttl: SessionIndex,
	pub minimum_validation_upgrade_delay: BlockNumber,
	pub minimum_backing_votes: u32,
	pub node_features: NodeFeatures,
	pub approval_voting_params: ApprovalVotingParams,
	pub scheduler_params: SchedulerParams<BlockNumber>,
	pub max_relay_parent_session_age: u32,
}

mod v13 {
	use super::*;

	#[frame_support::storage_alias]
	pub(crate) type ActiveConfig<T: Config> =
		StorageValue<Pallet<T>, V13HostConfiguration<BlockNumberFor<T>>, OptionQuery>;

	#[frame_support::storage_alias]
	pub(crate) type PendingConfigs<T: Config> = StorageValue<
		Pallet<T>,
		Vec<(SessionIndex, V13HostConfiguration<BlockNumberFor<T>>)>,
		OptionQuery,
	>;
}

mod v14 {
	use super::*;

	#[frame_support::storage_alias]
	pub(crate) type ActiveConfig<T: Config> =
		StorageValue<Pallet<T>, V14HostConfiguration<BlockNumberFor<T>>, OptionQuery>;

	#[frame_support::storage_alias]
	pub(crate) type PendingConfigs<T: Config> = StorageValue<
		Pallet<T>,
		Vec<(SessionIndex, V14HostConfiguration<BlockNumberFor<T>>)>,
		OptionQuery,
	>;
}

pub type MigrateToV14<T> = VersionedMigration<
	13,
	14,
	UncheckedMigrateToV14<T>,
	Pallet<T>,
	<T as frame_system::Config>::DbWeight,
>;

pub struct UncheckedMigrateToV14<T>(core::marker::PhantomData<T>);

impl<T: Config> UncheckedOnRuntimeUpgrade for UncheckedMigrateToV14<T> {
	#[cfg(feature = "try-runtime")]
	fn pre_upgrade() -> Result<Vec<u8>, sp_runtime::TryRuntimeError> {
		log::trace!(target: crate::configuration::LOG_TARGET, "Running pre_upgrade() for HostConfiguration MigrateToV14");
		Ok(Vec::new())
	}

	fn on_runtime_upgrade() -> Weight {
		log::info!(target: configuration::LOG_TARGET, "HostConfiguration MigrateToV14 started");
		let weight_consumed = migrate_to_v14::<T>();

		log::info!(target: configuration::LOG_TARGET, "HostConfiguration MigrateToV14 executed successfully");

		weight_consumed
	}

	#[cfg(feature = "try-runtime")]
	fn post_upgrade(_state: Vec<u8>) -> Result<(), sp_runtime::TryRuntimeError> {
		log::trace!(target: crate::configuration::LOG_TARGET, "Running post_upgrade() for HostConfiguration MigrateToV14");
		ensure!(
			StorageVersion::get::<Pallet<T>>() >= 14,
			"Storage version should be >= 14 after the migration"
		);

		Ok(())
	}
}

fn migrate_to_v14<T: Config>() -> Weight {
	// Unusual formatting is justified:
	// - make it easier to verify that fields assign what they supposed to assign.
	// - this code is transient and will be removed after all migrations are done.
	#[rustfmt::skip]
		let translate =
		|pre: V13HostConfiguration<BlockNumberFor<T>>| ->
		V14HostConfiguration<BlockNumberFor<T>>
			{
				V14HostConfiguration {
					max_code_size                            : pre.max_code_size,
					max_head_data_size                       : pre.max_head_data_size,
					max_upward_queue_count                   : pre.max_upward_queue_count,
					max_upward_queue_size                    : pre.max_upward_queue_size,
					max_upward_message_size                  : pre.max_upward_message_size,
					max_upward_message_num_per_candidate     : pre.max_upward_message_num_per_candidate,
					hrmp_max_message_num_per_candidate       : pre.hrmp_max_message_num_per_candidate,
					validation_upgrade_cooldown              : pre.validation_upgrade_cooldown,
					validation_upgrade_delay                 : pre.validation_upgrade_delay,
					async_backing_params                     : pre.async_backing_params,
					max_pov_size                             : pre.max_pov_size,
					max_downward_message_size                : pre.max_downward_message_size,
					hrmp_max_parachain_outbound_channels     : pre.hrmp_max_parachain_outbound_channels,
					hrmp_sender_deposit                      : pre.hrmp_sender_deposit,
					hrmp_recipient_deposit                   : pre.hrmp_recipient_deposit,
					hrmp_channel_max_capacity                : pre.hrmp_channel_max_capacity,
					hrmp_channel_max_total_size              : pre.hrmp_channel_max_total_size,
					hrmp_max_parachain_inbound_channels      : pre.hrmp_max_parachain_inbound_channels,
					hrmp_channel_max_message_size            : pre.hrmp_channel_max_message_size,
					executor_params                          : pre.executor_params,
					code_retention_period                    : pre.code_retention_period,
					max_validators                           : pre.max_validators,
					dispute_period                           : pre.dispute_period,
					dispute_post_conclusion_acceptance_period: pre.dispute_post_conclusion_acceptance_period,
					no_show_slots                            : pre.no_show_slots,
					n_delay_tranches                         : pre.n_delay_tranches,
					zeroth_delay_tranche_width               : pre.zeroth_delay_tranche_width,
					needed_approvals                         : pre.needed_approvals,
					relay_vrf_modulo_samples                 : pre.relay_vrf_modulo_samples,
					pvf_voting_ttl                           : pre.pvf_voting_ttl,
					minimum_validation_upgrade_delay         : pre.minimum_validation_upgrade_delay,
					minimum_backing_votes                    : pre.minimum_backing_votes,
					node_features                            : pre.node_features,
					approval_voting_params                   : pre.approval_voting_params,
					scheduler_params                         : pre.scheduler_params,
					max_relay_parent_session_age             : pre.max_relay_parent_session_age,
					// New field: default to the standard provides-window size.
					provides_window_size                     : crate::inclusion::DEFAULT_PROVIDES_WINDOW_SIZE,
				}
			};

	let v13 = v13::ActiveConfig::<T>::get()
		.defensive_proof("Could not decode old config")
		.unwrap_or_default();
	let v14 = translate(v13);
	v14::ActiveConfig::<T>::set(Some(v14));

	// Allowed to be empty.
	let pending_v13 = v13::PendingConfigs::<T>::get().unwrap_or_default();
	let mut pending_v14 = Vec::with_capacity(pending_v13.len());

	for (session, v13) in pending_v13.into_iter() {
		let v14 = translate(v13);
		pending_v14.push((session, v14));
	}
	v14::PendingConfigs::<T>::set(Some(pending_v14.clone()));

	let num_configs = (pending_v14.len() + 1) as u64;
	T::DbWeight::get().reads_writes(num_configs, num_configs)
}

impl<BlockNumber: Default + From<u32>> Default for V13HostConfiguration<BlockNumber> {
	fn default() -> Self {
		Self {
			async_backing_params: AsyncBackingParams {
				max_candidate_depth: 0,
				allowed_ancestry_len: 0,
			},
			no_show_slots: 1u32.into(),
			validation_upgrade_cooldown: Default::default(),
			validation_upgrade_delay: 2u32.into(),
			code_retention_period: Default::default(),
			max_code_size: polkadot_primitives::MAX_CODE_SIZE,
			max_pov_size: Default::default(),
			max_head_data_size: Default::default(),
			max_validators: None,
			dispute_period: 6,
			dispute_post_conclusion_acceptance_period: 100.into(),
			n_delay_tranches: 1,
			zeroth_delay_tranche_width: Default::default(),
			needed_approvals: Default::default(),
			relay_vrf_modulo_samples: Default::default(),
			max_upward_queue_count: Default::default(),
			max_upward_queue_size: Default::default(),
			max_downward_message_size: Default::default(),
			max_upward_message_size: Default::default(),
			max_upward_message_num_per_candidate: Default::default(),
			hrmp_sender_deposit: Default::default(),
			hrmp_recipient_deposit: Default::default(),
			hrmp_channel_max_capacity: Default::default(),
			hrmp_channel_max_total_size: Default::default(),
			hrmp_max_parachain_inbound_channels: Default::default(),
			hrmp_channel_max_message_size: Default::default(),
			hrmp_max_parachain_outbound_channels: Default::default(),
			hrmp_max_message_num_per_candidate: Default::default(),
			pvf_voting_ttl: 2u32.into(),
			minimum_validation_upgrade_delay: 2.into(),
			executor_params: Default::default(),
			approval_voting_params: ApprovalVotingParams { max_approval_coalesce_count: 1 },
			minimum_backing_votes: polkadot_primitives::LEGACY_MIN_BACKING_VOTES,
			node_features: NodeFeatures::EMPTY,
			scheduler_params: Default::default(),
			max_relay_parent_session_age: 0,
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::mock::{new_test_ext, Test};

	#[test]
	fn test_migrate_to_v14() {
		let v13 = V13HostConfiguration::<polkadot_primitives::BlockNumber>::default();

		let pending_configs = alloc::vec![(100, v13.clone()), (300, v13.clone())];

		new_test_ext(Default::default()).execute_with(|| {
			v13::ActiveConfig::<Test>::set(Some(v13.clone()));
			v13::PendingConfigs::<Test>::set(Some(pending_configs));

			migrate_to_v14::<Test>();

			let v14 = v14::ActiveConfig::<Test>::get().unwrap();

			let mut configs_to_check = v14::PendingConfigs::<Test>::get().unwrap();
			configs_to_check.push((0, v14.clone()));

			for (_, v14) in configs_to_check {
				// Existing fields carried over.
				assert_eq!(v13.max_code_size, v14.max_code_size);
				assert_eq!(v13.max_relay_parent_session_age, v14.max_relay_parent_session_age);
				// New field defaults to `DEFAULT_PROVIDES_WINDOW_SIZE`.
				assert_eq!(
					v14.provides_window_size,
					crate::inclusion::DEFAULT_PROVIDES_WINDOW_SIZE
				);
			}
		});
	}
}
