use crate::grpc_consumer::GrpcConsumer;
use crate::rpc_server::get_recommended_fee;
use crate::slot_cache::SlotCache;
use cadence_macros::statsd_count;
use cadence_macros::statsd_gauge;
use dashmap::DashMap;
use serde::Deserialize;
use serde::Serialize;
use solana::storage::confirmed_block::Message;
use solana_program_runtime::compute_budget::ComputeBudget;
use solana_program_runtime::prioritization_fee::PrioritizationFeeDetails;
use solana_sdk::instruction::CompiledInstruction;
use solana_sdk::transaction::TransactionError;
use solana_sdk::{pubkey::Pubkey, slot_history::Slot};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{channel, Receiver, Sender};
use tracing::{debug, error};
use yellowstone_grpc_proto::geyser::subscribe_update::UpdateOneof;
use yellowstone_grpc_proto::geyser::{SubscribeUpdate, SubscribeUpdateTransactionInfo};
use yellowstone_grpc_proto::prelude::{MessageHeader, Transaction, TransactionStatusMeta};
use yellowstone_grpc_proto::solana;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
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

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
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
    sampling_sender: Sender<(Vec<Pubkey>, bool, Option<u32>)>,
}

#[derive(Debug, Copy, Clone, PartialEq)]
enum TransactionValidationError {
    TransactionFailed,
    TransactionMissing,
    MessageMissing,
    InvalidAccount,
}

