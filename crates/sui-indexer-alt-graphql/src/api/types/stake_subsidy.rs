// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use crate::api::scalars::big_int::BigInt;
use async_graphql::SimpleObject;
use sui_types::sui_system_state::sui_system_state_inner_v1::StakeSubsidyV1;

/// Parameters that control the distribution of the stake subsidy.
#[derive(Clone, Debug, PartialEq, Eq, SimpleObject)]
pub(crate) struct StakeSubsidy {
    /// SUI set aside for stake subsidies -- reduces over time as stake subsidies are paid out over time.
    pub balance: Option<BigInt>,

    /// Number of times stake subsidies have been distributed.
    /// Subsidies are distributed with other staking rewards, at the end of the epoch.
    pub distribution_counter: Option<u64>,

    /// Amount of stake subsidy deducted from the balance per distribution -- decays over time.
    pub current_distribution_amount: Option<BigInt>,

    /// Maximum number of stake subsidy distributions that occur with the same distribution amount (before the amount is reduced).
    pub period_length: Option<u64>,

    /// Percentage of the current distribution amount to deduct at the end of the current subsidy period, expressed in basis points.
    pub decrease_rate: Option<u64>,
}

pub(crate) fn from_stake_subsidy_v1(value: StakeSubsidyV1) -> StakeSubsidy {
    StakeSubsidy {
        balance: Some(value.balance.value().into()),
        distribution_counter: Some(value.distribution_counter),
        current_distribution_amount: Some(value.current_distribution_amount.into()),
        period_length: Some(value.stake_subsidy_period_length),
        decrease_rate: Some(value.stake_subsidy_decrease_rate.into()),
    }
}
