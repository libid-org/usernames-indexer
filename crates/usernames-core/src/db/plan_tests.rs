//! The plan test: every read over the big tables, planned over
//! production-shaped volume.

use std::str::FromStr;

use sqlx::{
    postgres::PgConnectOptions,
    PgConnection,
};
use usernames_fixtures::database_url;

use super::{
    awaiting::plans::AwaitingPlans,
    escrow::plans::EscrowPlans,
    history::plans::HistoryPlans,
    reads::{
        Scope,
        Search,
    },
    referral::plans::ReferralPlans,
    referrals::plans::ReferralHistoryPlans,
    stats::plans::StatsPlans,
    *,
};

/// Rows in the shape production holds them, on the chain every probe
/// names; the file says how they are shaped.
const VOLUME: &str = include_str!("../../sql/plan_volume.sql");
/// The same tables, cleared of what earlier loads left behind.
const VACUUM: &str = include_str!("../../sql/plan_vacuum.sql");

/// One statement's plan, and the indexes it must seek: `a|b` where either
/// seeks exactly, `None` where only the row count decides between them.
pub(crate) struct Plan {
    pub(crate) name: &'static str,
    pub(crate) seeks: Option<&'static [&'static str]>,
    pub(crate) text: String,
}

impl Plan {
    /// The plan of a composed lookup, built with an `EXPLAIN ` prefix.
    pub(crate) async fn of(
        conn: &mut PgConnection,
        name: &'static str,
        seeks: Option<&'static [&'static str]>,
        mut statement: Statement,
    ) -> Self {
        let text = statement
            .build_query_scalar::<String>()
            .fetch_all(&mut *conn)
            .await
            .unwrap_or_else(|e| panic!("{name}: {e}"))
            .join("\n");
        Self { name, seeks, text }
    }

    /// Panics unless the plan seeks every index it names and scans none of
    /// the big tables.
    fn check(&self) {
        let Self { name, seeks, text } = self;
        for index in seeks.unwrap_or_default() {
            assert!(
                index.split('|').any(|index| text.contains(index)),
                "{name} does not seek {index}:\n{text}"
            );
        }
        for table in [
            "handles",
            "ids",
            "published",
            "events",
            "escrow_held",
            "escrow_refundable",
            "address_events",
            "handle_nodes",
            "referrals",
        ] {
            assert!(
                !text.contains(&format!("Seq Scan on {table}")),
                "{name} scans {table}:\n{text}"
            );
        }
    }
}

/// A database of a test's own on `DATABASE_URL`'s server: rows committed
/// there reach no other suite.
struct OwnDatabase {
    server: PgPool,
    name: String,
    pool: PgPool,
}

impl OwnDatabase {
    /// A fresh database named after `DATABASE_URL`'s with `suffix`; one an
    /// interrupted run left behind is dropped first.
    async fn create(url: &str, suffix: &str) -> Self {
        let options = PgConnectOptions::from_str(url).expect("DATABASE_URL");
        let name = format!("{}_{suffix}", options.get_database().unwrap_or("postgres"));
        let server = PgPool::connect_with(options.clone())
            .await
            .expect("connect");
        sqlx::raw_sql(&format!("DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"))
            .execute(&server)
            .await
            .expect("the leftover database dropped");
        sqlx::raw_sql(&format!("CREATE DATABASE \"{name}\""))
            .execute(&server)
            .await
            .expect("the database created");
        let pool = PgPool::connect_with(options.database(&name))
            .await
            .expect("connect to the database");
        Self { server, name, pool }
    }

    /// Drops the database.
    async fn remove(self) {
        self.pool.close().await;
        sqlx::raw_sql(&format!("DROP DATABASE \"{}\" WITH (FORCE)", self.name))
            .execute(&self.server)
            .await
            .expect("the database dropped");
    }
}

