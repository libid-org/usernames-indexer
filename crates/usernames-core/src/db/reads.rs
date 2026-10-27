//! What the API reads. Every statement, and the join shapes the projections
//! were designed for, is one file under `sql/`, bound here. Every read spans
//! the chains the store holds, or the one the caller names, and every row
//! says which chain it came from.

use alloy::primitives::{
    Address,
    B256,
};
use sqlx::QueryBuilder;

use super::{
    sql,
    ChainState,
    ChainStore,
    IndexPosition,
    Statement,
    Store,
};

/// What the ENS gateway answers from: where the index stands, and who holds
/// the node asked about, read together.
#[derive(Clone, Copy, Debug)]
pub struct EnsAnswer {
    /// Where the index stands; `None` before an indexer prepared the chain.
    pub position: Option<IndexPosition>,
    /// Who holds the node; `None` when nobody does.
    pub holder: Option<Address>,
}

/// The chain's row and the holder, as `ens_answer.sql` reads them.
#[derive(sqlx::FromRow)]
struct EnsRow {
    #[sqlx(flatten)]
    state: ChainState,
    owner: Option<Address>,
}

/// One `names.handles` row and its disclosed text: everything `resolveHandle`
/// answers from.
#[derive(sqlx::FromRow)]
pub struct HandleRow {
    /// The chain the handle is bound on.
    pub chain_id: i64,
    /// The platform the handle lives on.
    pub platform_id: B256,
    /// The holder, or `None` after retirement — the contract's `address(0)`.
    pub owner: Option<Address>,
    /// The proof-freshness watermark, kept even through retirement.
    pub observed_at: i64,
    /// The ceremony version that proved the last binding at this node. Kept
    /// through retirement: the contract never stored it, so the index does.
    pub ceremony_version: i64,
    /// The account id node this handle points back at (`idNodeByHandle`).
    pub id_node: B256,
    /// The handle's text, once the chain disclosed it.
    pub handle: Option<String>,
}

/// One `names.ids` row joined with the handle node it points at and the
/// holder's name: everything `resolveId` and the reverse display answer
/// from.
#[derive(sqlx::FromRow)]
pub struct IdentityRow {
    /// The chain the account is bound on.
    pub chain_id: i64,
    /// The platform the account lives on.
    pub platform_id: B256,
    /// The storage key the chain filed this account under.
    pub id_node: B256,
    /// The holder that proved the account.
    pub owner: Address,
    /// The proof-freshness watermark.
    pub observed_at: i64,
    /// The ceremony version that proved the binding.
    pub ceremony_version: i64,
    /// The handle node this account last proved (`handleNodeById`).
    pub handle_node: B256,
    /// That handle's text, once the chain disclosed it.
    pub handle: Option<String>,
    /// The holder the handle node currently resolves to.
    pub handle_owner: Option<Address>,
    /// The id node the handle currently points back at.
    pub handle_id_node: Option<B256>,
    /// Whether the owner's name is this handle, on this platform.
    pub published: bool,
}

impl IdentityRow {
    /// The `handleNodeById`/`idNodeByHandle` round trip, written once: the
    /// handle is presentable only while the node still points back at this
    /// id and still resolves to this wallet. Otherwise the account renamed
    /// or was overtaken, and showing the stale handle would mis-route a
    /// payment.
    pub fn handle_still_owned(&self) -> bool {
        self.handle_owner == Some(self.owner) && self.handle_id_node == Some(self.id_node)
    }

    /// The handle's text to present: disclosed, and still this account's.
    pub fn presentable_handle(&self) -> Option<&str> {
        if self.handle_still_owned() {
            self.handle.as_deref()
        } else {
            None
        }
    }

    /// Whether this is the wallet's name — only while the handle is still
    /// the account's, as `primaryHandleOf` answers.
    pub fn displayed(&self) -> bool {
        self.published && self.handle_still_owned()
    }
}

