//! The stats reads, planned.

use crate::{
    explain,
    Plan,
};
use alloy::primitives::Address;
use usernames_core::{
    chain::NATIVE_COIN,
    db::sql,
};

use sqlx::PgConnection;

use usernames_core::db::{
    fold::{
        Fold,
        JournalPosition,
    },
    stats::*,
};

/// The stats reads and the rollups' writes, planned.
pub(crate) trait StatsPlans {
    /// The totals, the referral shares held, every shape of a day page,
    /// and the folds of a window's rows and of a journal's page, with the
    /// indexes they seek.
    async fn stats_plans(&mut self) -> Vec<Plan>;
}

impl StatsPlans for PgConnection {
    async fn stats_plans(&mut self) -> Vec<Plan> {
        // A cursor near the newest day, where a page read the wrong way
        // has most of the days still ahead of it.
        let before = 190 * 86_400i64;
        let days: Vec<i64> = (161..190).map(|day| day * 86_400).collect();
        // Every day the volume holds: the longest page it can serve.
        let all_days: Vec<i64> = (1..=200).map(|day| day * 86_400).collect();
        let listed = |byte| {
            vec![ListedAddress {
                chain_id: 1,
                address: Address::repeat_byte(byte),
            }]
        };
        let listing = Listing {
            tokens: listed(0xEE),
            fee_receivers: listed(0xFE),
        };
        let chains = [1i64, 2, 3, 4, 5];
        let chain_measures = Measure::ALL
            .into_iter()
            .filter(|measure| *measure != Measure::AllChainsUsers);
        let chain_keys = Keys::of(&[1], chain_measures, &listing);
        let all_chains = Keys::of(&chains, [Measure::AllChainsUsers], &listing);
        let every = Keys::of(&chains, Measure::ALL, &listing);
        let measures: Vec<&str> = Measure::ALL.iter().map(|m| m.as_str()).collect();
        let (blocks, logs) = ([10i64, 20, 30], [0i64, 0, 0]);
        let (after, through) = (
            JournalPosition {
                block_number: 4_000,
                log_index: 0,
            },
            JournalPosition {
                block_number: 4_100,
                log_index: 0,
            },
        );
        let window = Fold::Window {
            blocks: &blocks,
            logs: &logs,
        };
        let page = Fold::Page { after, through };
        let reads = [
            (
                "the live handles",
                &["handles_live_idx"][..],
                sqlx::query_scalar::<_, String>(&explain(sql::STATS_HANDLES))
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "the totals",
                &["stats_totals_pkey:chain_id,measure"][..],
                sqlx::query_scalar(&explain(sql::STATS_TOTALS))
                    .bind(&every.every)
                    .bind(&every.chains)
                    .bind(&every.measures)
                    .bind(&every.keys)
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "the price candidates",
                &["stats_totals_pkey:chain_id,measure"][..],
                sqlx::query_scalar(&explain(sql::STATS_CANDIDATES))
                    .bind(NATIVE_COIN.to_vec())
                    .bind(&[1i64][..])
                    .bind(vec![Address::repeat_byte(0xEE).to_vec()])
                    .bind(CANDIDATES_PER_CHAIN)
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "the referral shares held",
                &["referrals_held_idx"][..],
                sqlx::query_scalar(&explain(sql::STATS_HELD_SHARES))
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "a chain's days",
                &["stats_counts_pkey"][..],
                sqlx::query_scalar(&explain(sql::STATS_CHAIN_DAYS))
                    .bind(1i64)
                    .bind(before)
                    .bind(31i64)
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "the days across chains",
                &["stats_counts_pkey"][..],
                sqlx::query_scalar(&explain(sql::STATS_ALL_DAYS))
                    .bind(before)
                    .bind(31i64)
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "a chain's day page",
                &["stats_counts_pkey"][..],
                sqlx::query_scalar(&explain(sql::STATS_DAY_COUNTS))
                    .bind(&[1i64][..])
                    .bind(&days)
                    .bind(&chain_keys.every)
                    .bind(&chain_keys.chains)
                    .bind(&chain_keys.measures)
                    .bind(&chain_keys.keys)
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "a chain's longest day page",
                &["stats_counts_pkey"][..],
                sqlx::query_scalar(&explain(sql::STATS_DAY_COUNTS))
                    .bind(&[1i64][..])
                    .bind(&all_days)
                    .bind(&chain_keys.every)
                    .bind(&chain_keys.chains)
                    .bind(&chain_keys.measures)
                    .bind(&chain_keys.keys)
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "a day page across chains",
                &["stats_counts_pkey"][..],
                sqlx::query_scalar(&explain(sql::STATS_DAY_COUNTS))
                    .bind(&chains[..])
                    .bind(&days)
                    .bind(&all_chains.every)
                    .bind(&all_chains.chains)
                    .bind(&all_chains.measures)
                    .bind(&all_chains.keys)
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "a journal's next page",
                &["events_pkey"][..],
                sqlx::query_scalar(&explain(sql::STATS_PAGE_END))
                    .bind(1i64)
                    .bind(after.block_number)
                    .bind(after.log_index)
                    .bind(10_000i64)
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "a window's fold",
                &[
                    "events_pkey",
                    "events_first_deposit_idx",
                    "events_binds_idx",
                    "referrals_pkey",
                ][..],
                sqlx::query_scalar(&explain(&window.statement()))
                    .bind(1i64)
                    .bind(&measures)
                    .bind(&blocks[..])
                    .bind(&logs[..])
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "a journal page's fold",
                &[
                    "events_pkey",
                    "events_first_deposit_idx",
                    "events_binds_idx",
                    "referrals_pkey",
                ][..],
                sqlx::query_scalar(&explain(&page.statement()))
                    .bind(1i64)
                    .bind(&measures)
                    .bind(after.block_number)
                    .bind(after.log_index)
                    .bind(through.block_number)
                    .bind(through.log_index)
                    .fetch_all(&mut *self)
                    .await,
            ),
        ];
        reads
            .into_iter()
            .map(|(name, seeks, text)| {
                let text: Vec<String> = text.unwrap_or_else(|e| panic!("{name}: {e}"));
                Plan {
                    name,
                    seeks: Some(seeks),
                    text: text.join("\n"),
                }
            })
            .collect()
    }
}
