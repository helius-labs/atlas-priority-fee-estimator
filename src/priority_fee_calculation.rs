use crate::model::PriorityFeesBySlot;
use crate::priority_fee_calculation::Calculations::Calculation1;
use cadence_macros::{statsd_count, statsd_gauge};
use solana_sdk::pubkey::Pubkey;
use statrs::statistics::Data;
use std::collections::HashMap;
use std::time::Instant;
use Calculations::Calculation2;

#[derive(Debug, Clone, PartialOrd, PartialEq, Eq, Ord, Hash)]
pub enum DataType<'a> {
    Global,
    AllAccounts,
    Account(&'a Pubkey),
}
///
/// The result with type of
///
pub type DataStats<'a> = HashMap<DataType<'a>, Data<Vec<f64>>>;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Ord, PartialOrd)]
pub enum Calculations<'a> {
    Calculation1 {
        accounts: &'a [Pubkey],
        include_vote: bool,
        include_empty_slots: bool,
        lookback_period: &'a Option<u32>,
    },
    Calculation2 {
        accounts: &'a [Pubkey],
        include_vote: bool,
        include_empty_slots: bool,
        lookback_period: &'a Option<u32>,
    },
}

impl<'a> Calculations<'a> {
    pub fn new_calculation1(
        accounts: &'a [Pubkey],
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
        accounts: &'a [Pubkey],
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
    ) -> anyhow::Result<DataStats> {
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
    use crate::model::PriorityFeesBySlot;
    use crate::priority_fee_calculation::{calculate_lookback_size, DataStats, DataType};
    use solana_sdk::clock::Slot;
    use solana_sdk::pubkey::Pubkey;
    use statrs::statistics::Data;

    ///
    /// Algo1: given the list of accounts the algorithm will:
    /// 1. collect all the transactions fees over n slots
    /// 2. collect all the transaction fees for all the accounts specified over n slots
    /// 3. will calculate the percentile distributions for each of two groups
    /// 4. will choose the highest value from each percentile between two groups
    ///
    pub(crate) fn get_priority_fee_estimates<'a>(
        accounts: &'a [Pubkey],
        include_vote: &bool,
        include_empty_slots: &bool,
        lookback_period: &Option<u32>,
        priority_fees: &PriorityFeesBySlot,
    ) -> anyhow::Result<DataStats<'a>> {
        let mut slots_vec: Vec<Slot> = priority_fees.iter().map(|entry| entry.slot).collect();
        slots_vec.sort();
        slots_vec.reverse();

        let slots_vec = slots_vec;

        let lookback = calculate_lookback_size(&lookback_period, slots_vec.len());

        let mut global_fees: Vec<f64> = Vec::new();
        let mut account_fees: Vec<f64> = Vec::new();
        for slot in &slots_vec[..lookback] {
            if let Some(slot_priority_fees) = priority_fees.get(slot) {
                if *include_vote {
                    global_fees.extend_from_slice(&slot_priority_fees.fees.vote_fees);
                }
                global_fees.extend_from_slice(&slot_priority_fees.fees.non_vote_fees);

                if 0 < accounts.len() {
                    let mut has_data = false;
                    accounts.iter().for_each(|account| {
                        if let Some(account_priority_fees) =
                            slot_priority_fees.account_fees.get(account)
                        {
                            if *include_vote {
                                account_fees.extend_from_slice(&account_priority_fees.vote_fees);
                            }
                            account_fees.extend_from_slice(&account_priority_fees.non_vote_fees);
                            has_data = true;
                        }
                    });
                    if !has_data {
                        if *include_empty_slots {
                            account_fees.push(0f64);
                        }
                    }
                }
            }
        }

        let mut data = DataStats::new();
        data.insert(DataType::Global, Data::new(global_fees));
        data.insert(DataType::AllAccounts, Data::new(account_fees));
        Ok(data)
    }
}

mod v2 {
    use crate::model::PriorityFeesBySlot;
    use crate::priority_fee_calculation::{calculate_lookback_size, DataStats, DataType};
    use solana_sdk::clock::Slot;
    use solana_sdk::pubkey::Pubkey;
    use statrs::statistics::Data;
    use std::collections::HashMap;