impl Into<&str> for TransactionValidationError {
    fn into(self) -> &'static str {
        match self {
            TransactionValidationError::TransactionFailed => "txn_failed",
            TransactionValidationError::TransactionMissing => "txn_missing",
            TransactionValidationError::MessageMissing => "message_missing",
            TransactionValidationError::InvalidAccount => "invalid_pubkey",
        }
    }
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

    Result::Ok((message, writable_accounts, is_vote))
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
    let instructions_for_processing: HashMap<&Pubkey, &CompiledInstruction> = instructions
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

                    let writable_accounts = vec!(
                        construct_writable_accounts(accounts, &header),
                        writable_accounts
                    ).concat();

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
        let (sampling_txn, sampling_rxn) = channel::<(Vec<Pubkey>, bool, Option<u32>)>(100);

        let tracker = Self {
            priority_fees: Arc::new(DashMap::new()),
            slot_cache: SlotCache::new(slot_cache_length),
            compute_budget: ComputeBudget::default(),
            sampling_sender: sampling_txn,
        };
        tracker.poll_fees(sampling_rxn);
        tracker
    }

    fn poll_fees(&self, mut sampling_rxn: Receiver<(Vec<Pubkey>, bool, Option<u32>)>) {
        {
            let priority_fee_tracker = self.clone();
            // task to run global fee comparison every 1 second
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_millis(1_000)).await;
                    priority_fee_tracker.record_general_fees();
                }
            });
        }

        {
            let priority_fee_tracker = self.clone();
            // task to poll the queue and run comparison to see what is the diff between algos
            tokio::spawn(async move {
                loop {
                    match sampling_rxn.recv().await {
                        Some((accounts, include_vote, lookback_period)) => priority_fee_tracker
                            .record_specific_fees(accounts, include_vote, lookback_period),
                        _ => {}
                    }
                }
            });
        }
    }

    fn record_general_fees(&self) {
        let global_fees = self.calculation1(&vec![], false, &None);
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

    fn record_specific_fees(
        &self,
        accounts: Vec<Pubkey>,
        include_vote: bool,
        lookback_period: Option<u32>,
    ) {
        let old_fee = self.calculation1(&accounts, include_vote, &lookback_period);
        let new_fee = self.calculation2(&accounts, include_vote, &lookback_period);
        let new_fee_last = self.calculation2(&accounts, include_vote, &Some(1));

        statsd_gauge!(
            "min_priority_fee",
            old_fee.min,
            "account" => "spec"
        );
        statsd_gauge!(
            "min_priority_fee_new",
            new_fee.min,
            "account" => "spec"
        );
        statsd_gauge!(
            "min_priority_fee_last",
            new_fee_last.min,
            "account" => "spec"
        );

        statsd_gauge!("low_priority_fee",
            old_fee.low,
            "account" => "spec"
        );
        statsd_gauge!("low_priority_fee_new",
            new_fee.low,
            "account" => "spec"
        );
        statsd_gauge!("low_priority_fee_last",
            new_fee_last.low,
            "account" => "spec"
        );

        statsd_gauge!(
            "medium_priority_fee",
            old_fee.medium,
            "account" => "spec"
        );
        statsd_gauge!(
            "medium_priority_fee_new",
            new_fee.medium,
            "account" => "spec"
        );
        statsd_gauge!(
            "medium_priority_fee_last",
            new_fee_last.medium,
            "account" => "spec"
        );

        statsd_gauge!(
            "high_priority_fee",
            old_fee.high,
            "account" => "spec"
        );
        statsd_gauge!(
            "high_priority_fee_new",
            new_fee.high,
            "account" => "spec"
        );
        statsd_gauge!(
            "high_priority_fee_last",
            new_fee_last.high,
            "account" => "spec"
        );

        statsd_gauge!(
            "very_high_priority_fee",
            old_fee.very_high,
            "account" => "spec"
        );
        statsd_gauge!(
            "very_high_priority_fee_new",
            new_fee.very_high,
            "account" => "spec"
        );
        statsd_gauge!(
            "very_high_priority_fee_last",
            new_fee_last.very_high,
            "account" => "spec"
        );

        statsd_gauge!(
            "unsafe_max_priority_fee",
            old_fee.unsafe_max,
            "account" => "spec"
        );
        statsd_gauge!(
            "unsafe_max_priority_fee_new",
            new_fee.unsafe_max,
            "account" => "spec"
        );
        statsd_gauge!(
            "very_high_priority_fee_last",
            new_fee_last.unsafe_max,
            "account" => "spec"
        );

        statsd_gauge!(
            "recommended_priority_fee",
            get_recommended_fee(old_fee),
            "account" => "spec"
        );
        statsd_gauge!(
            "recommended_priority_fee_new",
            get_recommended_fee(new_fee),
            "account" => "spec"
        );
        statsd_gauge!(
            "recommended_priority_fee_last",
            get_recommended_fee(new_fee_last),
            "account" => "spec"
        );
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

    // TODO: DKH - both algos should probably be in some enum (like algo1, algo2) and be passed to
    // this method instead of sending a bool flag. I'll refactor this in next iteration. already too many changes
    pub fn get_priority_fee_estimates(
        &self,
        accounts: Vec<Pubkey>,
        include_vote: bool,
        lookback_period: Option<u32>,
        calculation1: bool,
    ) -> MicroLamportPriorityFeeEstimates {
        let start = Instant::now();
        let micro_lamport_priority_fee_estimates = if calculation1 {
            self.calculation1(&accounts, include_vote, &lookback_period)
        } else {
            self.calculation2(&accounts, include_vote, &lookback_period)
        };

        statsd_gauge!(
            "get_priority_fee_estimates_time",
            start.elapsed().as_nanos() as u64
        );
        if let Err(e) = self.sampling_sender.try_send((
            accounts.to_owned(),
            include_vote,
            lookback_period.to_owned(),
        )) {
            debug!("Did not add sample for calculation, {:?}", e);
        }

        micro_lamport_priority_fee_estimates
    }

    /*
    Algo1: given the list of accounts the algorithm will:
     1. collect all the transactions fees over n slots
     2. collect all the transaction fees for all the accounts specified over n slots
     3. will calculate the percentile distributions for each of two groups
     4. will choose the highest value from each percentile between two groups
     */
    fn calculation1(
        &self,
        accounts: &Vec<Pubkey>,
        include_vote: bool,
        lookback_period: &Option<u32>,
    ) -> MicroLamportPriorityFeeEstimates {
        let mut account_fees = vec![];
        let mut transaction_fees = vec![];
        for (i, slot_priority_fees) in self.priority_fees.iter().enumerate() {
            if let Some(lookback_period) = lookback_period {
                if i >= *lookback_period as usize {
                    break;
                }
            }
            if include_vote {
                transaction_fees.extend_from_slice(&slot_priority_fees.fees.vote_fees);
            }
            transaction_fees.extend_from_slice(&slot_priority_fees.fees.non_vote_fees);
            for account in accounts {
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
        micro_lamport_priority_fee_estimates
    }

    /*
    Algo2: given the list of accounts the algorithm will:
     1. collect all the transactions fees over n slots
     2. for each specified account collect the fees and calculate the percentiles
     4. choose maximum values for each percentile between all transactions and each account
     */
    fn calculation2(
        &self,
        accounts: &Vec<Pubkey>,
        include_vote: bool,
        lookback_period: &Option<u32>,
    ) -> MicroLamportPriorityFeeEstimates {
        let mut slots_vec = Vec::with_capacity(self.slot_cache.len());
        self.slot_cache.copy_slots(&mut slots_vec);
        slots_vec.sort();
        slots_vec.reverse();

        let lookback = calculate_lookback_size(&lookback_period, slots_vec.len());

        let mut fees = vec![];
        let mut micro_lamport_priority_fee_estimates = MicroLamportPriorityFeeEstimates::default();

        for slot in &slots_vec[..lookback] {
            if let Some(slot_priority_fees) = self.priority_fees.get(slot) {
                if include_vote {
                    fees.extend_from_slice(&slot_priority_fees.fees.vote_fees);
                }
                fees.extend_from_slice(&slot_priority_fees.fees.non_vote_fees);
            }
        }
        micro_lamport_priority_fee_estimates =
            estimate_max_values(&mut fees, micro_lamport_priority_fee_estimates);

        for account in accounts {
            fees.clear();

            for slot in &slots_vec[..lookback] {
                if let Some(slot_priority_fees) = self.priority_fees.get(slot) {
                    let account_priority_fees = slot_priority_fees.account_fees.get(account);
                    if let Some(account_priority_fees) = account_priority_fees {
                        if include_vote {
                            fees.extend_from_slice(&account_priority_fees.vote_fees);
                        }
                        fees.extend_from_slice(&account_priority_fees.non_vote_fees);
                    }
                }
            }
            micro_lamport_priority_fee_estimates =
                estimate_max_values(&mut fees, micro_lamport_priority_fee_estimates);
        }
        micro_lamport_priority_fee_estimates
    }
}

fn estimate_max_values(
    mut fees: &mut Vec<f64>,
    mut estimates: MicroLamportPriorityFeeEstimates,
) -> MicroLamportPriorityFeeEstimates {
    estimates.min = percentile(&mut fees, 0).max(estimates.min);
    estimates.low = percentile(&mut fees, 25).max(estimates.low);
    estimates.medium = percentile(&mut fees, 50).max(estimates.medium);
    estimates.high = percentile(&mut fees, 75).max(estimates.high);
    estimates.very_high = percentile(&mut fees, 95).max(estimates.very_high);
    estimates.unsafe_max = percentile(&mut fees, 100).max(estimates.unsafe_max);
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

    let val = if r.fract() == 0.0 {
        values[r as usize - 1]
    } else {
        let ri = r.trunc() as usize - 1;
        let rf = r.fract();
        values[ri] + rf * (values[ri + 1] - values[ri])
    };
    val
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
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
        let estimates = tracker.calculation1(&vec![accounts.get(0).unwrap().clone()], false, &None);
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
        let estimates = tracker.calculation2(&vec![accounts.get(0).unwrap().clone()], false, &None);
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
            tracker.push_priority_fee_for_txn(
                i as Slot,
                accounts.clone(),
                fee as u64,
                false,
            );
        }

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = tracker.calculation1(&vec![accounts.get(0).unwrap().clone()], false, &None);
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

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = tracker.calculation2(&vec![accounts.get(0).unwrap().clone()], false, &None);
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
            tracker.push_priority_fee_for_txn(
                i as Slot,
                accounts.clone(),
                fee as u64,
                false,
            );
        }

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = tracker.calculation1(&vec![accounts.get(0).unwrap().clone()], false, &None);
        let expected_low_fee = 25.0;
        let expected_medium_fee = 50.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 95.0;
        assert_ne!(estimates.low, expected_low_fee);
        assert_ne!(estimates.medium, expected_medium_fee);
        assert_ne!(estimates.high, expected_high_fee);
        assert_ne!(estimates.very_high, expected_very_high_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = tracker.calculation2(&vec![accounts.get(0).unwrap().clone()], false, &None);
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
    async fn test_with_transactions_for_different_accounts() {
        // same test as above but with an extra slot to throw off the value
        init_metrics();
        let tracker = PriorityFeeTracker::new(100);

        // adding set of fees at beginning that would mess up percentiles if they were not removed
        let account_1 = Pubkey::new_unique();
        let account_2 = Pubkey::new_unique();
        let account_3 = Pubkey::new_unique();
        let account_4 = Pubkey::new_unique();

        // Simulate adding the fixed fees as both account-specific and transaction fees
        for val in 0..100 {
            match val {
                0..=24 => tracker.push_priority_fee_for_txn(
                    val as Slot,
                    vec![account_1],
                    val as u64,
                    false,
                ),
                25..=49 => tracker.push_priority_fee_for_txn(
                    val as Slot,
                    vec![account_2],
                    val as u64,
                    false,
                ),
                50..=74 => tracker.push_priority_fee_for_txn(
                    val as Slot,
                    vec![account_3],
                    val as u64,
                    false,
                ),
                75..=99 => tracker.push_priority_fee_for_txn(
                    val as Slot,
                    vec![account_4],
                    val as u64,
                    false,
                ),
                _ => {}
            }
        }

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = tracker.calculation1(&vec![account_1, account_4], false, &None);
        let expected_min_fee = 0.0;
        let expected_low_fee = 24.75;
        let expected_medium_fee = 49.5;
        let expected_high_fee = 86.75;
        let expected_very_high_fee = 96.55;
        let expected_max_fee = 99.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = tracker.calculation2(&vec![account_1, account_4], false, &None);
        let expected_min_fee = 75.0;
        let expected_low_fee = 81.0;
        let expected_medium_fee = 87.0;
        let expected_high_fee = 93.0;
        let expected_very_high_fee = 97.8;
        let expected_max_fee = 99.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);
    }

    #[tokio::test]
    async fn test_with_multiple_transactions_for_different_slots() {
        // same test as above but with an extra slot to throw off the value
        init_metrics();
        let tracker = PriorityFeeTracker::new(10);

        // adding set of fees at beginning that would mess up percentiles if they were not removed
        let account_1 = Pubkey::new_unique();
        let account_2 = Pubkey::new_unique();
        let account_3 = Pubkey::new_unique();
        let account_4 = Pubkey::new_unique();

        // Simulate adding the fixed fees as both account-specific and transaction fees
        for val in 0..100 {
            let slot = val / 10; // divide between 10 slots to have more than 1 transaction per slot
                                 // also evenly distribute the orders
            match val {
                val if 0 == val % 4 => tracker.push_priority_fee_for_txn(
                    slot as Slot,
                    vec![account_1],
                    val as u64,
                    false,
                ),
                val if 1 == val % 4 => tracker.push_priority_fee_for_txn(
                    slot as Slot,
                    vec![account_2],
                    val as u64,
                    false,
                ),
                val if 2 == val % 4 => tracker.push_priority_fee_for_txn(
                    slot as Slot,
                    vec![account_3],
                    val as u64,
                    false,
                ),
                val if 3 == val % 4 => tracker.push_priority_fee_for_txn(
                    slot as Slot,
                    vec![account_4],
                    val as u64,
                    false,
                ),
                _ => {}
            }
        }

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = tracker.calculation1(&vec![account_1, account_4], false, &None);
        let expected_min_fee = 0.0;
        let expected_low_fee = 24.75;
        let expected_medium_fee = 49.5;
        let expected_high_fee = 74.25;
        let expected_very_high_fee = 94.05;
        let expected_max_fee = 99.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);

        // Now test the fee estimates for a known priority level, let's say medium (50th percentile)
        let estimates = tracker.calculation2(&vec![account_1, account_4], false, &None);
        let expected_min_fee = 3.0;
        let expected_low_fee = 27.0;
        let expected_medium_fee = 51.0;
        let expected_high_fee = 75.0;
        let expected_very_high_fee = 94.19999999999999;
        let expected_max_fee = 99.0;
        assert_eq!(estimates.min, expected_min_fee);
        assert_eq!(estimates.low, expected_low_fee);
        assert_eq!(estimates.medium, expected_medium_fee);
        assert_eq!(estimates.high, expected_high_fee);
        assert_eq!(estimates.very_high, expected_very_high_fee);
        assert_eq!(estimates.unsafe_max, expected_max_fee);
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
        let estimates = tracker.calculation1(&vec![accounts.get(0).unwrap().clone()], false, &None);
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
        let estimates = tracker.calculation2(&vec![accounts.get(0).unwrap().clone()], false, &None);
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

    #[test]
    fn test_constructing_accounts() {
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
