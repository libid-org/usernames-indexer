//! `GET /v1/stats` and `GET /v1/stats/days` as the stats suites read them:
//! from a router of their own, whose cache lives as long as it does, one
//! chain's entry at a time.

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
use serde::de::DeserializeOwned;
use tower::ServiceExt;
use usernames_core::api::model::{
    ChainStats,
    Stats,
    StatsDays,
};

/// A running API, asked for its statistics.
pub trait StatsRoute {
    /// `GET /v1/stats`.
    fn stats(&self) -> impl Future<Output = Stats> + Send;
    /// Every page of `GET /v1/stats/days`, on `chain` or across every chain
    /// without it, oldest first.
    fn days<D: DeserializeOwned + Send>(
        &self,
        chain: Option<i64>,
    ) -> impl Future<Output = Vec<D>> + Send;
    /// One GET answered 200, in its type.
    fn answer<T: DeserializeOwned>(&self, path: &str) -> impl Future<Output = T> + Send;
}

impl StatsRoute for Router {
    async fn stats(&self) -> Stats {
        self.answer("/v1/stats").await
    }

    async fn days<D: DeserializeOwned + Send>(&self, chain: Option<i64>) -> Vec<D> {
        let on = chain
            .map(|chain| format!("&chain={chain}"))
            .unwrap_or_default();
        let mut days = Vec::new();
        let mut before = String::new();
        loop {
            let path = format!("/v1/stats/days?limit=100{on}{before}");
            let page: StatsDays<D> = self.answer(&path).await;
            assert_eq!(page.chain_id, chain);
            days.extend(page.days);
            match page.next {
                Some(next) => before = format!("&before={next}"),
                None => break,
            }
        }
        days.reverse();
        days
    }

    async fn answer<T: DeserializeOwned>(&self, path: &str) -> T {
        let request = Request::get(path).body(Body::empty()).expect("a request");
        let response = self.clone().oneshot(request).await.expect("request");
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let body = response.into_body().collect().await.expect("body");
        serde_json::from_slice(&body.to_bytes()).expect("the answer's shape")
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
