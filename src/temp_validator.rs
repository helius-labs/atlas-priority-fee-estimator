use cadence_macros::statsd_count;
use crate::priority_fee::PriorityLevel;
use jsonrpsee::core::__reexports::serde_json;
use jsonrpsee::server::middleware::rpc::RpcServiceT;
use jsonrpsee::types::Request;
use serde::{Deserialize, Serialize};
use solana_transaction_status::UiTransactionEncoding;
use tracing::info;

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(
    rename_all(serialize = "camelCase", deserialize = "camelCase"),
    deny_unknown_fields
)]
struct GetPriorityFeeEstimateOptionsFake {
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

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(
    rename_all(serialize = "camelCase", deserialize = "camelCase"),
    deny_unknown_fields
)]
struct GetPriorityFeeEstimateRequestFake {
    transaction: Option<String>,       // estimate fee for a txn
    account_keys: Option<Vec<String>>, // estimate fee for a list of accounts
    options: Option<GetPriorityFeeEstimateOptionsFake>,
}

/// RPC logger layer.
#[derive(Copy, Clone, Debug)]
pub struct RpcValidatorLayer;

impl RpcValidatorLayer {
    /// Create a new logging layer.
    pub fn new() -> Self {
        Self
    }
}

impl<S> tower::Layer<S> for RpcValidatorLayer {
    type Service = RpcValidator<S>;

    fn layer(&self, service: S) -> Self::Service {
        RpcValidator { service }
    }
}

/// A middleware that logs each RPC call and response.
#[derive(Debug)]
pub struct RpcValidator<S> {
    service: S,
}

impl<'a, S> RpcServiceT<'a> for RpcValidator<S>
where
    S: RpcServiceT<'a> + Send + Sync,
{
    type Future = S::Future;

    fn call(&self, req: Request<'a>) -> Self::Future {
        if let Some(params) = &req.params {
            if let Err(err_val) = serde_json::from_str::<Vec<GetPriorityFeeEstimateRequestFake>>(params.get()) {
                statsd_count!("rpc_payload_parse_failed", 1);
                info!("RPC parse error: {}, {}", err_val, params);
            }
        }

        self.service.call(req)
    }
}
