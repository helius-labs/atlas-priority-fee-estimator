use jsonrpsee::types::{error::INVALID_PARAMS_CODE, ErrorObjectOwned};

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum TransactionValidationError {
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

pub fn invalid_request(reason: &str) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        INVALID_PARAMS_CODE,
        format!("Invalid Request: {reason}"),
        None::<String>,
    )
}
