use crate::model::{
    MicroLamportPriorityFeeEstimates, Percentile, PriorityFeesBySlot, PriorityLevel,
};
use crate::priority_fee_calculation::Calculations::Calculation1;
use cadence_macros::{statsd_count, statsd_gauge};
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::time::Instant;
use Calculations::Calculation2;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum Calculations<'a> {
    Calculation1 {
        accounts: &'a Vec<Pubkey>,
        include_vote: bool,
        include_empty_slots: bool,
        lookback_period: &'a Option<u32>,
    },
    Calculation2 {
        accounts: &'a Vec<Pubkey>,
        include_vote: bool,
        include_empty_slots: bool,
        lookback_period: &'a Option<u32>,
    },
}

impl<'a> Calculations<'a> {
    pub fn new_calculation1(
        accounts: &'a Vec<Pubkey>,
        include_vote: bool,
        include_empty_slots: bool,
        lookback_period: &'a Option<u32>,
    ) -> Calculations<'a> {
        Calculation1 {
            accounts,
            include_vote,
            include_empty_slots,
            lookback_period,
        }
    }

    pub fn new_calculation2(
        accounts: &'a Vec<Pubkey>,
        include_vote: bool,
        include_empty_slots: bool,
        lookback_period: &'a Option<u32>,
    ) -> Calculations<'a> {
        Calculation2 {
            accounts,
            include_vote,
            include_empty_slots,
            lookback_period,
        }
    }

    pub fn get_priority_fee_estimates(
        &self,
        priority_fees: &PriorityFeesBySlot,
    ) -> anyhow::Result<MicroLamportPriorityFeeEstimates> {
        let start = Instant::now();

        let result = match self {
            Calculation1 {
                accounts,
                include_vote,
                include_empty_slots,
                lookback_period,
            } => v1::get_priority_fee_estimates(
                accounts,
                include_vote,
                include_empty_slots,
                lookback_period,
                priority_fees,
            ),
            Calculation2 {
                accounts,
                include_vote,
                include_empty_slots,
                lookback_period,
            } => v2::get_priority_fee_estimates(
                accounts,
                include_vote,
                include_empty_slots,
                lookback_period,
                priority_fees,
            ),
        };
        let version = match self {
            Calculation1 { .. } => "v1",
            Calculation2 { .. } => "v2",
        };
        statsd_gauge!(
            "get_priority_fee_estimates_time",
            start.elapsed().as_nanos() as u64,
            "version" => &version
        );
        statsd_count!(
            "get_priority_fee_calculation_version",
            1,
            "version" => &version
        );

        result
    }
}

mod v1 {
    use crate::model::{MicroLamportPriorityFeeEstimates, PriorityFeesBySlot};
    use crate::priority_fee_calculation::max_percentile;
    use solana_sdk::pubkey::Pubkey;

