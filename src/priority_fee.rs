use crate::errors::TransactionValidationError;
use crate::grpc_consumer::GrpcConsumer;
use crate::model::{
    Fees, MicroLamportPriorityFeeDetails, MicroLamportPriorityFeeEstimates, PriorityFeesBySlot,
    PriorityLevel, SlotPriorityFees,
};
use crate::priority_fee_calculation::Calculations;
use crate::priority_fee_calculation::Calculations::Calculation2;
use crate::rpc_server::get_recommended_fee;
use crate::slot_cache::SlotCache;
use cadence_macros::statsd_count;
use cadence_macros::statsd_gauge;
use dashmap::DashMap;
use solana::storage::confirmed_block::Message;
use solana_program_runtime::compute_budget::ComputeBudget;
use solana_program_runtime::prioritization_fee::PrioritizationFeeDetails;
use solana_sdk::instruction::CompiledInstruction;
use solana_sdk::transaction::TransactionError;
use solana_sdk::{pubkey::Pubkey, slot_history::Slot};
use statrs::statistics::{Data, Distribution, OrderStatistics};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tracing::error;
use yellowstone_grpc_proto::geyser::subscribe_update::UpdateOneof;
use yellowstone_grpc_proto::geyser::{SubscribeUpdate, SubscribeUpdateTransactionInfo};
use yellowstone_grpc_proto::prelude::{MessageHeader, Transaction, TransactionStatusMeta};
use yellowstone_grpc_proto::solana;

#[derive(Debug, Clone)]
pub struct PriorityFeeTracker {
    priority_fees: Arc<PriorityFeesBySlot>,
    compute_budget: ComputeBudget,
    slot_cache: SlotCache,
}

fn extract_from_meta(
    transaction_meta: Option<TransactionStatusMeta>,
) -> Result<Vec<Pubkey>, TransactionValidationError> {
    match transaction_meta {
        None => Ok(Vec::with_capacity(0)),
        Some(meta) => {
            if meta.err.is_some() {
                // statsd_count!("txn_failed", 1);
                return Result::Err(TransactionValidationError::TransactionFailed);
            }
            let writable = meta
                .loaded_writable_addresses
                .into_iter()
                .map(|v| Pubkey::try_from(v))
                .filter(|p| p.is_ok())
                .map(|p| p.unwrap())
                .collect::<Vec<Pubkey>>();

            Ok(writable)
        }
    }
}
fn extract_from_transaction(
    mut transaction: SubscribeUpdateTransactionInfo,
) -> Result<(Message, Vec<Pubkey>, bool), TransactionValidationError> {
    let writable_accounts = extract_from_meta(transaction.meta.take())?;
    let tran: &mut Transaction = transaction
        .transaction
        .as_mut()
        .ok_or(TransactionValidationError::TransactionMissing)?;
    let message: Message = tran
        .message
        .take()
        .ok_or(TransactionValidationError::MessageMissing)?;
    let is_vote = transaction.is_vote;

    Ok((message, writable_accounts, is_vote))
}

fn extract_from_message(
    message: Message,
) -> Result<
    (Vec<Pubkey>, Vec<CompiledInstruction>, Option<MessageHeader>),
    TransactionValidationError,
> {
    let account_keys = message.account_keys;
    let compiled_instructions = message.instructions;

    let accounts: Result<Vec<Pubkey>, _> = account_keys.into_iter().map(Pubkey::try_from).collect();
    if let Err(_) = accounts {
        return Err(TransactionValidationError::InvalidAccount);
    }
    let accounts = accounts.unwrap();

    let compiled_instructions: Vec<CompiledInstruction> = compiled_instructions
        .iter()
        .map(|ix| {
            CompiledInstruction::new_from_raw_parts(
                ix.program_id_index as u8,
                ix.data.clone(),
                ix.accounts.clone(),
            )
        })
        .collect();

    Ok((accounts, compiled_instructions, message.header))
}

fn calculate_priority_fee_details(
    accounts: &Vec<Pubkey>,
    instructions: &Vec<CompiledInstruction>,
    budget: &mut ComputeBudget,
) -> Result<PrioritizationFeeDetails, TransactionError> {
    let instructions_for_processing: Vec<(&Pubkey, &CompiledInstruction)> = instructions
        .iter()
        .filter_map(|ix: &CompiledInstruction| {
            let account = accounts.get(ix.program_id_index as usize);
            if account.is_none() {
                statsd_count!("program_id_index_not_found", 1);
                return None;
            }
            Some((account.unwrap(), ix))
        })
        .collect();

    budget.process_instructions(instructions_for_processing.into_iter(), true, true)
}

