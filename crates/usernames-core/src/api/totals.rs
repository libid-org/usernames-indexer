//! `GET /v1/escrow/totals/{address}`: an address's unclaimed totals per
//! chain and token, claimable now and awaiting a bind, in each chain's coin
//! and the tokens `/v1/stats` prices, valued at that answer's prices.

use std::collections::BTreeMap;

use alloy::primitives::{
    Address,
    U512,
};
use axum::{
    extract::State,
    Json,
};

use super::{
    model::{
        AddressTotals,
        Amount,
        ChainTotals,
        TokenTotals,
        TokenValue,
        Totals,
        Usd,
    },
    prices::{
        Quote,
        Quotes,
    },
    ApiError,
    ApiPath,
    AppState,
    Param,
};
use crate::{
    chain::NATIVE_COIN,
    db::{
        NamedTokens,
        TotalsRow,
    },
};

/// `GET /v1/escrow/totals/{address}` — per chain and token, what `claim`
/// pays the address now and what binding its handles there makes
/// claimable; unpaged, since only named tokens are summed.
pub(crate) async fn totals(
    State(state): State<AppState>,
    ApiPath(address): ApiPath<String>,
) -> Result<Json<AddressTotals>, ApiError> {
    let address = address.parse_address()?;
    state.synced(None).await?;
    let quotes = state.quotes().await;
    let rows = state.store.escrow_totals(address, &quotes.named()).await?;
    AddressTotals::of(address, rows, &quotes).map(Json)
}

impl AddressTotals {
    /// `rows`, by chain and token, valued at `quotes`.
    fn of(
        address: Address,
        rows: Vec<TotalsRow>,
        quotes: &Quotes,
    ) -> Result<Self, ApiError> {
        let mut chains: BTreeMap<i64, Vec<TokenTotals>> = BTreeMap::new();
        for row in rows {
            let quote = quotes.of(row.chain_id, row.token);
            chains
                .entry(row.chain_id)
                .or_default()
                .push(TokenTotals::of(row, quote)?);
        }
        let chains = chains
            .into_iter()
            .map(|(chain_id, tokens)| ChainTotals::of(chain_id, tokens))
            .collect::<Result<Vec<_>, _>>()?;
        let mut usd = Totals::<Usd>::default();
        for chain in &chains {
            usd.add(&chain.usd)?;
        }
        Ok(Self {
            address,
            priced_at: quotes.taken_at(),
            chains,
            usd,
        })
    }
}

impl ChainTotals {
    /// A chain's `tokens`, its coin first, and their USD sums.
    fn of(chain_id: i64, mut tokens: Vec<TokenTotals>) -> Result<Self, ApiError> {
        tokens.sort_by_key(|token| (token.token != NATIVE_COIN, token.token));
        let mut usd = Totals::<Usd>::default();
        for token in &tokens {
            usd.add(&Totals {
                claimable: token.claimable.usd.unwrap_or_default(),
                awaiting: token.awaiting.usd.unwrap_or_default(),
            })?;
        }
        Ok(Self {
            chain_id,
            tokens,
            usd,
        })
    }
}

impl TokenTotals {
    /// A stored row valued at `quote`. The books are this build's own
    /// writing, so a sum that is not a number is an internal error.
    fn of(row: TotalsRow, quote: Option<&Quote>) -> Result<Self, ApiError> {
        let Some((claimable, awaiting)) = row.amounts() else {
            return Err(ApiError::internal(format_args!(
                "escrow totals {}/{}: claimable {:?} awaiting {:?}",
                row.chain_id, row.token, row.claimable, row.awaiting
            )));
        };
        let (claimable_value, awaiting_value) = (
            TokenValue::of(claimable, quote),
            TokenValue::of(awaiting, quote),
        );
        Ok(Self {
            token: row.token,
            symbol: claimable_value
                .symbol
                .clone()
                .or_else(|| awaiting_value.symbol.clone()),
            decimals: claimable_value.decimals.or(awaiting_value.decimals),
            claimable: Amount::of(claimable, claimable_value),
            awaiting: Amount::of(awaiting, awaiting_value),
        })
    }
}

impl Amount {
    /// `amount` token units with the worth `valued` gives them.
    fn of(amount: U512, valued: TokenValue) -> Self {
        Self {
            amount,
            value: valued.value,
            usd: valued.usd,
        }
    }
}

impl Totals<Usd> {
    /// Adds `other`'s sums to these.
    fn add(&mut self, other: &Self) -> Result<(), ApiError> {
        let sum = |a: Usd, b: Usd| {
            a.checked_add(b)
                .ok_or_else(|| ApiError::internal("a USD total past 2^512 cents"))
        };
        self.claimable = sum(self.claimable, other.claimable)?;
        self.awaiting = sum(self.awaiting, other.awaiting)?;
        Ok(())
    }
}