/// `EXPLAIN` in front of a statement file.
pub(crate) fn explain(sql: &str) -> String {
    format!("EXPLAIN {sql}")
}
/// The store's lookups, planned.
pub(crate) trait StorePlans {
    /// The resolution and search lookups, in every shape the API asks them.
    async fn store_plans(&mut self) -> Vec<Plan>;
}

impl StorePlans for PgConnection {
    async fn store_plans(&mut self) -> Vec<Plan> {
        let platform = B256::repeat_byte(1);
        let node = B256::repeat_byte(3);
        let owner = Address::repeat_byte(2);
        let mut plans = Vec::new();
        for (name, seeks, statement) in [
            (
                "handle node, any chain",
                Some(&["handles_node_chain_idx"][..]),
                Scope(None).handle_lookup("EXPLAIN ", node),
            ),
            (
                "handle node, one chain",
                Some(&["handles_pkey|handles_node_chain_idx"][..]),
                Scope(Some(1)).handle_lookup("EXPLAIN ", node),
            ),
            (
                "a handle node's text, any chain",
                Some(&["handle_nodes_node_chain_idx"][..]),
                Scope(None).handle_node_lookup("EXPLAIN ", node),
            ),
            (
                "a handle node's text, one chain",
                Some(&["handle_nodes_pkey|handle_nodes_node_chain_idx"][..]),
                Scope(Some(1)).handle_node_lookup("EXPLAIN ", node),
            ),
            (
                "id node, any chain",
                Some(&["ids_node_chain_idx"][..]),
                Scope(None).id_lookup("EXPLAIN ", node),
            ),
            (
                "id node, one chain",
                Some(&["ids_pkey|ids_node_chain_idx"][..]),
                Scope(Some(1)).id_lookup("EXPLAIN ", node),
            ),
            (
                "wallet, any chain",
                Some(&["ids_owner_chain_idx"][..]),
                Scope(None).identities_lookup("EXPLAIN ", owner),
            ),
            (
                "wallet, one chain",
                Some(&["ids_owner_chain_idx"][..]),
                Scope(Some(1)).identities_lookup("EXPLAIN ", owner),
            ),
            (
                "search by wallet, any chain",
                Some(&["handles_owner_chain_node_idx"][..]),
                Search {
                    chain: None,
                    platform_id: None,
                    owner: Some(owner),
                    folded_query: None,
                    limit: 10,
                    offset: 0,
                }
                .lookup("EXPLAIN "),
            ),
            (
                "search by wallet on a platform, one chain",
                Some(&["handles_owner_chain_node_idx"][..]),
                Search {
                    chain: Some(1),
                    platform_id: Some(platform),
                    owner: Some(owner),
                    folded_query: None,
                    limit: 10,
                    offset: 0,
                }
                .lookup("EXPLAIN "),
            ),
            (
                "search by wallet and a short text, any chain",
                Some(&["handles_owner_chain_node_idx"][..]),
                Search {
                    chain: None,
                    platform_id: None,
                    owner: Some(owner),
                    folded_query: Some("a"),
                    limit: 10,
                    offset: 0,
                }
                .lookup("EXPLAIN "),
            ),
            (
                "search by text, any chain",
                Some(&["handle_nodes_handle_trgm_idx"][..]),
                Search {
                    chain: None,
                    platform_id: None,
                    owner: None,
                    folded_query: Some("ali"),
                    limit: 10,
                    offset: 0,
                }
                .lookup("EXPLAIN "),
            ),
            (
                "search by text on a platform, one chain",
                Some(&["handle_nodes_handle_trgm_idx"][..]),
                Search {
                    chain: Some(1),
                    platform_id: Some(platform),
                    owner: None,
                    folded_query: Some("ali"),
                    limit: 10,
                    offset: 0,
                }
                .lookup("EXPLAIN "),
            ),
        ] {
            plans.push(Plan::of(self, name, seeks, statement).await);
        }

        let reads = [
            (
                "the gateway's answer",
                &["handles_pkey|handles_node_chain_idx"][..],
                sqlx::query_scalar::<_, String>(&explain(sql::ENS_ANSWER))
                    .bind(1i64)
                    .bind(node)
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "a withdrawn name",
                &["published_pkey"][..],
                sqlx::query_scalar(&explain(sql::WITHDRAW_NAME))
                    .bind(1i64)
                    .bind(owner)
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "the name an unpublish withdraws",
                &["published_pkey"][..],
                sqlx::query_scalar(&explain(sql::PUBLISHED_NAME))
                    .bind(1i64)
                    .bind(owner)
                    .fetch_all(&mut *self)
                    .await,
            ),
            (
                "a name published over another",
                &["published_pkey"][..],
                sqlx::query_scalar(&explain(sql::PUBLISH))
                    .bind(1i64)
                    .bind(owner)
                    .bind(platform)
                    .bind(node)
                    .fetch_all(&mut *self)
                    .await,
            ),
        ];
        for (name, seeks, text) in reads {
            let text: Vec<String> = text.unwrap_or_else(|e| panic!("{name}: {e}"));
            plans.push(Plan {
                name,
                seeks: Some(seeks),
                text: text.join("\n"),
            });
        }
        plans
    }
}

