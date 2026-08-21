//! Plumbing both integration suites share: drive one request through the
//! real router and hand back the status plus the parsed body.

use alloy::primitives::Address;
use axum::{
    body::Body,
    http::{
        Request,
        StatusCode,
    },
};
use http_body_util::BodyExt;
use tower::ServiceExt;
use usernames_indexer::{
    api,
    db::ChainStore,
};

/// One GET against the same axum router a caller would hit. A non-JSON body
/// comes back as a JSON string, so a failed assertion still prints something.
pub async fn get(
    store: &ChainStore,
    contract: Address,
    path: &str,
) -> (StatusCode, serde_json::Value) {
    let state = api::AppState::new(store.clone(), contract);
    let response = api::router(state)
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .expect("request");
    let status = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| serde_json::json!(String::from_utf8_lossy(&bytes)));
    (status, value)
}
