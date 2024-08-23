use std::{
    collections::{HashMap, HashSet},
    fmt,
    str::FromStr,
    sync::Arc,
    time::Instant,
};

use crate::{
    errors::invalid_request,
    priority_fee::{MicroLamportPriorityFeeEstimates, PriorityFeeTracker, PriorityLevel},
    solana::solana_rpc::decode_and_deserialize,
};
use cadence_macros::{statsd_count, statsd_time};
use jsonrpsee::{
    core::{async_trait, RpcResult},
    proc_macros::rpc,
    types::ErrorObjectOwned,
};
use serde::{Deserialize, Serialize};
use solana_account_decoder::parse_address_lookup_table::{
    parse_address_lookup_table, LookupTableAccountType,
};
use solana_client::rpc_client::RpcClient;
use solana_sdk::{pubkey::Pubkey, transaction::VersionedTransaction};
use solana_transaction_status::UiTransactionEncoding;
use tracing::info;

pub struct AtlasPriorityFeeEstimator {
    pub priority_fee_tracker: Arc<PriorityFeeTracker>,
    pub rpc_client: Option<RpcClient>,
    pub max_lookback_slots: usize,
}

impl fmt::Debug for AtlasPriorityFeeEstimator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AtlasPriorityFeeEstimator")
            .field("priority_fee_tracker", &self.priority_fee_tracker)
            .field("rpc_client", &"RpcClient { ... }") // RpcClient does not implement Debug
            .finish()
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(
    rename_all(serialize = "camelCase", deserialize = "camelCase"),
    deny_unknown_fields
)]
pub struct GetPriorityFeeEstimateRequest {
    pub transaction: Option<String>,       // estimate fee for a txn
    pub account_keys: Option<Vec<String>>, // estimate fee for a list of accounts
    pub options: Option<GetPriorityFeeEstimateOptions>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(
    rename_all(serialize = "camelCase", deserialize = "camelCase"),
    deny_unknown_fields
)]
pub struct GetPriorityFeeEstimateOptions {
    // controls input txn encoding
    pub transaction_encoding: Option<UiTransactionEncoding>,
    // controls custom priority fee level response
    pub priority_level: Option<PriorityLevel>, // Default to MEDIUM
    pub include_all_priority_fee_levels: Option<bool>, // Include all priority level estimates in the response
    #[serde()]
    pub lookback_slots: Option<u32>, // how many slots to look back on, default 50, min 1, max 300
    pub include_vote: Option<bool>, // include vote txns in the estimate
    // returns recommended fee, incompatible with custom controls. Currently the recommended fee is the median fee excluding vote txns
    pub recommended: Option<bool>, // return the recommended fee (median fee excluding vote txns)
}

#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all(serialize = "camelCase", deserialize = "camelCase"))]
pub struct GetPriorityFeeEstimateResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority_fee_estimate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority_fee_levels: Option<MicroLamportPriorityFeeEstimates>,
}

#[rpc(server)]
pub trait AtlasPriorityFeeEstimatorRpc {
    #[method(name = "health")]
    fn health(&self) -> String;
    #[method(name = "getPriorityFeeEstimate")]
    fn get_priority_fee_estimate(
        &self,
        get_priority_fee_estimate_request: GetPriorityFeeEstimateRequest,
    ) -> RpcResult<GetPriorityFeeEstimateResponse>;

    #[method(name = "getTestPriorityFeeEstimate")]
    fn get_test_priority_fee_estimate(
        &self,
        get_priority_fee_estimate_request: GetPriorityFeeEstimateRequest,
    ) -> RpcResult<GetPriorityFeeEstimateResponse>;
}

fn validate_get_priority_fee_estimate_request(
    get_priority_fee_estimate_request: &GetPriorityFeeEstimateRequest,
) -> Option<ErrorObjectOwned> {
    if get_priority_fee_estimate_request.transaction.is_some()
        && get_priority_fee_estimate_request.account_keys.is_some()
    {
        return Some(invalid_request(
            "transaction and account_keys cannot both be provided ",
        ));
    }
    if let Some(account_keys) = get_priority_fee_estimate_request.account_keys.clone() {
        if account_keys.len() > 500 {
            return Some(invalid_request("number of account_keys must be <= 500"));
        }
    }
    if let Some(options) = get_priority_fee_estimate_request.options.clone() {
        let custom_controls_set = options.priority_level.is_some()
            || options.include_all_priority_fee_levels.is_some()
            || options.lookback_slots.is_some()
            || options.include_vote.is_some();
        let recommended_set = options.recommended.is_some();
        if custom_controls_set && recommended_set {
            return Some(invalid_request(
                "recommended cannot be used with priority_level, include_all_priority_fee_levels, lookback_slots, include_vote",
            ));
        }
    }
    None
}