    ///
    /// Algo1: given the list of accounts the algorithm will:
    /// 1. collect all the transactions fees over n slots
    /// 2. collect all the transaction fees for all the accounts specified over n slots
    /// 3. will calculate the percentile distributions for each of two groups
    /// 4. will choose the highest value from each percentile between two groups
    ///
    pub(crate) fn get_priority_fee_estimates(
        accounts: &[Pubkey],
        include_vote: &bool,
        include_empty_slots: &bool,
        lookback_period: &Option<u32>,
        priority_fees: &PriorityFeesBySlot,
    ) -> anyhow::Result<MicroLamportPriorityFeeEstimates> {
        let mut account_fees = vec![];
        let mut transaction_fees = vec![];
        for (i, slot_priority_fees) in priority_fees.iter().enumerate() {
            if let Some(lookback_period) = lookback_period {
                if i >= *lookback_period as usize {
                    break;
                }
            }
            if *include_vote {
                transaction_fees.extend_from_slice(&slot_priority_fees.fees.vote_fees);
            }
            transaction_fees.extend_from_slice(&slot_priority_fees.fees.non_vote_fees);
            for account in accounts {
                let account_priority_fees = slot_priority_fees.account_fees.get(account);
                if let Some(account_priority_fees) = account_priority_fees {
                    if *include_vote {
                        account_fees.extend_from_slice(&account_priority_fees.vote_fees);
                    }
                    account_fees.extend_from_slice(&account_priority_fees.non_vote_fees);
                }
            }
        }
        if *include_empty_slots {
            let lookback = lookback_period
                .map(|v| v as usize)
                .unwrap_or(priority_fees.len());
            let account_max_size = account_fees.len().max(lookback);
            let transaction_max_size = transaction_fees.len().max(lookback);
            // if there are less data than number of slots - append 0s for up to number of slots so we don't overestimate the values
            account_fees.resize(account_max_size, 0f64);
            transaction_fees.resize(transaction_max_size, 0f64);
        }

        let micro_lamport_priority_fee_estimates = MicroLamportPriorityFeeEstimates {
            min: max_percentile(&mut account_fees, &mut transaction_fees, 0),
            low: max_percentile(&mut account_fees, &mut transaction_fees, 25),
            medium: max_percentile(&mut account_fees, &mut transaction_fees, 50),
            high: max_percentile(&mut account_fees, &mut transaction_fees, 75),
            very_high: max_percentile(&mut account_fees, &mut transaction_fees, 95),
            unsafe_max: max_percentile(&mut account_fees, &mut transaction_fees, 100),
        };

        Ok(micro_lamport_priority_fee_estimates)
    }
}

mod v2 {
    use crate::model::{MicroLamportPriorityFeeEstimates, PriorityFeesBySlot};
    use crate::priority_fee_calculation::{calculate_lookback_size, estimate_max_values};
    use solana_sdk::clock::Slot;
    use solana_sdk::pubkey::Pubkey;

    ///
    /// Algo2: given the list of accounts the algorithm will:
    /// 1. collect all the transactions fees over n slots
    /// 2. for each specified account collect the fees and calculate the percentiles
    /// 4. choose maximum values for each percentile between all transactions and each account
    ///
    pub(crate) fn get_priority_fee_estimates(
        accounts: &[Pubkey],
        include_vote: &bool,
        include_empty_slots: &bool,
        lookback_period: &Option<u32>,
        priority_fees: &PriorityFeesBySlot,
    ) -> anyhow::Result<MicroLamportPriorityFeeEstimates> {
        let mut slots_vec: Vec<Slot> = priority_fees.iter().map(|value| value.slot).collect();
        slots_vec.sort();
        slots_vec.reverse();

        let slots_vec = slots_vec;

        let lookback = calculate_lookback_size(&lookback_period, slots_vec.len());

        let mut fees = Vec::with_capacity(slots_vec.len());
        let mut micro_lamport_priority_fee_estimates = MicroLamportPriorityFeeEstimates::default();

        for slot in &slots_vec[..lookback] {
            if let Some(slot_priority_fees) = priority_fees.get(slot) {
                if *include_vote {
                    fees.extend_from_slice(&slot_priority_fees.fees.vote_fees);
                }
                fees.extend_from_slice(&slot_priority_fees.fees.non_vote_fees);
            }
        }
        micro_lamport_priority_fee_estimates =
            estimate_max_values(&mut fees, micro_lamport_priority_fee_estimates);

        for account in accounts {
            fees.clear();
            let mut zero_slots: usize = 0;
            for slot in &slots_vec[..lookback] {
                if let Some(slot_priority_fees) = priority_fees.get(slot) {
                    let account_priority_fees = slot_priority_fees.account_fees.get(account);
                    if let Some(account_priority_fees) = account_priority_fees {
                        if *include_vote {
                            fees.extend_from_slice(&account_priority_fees.vote_fees);
                        }
                        fees.extend_from_slice(&account_priority_fees.non_vote_fees);
                    } else {
                        zero_slots += 1;
                    }
                } else {
                    zero_slots += 1;
                }
            }
            if *include_empty_slots {
                // for all empty slot we need to add a 0
                fees.resize(fees.len() + zero_slots, 0f64);
            }
            micro_lamport_priority_fee_estimates =
                estimate_max_values(&mut fees, micro_lamport_priority_fee_estimates);
        }
        Ok(micro_lamport_priority_fee_estimates)
    }
}

