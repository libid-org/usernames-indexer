//! Referral codes over the bound handle nodes: the node a code names, the
//! shortest code a node goes by, and the referrer a code shows.

use alloy::primitives::{
    Address,
    B256,
};
use sqlx::QueryBuilder;

use super::{
    sql,
    Statement,
    Store,
};
use crate::referral::Code;

/// A referrer's handle node on one chain that bound it.
#[derive(sqlx::FromRow)]
pub struct ReferrerRow {
    /// The chain that bound the node.
    pub chain_id: i64,
    /// The platform the node is a handle on.
    pub platform_id: B256,
    /// Its holder there, or `None` after retirement.
    pub owner: Option<Address>,
    /// Its text, where the holder published it as its name and the chain
    /// disclosed it.
    pub published_handle: Option<String>,
}

impl Code {
    /// The node this code names, behind `prefix`: of the bound nodes its
    /// bytes start, the one bound first. Above a prefix of all 0xff the
    /// range is open.
    pub(crate) fn owner_lookup(&self, prefix: &str) -> Statement {
        let mut statement =
            QueryBuilder::new(format!("{prefix}{}", sql::REFERRAL_CODE_OWNER));
        statement
            .push(" WHERE h.handle_node >= ")
            .push_bind(self.prefix().to_vec());
        if let Some(past) = self.past() {
            statement.push(" AND h.handle_node < ").push_bind(past);
        }
        statement.push(
            " ORDER BY bound.block_time, bound.chain_id, bound.block_number, \
             bound.log_index LIMIT 1",
        );
        statement
    }
}

impl Store {
    /// The node `code` names, `None` while no chain has bound a node it
    /// starts.
    pub async fn code_owner(&self, code: &Code) -> Result<Option<B256>, sqlx::Error> {
        code.owner_lookup("")
            .build_query_scalar()
            .fetch_optional(&self.pool)
            .await
    }

    /// The shortest code that names `node`, `None` when no chain has bound
    /// it. A code goes to the node bound first, so a node bound later than
    /// another sharing its first bytes takes a longer one; its whole 32
    /// bytes name it alone.
    pub async fn code_of(&self, node: B256) -> Result<Option<Code>, sqlx::Error> {
        for code in Code::prefixes(node) {
            match self.code_owner(&code).await? {
                Some(owner) if owner == node => return Ok(Some(code)),
                Some(_) => continue,
                None => return Ok(None),
            }
        }
        Ok(None)
    }

    /// `node` on every chain that bound it, by chain.
    pub async fn referrer(&self, node: B256) -> Result<Vec<ReferrerRow>, sqlx::Error> {
        sqlx::query_as(sql::REFERRAL_CODE_REFERRER)
            .bind(node)
            .fetch_all(&self.pool)
            .await
    }
}

#[cfg(test)]
pub(crate) mod plans {
    use sqlx::PgConnection;

    use super::*;
    use crate::db::plan_tests::{
        explain,
        Plan,
    };

    /// The referral codes' reads, planned.
    pub(crate) trait ReferralPlans {
        /// A code's owner, in the middle of the node range and at its top,
        /// and a referrer.
        async fn referral_plans(&mut self) -> Vec<Plan>;
    }

    impl ReferralPlans for PgConnection {
        async fn referral_plans(&mut self) -> Vec<Plan> {
            let owner: &[&str] = &["handles_node_chain_idx", "events_binds_idx"];
            let mut plans = Vec::new();
            for (name, node) in [
                ("a code's owner", B256::repeat_byte(0x7c)),
                ("a code's owner, all 0xff", B256::repeat_byte(0xff)),
            ] {
                let code = Code::prefixes(node).next().expect("a shortest code");
                plans.push(
                    Plan::of(self, name, Some(owner), code.owner_lookup("EXPLAIN "))
                        .await,
                );
            }
            let text: Vec<String> =
                sqlx::query_scalar(&explain(sql::REFERRAL_CODE_REFERRER))
                    .bind(B256::repeat_byte(3))
                    .fetch_all(&mut *self)
                    .await
                    .expect("a referrer's plan");
            plans.push(Plan {
                name: "a referrer",
                seeks: Some(&[
                    "handles_node_chain_idx",
                    "published_pkey",
                    "handle_nodes_pkey|handle_nodes_node_chain_idx",
                ]),
                text: text.join("\n"),
            });
            plans
        }
    }
}