/// One search hit: a live, disclosed handle, its owner, and the id node it
/// pairs with.
#[derive(sqlx::FromRow)]
pub struct SearchRow {
    /// The chain the handle is bound on.
    pub chain_id: i64,
    /// The platform the handle lives on.
    pub platform_id: B256,
    /// The storage key the chain filed the handle under.
    pub handle_node: B256,
    /// The account id node behind it (`idNodeByHandle`).
    pub id_node: B256,
    /// The handle's text.
    pub handle: String,
    /// The holder it resolves to (retired handles never match).
    pub owner: Address,
    /// Whether this handle is the owner's name.
    pub published: bool,
}

impl Store {
    /// The chain a name belongs to, if any chain's indexer declared it.
    pub async fn chain_named(&self, name: &str) -> Result<Option<i64>, sqlx::Error> {
        sqlx::query_scalar(sql::CHAIN_NAMED)
            .bind(name)
            .fetch_optional(&self.pool)
            .await
    }

    /// Whether an indexer has committed a first window — on the chain named,
    /// or on any chain when none is. Before that there is nothing to answer
    /// from, and a 404 would claim more than the store knows.
    pub async fn synced(&self, chain: Option<i64>) -> Result<bool, sqlx::Error> {
        let mut statement = QueryBuilder::new(sql::SYNCED);
        statement.scoped("chain_id", chain);
        statement.push(" LIMIT 1");
        let found: Option<bool> = statement
            .build_query_scalar()
            .fetch_optional(&self.pool)
            .await?;
        Ok(found.is_some())
    }

    /// The rows `resolveHandle` answers from, one per chain the handle node
    /// is bound on.
    pub async fn resolve_handle(
        &self,
        chain: Option<i64>,
        handle_node: B256,
    ) -> Result<Vec<HandleRow>, sqlx::Error> {
        Scope(chain)
            .handle_lookup("", handle_node)
            .build_query_as()
            .fetch_all(&self.pool)
            .await
    }

    /// The rows `resolveId` answers from, one per chain the id node is bound
    /// on.
    pub async fn resolve_id(
        &self,
        chain: Option<i64>,
        id_node: B256,
    ) -> Result<Vec<IdentityRow>, sqlx::Error> {
        Scope(chain)
            .id_lookup("", id_node)
            .build_query_as()
            .fetch_all(&self.pool)
            .await
    }

    /// Every identity a wallet proved, chain by chain and platform by
    /// platform, each saying whether its handle is the wallet's name
    /// (`primaryHandleOf`).
    pub async fn identities_of(
        &self,
        chain: Option<i64>,
        owner: Address,
    ) -> Result<Vec<IdentityRow>, sqlx::Error> {
        Scope(chain)
            .identities_lookup("", owner)
            .build_query_as()
            .fetch_all(&self.pool)
            .await
    }

    /// Live, disclosed handles a search lists, across the chains in scope:
    /// those a wallet holds, those matching a folded partial query — exact
    /// first, then prefix, then substring, then trigram-fuzzy — or both.
    pub async fn search_handles(
        &self,
        search: &Search<'_>,
    ) -> Result<Vec<SearchRow>, sqlx::Error> {
        search
            .lookup("")
            .build_query_as()
            .fetch_all(&self.pool)
            .await
    }
}

impl ChainStore {
    /// What the ENS gateway answers for a handle node on this chain: where
    /// the index stands and who holds the node, from one snapshot, so a
    /// replay clearing the chain between the two cannot be signed for.
    /// `ens_answer.sql` says which bindings it answers for.
    pub async fn ens_answer(&self, handle_node: B256) -> Result<EnsAnswer, sqlx::Error> {
        let row: Option<EnsRow> = sqlx::query_as(sql::ENS_ANSWER)
            .bind(self.chain_id)
            .bind(handle_node)
            .fetch_optional(&self.pool)
            .await?;
        Ok(match row {
            Some(row) => EnsAnswer {
                position: Some(row.state.position()),
                holder: row.owner,
            },
            None => EnsAnswer {
                position: None,
                holder: None,
            },
        })
    }
}