    ///
    /// Algo2: given the list of accounts the algorithm will:
    /// 1. collect all the transactions fees over n slots
    /// 2. for each specified account collect the fees and calculate the percentiles
    /// 4. choose maximum values for each percentile between all transactions and each account
    ///
    pub(crate) fn get_priority_fee_estimates<'a>(
        accounts: &'a [Pubkey],
        include_vote: &bool,
        include_empty_slots: &bool,
        lookback_period: &Option<u32>,
        priority_fees: &PriorityFeesBySlot,
    ) -> anyhow::Result<DataStats<'a>> {
        let mut slots_vec: Vec<Slot> = priority_fees.iter().map(|entry| entry.slot).collect();
        slots_vec.sort();
        slots_vec.reverse();

        let slots_vec = slots_vec;

        let lookback = calculate_lookback_size(&lookback_period, slots_vec.len());

        let mut data: HashMap<DataType<'a>, Vec<f64>> = HashMap::new();
        for slot in &slots_vec[..lookback] {
            if let Some(slot_priority_fees) = priority_fees.get(slot) {
                let fees: &mut Vec<f64> = data.entry(DataType::Global).or_insert(Vec::new());

                if *include_vote {
                    fees.extend_from_slice(&slot_priority_fees.fees.vote_fees);
                }
                fees.extend_from_slice(&slot_priority_fees.fees.non_vote_fees);

                accounts.iter().for_each(|account| {
                    /*
                    Always insert the fees list for every account
                    If account has no fees than we can fill the buckets with 0 for every slot missed
                     */
                    let fees: &mut Vec<f64> =
                        data.entry(DataType::Account(account)).or_insert(Vec::new());
                    if let Some(account_priority_fees) =
                        slot_priority_fees.account_fees.get(account)
                    {
                        if *include_vote {
                            fees.extend_from_slice(&account_priority_fees.vote_fees);
                        }
                        fees.extend_from_slice(&account_priority_fees.non_vote_fees);
                    } else if *include_empty_slots {
                        // for all empty slot we need to add a 0
                        fees.push(0f64);
                    }
                });
            }
        }

        let data = data
            .into_iter()
            .map(|(data_type, fees)| (data_type, Data::new(fees)))
            .collect::<DataStats>();
        Ok(data)
    }
}

