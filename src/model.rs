use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use solana_sdk::clock::Slot;
use solana_sdk::pubkey::Pubkey;
use std::collections::HashMap;
use std::fmt::{Debug, Display, Formatter};

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
        match s.trim().to_uppercase().as_str() {
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

pub type Percentile = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransactionVersion {
    LegacyV0,
    V1,
}

impl TransactionVersion {
    pub const ALL: [Self; 2] = [Self::LegacyV0, Self::V1];

    pub const fn as_tag(self) -> &'static str {
        match self {
            Self::LegacyV0 => "legacy_v0",
            Self::V1 => "v1",
        }
    }
}

#[derive(Debug, Clone, PartialOrd, PartialEq, Eq, Ord, Hash)]
pub enum DataType<'a> {
    Global,
    AllAccounts,
    Account(&'a Pubkey),
}

impl Display for DataType<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            DataType::Global => f.write_str("Global"),
            DataType::AllAccounts => f.write_str("All Accounts"),
            DataType::Account(pubkey) => f.write_str(pubkey.to_string().as_str()),
        }
    }
}

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

#[derive(Deserialize, Serialize, Debug, Clone, Default)]
#[serde(rename_all(serialize = "camelCase", deserialize = "camelCase"))]
pub struct MicroLamportPriorityFeeDetails {
    pub estimates: MicroLamportPriorityFeeEstimates,
    pub mean: f64,
    pub stdev: f64,
    pub skew: f64,
    pub count: usize,
}

#[derive(Debug, Clone)]
pub struct Fees {
    pub(crate) non_vote_fees: Vec<f64>,
    pub(crate) vote_fees: Vec<f64>,
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

#[derive(Debug, Clone)]
pub struct SlotPriorityFees {
    pub(crate) slot: Slot,
    pub(crate) fees: Fees,
    pub(crate) account_fees: DashMap<Pubkey, Fees>,
    pub(crate) version_fees: HashMap<TransactionVersion, Fees>,
}

impl SlotPriorityFees {
    #[cfg(test)]
    pub(crate) fn new(slot: Slot, accounts: Vec<Pubkey>, priority_fee: u64, is_vote: bool) -> Self {
        Self::new_with_version(
            slot,
            accounts,
            priority_fee,
            is_vote,
            TransactionVersion::LegacyV0,
        )
    }

    pub(crate) fn new_with_version(
        slot: Slot,
        accounts: Vec<Pubkey>,
        priority_fee: u64,
        is_vote: bool,
        version: TransactionVersion,
    ) -> Self {
        let account_fees = DashMap::new();
        let fees = Fees::new(priority_fee as f64, is_vote);
        let version_fees = HashMap::from([(version, fees.clone())]);
        for account in accounts {
            account_fees.insert(account, fees.clone());
        }
        Self {
            slot,
            fees,
            account_fees,
            version_fees,
        }
    }

    pub(crate) fn add_version_fee(
        &mut self,
        version: TransactionVersion,
        priority_fee: u64,
        is_vote: bool,
    ) {
        self.version_fees
            .entry(version)
            .and_modify(|fees| fees.add_fee(priority_fee as f64, is_vote))
            .or_insert_with(|| Fees::new(priority_fee as f64, is_vote));
    }
}

pub type PriorityFeesBySlot = DashMap<Slot, SlotPriorityFees>;
