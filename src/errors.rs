use jsonrpsee::types::{error::INVALID_PARAMS_CODE, ErrorObjectOwned};

pub fn invalid_request(reason: &str) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(
        INVALID_PARAMS_CODE,
        format!("Invalid Request: {reason}"),
        None::<String>,
    )
}
