use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use cadence_macros::statsd_count;
use cadence_macros::statsd_gauge;
use dashmap::DashMap;
use serde::Deserialize;
use serde::Serialize;
use solana_program_runtime::compute_budget::ComputeBudget;
use solana_sdk::instruction::CompiledInstruction;
use solana_sdk::{pubkey::Pubkey, slot_history::Slot};
use tracing::error;
use yellowstone_grpc_proto::geyser::subscribe_update::UpdateOneof;
use yellowstone_grpc_proto::geyser::SubscribeUpdate;

use crate::grpc_consumer::GrpcConsumer;
use crate::slot_cache::SlotCache;

#[derive(Deserialize, Clone)]
pub enum PriorityLevel {
    Min,       // 0th percentile
    Low,       // 25th percentile
    Medium,    // 50th percentile
    High,      // 75th percentile
    VeryHigh,  // 95th percentile
    UnsafeMax, // 100th percentile
    Default,   // 50th percentile
}

impl From<String> for PriorityLevel {
    fn from(s: String) -> Self {
        match s.as_str() {
            "NONE" => PriorityLevel::Min,
            "LOW" => PriorityLevel::Low,
            "MEDIUM" => PriorityLevel::Medium,
            "HIGH" => PriorityLevel::High,
            "VERY_HIGH" => PriorityLevel::VeryHigh,
            "UNSAFE_MAX" => PriorityLevel::UnsafeMax,
            _ => PriorityLevel::Default,
        }
    }
}

impl Into<Percentile> for PriorityLevel {
    fn into(self) -> Percentile {
        match self {
            PriorityLevel::Min => 0,
            PriorityLevel::Low => 25,
            PriorityLevel::Medium => 50,
            PriorityLevel::High => 75,
            PriorityLevel::VeryHigh => 95,
            PriorityLevel::UnsafeMax => 100,
            PriorityLevel::Default => 50,
        }
    }
}

type Percentile = usize;

#[derive(Deserialize, Serialize, Debug, Clone)]
#[serde(rename_all(serialize = "camelCase", deserialize = "camelCase"))]
pub struct MicroLamportPriorityFeeEstimates {
    pub min: f64,
    pub low: f64,
    pub medium: f64,
    pub high: f64,
    pub very_high: f64,
    pub unsafe_max: f64,
}

#[derive(Debug, Clone)]
struct SlotPriorityFees {
    fees: Fees,
    account_fees: DashMap<Pubkey, Fees>,
}
type PriorityFeesBySlot = DashMap<Slot, SlotPriorityFees>;

impl SlotPriorityFees {
    fn new(accounts: Vec<Pubkey>, priority_fee: u64, is_vote: bool) -> Self {
        let account_fees = DashMap::new();
        let fees = Fees::new(priority_fee as f64, is_vote);
        for account in accounts {
            account_fees.insert(account, fees.clone());
        }
        if is_vote {
            Self { fees, account_fees }
        } else {
            Self { fees, account_fees }
        }
    }
}

#[derive(Debug, Clone)]
pub struct PriorityFeeTracker {
    priority_fees: Arc<PriorityFeesBySlot>,
    compute_budget: ComputeBudget,
    slot_cache: SlotCache,
}

impl GrpcConsumer for PriorityFeeTracker {
    fn consume(&self, update: &SubscribeUpdate) -> Result<(), String> {
        match update.update_oneof.clone() {
            Some(UpdateOneof::Block(block)) => {
                statsd_count!("blocks_processed", 1);
                statsd_count!("txns_received", block.transactions.len() as i64);
                let slot = block.slot;
                for txn in block.transactions {
                    // skip failed txs
                    if txn.meta.map_or(false, |meta| meta.err.is_some()) {
                        statsd_count!("txn_failed", 1);
                        return Ok(());
                    }
                    if txn.transaction.is_none() {
                        statsd_count!("txn_missing", 1);
                        continue;
                    }
                    let transaction = txn.transaction.unwrap();
                    let message = transaction.message;
                    if message.is_none() {
                        statsd_count!("message_missing", 1);
                        continue;
                    }
                    let message = message.unwrap();
                    let mut account_keys = message.account_keys;
                    for lookup in message.address_table_lookups {
                        account_keys.push(lookup.account_key);
                    }
                    let accounts: Vec<Pubkey> = account_keys
                        .into_iter()
                        .filter_map(|p| {
                            let pubkey: Option<[u8; 32]> = p.try_into().ok();
                            if pubkey.is_none() {
                                statsd_count!("invalid_pubkey", 1);
                            }
                            pubkey
                        })
                        .map(|p| Pubkey::new_from_array(p))
                        .collect();

                    let compiled_instructions: Vec<CompiledInstruction> = message
                        .instructions
                        .iter()
                        .map(|ix| {
                            CompiledInstruction::new_from_raw_parts(
                                ix.program_id_index as u8,
                                ix.data.clone(),
                                ix.accounts.clone(),
                            )
                        })
                        .collect();
                    let instructions_for_processing: Vec<(&Pubkey, &CompiledInstruction)> =
                        compiled_instructions
                            .iter()
                            .filter_map(|ix| {
                                let account = accounts.get(ix.program_id_index as usize);
                                if account.is_none() {
                                    statsd_count!("program_id_index_not_found", 1);
                                    return None;
                                }
                                Some((account.unwrap(), ix))
                            })
                            .collect();
                    statsd_count!(
                        "priority_fee_tracker.accounts_processed",
                        accounts.len() as i64
                    );
                    let priority_fee_details = self.compute_budget.clone().process_instructions(
                        instructions_for_processing.into_iter(),
                        true,
                        true,
                    );
                    statsd_count!("txns_processed", 1);
                    match priority_fee_details {
                        Ok(priority_fee_details) => self.push_priority_fee_for_txn(
                            slot,
                            accounts,
                            priority_fee_details.get_priority(),
                            txn.is_vote,
                        ),
                        Err(e) => {
                            error!("error processing priority fee details: {:?}", e);
                        }
                    }
                }
            }
            _ => return Ok(()),
        }
        Ok(())
    }
}