fn calculate_lookback_size(pref_num_slots: &Option<u32>, max_available_slots: usize) -> usize {
    max_available_slots.min(
        pref_num_slots
            .map(|v| v as usize)
            .unwrap_or(max_available_slots),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Fees, SlotPriorityFees};
    use crate::priority_fee_calculation::DataType::{Account, AllAccounts, Global};
    use cadence::{NopMetricSink, StatsdClient};
    use cadence_macros::set_global_default;
    use solana_sdk::clock::Slot;
    use statrs::statistics::OrderStatistics;

    fn init_metrics() {
        let noop = NopMetricSink {};
        let client = StatsdClient::builder("", noop).build();
        set_global_default(client)
    }


    #[tokio::test]
    async fn test_specific_fee_estimates_for_global_accounts_only() {
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

        let accounts = vec![];

        // Scenario 1: no vote transactions and no empty slots and default lookback period
        let calc = Calculations::new_calculation1(&accounts, false, false, &None);
        let mut estimates: DataStats = calc
            .get_priority_fee_estimates(&tracker)
            .expect("estimates to be valid");

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.len(), 2);
        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Global).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }

        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&AllAccounts).unwrap();
            assert!(stats.percentile(0).is_nan());
            assert!(stats.percentile(25).is_nan());
            assert!(stats.percentile(50).is_nan());
            assert!(stats.percentile(75).is_nan());
            assert!(stats.percentile(95).is_nan());
            assert!(stats.percentile(99).is_nan());
        }


        // Scenario 2: with vote transactions and no empty slots and default lookback period
        let calc = Calculations::new_calculation1(&accounts, true, false, &None);
        let mut estimates: DataStats = calc
            .get_priority_fee_estimates(&tracker)
            .expect("estimates to be valid");

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.len(), 2);
        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Global).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }

        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&AllAccounts).unwrap();
            assert!(stats.percentile(0).is_nan());
            assert!(stats.percentile(25).is_nan());
            assert!(stats.percentile(50).is_nan());
            assert!(stats.percentile(75).is_nan());
            assert!(stats.percentile(95).is_nan());
            assert!(stats.percentile(99).is_nan());
        }




        // Scenario 3: with vote transactions and empty slots and default lookback period
        let calc = Calculations::new_calculation1(&accounts, true, true, &None);
        let mut estimates: DataStats = calc
            .get_priority_fee_estimates(&tracker)
            .expect("estimates to be valid");

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.len(), 2);
        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Global).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }

        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&AllAccounts).unwrap();
            assert!(stats.percentile(0).is_nan());
            assert!(stats.percentile(25).is_nan());
            assert!(stats.percentile(50).is_nan());
            assert!(stats.percentile(75).is_nan());
            assert!(stats.percentile(95).is_nan());
            assert!(stats.percentile(99).is_nan());
        }



        // Scenario 4: with vote transactions and empty slots and different lookback period
        let calc = Calculations::new_calculation1(&accounts, true, true, &Some(1));
        let mut estimates: DataStats = calc
            .get_priority_fee_estimates(&tracker)
            .expect("estimates to be valid");

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.len(), 2);
        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Global).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }

        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&AllAccounts).unwrap();
            assert!(stats.percentile(0).is_nan());
            assert!(stats.percentile(25).is_nan());
            assert!(stats.percentile(50).is_nan());
            assert!(stats.percentile(75).is_nan());
            assert!(stats.percentile(95).is_nan());
            assert!(stats.percentile(99).is_nan());
        }
    }


    #[tokio::test]
    async fn test_specific_fee_estimates_for_global_accounts_only_v2() {
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

        let accounts = vec![];

        // Scenario 1: no vote transactions and no empty slots and default lookback period
        let calc = Calculations::new_calculation2(&accounts, false, false, &None);
        let mut estimates: DataStats = calc
            .get_priority_fee_estimates(&tracker)
            .expect("estimates to be valid");

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.len(), 1);
        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Global).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }

        // Scenario 2: with vote transactions and no empty slots and default lookback period
        let calc = Calculations::new_calculation2(&accounts, true, false, &None);
        let mut estimates: DataStats = calc
            .get_priority_fee_estimates(&tracker)
            .expect("estimates to be valid");

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.len(), 1);
        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Global).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }


        // Scenario 3: with vote transactions and empty slots and default lookback period
        let calc = Calculations::new_calculation2(&accounts, true, true, &None);
        let mut estimates: DataStats = calc
            .get_priority_fee_estimates(&tracker)
            .expect("estimates to be valid");

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.len(), 1);
        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Global).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }


        // Scenario 4: with vote transactions and empty slots and different lookback period
        let calc = Calculations::new_calculation2(&accounts, true, true, &Some(1));
        let mut estimates: DataStats = calc
            .get_priority_fee_estimates(&tracker)
            .expect("estimates to be valid");

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.len(), 1);
        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Global).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }

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

        // Scenario 1: no vote transactions and no empty slots and default lookback period
        let calc = Calculations::new_calculation1(&accounts[..=0], false, false, &None);
        let mut estimates: DataStats = calc
            .get_priority_fee_estimates(&tracker)
            .expect("estimates to be valid");

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.len(), 2);
        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Global).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }

        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&AllAccounts).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }



        // Scenario 2: no vote transactions and no empty slots and default lookback period
        let calc = Calculations::new_calculation1(&accounts[..=0], true, false, &None);
        let mut estimates: DataStats = calc
            .get_priority_fee_estimates(&tracker)
            .expect("estimates to be valid");

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.len(), 2);
        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Global).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }

        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&AllAccounts).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);

        }


        // Scenario 1: no vote transactions and no empty slots and default lookback period
        let calc = Calculations::new_calculation1(&accounts[..=0], true, true, &None);
        let mut estimates: DataStats = calc
            .get_priority_fee_estimates(&tracker)
            .expect("estimates to be valid");

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.len(), 2);
        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Global).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }

        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&AllAccounts).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);

        }



        // Scenario 1: no vote transactions and no empty slots and default lookback period
        let calc = Calculations::new_calculation1(&accounts[..=0], true, true, &Some(1));
        let mut estimates: DataStats = calc
            .get_priority_fee_estimates(&tracker)
            .expect("estimates to be valid");

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.len(), 2);
        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Global).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }

        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&AllAccounts).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);

        }
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

        // Scenario 1: no vote transactions and no empty slots and default lookback period
        let calc = Calculations::new_calculation2(&accounts[..=0], false, false, &None);
        let mut estimates: DataStats = calc
            .get_priority_fee_estimates(&tracker)
            .expect("estimates to be valid");

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.len(), 2);
        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Global).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }

        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Account(&account_1)).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }



        // Scenario 2: no vote transactions and no empty slots and default lookback period
        let calc = Calculations::new_calculation2(&accounts[..=0], true, false, &None);
        let mut estimates: DataStats = calc
            .get_priority_fee_estimates(&tracker)
            .expect("estimates to be valid");

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.len(), 2);
        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Global).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }

        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Account(&account_1)).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);

        }


        // Scenario 1: no vote transactions and no empty slots and default lookback period
        let calc = Calculations::new_calculation2(&accounts[..=0], true, true, &None);
        let mut estimates: DataStats = calc
            .get_priority_fee_estimates(&tracker)
            .expect("estimates to be valid");

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.len(), 2);
        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Global).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }

        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Account(&account_1)).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);

        }



        // Scenario 1: no vote transactions and no empty slots and default lookback period
        let calc = Calculations::new_calculation2(&accounts[..=0], true, true, &Some(1));
        let mut estimates: DataStats = calc
            .get_priority_fee_estimates(&tracker)
            .expect("estimates to be valid");

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.len(), 2);
        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Global).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);
        }

        {
            let stats: &mut Data<Vec<f64>> = estimates.get_mut(&Account(&account_1)).unwrap();
            assert_eq!(stats.percentile(0).round(), 0.0);
            assert_eq!(stats.percentile(25).round(), 25.0);
            assert_eq!(stats.percentile(50).round(), 50.0);
            assert_eq!(stats.percentile(75).round(), 75.0);
            assert_eq!(stats.percentile(95).round(), 96.0);
            assert_eq!(stats.percentile(99).round(), 100.0);

        }
    }

    fn push_priority_fee_for_txn(
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