pub(crate) fn construct_writable_accounts<T>(
    message_accounts: Vec<T>,
    header: &Option<MessageHeader>,
) -> Vec<T> {
    if header.is_none() {
        return message_accounts;
    }

    let header = header.as_ref().unwrap();
    let min_pos_non_sig_write_accts = header.num_required_signatures as usize;
    let max_pos_write_sig =
        min_pos_non_sig_write_accts.saturating_sub(header.num_readonly_signed_accounts as usize);
    let max_non_sig_write_accts = message_accounts
        .len()
        .checked_sub(header.num_readonly_unsigned_accounts as usize)
        .unwrap_or(message_accounts.len());

    message_accounts
        .into_iter()
        .enumerate()
        .filter(|data: &(usize, T)| {
            data.0 < max_pos_write_sig
                || (data.0 >= min_pos_non_sig_write_accts && data.0 < max_non_sig_write_accts)
        })
        .map(|data: (usize, T)| data.1)
        .collect()
}

impl GrpcConsumer for PriorityFeeTracker {
    fn consume(&self, update: &SubscribeUpdate) -> Result<(), String> {
        match update.update_oneof.clone() {
            Some(UpdateOneof::Block(block)) => {
                statsd_count!("blocks_processed", 1);
                statsd_count!("txns_received", block.transactions.len() as i64);
                let slot = block.slot;
                for txn in block.transactions {
                    let res = extract_from_transaction(txn);
                    if let Err(error) = res {
                        statsd_count!(error.into(), 1);
                        continue;
                    }
                    let (message, writable_accounts, is_vote) = res.unwrap();

                    let res = extract_from_message(message);
                    if let Err(error) = res {
                        statsd_count!(error.into(), 1);
                        continue;
                    }
                    let (accounts, instructions, header) = res.unwrap();
                    let mut compute_budget = self.compute_budget;
                    let priority_fee_details = calculate_priority_fee_details(
                        &accounts,
                        &instructions,
                        &mut compute_budget,
                    );

                    let writable_accounts = vec![
                        construct_writable_accounts(accounts, &header),
                        writable_accounts,
                    ]
                    .concat();

                    statsd_count!(
                        "priority_fee_tracker.accounts_processed",
                        writable_accounts.len() as i64
                    );
                    statsd_count!("txns_processed", 1);
                    match priority_fee_details {
                        Ok(priority_fee_details) => self.push_priority_fee_for_txn(
                            slot,
                            writable_accounts,
                            priority_fee_details.get_priority(),
                            is_vote,
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
        // task to run global fee comparison every 1 second
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(1_000)).await;
                priority_fee_tracker.record_general_fees();
            }
        });
    }

    fn record_general_fees(&self) {
        let global_fees = self.calculate_priority_fee(&Calculation2 {
            accounts: &vec![],
            include_vote: false,
            include_empty_slots: false,
            lookback_period: &None,
        });
        if let Ok(global_fees) = global_fees {
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
            statsd_gauge!(
                "recommended_priority_fee",
                get_recommended_fee(global_fees) as u64,
                "account" => "none"
            );
        }
    }

    pub fn push_priority_fee_for_txn(
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
            self.priority_fees.insert(
                slot,
                SlotPriorityFees::new(slot, accounts, priority_fee, is_vote),
            );
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

    pub fn calculate_priority_fee(
        &self,
        calculation: &Calculations,
    ) -> anyhow::Result<MicroLamportPriorityFeeEstimates> {
        let res = calculation.get_priority_fee_estimates(&self.priority_fees)?;
        let res = res.into_iter().fold(
            MicroLamportPriorityFeeEstimates::default(),
            |estimate, mut data| estimate_max_values(&mut data.1, estimate),
        );
        Ok(res)
    }

    pub fn calculate_priority_fee_details(
        &self,
        calculation: &Calculations,
    ) -> anyhow::Result<(MicroLamportPriorityFeeEstimates, HashMap<String, MicroLamportPriorityFeeDetails>)> {
        let data = calculation.get_priority_fee_estimates(&self.priority_fees)?;
        let final_result: MicroLamportPriorityFeeEstimates = data.clone().into_iter().fold(
            MicroLamportPriorityFeeEstimates::default(),
            |estimate, mut data| estimate_max_values(&mut data.1, estimate),
        );
        let results: HashMap<String, MicroLamportPriorityFeeDetails> = data
            .into_iter()
            .map(|mut data| {
                let estimate = MicroLamportPriorityFeeDetails {
                    estimates: estimate_max_values(
                        &mut data.1,
                        MicroLamportPriorityFeeEstimates::default(),
                    ),
                    mean: data.1.mean().unwrap_or(f64::NAN).round(),
                    stdev: data.1.std_dev().unwrap_or(f64::NAN).round(),
                    skew: data.1.skewness().unwrap_or(f64::NAN).round(),
                    count: data.1.len(),
                };
                (data.0.to_string(), estimate)
            })
            .collect();
        Ok((final_result, results))
    }
}

fn estimate_max_values(
    fees: &mut Data<Vec<f64>>,
    mut estimates: MicroLamportPriorityFeeEstimates,
) -> MicroLamportPriorityFeeEstimates {
    estimates.min = fees
        .percentile(PriorityLevel::Min.into())
        .round()
        .max(estimates.min);
    estimates.low = fees
        .percentile(PriorityLevel::Low.into())
        .round()
        .max(estimates.low);
    estimates.medium = fees
        .percentile(PriorityLevel::Medium.into())
        .round()
        .max(estimates.medium);
    estimates.high = fees
        .percentile(PriorityLevel::High.into())
        .round()
        .max(estimates.high);
    estimates.very_high = fees
        .percentile(PriorityLevel::VeryHigh.into())
        .round()
        .max(estimates.very_high);
    estimates.unsafe_max = fees
        .percentile(PriorityLevel::UnsafeMax.into())
        .round()
        .max(estimates.unsafe_max);
    estimates
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use cadence::{NopMetricSink, StatsdClient};
    use cadence_macros::set_global_default;
    use solana_sdk::compute_budget::ComputeBudgetInstruction::{
        RequestHeapFrame, SetComputeUnitLimit, SetComputeUnitPrice, SetLoadedAccountsDataSizeLimit,
    };
    use std::collections::HashSet;

    fn init_metrics() {
        let noop = NopMetricSink {};
        let client = StatsdClient::builder("", noop).build();
        set_global_default(client)
    }

    fn calculation1(
        accounts: &Vec<Pubkey>,
        include_vote: bool,
        include_empty_slot: bool,
        lookback_period: &Option<u32>,
        tracker: &PriorityFeeTracker,
    ) -> MicroLamportPriorityFeeEstimates {
        let calc = Calculations::new_calculation1(
            accounts,
            include_vote,
            include_empty_slot,
            lookback_period,
        );
        tracker
            .calculate_priority_fee(&calc)
            .expect(format!("estimates for calc1 to be valid with {:?}", calc).as_str())
    }

    fn calculation_details_1(
        accounts: &Vec<Pubkey>,
        include_vote: bool,
        include_empty_slot: bool,
        lookback_period: &Option<u32>,
        tracker: &PriorityFeeTracker,
    ) -> HashMap<String, MicroLamportPriorityFeeDetails> {
        let calc = Calculations::new_calculation1(
            accounts,
            include_vote,
            include_empty_slot,
            lookback_period,
        );
        tracker
            .calculate_priority_fee_details(&calc)
            .expect(format!("estimates for calc1 to be valid with {:?}", calc).as_str())
            .1
    }

    fn calculation2(
        accounts: &Vec<Pubkey>,
        include_vote: bool,
        include_empty_slot: bool,
        lookback_period: &Option<u32>,
        tracker: &PriorityFeeTracker,
    ) -> MicroLamportPriorityFeeEstimates {
        let calc = Calculations::new_calculation2(
            accounts,
            include_vote,
            include_empty_slot,
            lookback_period,
        );
        tracker
            .calculate_priority_fee(&calc)
            .expect(format!("estimates for calc2 to be valid with {:?}", calc).as_str())
    }

    fn calculation_details_2(
        accounts: &Vec<Pubkey>,
        include_vote: bool,
        include_empty_slot: bool,
        lookback_period: &Option<u32>,
        tracker: &PriorityFeeTracker,
    ) -> HashMap<String, MicroLamportPriorityFeeDetails> {
        let calc = Calculations::new_calculation2(
            accounts,
            include_vote,
            include_empty_slot,
            lookback_period,
        );
        tracker
            .calculate_priority_fee_details(&calc)
            .expect(format!("estimates for calc2 to be valid with {:?}", calc).as_str())
            .1
    }

    #[tokio::test]
    async fn test_specific_fee_estimates_with_no_account() {
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
        let acct = vec![];
        let estimates = calculation1(&acct, false, false, &None, &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation1(&acct, false, true, &None, &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation1(&acct, false, true, &Some(150), &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation1(&acct, true, true, &Some(150), &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
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

        let acct = vec![];
        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation2(&acct, false, false, &None, &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation2(&acct, false, false, &Some(150), &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation2(&acct, false, true, &None, &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation2(&acct, false, true, &Some(150), &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);
        // NOTE: calculation 2
    }

    #[tokio::test]
    async fn test_specific_fee_estimates_details_with_no_account() {
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
        let acct = vec![];
        let estimates = calculation_details_1(&acct, false, false, &None, &tracker);
        assert_eq!(estimates.len(), 2);

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.get("Global").unwrap().estimates.min, 0.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.low, 25.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.medium, 50.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.high, 75.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.very_high, 96.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.unsafe_max, 100.0);
        assert_eq!(estimates.get("Global").unwrap().count, 101);
        assert_eq!(estimates.get("Global").unwrap().mean, 50.0);
        assert_eq!(estimates.get("Global").unwrap().stdev, 29.0);
        assert!(estimates.get("Global").unwrap().skew.is_nan());

        assert_eq!(estimates.get("All Accounts").unwrap().estimates.min, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.low, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.medium, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.high, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.very_high, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.unsafe_max, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().count, 0);
        assert!(estimates.get("All Accounts").unwrap().mean.is_nan());
        assert!(estimates.get("All Accounts").unwrap().stdev.is_nan());
        assert!(estimates.get("All Accounts").unwrap().skew.is_nan());

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation_details_1(&acct, false, true, &None, &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.get("Global").unwrap().estimates.min, 0.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.low, 25.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.medium, 50.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.high, 75.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.very_high, 96.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.unsafe_max, 100.0);
        assert_eq!(estimates.get("Global").unwrap().count, 101);
        assert_eq!(estimates.get("Global").unwrap().mean, 50.0);
        assert_eq!(estimates.get("Global").unwrap().stdev, 29.0);
        assert!(estimates.get("Global").unwrap().skew.is_nan());

        assert_eq!(estimates.get("All Accounts").unwrap().estimates.min, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.low, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.medium, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.high, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.very_high, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.unsafe_max, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().count, 0);
        assert!(estimates.get("All Accounts").unwrap().mean.is_nan());
        assert!(estimates.get("All Accounts").unwrap().stdev.is_nan());
        assert!(estimates.get("All Accounts").unwrap().skew.is_nan());

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation_details_1(&acct, false, true, &Some(150), &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.get("Global").unwrap().estimates.min, 0.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.low, 25.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.medium, 50.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.high, 75.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.very_high, 96.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.unsafe_max, 100.0);
        assert_eq!(estimates.get("Global").unwrap().count, 101);
        assert_eq!(estimates.get("Global").unwrap().mean, 50.0);
        assert_eq!(estimates.get("Global").unwrap().stdev, 29.0);
        assert!(estimates.get("Global").unwrap().skew.is_nan());

        assert_eq!(estimates.get("All Accounts").unwrap().estimates.min, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.low, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.medium, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.high, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.very_high, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.unsafe_max, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().count, 0);
        assert!(estimates.get("All Accounts").unwrap().mean.is_nan());
        assert!(estimates.get("All Accounts").unwrap().stdev.is_nan());
        assert!(estimates.get("All Accounts").unwrap().skew.is_nan());


        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation_details_1(&acct, true, true, &Some(150), &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.get("Global").unwrap().estimates.min, 0.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.low, 25.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.medium, 50.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.high, 75.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.very_high, 96.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.unsafe_max, 100.0);
        assert_eq!(estimates.get("Global").unwrap().count, 101);
        assert_eq!(estimates.get("Global").unwrap().mean, 50.0);
        assert_eq!(estimates.get("Global").unwrap().stdev, 29.0);
        assert!(estimates.get("Global").unwrap().skew.is_nan());

        assert_eq!(estimates.get("All Accounts").unwrap().estimates.min, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.low, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.medium, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.high, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.very_high, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().estimates.unsafe_max, 0.0);
        assert_eq!(estimates.get("All Accounts").unwrap().count, 0);
        assert!(estimates.get("All Accounts").unwrap().mean.is_nan());
        assert!(estimates.get("All Accounts").unwrap().stdev.is_nan());
        assert!(estimates.get("All Accounts").unwrap().skew.is_nan());
    }

    #[tokio::test]
    async fn test_specific_fee_estimates_detail_v2() {
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

        let acct = vec![];
        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation_details_2(&acct, false, false, &None, &tracker);
        assert_eq!(estimates.len(), 1);

        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.get("Global").unwrap().estimates.min, 0.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.low, 25.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.medium, 50.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.high, 75.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.very_high, 96.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.unsafe_max, 100.0);
        assert_eq!(estimates.get("Global").unwrap().count, 101);
        assert_eq!(estimates.get("Global").unwrap().mean, 50.0);
        assert_eq!(estimates.get("Global").unwrap().stdev, 29.0);
        assert!(estimates.get("Global").unwrap().skew.is_nan());


        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation_details_2(&acct, false, false, &Some(150), &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.get("Global").unwrap().estimates.min, 0.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.low, 25.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.medium, 50.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.high, 75.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.very_high, 96.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.unsafe_max, 100.0);
        assert_eq!(estimates.get("Global").unwrap().count, 101);
        assert_eq!(estimates.get("Global").unwrap().mean, 50.0);
        assert_eq!(estimates.get("Global").unwrap().stdev, 29.0);
        assert!(estimates.get("Global").unwrap().skew.is_nan());

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation_details_2(&acct, false, true, &None, &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.get("Global").unwrap().estimates.min, 0.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.low, 25.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.medium, 50.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.high, 75.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.very_high, 96.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.unsafe_max, 100.0);
        assert_eq!(estimates.get("Global").unwrap().count, 101);
        assert_eq!(estimates.get("Global").unwrap().mean, 50.0);
        assert_eq!(estimates.get("Global").unwrap().stdev, 29.0);
        assert!(estimates.get("Global").unwrap().skew.is_nan());

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation_details_2(&acct, false, true, &Some(150), &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.get("Global").unwrap().estimates.min, 0.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.low, 25.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.medium, 50.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.high, 75.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.very_high, 96.0);
        assert_eq!(estimates.get("Global").unwrap().estimates.unsafe_max, 100.0);
        assert_eq!(estimates.get("Global").unwrap().count, 101);
        assert_eq!(estimates.get("Global").unwrap().mean, 50.0);
        assert_eq!(estimates.get("Global").unwrap().stdev, 29.0);
        assert!(estimates.get("Global").unwrap().skew.is_nan());
        // NOTE: calculation 2
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

        let estimates = calculation1(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            false,
            &None,
            &tracker,
        );
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.min, 0.0);
        assert_eq!(estimates.low, 25.0);
        assert_eq!(estimates.medium, 50.0);
        assert_eq!(estimates.high, 75.0);
        assert_eq!(estimates.very_high, 96.0);
        assert_eq!(estimates.unsafe_max, 100.0);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation1(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            false,
            &Some(150),
            &tracker,
        );
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.min, 0.0);
        assert_eq!(estimates.low, 25.0);
        assert_eq!(estimates.medium, 50.0);
        assert_eq!(estimates.high, 75.0);
        assert_eq!(estimates.very_high, 96.0);
        assert_eq!(estimates.unsafe_max, 100.0);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation1(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            true,
            &None,
            &tracker,
        );
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.min, 0.0);
        assert_eq!(estimates.low, 25.0);
        assert_eq!(estimates.medium, 50.0);
        assert_eq!(estimates.high, 75.0);
        assert_eq!(estimates.very_high, 96.0);
        assert_eq!(estimates.unsafe_max, 100.0);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation1(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            true,
            &Some(150),
            &tracker,
        );
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        assert_eq!(estimates.min, 0.0);
        assert_eq!(estimates.low, 25.0);
        assert_eq!(estimates.medium, 50.0);
        assert_eq!(estimates.high, 75.0);
        assert_eq!(estimates.very_high, 96.0);
        assert_eq!(estimates.unsafe_max, 100.0);
    }

    #[tokio::test]
    async fn test_specific_fee_estimates_with_no_account_v2() {
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
        let estimates = calculation2(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            false,
            &None,
            &tracker,
        );
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation2(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            false,
            &Some(150),
            &tracker,
        );
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation2(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            true,
            &None,
            &tracker,
        );
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation2(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            true,
            &Some(150),
            &tracker,
        );
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);
        // NOTE: calculation 2
    }

    #[tokio::test]
    async fn test_with_many_slots() {
        init_metrics();
        let tracker = PriorityFeeTracker::new(101);

        // adding set of fees at beginning that would mess up percentiles if they were not removed
        let mut fees = vec![10.0, 9.0, 8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0, 1.0];
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
        for (i, fee) in fees.clone().into_iter().enumerate() {
            tracker.push_priority_fee_for_txn(i as Slot, accounts.clone(), fee as u64, false);
        }

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation1(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            false,
            &None,
            &tracker,
        );
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation1(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            false,
            &Some(150),
            &tracker,
        );
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation1(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            true,
            &None,
            &tracker,
        );
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation1(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            true,
            &Some(150),
            &tracker,
        );
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.00;
        let expected_high_fee = 75.00;
        let expected_very_high_fee = 96.00;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);
    }

    #[tokio::test]
    async fn test_with_many_slots_v2() {
        init_metrics();
        let tracker = PriorityFeeTracker::new(101);

        // adding set of fees at beginning that would mess up percentiles if they were not removed
        let mut fees = vec![10.0, 9.0, 8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0, 1.0];
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
        for (i, fee) in fees.clone().into_iter().enumerate() {
            tracker.push_priority_fee_for_txn(i as Slot, accounts.clone(), fee as u64, false);
        }

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation2(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            false,
            &None,
            &tracker,
        );
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation2(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            false,
            &Some(150),
            &tracker,
        );
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation2(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            true,
            &None,
            &tracker,
        );
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation2(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            true,
            &Some(150),
            &tracker,
        );
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
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
        let mut i = 0;
        while i <= 100 {
            fees.push(i as f64);
            i += 1;
        }

        let account_1 = Pubkey::new_unique();
        let account_2 = Pubkey::new_unique();
        let account_3 = Pubkey::new_unique();
        let account_unrelated = Pubkey::new_unique();
        let accounts = vec![account_1, account_2, account_3];

        // Simulate adding the fixed fees as both account-specific and transaction fees
        for (i, fee) in fees.clone().into_iter().enumerate() {
            if 0 == i.rem_euclid(10usize) {
                tracker.push_priority_fee_for_txn(
                    i as Slot,
                    vec![account_unrelated],
                    fee as u64,
                    false,
                );
            } else {
                tracker.push_priority_fee_for_txn(i as Slot, accounts.clone(), fee as u64, false);
            }
        }

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation1(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            false,
            &None,
            &tracker,
        );
        let expected_low_fee = 24.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation1(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            false,
            &Some(150),
            &tracker,
        );
        let expected_low_fee = 24.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation1(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            true,
            &None,
            &tracker,
        );
        let expected_low_fee = 24.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation1(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            true,
            &Some(150),
            &tracker,
        );
        let expected_low_fee = 24.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
    }

    #[tokio::test]
    async fn test_with_many_slots_broken_v2() {
        // same test as above but with an extra slot to throw off the value
        init_metrics();
        let tracker = PriorityFeeTracker::new(102);

        // adding set of fees at beginning that would mess up percentiles if they were not removed

        // adding set of fees at beginning that would mess up percentiles if they were not removed
        let mut fees = vec![10.0, 9.0, 8.0, 7.0, 6.0, 5.0, 4.0, 3.0, 2.0, 1.0];
        let mut i = 0;
        while i <= 100 {
            fees.push(i as f64);
            i += 1;
        }

        let account_1 = Pubkey::new_unique();
        let account_2 = Pubkey::new_unique();
        let account_3 = Pubkey::new_unique();
        let account_unrelated = Pubkey::new_unique();
        let accounts = vec![account_1, account_2, account_3];

        // Simulate adding the fixed fees as both account-specific and transaction fees
        for (i, fee) in fees.clone().into_iter().enumerate() {
            if 0 == i.rem_euclid(10usize) {
                tracker.push_priority_fee_for_txn(
                    i as Slot,
                    vec![account_unrelated],
                    fee as u64,
                    false,
                );
            } else {
                tracker.push_priority_fee_for_txn(i as Slot, accounts.clone(), fee as u64, false);
            }
        }

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation2(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            false,
            &None,
            &tracker,
        );
        let expected_low_fee = 24.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation2(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            false,
            &Some(150),
            &tracker,
        );
        let expected_low_fee = 24.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation2(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            true,
            &None,
            &tracker,
        );
        let expected_low_fee = 24.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = calculation2(
            &vec![accounts.get(0).unwrap().clone()],
            false,
            true,
            &Some(150),
            &tracker,
        );
        let expected_low_fee = 24.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
    }

    #[tokio::test]
    async fn test_exclude_vote() {
        // same test as above but with an extra slot to throw off the value
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
        for _ in 0..10 {
            tracker.push_priority_fee_for_txn(1, accounts.clone(), 1000000, true);
        }

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let v = vec![account_1];
        let estimates = calculation1(&v, false, false, &None, &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        let estimates = calculation1(&v, false, false, &Some(150), &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        let estimates = calculation1(&v, false, true, &None, &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        let estimates = calculation1(&v, false, true, &Some(150), &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);
    }

    #[tokio::test]
    async fn test_exclude_vote_v2() {
        // same test as above but with an extra slot to throw off the value
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
        for _ in 0..10 {
            tracker.push_priority_fee_for_txn(1, accounts.clone(), 1000000, true);
        }

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let v = vec![account_1];
        let estimates = calculation2(&v, false, false, &None, &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        let estimates = calculation2(&v, false, false, &Some(150), &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        let estimates = calculation2(&v, false, true, &None, &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        let estimates = calculation2(&v, false, true, &Some(150), &tracker);
        // Since the fixed fees are evenly distributed, the 50th percentile should be the middle value
        let expected_min_fee = 0.0;
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 96.0;
        let expected_max_fee = 100.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);
    }

    #[test]
    fn test_constructing_accounts() {
        // same test as above but with an extra slot to throw off the value
        init_metrics();
        for (test_id, data) in generate_data().iter().enumerate() {
            let (message_accounts, header, expectation) = data;
            let result = construct_writable_accounts(message_accounts.clone(), header);
            assert_eq!(result.len(), expectation.len());
            let expectation = &expectation.clone().into_iter().collect::<HashSet<Pubkey>>();
            let result = &result.clone().into_iter().collect::<HashSet<Pubkey>>();
            let diff1 = expectation - result;
            let diff2 = result - expectation;

            assert!(
                diff1.is_empty(),
                "Error ${test_id}: {:?}, {:?} not equal to {:?}",
                message_accounts,
                header,
                expectation
            );
            assert!(
                diff2.is_empty(),
                "Error ${test_id}: {:?}, {:?} not equal to {:?}",
                message_accounts,
                header,
                expectation
            );
        }
    }

    const MICRO_LAMPORTS_PER_LAMPORT: u128 = 1_000_000;
    #[test]
    fn test_budget_calculation() -> Result<(), anyhow::Error> {
        // Ok(PrioritizationFeeDetails { fee: 4923, priority: 39066 })
        let account_keys = [
            "89oWV7LXUtEgh4sYQS7gBGPVvsznCzgf9ip5h7SHWDSr", // random account
            "GitvMpoCygGyDGUK4SMZdHpEqCqwif2cnuLAhmca96zw", // random account
            "731dL3MmiEKaAT711BWxizxKyHcwLxqWQ2pLroPmbMye", // random account
            "ComputeBudget111111111111111111111111111111",  // budget program system account
            "11111111111111111111111111111111",             // budget program for accounts account
        ];

        let comp1 = SetComputeUnitLimit(100_000)
            .pack()
            .context("Could not pack compute unit limit")?;
        let comp2 = SetComputeUnitPrice(200_000)
            .pack()
            .context("Could not pack compute unit price")?;
        let comp3 = SetLoadedAccountsDataSizeLimit(300_000)
            .pack()
            .context("Could not pack loaded account data size limit")?;
        let comp4 = RequestHeapFrame(64 * 1024)
            .pack()
            .context("Could not pack request for heap frame")?;

        let random_account = "some app -- ignore".as_bytes().to_vec();
        let instructions = [
            (2u8, random_account, vec![]),
            (3u8, comp1.clone(), Vec::with_capacity(0)),
            (3u8, comp2.clone(), Vec::with_capacity(0)),
            (3u8, comp3.clone(), Vec::with_capacity(0)),
            (3u8, comp4.clone(), Vec::with_capacity(0)),
            (4u8, comp1, vec![0u8, 1u8].to_vec()),
        ];

        let fee = calculate_priority_fee_details(
            &construct_accounts(&account_keys[..])?,
            &construct_instructions(instructions.to_vec())?,
            &mut ComputeBudget::default(),
        )?;
        assert_eq!(
            fee.get_fee() as u128,
            (200_000 * 100_000 + MICRO_LAMPORTS_PER_LAMPORT - 1)
                / MICRO_LAMPORTS_PER_LAMPORT as u128
        );
        assert_eq!(fee.get_priority(), 200_000);

        Ok::<(), anyhow::Error>(())
    }

    fn construct_instructions(
        instructions: Vec<(u8, Vec<u8>, Vec<u8>)>,
    ) -> Result<Vec<CompiledInstruction>, anyhow::Error> {
        let res: Result<Vec<CompiledInstruction>, _> = instructions
            .into_iter()
            .map(|data| {
                let (prog_id, operations, accounts) = data;
                Ok::<CompiledInstruction, anyhow::Error>(CompiledInstruction::new_from_raw_parts(
                    prog_id, operations, accounts,
                ))
            })
            .collect();

        res.with_context(|| "Failed to parse account keys")
    }

    fn construct_accounts(account_keys: &[&str]) -> Result<Vec<Pubkey>, anyhow::Error> {
        let res: Result<Vec<Pubkey>, _> =
            account_keys.iter().map(|&s| Pubkey::try_from(s)).collect();

        res.with_context(|| "Failed to parse account keys")
    }

    fn generate_data() -> Vec<(Vec<Pubkey>, Option<MessageHeader>, Vec<Pubkey>)> {
        let p1 = Pubkey::new_unique();
        let p2 = Pubkey::new_unique();
        let p3 = Pubkey::new_unique();
        let p4 = Pubkey::new_unique();
        let p5 = Pubkey::new_unique();

        vec![
            (Vec::with_capacity(0), None, Vec::with_capacity(0)),
            (vec![p1], None, vec![p1]),
            (vec![p1, p2], None, vec![p1, p2]),
            // one unsigned account
            (
                vec![p2, p3, p4],
                Some(MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 1,
                }),
                vec![p2, p3],
            ),
            // 2 unsigned accounts
            (
                vec![p2, p3, p4],
                Some(MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 2,
                }),
                vec![p2],
            ),
            // all unsigned accounts
            (
                vec![p2, p3, p4],
                Some(MessageHeader {
                    num_required_signatures: 0,
                    num_readonly_signed_accounts: 1,
                    num_readonly_unsigned_accounts: 2,
                }),
                vec![p2],
            ),
            // should not happen but just in case we should check that we can handle bad data
            // too many signatures
            (
                vec![p2, p3, p4],
                Some(MessageHeader {
                    num_required_signatures: 5,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 0,
                }),
                vec![p2, p3, p4],
            ),
            // too many read only signed
            (
                vec![p2, p3, p4],
                Some(MessageHeader {
                    num_required_signatures: 0,
                    num_readonly_signed_accounts: 5,
                    num_readonly_unsigned_accounts: 0,
                }),
                vec![p2, p3, p4],
            ),
            // too many read only signed
            (
                vec![p2, p3, p4],
                Some(MessageHeader {
                    num_required_signatures: 0,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 5,
                }),
                vec![p2, p3, p4],
            ),
            // too many read only signed
            (
                vec![p3, p4, p5],
                Some(MessageHeader {
                    num_required_signatures: 0,
                    num_readonly_signed_accounts: 0,
                    num_readonly_unsigned_accounts: 5,
                }),
                vec![p3, p4, p5],
            ),
            // Specific cases for signed read accounts
            (
                vec![p2, p3, p4, p5],
                Some(MessageHeader {
                    num_required_signatures: 2,
                    num_readonly_signed_accounts: 1,
                    num_readonly_unsigned_accounts: 1,
                }),
                vec![p2, p4],
            ),
            // Specific cases for signed read accounts
            (
                vec![p2, p3, p4, p5],
                Some(MessageHeader {
                    num_required_signatures: 2,
                    num_readonly_signed_accounts: 1,
                    num_readonly_unsigned_accounts: 2,
                }),
                vec![p2],
            ),
            // Specific cases for signed read accounts
            (
                vec![p2, p3, p4, p5],
                Some(MessageHeader {
                    num_required_signatures: 1,
                    num_readonly_signed_accounts: 1,
                    num_readonly_unsigned_accounts: 2,
                }),
                vec![p3],
            ),
        ]
    }
}