impl PriorityFeeTracker {
    pub fn new(slot_cache_length: usize) -> Self {
        let tracker = Self {
            priority_fees: Arc::new(DashMap::new()),
            slot_cache: SlotCache::new(slot_cache_length),
            compute_budget: ComputeBudget::default(),
        };
        tracker.poll_fees();
        tracker
    }

    fn poll_fees(&self) {
        let priority_fee_tracker = self.clone();
        tokio::spawn(async move {
            loop {
                let global_fees =
                    priority_fee_tracker.get_priority_fee_estimates(vec![], false, None);
                statsd_gauge!(
                    "min_priority_fee",
                    global_fees.min as u64,
                    "account" => "none"
                );
                statsd_gauge!("low_priority_fee", global_fees.low as u64, "account" => "none");
                statsd_gauge!(
                    "medium_priority_fee",
                    global_fees.medium as u64,
                    "account" => "none"
                );
                statsd_gauge!(
                    "high_priority_fee",
                    global_fees.high as u64,
                    "account" => "none"
                );
                statsd_gauge!(
                    "very_high_priority_fee",
                    global_fees.very_high as u64,
                    "account" => "none"
                );
                statsd_gauge!(
                    "unsafe_max_priority_fee",
                    global_fees.unsafe_max as u64,
                    "account" => "none"
                );
                tokio::time::sleep(Duration::from_millis(1000)).await
            }
        });
    }