fn estimate_max_values(
    mut fees: &mut Vec<f64>,
    mut estimates: MicroLamportPriorityFeeEstimates,
) -> MicroLamportPriorityFeeEstimates {
    let vals: HashMap<Percentile, f64> = percentile(
        &mut fees,
        &[
            PriorityLevel::Min.into(),
            PriorityLevel::Low.into(),
            PriorityLevel::Medium.into(),
            PriorityLevel::High.into(),
            PriorityLevel::VeryHigh.into(),
            PriorityLevel::UnsafeMax.into(),
        ],
    );
    estimates.min = vals
        .get(&PriorityLevel::Min.into())
        .unwrap_or(&estimates.min)
        .max(estimates.min);
    estimates.low = vals
        .get(&PriorityLevel::Low.into())
        .unwrap_or(&estimates.low)
        .max(estimates.low);
    estimates.medium = vals
        .get(&PriorityLevel::Medium.into())
        .unwrap_or(&estimates.medium)
        .max(estimates.medium);
    estimates.high = vals
        .get(&PriorityLevel::High.into())
        .unwrap_or(&estimates.high)
        .max(estimates.high);
    estimates.very_high = vals
        .get(&PriorityLevel::VeryHigh.into())
        .unwrap_or(&estimates.very_high)
        .max(estimates.very_high);
    estimates.unsafe_max = vals
        .get(&PriorityLevel::UnsafeMax.into())
        .unwrap_or(&estimates.unsafe_max)
        .max(estimates.unsafe_max);
    estimates
}

fn max(a: f64, b: f64) -> f64 {
    if a > b {
        a
    } else {
        b
    }
}
fn calculate_lookback_size(pref_num_slots: &Option<u32>, max_available_slots: usize) -> usize {
    max_available_slots.min(
        pref_num_slots
            .map(|v| v as usize)
            .unwrap_or(max_available_slots),
    )
}

fn max_percentile(
    account_fees: &mut Vec<f64>,
    transaction_fees: &mut Vec<f64>,
    percentile_value: Percentile,
) -> f64 {
    let results1 = percentile(account_fees, &[percentile_value]);
    let results2 = percentile(transaction_fees, &[percentile_value]);
    max(
        *results1.get(&percentile_value).unwrap_or(&0f64),
        *results2.get(&percentile_value).unwrap_or(&0f64),
    )
}

