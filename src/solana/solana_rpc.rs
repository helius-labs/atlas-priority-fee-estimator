// code copied out of solana repo because of version conflicts

use base64::prelude::BASE64_STANDARD;
use base64::Engine;
use jsonrpsee::core::RpcResult;
use solana_message::v1::MAX_TRANSACTION_SIZE;
use solana_transaction::versioned::VersionedTransaction;
use solana_transaction_status::TransactionBinaryEncoding;

use crate::errors::invalid_request;

// SIMD-0296 raised the wire limit from 1232 to 4096 bytes, reachable only via
// the v1 format (SIMD-0385). Sized off MAX_TRANSACTION_SIZE so these track it.
const MAX_BASE58_SIZE: usize = MAX_TRANSACTION_SIZE * 2; // base58 expands ~1.37x
const MAX_BASE64_SIZE: usize = MAX_TRANSACTION_SIZE.div_ceil(3) * 4;
/// Decodes a wire transaction. `wincode` dispatches on the leading byte, so a
/// single call covers legacy, V0, and V1 (SIMD-0385) — the latter is not
/// readable via bincode, which misreads its `0x81` version byte as a signature
/// count.
pub fn decode_and_deserialize(
    encoded: String,
    encoding: TransactionBinaryEncoding,
) -> RpcResult<(Vec<u8>, VersionedTransaction)> {
    let wire_output = match encoding {
        TransactionBinaryEncoding::Base58 => {
            if encoded.len() > MAX_BASE58_SIZE {
                return Err(invalid_request("base58 encoded too large"));
            }
            bs58::decode(encoded)
                .into_vec()
                .map_err(|e| invalid_request(format!("invalid base58 encoding: {e:?}").as_str()))?
        }
        TransactionBinaryEncoding::Base64 => {
            if encoded.len() > MAX_BASE64_SIZE {
                return Err(invalid_request("base64 encoded too large"));
            }
            BASE64_STANDARD
                .decode(encoded)
                .map_err(|e| invalid_request(&format!("invalid base64 encoding: {e:?}")))?
        }
    };
    if wire_output.len() > MAX_TRANSACTION_SIZE {
        return Err(invalid_request("decoded too large"));
    }
    let transaction = wincode::deserialize::<VersionedTransaction>(&wire_output)
        .map_err(|err| invalid_request(&format!("failed to deserialize: {err}")))?;
    Ok((wire_output, transaction))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_transaction::versioned::VersionedTransaction;

    // Both vectors are the same SOL transfer: 42,000 CU limit at 10,000
    // micro-lamports/CU (420 lamports total priority).
    const V1_TRANSFER: &str = "gQEAAQcAAABHDtRHaN4h+Qz/NWrSLWqQy0Ll/OZ997p367TlR+AvbwEDCQzu6znzPcjsG+gTC/86u2toD3DdQweOolXhtp3f+rQGm4hX/quBhPtof2NGGMA12sQ53BrrO1WYoPAAAAAAAQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAApAEAAAAAAAAQpAAAAgIMAAABAgAAAOgDAAAAAAAApLMYd8TpAEgv1M4fT21eDi5Mu0gKcD7B+vX9RTFWPziVKWApM3s3np5zEA+nwI5ZDiIc0JwBfJYNLQZulrWnAg==";
    const V0_TRANSFER: &str = "AbHiaSQZX+4C3Uz+h6T9D+4dDf6Yn0+0muIOSOaCv2erKTxWtUUmtpnUySMA+R5utWI6VglGwUsFlunKwv/g9gOAAQACBAkM7us58z3I7BvoEwv/OrtraA9w3UMHjqJV4bad3/q0BpuIV/6rgYT7aH9jRhjANdrEOdwa6ztVmKDwAAAAAAEAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAMGRm/lIRcy/+ytunLDm+e8jOW7xfcSayxDmzpAAAAARw7UR2jeIfkM/zVq0i1qkMtC5fzmffe6d+u05UfgL28DAwAJAxAnAAAAAAAAAwAFAhCkAAACAgABDAIAAADoAwAAAAAAAAA=";

    fn decode(encoded: &str) -> VersionedTransaction {
        let (_, tx) = decode_and_deserialize(
            encoded.to_string(),
            TransactionBinaryEncoding::Base64,
        )
        .expect("vector should deserialize");
        tx
    }

    #[test]
    fn test_decodes_v1_transaction() {
        let tx = decode(V1_TRANSFER);
        // V1 carries compute budget in the message config, not in
        // ComputeBudgetProgram instructions, and has no lookup tables.
        assert!(tx.message.address_table_lookups().is_none());
        assert!(!tx.message.static_account_keys().is_empty());
    }

    #[test]
    fn test_decodes_v0_transaction_unchanged() {
        let tx = decode(V0_TRANSFER);
        assert!(!tx.message.static_account_keys().is_empty());
    }

    #[test]
    fn test_rejects_oversized_payload() {
        let too_big = "A".repeat(MAX_BASE64_SIZE + 1);
        let err = decode_and_deserialize(
            too_big,
            TransactionBinaryEncoding::Base64,
        )
        .expect_err("oversized payload should be rejected");
        assert!(err.message().contains("too large"));
    }

    #[test]
    fn test_accepts_payload_above_old_packet_limit() {
        // 1232 bytes was the old hard ceiling and would have been rejected
        // outright; anything up to MAX_TRANSACTION_SIZE must now clear the gate.
        let above_old_limit = BASE64_STANDARD.encode(vec![0u8; MAX_TRANSACTION_SIZE]);
        assert!(above_old_limit.len() > 1644, "must exceed the old base64 cap");

        let result = decode_and_deserialize(above_old_limit, TransactionBinaryEncoding::Base64);
        assert!(
            result.is_ok(),
            "payload within MAX_TRANSACTION_SIZE must not be size-rejected: {:?}",
            result.err()
        );
    }
}