/// Every read over the big tables seeks the index built for it, in every
/// shape it is asked in, the indexer's own reads included. The index is
/// named, not just "some index": a lookup missing its leading column
/// walks a whole index and still reads as an index scan.
///
/// The plans are taken over [`VOLUME`], inside a transaction that rolls
/// back with its statistics: on a handful of rows every index costs the
/// same. Sequential scans stay switched off so none hides behind a small
/// table.
#[tokio::test]
async fn every_read_over_the_big_tables_seeks_its_index() {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        assert!(
            std::env::var_os("CI").is_none(),
            "DATABASE_URL not set under CI"
        );
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let pool = PgPool::connect(&url).await.expect("connect");
    MIGRATOR.run(&pool).await.expect("migrations");
    sqlx::raw_sql(VACUUM)
        .execute(&pool)
        .await
        .expect("the tables vacuumed");
    let mut conn = pool.begin().await.expect("transaction");
    sqlx::query("SET LOCAL enable_seqscan = off")
        .execute(&mut *conn)
        .await
        .expect("session setting");
    sqlx::raw_sql(VOLUME)
        .execute(&mut *conn)
        .await
        .expect("the volume loads");

    let mut all = conn.store_plans().await;
    all.extend(conn.history_plans().await);
    all.extend(conn.escrow_plans().await);
    all.extend(conn.referral_plans().await);
    all.extend(conn.referral_history_plans().await);
    all.extend(conn.awaiting_plans().await);
    for plan in all {
        plan.check();
    }
    conn.rollback().await.expect("rollback")
}

/// The `/v1/stats` aggregates read their covering indexes whole, without
/// the heap: a vacuumed journal allows that, and rows loaded in a
/// transaction that rolls back never do. So they are planned in a database
/// of the test's own, over [`VOLUME`] committed and vacuumed.
#[tokio::test]
async fn the_stats_aggregates_read_their_covering_indexes() {
    let Some(url) = database_url() else {
        eprintln!("skipping: DATABASE_URL not set");
        return;
    };
    let own = OwnDatabase::create(&url, "stats_plans").await;
    MIGRATOR.run(&own.pool).await.expect("migrations");
    let mut tx = own.pool.begin().await.expect("transaction");
    sqlx::raw_sql(VOLUME)
        .execute(&mut *tx)
        .await
        .expect("the volume loads");
    tx.commit().await.expect("the volume committed");
    sqlx::raw_sql(VACUUM)
        .execute(&own.pool)
        .await
        .expect("the tables vacuumed");
    let mut conn = own.pool.acquire().await.expect("connection");
    sqlx::query("SET enable_seqscan = off")
        .execute(&mut *conn)
        .await
        .expect("session setting");
    let plans = conn.stats_plans().await;
    drop(conn);
    own.remove().await;
    for plan in plans {
        plan.check();
    }
}
