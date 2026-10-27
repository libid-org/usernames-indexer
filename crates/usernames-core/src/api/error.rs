//! How the API refuses: one envelope for every refusal, its extractors'
//! included.

use std::fmt;

use axum::{
    extract::{
        rejection::{
            PathRejection,
            QueryRejection,
        },
        FromRequestParts,
        Path,
        Query,
    },
    http::{
        request::Parts,
        StatusCode,
    },
    response::{
        IntoResponse,
        Response,
    },
    Json,
};
use libid_identity::HandleError;
use serde::de::DeserializeOwned;

use super::model::{
    ErrorBody,
    ErrorCode,
    ErrorDetail,
};
use crate::db::InvalidCursor;
/// A refusal: its status, and the code and prose of the [`ErrorBody`] it
/// renders as.
pub(crate) struct ApiError {
    status: StatusCode,
    code: ErrorCode,
    message: String,
    /// The operator-facing cause, logged when the response renders and never
    /// serialized to the client.
    source: Option<sqlx::Error>,
}

impl ApiError {
    pub(crate) fn not_found(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            code,
            message: message.into(),
            source: None,
        }
    }

    pub(crate) fn bad_request(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code,
            message: message.into(),
            source: None,
        }
    }

    /// A stored value this build cannot read back. Logged with `what` where
    /// it is found; the client gets the same opaque 500 as a database error.
    pub(crate) fn internal(what: impl fmt::Display) -> Self {
        tracing::error!(%what, "unreadable stored value");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: ErrorCode::Internal,
            message: "internal error".into(),
            source: None,
        }
    }

    pub(crate) fn not_synced(chain: Option<i64>) -> Self {
        let message = match chain {
            Some(chain) => {
                format!("no indexer has finished a first window on chain {chain}")
            }
            None => "no indexer has finished a first window on any chain".to_string(),
        };
        Self {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: ErrorCode::NotSynced,
            message,
            source: None,
        }
    }
}

/// Text the platform could never hold, refused the way the contract's
/// `resolveHandle` answers it — the zero address, so a 404 and not a 400:
/// "nobody holds this", not "you asked wrong".
impl From<HandleError> for ApiError {
    fn from(e: HandleError) -> Self {
        Self::not_found(
            ErrorCode::HandleImpossible,
            format!("no handle can exist on this platform for this text: {e}"),
        )
    }
}

/// A path segment axum could not take apart, such as invalid UTF-8: a 400 in
/// the envelope, or the 500 of a route that names no such segment.
impl From<PathRejection> for ApiError {
    fn from(rejection: PathRejection) -> Self {
        if rejection.status().is_server_error() {
            Self::internal(rejection.body_text())
        } else {
            Self::bad_request(ErrorCode::InvalidPath, rejection.body_text())
        }
    }
}

/// A query string axum could not take apart, such as a repeated key.
impl From<QueryRejection> for ApiError {
    fn from(rejection: QueryRejection) -> Self {
        Self::bad_request(ErrorCode::InvalidQuery, rejection.body_text())
    }
}

impl From<InvalidCursor> for ApiError {
    fn from(e: InvalidCursor) -> Self {
        Self::bad_request(ErrorCode::InvalidCursor, e.to_string())
    }
}

impl From<sqlx::Error> for ApiError {
    // A pure conversion: the logging happens where the error is handled
    // (IntoResponse), not as a side effect of the type change.
    fn from(e: sqlx::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: ErrorCode::Internal,
            message: "internal error".into(),
            source: Some(e),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        if let Some(error) = &self.source {
            tracing::error!(%error, "query failed");
        }
        let body = ErrorBody {
            error: ErrorDetail {
                code: self.code,
                message: self.message,
            },
        };
        (self.status, Json(body)).into_response()
    }
}

/// `Path`, refused in the API's envelope instead of axum's plain text.
pub(crate) struct ApiPath<T>(pub(crate) T);

impl<T, S> FromRequestParts<S> for ApiPath<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let Path(value) = Path::<T>::from_request_parts(parts, state).await?;
        Ok(Self(value))
    }
}

/// `Query`, refused in the API's envelope instead of axum's plain text.
pub(crate) struct ApiQuery<T>(pub(crate) T);

impl<T, S> FromRequestParts<S> for ApiQuery<T>
where
    T: DeserializeOwned,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let Query(value) = Query::<T>::from_request_parts(parts, state).await?;
        Ok(Self(value))
    }
}