    fn push_priority_fee_for_txn(
        &self,
        slot: Slot,
        accounts: Vec<Pubkey>,
        priority_fee: u64,
        is_vote: bool,
    ) {
        // update the slot cache so we can keep track of the slots we have processed in order
        // for removal later
        let slot_to_remove = self.slot_cache.push_pop(slot);
        if !self.priority_fees.contains_key(&slot) {
            self.priority_fees
                .insert(slot, SlotPriorityFees::new(accounts, priority_fee, is_vote));
        } else {
            // update the slot priority fees
            self.priority_fees.entry(slot).and_modify(|priority_fees| {
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
        if slot_to_remove.is_some() {
            self.priority_fees.remove(&slot_to_remove.unwrap());
        }
    }

    pub fn get_priority_fee_estimates(
        &self,
        accounts: Vec<Pubkey>,
        include_vote: bool,
        lookback_period: Option<usize>,
    ) -> MicroLamportPriorityFeeEstimates {
        let start = std::time::Instant::now();
        let mut account_fees = vec![];
        let mut transaction_fees = vec![];
        let rc_accounts = Rc::new(accounts);
        for (i, slot_priority_fees) in self.priority_fees.iter().enumerate() {
            if let Some(lookback_period) = lookback_period {
                if i >= lookback_period as usize {
                    break;
                }
            }
            if include_vote {
                transaction_fees.extend_from_slice(&slot_priority_fees.fees.vote_fees);
            }
            transaction_fees.extend_from_slice(&slot_priority_fees.fees.non_vote_fees);
            for account in rc_accounts.iter() {
                let account_priority_fees = slot_priority_fees.account_fees.get(account);
                if let Some(account_priority_fees) = account_priority_fees {
                    if include_vote {
                        account_fees.extend_from_slice(&account_priority_fees.vote_fees);
                    }
                    account_fees.extend_from_slice(&account_priority_fees.non_vote_fees);
                }
            }
        }
        let micro_lamport_priority_fee_estimates = MicroLamportPriorityFeeEstimates {
            min: max_percentile(&mut account_fees, &mut transaction_fees, 0),
            low: max_percentile(&mut account_fees, &mut transaction_fees, 25),
            medium: max_percentile(&mut account_fees, &mut transaction_fees, 50),
            high: max_percentile(&mut account_fees, &mut transaction_fees, 75),
            very_high: max_percentile(&mut account_fees, &mut transaction_fees, 95),
            unsafe_max: max_percentile(&mut account_fees, &mut transaction_fees, 100),
        };
        let end = std::time::Instant::now();
        statsd_gauge!(
            "get_priority_fee_estimates_time",
            end.duration_since(start).as_nanos() as u64
        );
        micro_lamport_priority_fee_estimates
    }
}

fn max(a: f64, b: f64) -> f64 {
    if a > b {
        a
    } else {
        b
    }
}

#[derive(Debug, Clone)]
pub struct Fees {
    non_vote_fees: Vec<f64>,
    vote_fees: Vec<f64>,
}

impl Fees {
    pub fn new(fee: f64, is_vote: bool) -> Self {
        if is_vote {
            Self {
                vote_fees: vec![fee],
                non_vote_fees: vec![],
            }
        } else {
            Self {
                vote_fees: vec![],
                non_vote_fees: vec![fee],
            }
        }
    }

    pub fn add_fee(&mut self, fee: f64, is_vote: bool) {
        if is_vote {
            self.vote_fees.push(fee);
        } else {
            self.non_vote_fees.push(fee);
        }
    }
}

fn max_percentile(
    account_fees: &mut Vec<f64>,
    transaction_fees: &mut Vec<f64>,
    percentile_value: Percentile,
) -> f64 {
    max(
        percentile(account_fees, percentile_value),
        percentile(transaction_fees, percentile_value),
    )
}

// pulled from here - https://www.calculatorsoup.com/calculators/statistics/percentile-calculator.php
// couldn't find any good libraries that worked
fn percentile(values: &mut Vec<f64>, percentile: Percentile) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = values.len() as f64;
    let r = (percentile as f64 / 100.0) * (n - 1.0) + 1.0;

    if r.fract() == 0.0 {
        values[r as usize - 1]
    } else {
        let ri = r.trunc() as usize - 1;
        let rf = r.fract();
        values[ri] + rf * (values[ri + 1] - values[ri])
    }
}

#[cfg(test)]
mod tests {
    use cadence::{NopMetricSink, StatsdClient};
    use cadence_macros::set_global_default;

    use super::*;

    fn init_metrics() {
        let noop = NopMetricSink {};
        let client = StatsdClient::builder("", noop).build();
        set_global_default(client)
    }

    #[tokio::test]
    async fn test_specific_fee_estimates() {
        init_metrics();
        let tracker = PriorityFeeTracker::new(10);

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
            tracker.push_priority_fee_for_txn(1, accounts.clone(), fee as u64, false);
        }

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates =
            tracker.get_priority_fee_estimates(vec![accounts.get(0).unwrap().clone()], false, None);
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
    }

    #[tokio::test]
    async fn test_with_many_slots() {
        init_metrics();
        let tracker = PriorityFeeTracker::new(101);

        // adding set of fees at beginning that would mess up percentiles if they were not removed
        let mut fees = vec![10.0, 9.0, 8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0, 1.0];
        let mut i = 1;
        while i <= 100 {
            fees.push(i as f64);
            i += 1;
        }

        let account_1 = Pubkey::new_unique();
        let account_2 = Pubkey::new_unique();
        let account_3 = Pubkey::new_unique();
        let accounts = vec![account_1, account_2, account_3];

        // Simulate adding the fixed fees as both account-specific and transaction fees
        for (i, fee) in fees.clone().into_iter().enumerate() {
            tracker.push_priority_fee_for_txn(i as Slot, accounts.clone(), fee as u64, false);
        }

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates =
            tracker.get_priority_fee_estimates(vec![accounts.get(0).unwrap().clone()], false, None);
        let expected_min_fee = 1.0;
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
    }

    #[tokio::test]
    async fn test_with_many_slots_broken() {
        // same test as above but with an extra slot to throw off the value
        init_metrics();
        let tracker = PriorityFeeTracker::new(102);

        // adding set of fees at beginning that would mess up percentiles if they were not removed
        let mut fees = vec![10.0, 9.0, 8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0, 1.0];
        let mut i = 1;
        while i <= 100 {
            fees.push(i as f64);
            i += 1;
        }

        let account_1 = Pubkey::new_unique();
        let account_2 = Pubkey::new_unique();
        let account_3 = Pubkey::new_unique();
        let accounts = vec![account_1, account_2, account_3];

        // Simulate adding the fixed fees as both account-specific and transaction fees
        for (i, fee) in fees.clone().into_iter().enumerate() {
            tracker.push_priority_fee_for_txn(i as Slot, accounts.clone(), fee as u64, false);
        }

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates =
            tracker.get_priority_fee_estimates(vec![accounts.get(0).unwrap().clone()], false, None);
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 95.0;
        assert_ne!(estimates.low, expected_low_fee);
        assert_ne!(estimates.medium, expected_medium_fee);
        assert_ne!(estimates.high, expected_high_fee);
        assert_ne!(estimates.very_high, expected_very_high_fee);
    }

    #[tokio::test]
    async fn test_exclude_vote() {
        let tracker = PriorityFeeTracker::new(10);

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
            tracker.push_priority_fee_for_txn(1, accounts.clone(), fee as u64, false);
        }
        for _ in 0..10 {
            tracker.push_priority_fee_for_txn(1, accounts.clone(), 1000000, true);
        }

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates =
            tracker.get_priority_fee_estimates(vec![accounts.get(0).unwrap().clone()], false, None);
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
    }
}
