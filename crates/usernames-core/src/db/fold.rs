//! The stats rollups' writes: the journal rows a window adds, folded into
//! the rollups as the window commits; a journal written before them, folded
//! page by page; and the users across every chain, folded again from the
//! other chains' journals when a chain's rollups are cleared. All of them
//! run one statement, after the head that picks their rows.

use sqlx::PgConnection;

use super::{
    sql,
    stats::Measure,
};

/// The most journal rows one fold of a journal's page reads.
pub(crate) const FOLD_PAGE: i64 = 10_000;

/// A row's place in a chain's journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, sqlx::FromRow)]
pub struct JournalPosition {
    /// Its block.
    pub block_number: i64,
    /// Its log index in the block.
    pub log_index: i64,
}

impl JournalPosition {
    /// Before the journal's first row.
    pub(crate) const START: Self = Self {
        block_number: -1,
        log_index: -1,
    };

    /// Past every row.
    const END: Self = Self {
        block_number: i64::MAX,
        log_index: i64::MAX,
    };
}

/// Which journal rows a fold reads.
pub enum Fold<'a> {
    /// The rows a window added, by block and log index, pairwise.
    Window {
        /// Their blocks.
        blocks: &'a [i64],
        /// Their log indexes.
        logs: &'a [i64],
    },
    /// A chain's rows after one position, through another.
    Page {
        /// The position the page starts after.
        after: JournalPosition,
        /// The last position it reads.
        through: JournalPosition,
    },
}

impl Fold<'_> {
    /// The fold's statement: its head, what the rows count, and the fold.
    pub fn statement(&self) -> String {
        let head = match self {
            Fold::Window { .. } => sql::STATS_FOLD_WINDOW,
            Fold::Page { .. } => sql::STATS_FOLD_PAGE,
        };
        [head, sql::STATS_FOLD_FACTS, sql::STATS_FOLD].concat()
    }
}

/// A chain whose journal counts the users across every chain again, as far
/// as its rollups hold it.
#[derive(sqlx::FromRow)]
struct Refold {
    chain_id: i64,
    /// Whether the rollups hold the journal whole.
    folded: bool,
    /// Else, the last row they hold.
    #[sqlx(flatten)]
    folded_through: JournalPosition,
}

impl Refold {
    /// The last row of the journal the rollups hold.
    fn through(&self) -> JournalPosition {
        match self.folded {
            true => JournalPosition::END,
            false => self.folded_through,
        }
    }
}

/// The rollups, written on a connection inside a transaction.
pub(crate) trait Rollups {
    /// Fold `rows` of chain `chain_id` into its rollups, under `measures`.
    /// Takes the fold lock, shared, until the transaction ends.
    async fn fold(
        &mut self,
        chain_id: i64,
        rows: Fold<'_>,
        measures: &[Measure],
    ) -> Result<(), sqlx::Error>;

    /// Where the page of chain `chain_id`'s journal after `after` ends,
    /// `rows` rows on or at its last row; `None` when no row follows.
    async fn page_end(
        &mut self,
        chain_id: i64,
        after: JournalPosition,
        rows: i64,
    ) -> Result<Option<JournalPosition>, sqlx::Error>;

    /// Delete chain `chain_id`'s rollups and the users across every chain,
    /// then fold every other chain's journal into those users again, as far
    /// as its rollups hold it. Takes the clear lock, so no fold runs
    /// meanwhile.
    async fn clear_rollups(&mut self, chain_id: i64) -> Result<(), sqlx::Error>;
}

impl Rollups for PgConnection {
    async fn fold(
        &mut self,
        chain_id: i64,
        rows: Fold<'_>,
        measures: &[Measure],
    ) -> Result<(), sqlx::Error> {
        sqlx::query(sql::STATS_FOLD_LOCK)
            .execute(&mut *self)
            .await?;
        let measures: Vec<&str> =
            measures.iter().map(|measure| measure.as_str()).collect();
        let statement = rows.statement();
        let query = sqlx::query(&statement).bind(chain_id).bind(&measures);
        let query = match rows {
            Fold::Window { blocks, logs } => query.bind(blocks).bind(logs),
            Fold::Page { after, through } => query
                .bind(after.block_number)
                .bind(after.log_index)
                .bind(through.block_number)
                .bind(through.log_index),
        };
        query.execute(&mut *self).await?;
        Ok(())
    }

    async fn page_end(
        &mut self,
        chain_id: i64,
        after: JournalPosition,
        rows: i64,
    ) -> Result<Option<JournalPosition>, sqlx::Error> {
        sqlx::query_as(sql::STATS_PAGE_END)
            .bind(chain_id)
            .bind(after.block_number)
            .bind(after.log_index)
            .bind(rows)
            .fetch_optional(&mut *self)
            .await
    }

    async fn clear_rollups(&mut self, chain_id: i64) -> Result<(), sqlx::Error> {
        sqlx::query(sql::STATS_CLEAR_LOCK)
            .execute(&mut *self)
            .await?;
        sqlx::query(sql::STATS_CLEAR)
            .bind(chain_id)
            .execute(&mut *self)
            .await?;
        let refolds: Vec<Refold> = sqlx::query_as(sql::STATS_REFOLDS)
            .bind(chain_id)
            .fetch_all(&mut *self)
            .await?;
        for refold in refolds {
            let (chain_id, through) = (refold.chain_id, refold.through());
            let mut after = JournalPosition::START;
            while let Some(end) = self.page_end(chain_id, after, FOLD_PAGE).await? {
                let end = end.min(through);
                if end <= after {
                    break;
                }
                let page = Fold::Page {
                    after,
                    through: end,
                };
                self.fold(chain_id, page, &[Measure::AllChainsUsers])
                    .await?;
                after = end;
            }
        }
        Ok(())
    }
}