/// returns account keys from transaction
fn get_from_account_keys(transaction: &VersionedTransaction) -> Vec<String> {
    transaction
        .message
        .static_account_keys()
        .iter()
        .map(|key| key.to_string())
        .collect()
}

/// gets address lookup tables and then fetches them from an RPC. Returns
/// the accounts asked for
fn get_from_address_lookup_tables(
    rpc_client: &RpcClient,
    transaction: &VersionedTransaction,
) -> Vec<String> {
    let mut account_keys = vec![];
    let address_table_lookups: Option<&[solana_sdk::message::v0::MessageAddressTableLookup]> =
        transaction.message.address_table_lookups();
    if let Some(address_table_lookups) = address_table_lookups {
        let start = Instant::now();
        let mut lookup_table_indices = HashMap::new();
        let address_table_lookup_accounts: Vec<Pubkey> = address_table_lookups
            .iter()
            .map(|a| {
                let indices: HashSet<u8> = HashSet::from_iter(
                    vec![a.readonly_indexes.clone(), a.writable_indexes.clone()].concat(),
                );
                lookup_table_indices.insert(a.account_key.to_string(), indices);
                a.account_key
            })
            .collect();
        statsd_count!(
            "get_from_address_lookup_tables_num_accounts",
            address_table_lookup_accounts.len() as i64
        );
        let accounts = rpc_client.get_multiple_accounts(address_table_lookup_accounts.as_slice());
        match accounts {
            Ok(accounts) => {
                for (i, account) in accounts.into_iter().enumerate() {
                    if account.is_none() {
                        continue;
                    }
                    let account = account.unwrap();
                    let account_pubkey = address_table_lookup_accounts[i];
                    let indices = lookup_table_indices.get(&account_pubkey.to_string());
                    if indices.is_none() {
                        continue;
                    }
                    let indices = indices.unwrap();
                    let address_lookup_table = parse_address_lookup_table(&account.data);
                    match address_lookup_table {
                        Ok(address_lookup_table) => {
                            if let LookupTableAccountType::LookupTable(address_lookup_table) =
                                address_lookup_table
                            {
                                for (j, account_in_lookup_table) in
                                    address_lookup_table.addresses.iter().enumerate()
                                {
                                    if indices.contains(&(j as u8)) {
                                        account_keys.push(account_in_lookup_table.to_string());
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            info!(
                                "error parsing address lookup table {}: {:?}",
                                account_pubkey.to_string(),
                                e
                            );
                        }
                    }
                }
            }
            Err(e) => {
                info!("error getting accounts: {:?}", e);
            }
        }
        statsd_time!("get_from_address_lookup_tables", start.elapsed());
    }
    account_keys
}

fn get_accounts(
    rpc_client: &Option<RpcClient>,
    get_priority_fee_estimate_request: GetPriorityFeeEstimateRequest,
) -> RpcResult<Vec<String>> {
    if let Some(account_keys) = get_priority_fee_estimate_request.account_keys {
        return Ok(account_keys);
    }
    if let Some(rpc_client) = rpc_client {
        if let Some(transaction) = get_priority_fee_estimate_request.transaction {
            let tx_encoding = if let Some(options) = get_priority_fee_estimate_request.options {
                options
                    .transaction_encoding
                    .unwrap_or(UiTransactionEncoding::Base58)
            } else {
                UiTransactionEncoding::Base58
            };
            let binary_encoding = tx_encoding.into_binary_encoding().ok_or_else(|| {
                invalid_request(&format!(
                    "unsupported encoding: {tx_encoding}. Supported encodings: base58, base64"
                ))
            })?;
            let (_, transaction) =
                decode_and_deserialize::<VersionedTransaction>(transaction, binary_encoding)?;
            let account_keys: Vec<String> = vec![
                get_from_account_keys(&transaction),
                get_from_address_lookup_tables(rpc_client, &transaction),
            ]
            .concat();
            return Ok(account_keys);
        }
    }
    Ok(vec![])
}

#[async_trait]
impl AtlasPriorityFeeEstimatorRpcServer for AtlasPriorityFeeEstimator {
    fn health(&self) -> String {
        "ok".to_string()
    }
    fn get_priority_fee_estimate(
        &self,
        get_priority_fee_estimate_request: GetPriorityFeeEstimateRequest,
    ) -> RpcResult<GetPriorityFeeEstimateResponse> {
        let algo = |accounts: Vec<Pubkey>,
             include_vote: bool,
             lookback_period: Option<u32>|
             -> MicroLamportPriorityFeeEstimates {
                self.priority_fee_tracker.get_priority_fee_estimates(
                    accounts,
                    include_vote,
                    lookback_period,
                    true
                )
            };
        self.execute_priority_fee_estimate_coordinator(get_priority_fee_estimate_request, algo)
    }

    fn get_test_priority_fee_estimate(
        &self,
        get_priority_fee_estimate_request: GetPriorityFeeEstimateRequest,
    ) -> RpcResult<GetPriorityFeeEstimateResponse> {
        let algo = |accounts: Vec<Pubkey>,
                    include_vote: bool,
                    lookback_period: Option<u32>|
                    -> MicroLamportPriorityFeeEstimates {
            self.priority_fee_tracker.get_priority_fee_estimates(
                accounts,
                include_vote,
                lookback_period,
                false
            )
        };
        self.execute_priority_fee_estimate_coordinator(get_priority_fee_estimate_request, algo)
    }
}

impl AtlasPriorityFeeEstimator {
    pub fn new(
        priority_fee_tracker: Arc<PriorityFeeTracker>,
        rpc_url: String,
        max_lookback_slots: usize,
    ) -> Self {
        let server = AtlasPriorityFeeEstimator {
            priority_fee_tracker,
            rpc_client: Some(RpcClient::new(rpc_url)),
            max_lookback_slots,
        };
        server
    }

    fn execute_priority_fee_estimate_coordinator<F>(
        &self,
        get_priority_fee_estimate_request: GetPriorityFeeEstimateRequest,
        algo_fn: F,
    ) -> RpcResult<GetPriorityFeeEstimateResponse>
    where
        F: Fn(Vec<Pubkey>, bool, Option<u32>) -> MicroLamportPriorityFeeEstimates,
    {
        let options = get_priority_fee_estimate_request.options.clone();
        let reason = validate_get_priority_fee_estimate_request(&get_priority_fee_estimate_request);
        if let Some(reason) = reason {
            return Err(reason);
        }
        let accounts = get_accounts(&self.rpc_client, get_priority_fee_estimate_request);
        if let Err(e) = accounts {
            return Err(e);
        }
        let accounts = accounts
            .unwrap()
            .iter()
            .filter_map(|a| Pubkey::from_str(a).ok())
            .collect();
        let lookback_slots = options.clone().map(|o| o.lookback_slots).flatten();
        if let Some(lookback_slots) = lookback_slots {
            if lookback_slots < 1 || lookback_slots as usize > self.max_lookback_slots {
                return Err(invalid_request("lookback_slots must be between 1 and 150"));
            }
        }
        let include_vote = should_include_vote(&options);
        let priority_fee_levels = algo_fn(accounts, include_vote, lookback_slots);
        if let Some(options) = options.clone() {
            if options.include_all_priority_fee_levels == Some(true) {
                return Ok(GetPriorityFeeEstimateResponse {
                    priority_fee_estimate: None,
                    priority_fee_levels: Some(priority_fee_levels),
                });
            }
            if let Some(priority_level) = options.priority_level {
                let priority_fee = match priority_level {
                    PriorityLevel::Min => priority_fee_levels.min,
                    PriorityLevel::Low => priority_fee_levels.low,
                    PriorityLevel::Medium => priority_fee_levels.medium,
                    PriorityLevel::High => priority_fee_levels.high,
                    PriorityLevel::VeryHigh => priority_fee_levels.very_high,
                    PriorityLevel::UnsafeMax => priority_fee_levels.unsafe_max,
                    PriorityLevel::Default => priority_fee_levels.medium,
                };
                return Ok(GetPriorityFeeEstimateResponse {
                    priority_fee_estimate: Some(priority_fee),
                    priority_fee_levels: None,
                });
            }
        }
        let recommended = options.map_or(false, |o: GetPriorityFeeEstimateOptions| {
            o.recommended.unwrap_or(false)
        });
        let priority_fee = if recommended {
            get_recommended_fee(priority_fee_levels)
        } else {
            priority_fee_levels.medium
        };
        Ok(GetPriorityFeeEstimateResponse {
            priority_fee_estimate: Some(priority_fee),
            priority_fee_levels: None,
        })
    }
}

// default to true for backwards compatibility. Recommended fee does not include vote txns
fn should_include_vote(options: &Option<GetPriorityFeeEstimateOptions>) -> bool {
    if let Some(options) = options {
        return options.include_vote.unwrap_or(false);
    }
    true
}

const MIN_RECOMMENDED_PRIORITY_FEE: f64 = 10_000.0;

// Safety buffer on the recommended fee to cover cases where the priority fee is increasing over each block.
const RECOMMENDED_FEE_SAFETY_BUFFER: f64 = 0.05;

pub fn get_recommended_fee(priority_fee_levels: MicroLamportPriorityFeeEstimates) -> f64 {
    let recommended = if priority_fee_levels.medium > MIN_RECOMMENDED_PRIORITY_FEE {
        priority_fee_levels.medium
    } else {
        MIN_RECOMMENDED_PRIORITY_FEE
    };
    let recommended_with_buffer = recommended * (1.0 + RECOMMENDED_FEE_SAFETY_BUFFER);
    recommended_with_buffer.ceil()
}

#[cfg(test)]
mod tests {
    use crate::priority_fee::PriorityFeeTracker;
    use crate::rpc_server::{
        AtlasPriorityFeeEstimator, AtlasPriorityFeeEstimatorRpcServer,
        GetPriorityFeeEstimateOptions, GetPriorityFeeEstimateRequest,
    };
    use cadence::{NopMetricSink, StatsdClient};
    use jsonrpsee::core::Cow;
    use jsonrpsee::core::__reexports::serde_json;
    use jsonrpsee::core::__reexports::serde_json::value::RawValue;
    use jsonrpsee::types::{Id, Request, TwoPointZero};
    use solana_sdk::clock::Slot;
    use solana_sdk::pubkey::Pubkey;
    use std::sync::Arc;

    #[tokio::test(flavor = "current_thread")]
    async fn test_calculating_fees_with_all_options_none() {
        prep_statsd();

        let acc1 = Pubkey::new_unique();
        let acc2 = Pubkey::new_unique();
        let tracker = PriorityFeeTracker::new(150);
        tracker.push_priority_fee_for_txn(1 as Slot, vec![acc1, acc2], 100u64, false);

        let server = AtlasPriorityFeeEstimator {
            priority_fee_tracker: Arc::new(tracker),
            rpc_client: None,
            max_lookback_slots: 150,
        };

        let result = server.get_priority_fee_estimate(GetPriorityFeeEstimateRequest {
            account_keys: Some(vec![acc1.to_string(), acc2.to_string()]),
            options: Some(GetPriorityFeeEstimateOptions::default()),
            ..Default::default()
        });
        let resp = result.unwrap();
        assert_eq!(resp.priority_fee_estimate, Some(100.0));
        assert!(resp.priority_fee_levels.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_calculating_fees_with_no_options() {
        prep_statsd();

        let acc1 = Pubkey::new_unique();
        let acc2 = Pubkey::new_unique();
        let tracker = PriorityFeeTracker::new(150);
        tracker.push_priority_fee_for_txn(1 as Slot, vec![acc1, acc2], 100u64, false);

        let server = AtlasPriorityFeeEstimator {
            priority_fee_tracker: Arc::new(tracker),
            rpc_client: None,
            max_lookback_slots: 150,
        };

        let result = server.get_priority_fee_estimate(GetPriorityFeeEstimateRequest {
            account_keys: Some(vec![acc1.to_string(), acc2.to_string()]),
            ..Default::default()
        });
        let resp = result.unwrap();
        assert_eq!(resp.priority_fee_estimate, Some(100.0));
        assert!(resp.priority_fee_levels.is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn test_calculating_all_fees() {
        prep_statsd();

        let acc1 = Pubkey::new_unique();
        let acc2 = Pubkey::new_unique();
        let tracker = PriorityFeeTracker::new(150);
        tracker.push_priority_fee_for_txn(1 as Slot, vec![acc1], 100u64, false);
        tracker.push_priority_fee_for_txn(1 as Slot, vec![acc2], 200u64, false);

        let server = AtlasPriorityFeeEstimator {
            priority_fee_tracker: Arc::new(tracker),
            rpc_client: None,
            max_lookback_slots: 150,
        };

        let result = server.get_priority_fee_estimate(GetPriorityFeeEstimateRequest {
            account_keys: Some(vec![acc1.to_string(), acc2.to_string()]),
            options: Some(GetPriorityFeeEstimateOptions {
                include_all_priority_fee_levels: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        let resp = result.unwrap();
        let levels = resp.priority_fee_levels.unwrap();
        assert_eq!(levels.min, 100.0);
        assert_eq!(levels.low, 125.0);
        assert_eq!(levels.medium, 150.0);
        assert_eq!(levels.high, 175.0);
        assert_eq!(levels.very_high, 195.0);
        assert_eq!(levels.unsafe_max, 200.0);
        assert!(resp.priority_fee_estimate.is_none());
    }
    #[tokio::test(flavor = "current_thread")]
    async fn test_calculating_recommended_given_very_low_calculated_fee() {
        prep_statsd();

        let acc1 = Pubkey::new_unique();
        let acc2 = Pubkey::new_unique();
        let tracker = PriorityFeeTracker::new(150);
        tracker.push_priority_fee_for_txn(1 as Slot, vec![acc1], 100u64, false);
        tracker.push_priority_fee_for_txn(1 as Slot, vec![acc2], 200u64, false);

        let server = AtlasPriorityFeeEstimator {
            priority_fee_tracker: Arc::new(tracker),
            rpc_client: None,
            max_lookback_slots: 150,
        };

        let result = server.get_priority_fee_estimate(GetPriorityFeeEstimateRequest {
            account_keys: Some(vec![acc1.to_string(), acc2.to_string()]),
            options: Some(GetPriorityFeeEstimateOptions {
                recommended: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        });
        let resp = result.unwrap();
        assert!(resp.priority_fee_levels.is_none());
        assert_eq!(resp.priority_fee_estimate, Some(10500.0));
    }

    #[test]
    fn test_parsing_wrong_fields() {
        for (param, error) in bad_params() {
            let json_val = format!("{{\"jsonrpc\": \"2.0\",\"id\": \"1\", \"method\": \"getPriorityFeeEstimate\", \"params\": {param} }}");
            let res = serde_json::from_str::<jsonrpsee::types::Request>(json_val.as_str());
            let res = res.unwrap();
            assert_request(&res, Id::Str(Cow::const_str("1")), "getPriorityFeeEstimate");

            let params: serde_json::error::Result<GetPriorityFeeEstimateRequest> =
                serde_json::from_str(res.params.map(RawValue::get).unwrap());
            assert!(params.is_err());
            assert_eq!(params.err().unwrap().to_string(), error, "testing {param}");
        }
    }

    fn prep_statsd() {
        let systemd_client = StatsdClient::builder("test", NopMetricSink)
            .with_error_handler(|e| eprintln!("metric error: {}", e))
            .build();
        cadence_macros::set_global_default(systemd_client);
    }

    fn assert_request<'a>(request: &Request<'a>, id: Id<'a>, method: &str) {
        assert_eq!(request.jsonrpc, TwoPointZero);
        assert_eq!(request.id, id);
        assert_eq!(request.method, method);
    }

    fn bad_params<'a>() -> Vec<(&'a str, &'a str)> {
        vec![
            (r#"{"transactions": null}"#,"unknown field `transactions`, expected one of `transaction`, `accountKeys`, `options` at line 1 column 15"),
            (r#"{"account_keys": null}"#,"unknown field `account_keys`, expected one of `transaction`, `accountKeys`, `options` at line 1 column 15"),
            (r#"{"accountkeys": null}"#,"unknown field `accountkeys`, expected one of `transaction`, `accountKeys`, `options` at line 1 column 14"),
            (r#"{"accountKeys": [1, 2]}"#, "invalid type: integer `1`, expected a string at line 1 column 18"),
            (r#"{"option": null}"#, "unknown field `option`, expected one of `transaction`, `accountKeys`, `options` at line 1 column 9"),
            (r#"{"options": {"transaction_encoding":null}}"#, "unknown field `transaction_encoding`, expected one of `transactionEncoding`, `priorityLevel`, `includeAllPriorityFeeLevels`, `lookbackSlots`, `includeVote`, `recommended` at line 1 column 35"),
            (r#"{"options": {"priorityLevel":"HIGH"}}"#, "unknown variant `HIGH`, expected one of `Min`, `Low`, `Medium`, `High`, `VeryHigh`, `UnsafeMax`, `Default` at line 1 column 35"),
            (r#"{"options": {"includeAllPriorityFeeLevels":"no"}}"#, "invalid type: string \"no\", expected a boolean at line 1 column 47"),
            (r#"{"options": {"lookbackSlots":"no"}}"#,  "invalid type: string \"no\", expected u32 at line 1 column 33"),
            (r#"{"options": {"lookbackSlots":"-1"}}"#,  "invalid type: string \"-1\", expected u32 at line 1 column 33"),
        ]
    }
}
