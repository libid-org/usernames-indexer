//! What a request names, from its path and query string, and the page it
//! is answered with.

use std::{
    fmt::Display,
    str::FromStr,
};

use alloy::primitives::{
    Address,
    B256,
};
use serde::Deserialize;

use super::{
    error::ApiError,
    model::ErrorCode,
};
use crate::{
    db::{
        InvalidCursor,
        Page,
        Rows,
    },
    nodes::{
        KnownPlatform,
        Platform,
    },
};

/// A list page holds this many entries when the request names no `limit`.
const PAGE_DEFAULT_LIMIT: i64 = 20;
/// The most entries one list page holds.
const PAGE_MAX_LIMIT: i64 = 100;

/// `?chain=8453&before=<next>&limit=20`: one page of a list, the cursor the
/// one the previous page handed out.
#[derive(Deserialize)]
pub(crate) struct PageParams {
    chain: Option<String>,
    before: Option<String>,
    limit: Option<String>,
}

impl PageParams {
    pub(crate) fn page<C: FromStr<Err = InvalidCursor>>(
        self,
    ) -> Result<Page<C>, ApiError> {
        Ok(Page {
            chain: ChainFilter { chain: self.chain }.chain_id()?,
            before: self.before.as_deref().map(str::parse).transpose()?,
            limit: self
                .limit
                .as_deref()
                .map(|limit| limit.parse_count("limit"))
                .transpose()?
                .unwrap_or(PAGE_DEFAULT_LIMIT)
                .clamp(1, PAGE_MAX_LIMIT),
        })
    }
}

impl<R, C: Display> Rows<R, C> {
    /// The page's rows as the API serves them, and the cursor that fetches the
    /// next one.
    pub(crate) fn served<T>(
        self,
        of: impl Fn(R) -> Result<T, ApiError>,
    ) -> Result<(Vec<T>, Option<String>), ApiError> {
        let served = self.rows.into_iter().map(of).collect::<Result<_, _>>()?;
        Ok((served, self.next.map(|next| next.to_string())))
    }
}

/// A handle as a request names it: by its text on a platform, folded and
/// hashed here the way `depositToHandle` keys it, or by its node.
pub(crate) enum HandleKey {
    Text {
        platform: KnownPlatform,
        handle_node: B256,
    },
    Node(B256),
}

impl HandleKey {
    pub(crate) fn of_text(platform: &str, handle: &str) -> Result<Self, ApiError> {
        let platform = platform.parse_known_platform()?;
        let (_, handle_node) = platform.folded_handle_node(handle);
        Ok(Self::Text {
            platform,
            handle_node,
        })
    }

    pub(crate) fn of_node(node: &str) -> Result<Self, ApiError> {
        node.parse_node().map(Self::Node)
    }

    pub(crate) fn node(&self) -> B256 {
        match self {
            Self::Text { handle_node, .. } => *handle_node,
            Self::Node(node) => *node,
        }
    }
}

/// `?chain=8453` narrows a read to one chain; absent, every chain the store
/// holds answers, and each result says which chain it is from.
#[derive(Deserialize)]
pub(crate) struct ChainFilter {
    pub(crate) chain: Option<String>,
}

impl ChainFilter {
    /// The chain `?chain=` names, if it names one.
    pub(crate) fn chain_id(&self) -> Result<Option<i64>, ApiError> {
        self.chain
            .as_deref()
            .map(|raw| {
                raw.parse().map_err(|_| {
                    ApiError::bad_request(
                        ErrorCode::InvalidChain,
                        format!("{raw:?} is not a chain id"),
                    )
                })
            })
            .transpose()
    }
}

/// Text a request carries, taken apart into what the store reads and
/// refused in the API's envelope.
pub(crate) trait Param {
    /// A platform by its key or its 0x-hex id, known to this build or not.
    fn parse_platform(&self) -> Result<Platform, ApiError>;
    /// A platform text hashes on: one this build has node tags for.
    fn parse_known_platform(&self) -> Result<KnownPlatform, ApiError>;
    /// A 0x-hex address.
    fn parse_address(&self) -> Result<Address, ApiError>;
    /// A 0x-hex 32-byte node.
    fn parse_node(&self) -> Result<B256, ApiError>;
    /// A count from the query string named `name`: typed as a number in the
    /// extractor, `limit=ten` would get axum's plain-text 400.
    fn parse_count(&self, name: &str) -> Result<i64, ApiError>;
    /// Refused when it holds a NUL byte: Postgres cannot compare TEXT holding
    /// one, and no stored value ever does.
    fn refuse_nul(&self, what: &str) -> Result<(), ApiError>;
}

impl Param for str {
    fn parse_platform(&self) -> Result<Platform, ApiError> {
        self.parse::<Platform>()
            .map_err(|e| ApiError::bad_request(ErrorCode::InvalidPlatform, e.to_string()))
    }

    fn parse_known_platform(&self) -> Result<KnownPlatform, ApiError> {
        self.parse_platform()?.known().ok_or_else(|| {
            ApiError::bad_request(
                ErrorCode::InvalidPlatform,
                "this build cannot hash text on that platform: ask by node",
            )
        })
    }

    fn parse_address(&self) -> Result<Address, ApiError> {
        Address::from_str(self).map_err(|_| {
            ApiError::bad_request(
                ErrorCode::InvalidAddress,
                format!("{self:?} is not an address"),
            )
        })
    }

    fn parse_node(&self) -> Result<B256, ApiError> {
        B256::from_str(self).map_err(|_| {
            ApiError::bad_request(
                ErrorCode::InvalidNode,
                format!("{self:?} is not a 0x-hex 32-byte node"),
            )
        })
    }

    fn parse_count(&self, name: &str) -> Result<i64, ApiError> {
        self.parse::<i64>().map_err(|_| {
            ApiError::bad_request(
                ErrorCode::InvalidArgument,
                format!("{name} must be an integer, not {self:?}"),
            )
        })
    }

    fn refuse_nul(&self, what: &str) -> Result<(), ApiError> {
        if self.contains('\0') {
            return Err(ApiError::bad_request(
                ErrorCode::InvalidArgument,
                format!("{what} must not contain a NUL byte"),
            ));
        }
        Ok(())
    }
}
