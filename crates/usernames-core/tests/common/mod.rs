//! Plumbing both integration suites share: drive one request through the
//! real router and hand back what a caller gets, in the type the API
//! declares for it.

use axum::{
    body::Body,
    http::{
        Request,
        StatusCode,
    },
};
use http_body_util::BodyExt;
use serde::de::DeserializeOwned;
use tower::ServiceExt;
use usernames_core::{
    api::{
        self,
        model::ErrorBody,
    },
    db::{
        ChainStore,
        Store,
    },
};

/// What one GET came back with: the answer in its type, or the refusal.
pub struct Reply<T> {
    pub status: StatusCode,
    pub body: Result<T, ErrorBody>,
}

impl<T: std::fmt::Debug> Reply<T> {
    /// The answer; a refusal here fails the test with its code and message.
    pub fn answer(self) -> T {
        match self.body {
            Ok(answer) => answer,
            Err(refusal) => panic!(
                "{}: {}: {}",
                self.status, refusal.error.code, refusal.error.message
            ),
        }
    }

    /// The status and code of a refusal; an answer here fails the test.
    pub fn refusal(self) -> (StatusCode, String) {
        match self.body {
            Err(refusal) => (self.status, refusal.error.code),
            Ok(answer) => panic!("answered {}: {answer:?}", self.status),
        }
    }
}

/// One GET against the same axum router a caller would hit, decoded into the
/// type the API answers with — or, off the success range, into the error
/// envelope. A body in neither shape is a failed test, not a value.
pub async fn get<T: DeserializeOwned>(store: &ChainStore, path: &str) -> Reply<T> {
    let state = api::AppState::new(Store::new(store.pool().clone()));
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
    let text = || String::from_utf8_lossy(&bytes).into_owned();
    let body = if status.is_success() {
        Ok(serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!("{status}: not the answer's shape ({e}): {}", text())
        }))
    } else {
        Err(serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            panic!("{status}: not the error shape ({e}): {}", text())
        }))
    };
    Reply { status, body }
}