/// A lookup narrowed, or not, to one chain.
trait Scoped {
    /// `AND column = chain`, when a chain is named.
    fn scoped(&mut self, column: &'static str, chain: Option<i64>);
}

impl Scoped for Statement {
    fn scoped(&mut self, column: &'static str, chain: Option<i64>) {
        if let Some(chain) = chain {
            self.push(" AND ").push(column).push(" = ").push_bind(chain);
        }
    }
}

/// The chains a read covers: the one named, or every chain the store holds.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Scope(pub(crate) Option<i64>);

impl Scope {
    /// `resolveHandle`'s rows, behind `prefix`.
    pub(crate) fn handle_lookup(self, prefix: &str, handle_node: B256) -> Statement {
        let mut statement = QueryBuilder::new(format!(
            "{prefix}{} WHERE h.handle_node = ",
            sql::HANDLE_PROJECTION
        ));
        statement.push_bind(handle_node);
        statement.scoped("h.chain_id", self.0);
        statement.push(" ORDER BY h.chain_id");
        statement
    }

    /// `resolveId`'s rows, behind `prefix`.
    pub(crate) fn id_lookup(self, prefix: &str, id_node: B256) -> Statement {
        let mut statement = QueryBuilder::new(format!(
            "{prefix}{} WHERE i.id_node = ",
            sql::IDENTITY_PROJECTION
        ));
        statement.push_bind(id_node);
        statement.scoped("i.chain_id", self.0);
        statement.push(" ORDER BY i.chain_id");
        statement
    }

    /// The identities `owner` proved, behind `prefix`.
    pub(crate) fn identities_lookup(self, prefix: &str, owner: Address) -> Statement {
        let mut statement = QueryBuilder::new(format!(
            "{prefix}{} WHERE i.owner = ",
            sql::IDENTITY_PROJECTION
        ));
        statement.push_bind(owner);
        statement.scoped("i.chain_id", self.0);
        statement.push(" ORDER BY i.chain_id, i.platform_id, i.id_node");
        statement
    }
}

/// A search: live, disclosed handles in the chains in scope, narrowed by
/// whatever the caller named.
#[derive(Clone, Copy, Debug)]
pub struct Search<'a> {
    /// The chain searched, or every chain.
    pub chain: Option<i64>,
    /// The platform searched, or every platform.
    pub platform_id: Option<B256>,
    /// The holder whose handles are listed, or anyone's.
    pub owner: Option<Address>,
    /// Folded partial text: hits rank exact first, then prefix, then
    /// substring, then trigram similarity.
    pub folded_query: Option<&'a str>,
    /// How many hits to answer.
    pub limit: i64,
    /// How many hits to skip: a ranking is not a key order.
    pub offset: i64,
}

impl Search<'_> {
    /// The search's statement behind `prefix`, ranked by the text when there
    /// is one and by handle and chain otherwise.
    pub(crate) fn lookup(&self, prefix: &str) -> Statement {
        let mut statement =
            QueryBuilder::new(format!("{prefix}{}", sql::SEARCH_PROJECTION));
        statement.scoped("h.chain_id", self.chain);
        if let Some(platform_id) = self.platform_id {
            statement
                .push(" AND v.platform_id = ")
                .push_bind(platform_id);
        }
        if let Some(owner) = self.owner {
            statement.push(" AND h.owner = ").push_bind(owner);
        }
        match self.folded_query {
            Some(query) => {
                // LIKE metacharacters escaped, so the text matches itself.
                let like = query
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_");
                statement
                    .push(" AND (v.handle LIKE '%' || ")
                    .push_bind(like.clone())
                    .push(r#" || '%' ESCAPE '\' OR v.handle % "#)
                    .push_bind(query.to_string())
                    .push(") ORDER BY (v.handle = ")
                    .push_bind(query.to_string())
                    .push(") DESC, (v.handle LIKE ")
                    .push_bind(like.clone())
                    .push(r#" || '%' ESCAPE '\') DESC, (v.handle LIKE '%' || "#)
                    .push_bind(like)
                    .push(r#" || '%' ESCAPE '\') DESC, similarity(v.handle, "#)
                    .push_bind(query.to_string())
                    .push(") DESC, v.handle ASC, h.chain_id ASC");
            }
            None => {
                statement.push(" ORDER BY v.handle ASC, h.chain_id ASC");
            }
        }
        statement
            .push(" LIMIT ")
            .push_bind(self.limit)
            .push(" OFFSET ")
            .push_bind(self.offset);
        statement
    }
}
