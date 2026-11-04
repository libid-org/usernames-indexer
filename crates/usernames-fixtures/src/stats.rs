//! `GET /v1/stats` as the stats suites read it: from a router of their own,
//! whose cache lives as long as it does, one chain's entry at a time.

use std::future::Future;

use axum::{
    body::Body,
    http::{
        Request,
        StatusCode,
    },
    Router,
};
use http_body_util::BodyExt;
use tower::ServiceExt;
use usernames_core::api::model::{
    ChainStats,
    Stats,
};

/// A running API, asked for its statistics.
pub trait StatsRoute {
    /// `GET /v1/stats`.
    fn stats(&self) -> impl Future<Output = Stats> + Send;
}

impl StatsRoute for Router {
    async fn stats(&self) -> Stats {
        let request = Request::get("/v1/stats")
            .body(Body::empty())
            .expect("a request");
        let response = self.clone().oneshot(request).await.expect("request");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.expect("body");
        serde_json::from_slice(&body.to_bytes()).expect("the stats' shape")
    }
}

/// One chain's entry in an answer.
pub trait ChainEntry {
    /// The entry of `chain_id`; its absence fails the test.
    fn chain(self, chain_id: i64) -> ChainStats;
}

impl ChainEntry for Stats {
    fn chain(self, chain_id: i64) -> ChainStats {
        self.chains
            .into_iter()
            .find(|chain| chain.chain_id == chain_id)
            .unwrap_or_else(|| panic!("chain {chain_id} listed"))
    }
}
