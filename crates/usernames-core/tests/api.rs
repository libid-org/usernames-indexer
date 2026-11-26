//! The API's router: refusals in its one error envelope.

use usernames_core::{
    api::model::ErrorCode,
    db::Store,
};

use alloy::primitives::Address;
use axum::{
    body::Body,
    http::{
        Request,
        StatusCode,
    },
};
use http_body_util::BodyExt;
use sqlx::PgPool;
use tower::ServiceExt;

use usernames_core::api::{
    model::ErrorBody,
    *,
};

/// The status and code a route refuses `path` with. Every refusal here
/// comes before the store is read, so the pool never connects.
async fn refusal(path: &str) -> (StatusCode, ErrorCode) {
    let pool = PgPool::connect_lazy("postgres://localhost/unread").expect("lazy pool");
    let response = AppState::new(Store::new(pool))
        .router()
        .oneshot(
            Request::builder()
                .uri(path)
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("response");
    let status = response.status();
    let body = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let refusal: ErrorBody = serde_json::from_slice(&body).expect("the error envelope");
    (status, refusal.error.code)
}

#[tokio::test]
async fn a_malformed_list_request_is_refused_in_the_envelope() {
    let address = Address::repeat_byte(0xA1);
    let bad = |code: ErrorCode| (StatusCode::BAD_REQUEST, code);
    let cases = [
        (
            "/v1/escrow/node/0x1234".to_string(),
            bad(ErrorCode::InvalidNode),
        ),
        (
            "/v1/history/node/alice".to_string(),
            bad(ErrorCode::InvalidNode),
        ),
        (
            "/v1/escrow/claimable/0xnope".to_string(),
            bad(ErrorCode::InvalidAddress),
        ),
        (
            "/v1/escrow/awaiting/0xnope".to_string(),
            bad(ErrorCode::InvalidAddress),
        ),
        (
            "/v1/referrals/address/alice".to_string(),
            bad(ErrorCode::InvalidAddress),
        ),
        (
            format!("/v1/escrow/awaiting/{address}?before=nope"),
            bad(ErrorCode::InvalidCursor),
        ),
        (
            format!("/v1/referrals/address/{address}?limit=ten"),
            bad(ErrorCode::InvalidArgument),
        ),
        (
            "/v1/referrals/node/0x1234".to_string(),
            bad(ErrorCode::InvalidNode),
        ),
        (
            "/v1/referrals/handle/myspace/tom".to_string(),
            bad(ErrorCode::InvalidPlatform),
        ),
        (
            "/v1/referrals/handle/x/alice?before=nope".to_string(),
            bad(ErrorCode::InvalidCursor),
        ),
        (
            "/v1/escrow/unclaimed?token=usdc".to_string(),
            bad(ErrorCode::InvalidAddress),
        ),
        (
            "/v1/escrow/handle/myspace/tom".to_string(),
            bad(ErrorCode::InvalidPlatform),
        ),
        (
            "/v1/escrow/unclaimed?limit=ten".to_string(),
            bad(ErrorCode::InvalidArgument),
        ),
        (
            "/v1/history/handle/x/nobody?limit=1.5".to_string(),
            bad(ErrorCode::InvalidArgument),
        ),
        (
            "/v1/escrow/unclaimed?chain=base".to_string(),
            bad(ErrorCode::InvalidChain),
        ),
        (
            "/v1/escrow/unclaimed?before=nope".to_string(),
            bad(ErrorCode::InvalidCursor),
        ),
        (
            "/v1/stats/days?before=yesterday".to_string(),
            bad(ErrorCode::InvalidCursor),
        ),
        (
            "/v1/stats/days?chain=base".to_string(),
            bad(ErrorCode::InvalidChain),
        ),
        (
            "/v1/stats/days?limit=ten".to_string(),
            bad(ErrorCode::InvalidArgument),
        ),
        (
            "/v1/history/chain/base".to_string(),
            bad(ErrorCode::InvalidChain),
        ),
        (
            "/v1/history/chain/1?day=1700000001".to_string(),
            bad(ErrorCode::InvalidArgument),
        ),
        (
            "/v1/history/chain/1?day=today".to_string(),
            bad(ErrorCode::InvalidArgument),
        ),
        (
            "/v1/history/chain/1?kind=minted".to_string(),
            bad(ErrorCode::InvalidArgument),
        ),
        (
            "/v1/history/chain/1?token=usdc".to_string(),
            bad(ErrorCode::InvalidAddress),
        ),
        (
            "/v1/history/chain/1?before=yesterday".to_string(),
            bad(ErrorCode::InvalidCursor),
        ),
        (
            format!("/v1/history/address/{address}?before=yesterday"),
            bad(ErrorCode::InvalidCursor),
        ),
        (
            format!("/v1/escrow/refundable/{address}?before=1-2"),
            bad(ErrorCode::InvalidCursor),
        ),
        (
            "/v1/history/address/%FF".to_string(),
            bad(ErrorCode::InvalidPath),
        ),
        (
            "/v1/resolve/address/%FF".to_string(),
            bad(ErrorCode::InvalidPath),
        ),
        (
            "/v1/search?q=al&q=bo".to_string(),
            bad(ErrorCode::InvalidQuery),
        ),
        (
            "/v1/search?q=ali&limit=ten".to_string(),
            bad(ErrorCode::InvalidArgument),
        ),
        (
            "/v1/search?q=ali&offset=1.5".to_string(),
            bad(ErrorCode::InvalidArgument),
        ),
        (
            "/v1/resolve/handle/x/alice?chain=1&chain=2".to_string(),
            bad(ErrorCode::InvalidQuery),
        ),
        (
            format!("/v1/escrow/claimable/{address}?chain=1&chain=2"),
            bad(ErrorCode::InvalidQuery),
        ),
        (
            "/v1/referral/code/0OIl0OIl0OIl".to_string(),
            bad(ErrorCode::InvalidCode),
        ),
        (
            "/v1/referral/code/2g".to_string(),
            bad(ErrorCode::InvalidCode),
        ),
        (
            format!("/v1/referral/code/{}", "z".repeat(45)),
            bad(ErrorCode::InvalidCode),
        ),
        (
            "/v1/referral/node/0x1234/code".to_string(),
            bad(ErrorCode::InvalidNode),
        ),
    ];
    for (path, expected) in cases {
        assert_eq!(refusal(&path).await, expected, "{path}");
    }
}