// pulled from here - https://www.calculatorsoup.com/calculators/statistics/percentile-calculator.php
// couldn't find any good libraries that worked
fn percentile(values: &mut Vec<f64>, percentiles: &[Percentile]) -> HashMap<Percentile, f64> {
    if values.is_empty() {
        return HashMap::with_capacity(0);
    }

    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = values.len() as f64;

    percentiles.into_iter().fold(
        HashMap::with_capacity(percentiles.len()),
        |mut data, &percentile| {
            let r = (percentile as f64 / 100.0) * (n - 1.0) + 1.0;

            let val = if r.fract() == 0.0 {
                values[r as usize - 1]
            } else {
                let ri = r.trunc() as usize - 1;
                let rf = r.fract();
                values[ri] + rf * (values[ri + 1] - values[ri])
            };
            data.insert(percentile, val);
            data
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Fees, SlotPriorityFees};
    use cadence::{NopMetricSink, StatsdClient};
    use cadence_macros::set_global_default;
    use solana_sdk::clock::Slot;

    fn init_metrics() {
        let noop = NopMetricSink {};
        let client = StatsdClient::builder("", noop).build();
        set_global_default(client)
    }

    #[tokio::test]
    async fn test_specific_fee_estimates() {
        init_metrics();
        let tracker = PriorityFeesBySlot::new();

        let mut fees = vec![];
        let mut i = 0;
        while i <= 100 {
            fees.push(i as f64);
            i += 1;
        }
        let account_1 = Pubkey::new_unique();
        let account_2 = Pubkey::new_unique();
        let account_3 = Pubkey::new_unique();
        let accounts = vec![account_1, account_2, account_3];

        // Simulate adding the fixed fees as both account-specific and transaction fees
        for fee in fees.clone() {
            push_priority_fee_for_txn(1, accounts.clone(), fee as u64, false, &tracker);
        }

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = Calculations::new_calculation1(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            false,
            &None,
        )
        .get_priority_fee_estimates(&tracker)
        .expect("estimates to be valid");

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 95.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)

        let estimates = Calculations::new_calculation1(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            false,
            &Some(150),
        )
        .get_priority_fee_estimates(&tracker)
        .expect("estimates to be valid");
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 95.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = Calculations::new_calculation1(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            true,
            &None,
        )
        .get_priority_fee_estimates(&tracker)
        .expect("estimates to be valid");
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 95.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = Calculations::new_calculation1(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            true,
            &Some(150),
        )
        .get_priority_fee_estimates(&tracker)
        .expect("estimates to be valid");
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 0.0;
        let expected_medium_fee = 25.5;
        let expected_high_fee = 62.75;
        let expected_very_high_fee = 92.54999999999998;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);
    }

    #[tokio::test]
    async fn test_specific_fee_estimates_v2() {
        init_metrics();
        let tracker = PriorityFeesBySlot::new();

        let mut fees = vec![];
        let mut i = 0;
        while i <= 100 {
            fees.push(i as f64);
            i += 1;
        }
        let account_1 = Pubkey::new_unique();
        let account_2 = Pubkey::new_unique();
        let account_3 = Pubkey::new_unique();
        let accounts = vec![account_1, account_2, account_3];

        // Simulate adding the fixed fees as both account-specific and transaction fees
        for fee in fees.clone() {
            push_priority_fee_for_txn(1, accounts.clone(), fee as u64, false, &tracker);
        }

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = Calculations::new_calculation2(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            false,
            &None,
        )
        .get_priority_fee_estimates(&tracker)
        .expect("estimates to be valid");
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 95.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = Calculations::new_calculation2(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            false,
            &Some(150),
        )
        .get_priority_fee_estimates(&tracker)
        .expect("estimates to be valid");
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 95.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = Calculations::new_calculation2(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            true,
            &None,
        )
        .get_priority_fee_estimates(&tracker)
        .expect("estimates to be valid");
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 95.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = Calculations::new_calculation2(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            true,
            &Some(150),
        )
        .get_priority_fee_estimates(&tracker)
        .expect("estimates to be valid");
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 95.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);
        // NOTE: calculation 2
    }

    pub fn push_priority_fee_for_txn(
        slot: Slot,
        accounts: Vec<Pubkey>,
        priority_fee: u64,
        is_vote: bool,
        priority_fees: &PriorityFeesBySlot,
    ) {
        // update the slot cache so we can keep track of the slots we have processed in order
        // for removal later
        if !priority_fees.contains_key(&slot) {
            priority_fees.insert(
                slot,
                SlotPriorityFees::new(slot, accounts, priority_fee, is_vote),
            );
        } else {
            // update the slot priority fees
            priority_fees.entry(slot).and_modify(|priority_fees| {
                priority_fees.fees.add_fee(priority_fee as f64, is_vote);
                for account in accounts {
                    priority_fees
                        .account_fees
                        .entry(account)
                        .and_modify(|fees| fees.add_fee(priority_fee as f64, is_vote))
                        .or_insert(Fees::new(priority_fee as f64, is_vote));
                }
            });
        }
    }
}
