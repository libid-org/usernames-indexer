//! Referral codes, end to end: binds go through the apply path the indexer
//! uses, and every assertion goes through the axum handlers a caller hits.
//!
//! Skips silently when `DATABASE_URL` is unset; everything past that check
//! panics on failure. The suite's chains are its own; its nodes start with
//! bytes no other suite's handles do.

use alloy::primitives::{
    Address,
    B256,
};
use axum::http::StatusCode;
use tokio::sync::Mutex;
use usernames_core::{
    api::model::{
        ErrorCode,
        Referrer,
    },
    db::{
        ChainStore,
        ChainWriter,
    },
    events::NamesEvent,
    nodes::KnownPlatform,
    referral::Code,
};
use usernames_fixtures::{
    Events,
    Reads,
    Reply,
    Seed,
    Suite,
    Writes,
};

const CHAIN: i64 = 31501;
const OTHER_CHAIN: i64 = 31502;

static SUITE: Mutex<()> = Mutex::const_new(());

const X: KnownPlatform = KnownPlatform::X;
const HOLDER: Address = Address::repeat_byte(0xA1);
const OTHER_HOLDER: Address = Address::repeat_byte(0xB2);

/// A node whose first bytes are `head`, the rest `tail` repeated: two such
/// nodes share exactly the bytes their heads share.
fn crafted(head: &[u8], tail: u8) -> B256 {
    let mut node = B256::repeat_byte(tail);
    node[..head.len()].copy_from_slice(head);
    node
}

/// The suite's two chains, emptied, and the right to write each.
struct Referrals {
    suite: Suite,
    other: ChainWriter,
}

impl Referrals {
    async fn open() -> Option<Self> {
        let suite = Suite::open(&SUITE, CHAIN).await?;
        let other = ChainStore::new(suite.pool.clone(), OTHER_CHAIN);
        other.forget().await;
        let other = other.writer().await;
        Some(Self { suite, other })
    }

    /// The chain's writer: the suite's own, or the other chain's.
    fn on(&self, chain: i64) -> &ChainWriter {
        if chain == CHAIN {
            &self.suite.writer
        } else {
            &self.other
        }
    }

    /// `node` bound to `holder` on `chain` at `block`, its text unknown and
    /// its account's id node the same bytes.
    async fn bind(&self, chain: i64, block: u64, node: B256, holder: Address) {
        self.on(chain)
            .apply(
                block,
                NamesEvent::IdentityBound {
                    holder,
                    id_node: node,
                    handle_node: node,
                    platform_id: X.id(),
                    observed_at: block,
                    ceremony_version: 1,
                },
            )
            .await;
    }

    /// `GET /v1/referral/node/{node}/code`.
    async fn code_of(&self, node: B256) -> Reply<Referrer> {
        self.suite
            .pool
            .get(&format!("/v1/referral/node/{node}/code"))
            .await
    }

    /// `GET /v1/referral/code/{code}`.
    async fn named(&self, code: &str) -> Reply<Referrer> {
        self.suite
            .pool
            .get(&format!("/v1/referral/code/{code}"))
            .await
    }

    fn done(self) {
        let Self { suite, other } = self;
        drop(other);
        suite.done();
    }
}

#[tokio::test]
async fn a_published_handle_goes_by_an_8_byte_code_that_shows_its_text() {
    let Some(referrals) = Referrals::open().await else {
        return;
    };
    let node = X.handle_node("referrer_alice").unwrap();
    referrals
        .suite
        .apply_tx(
            1,
            NamesEvent::bind(HOLDER, X, "71", "referrer_alice", 1, true),
        )
        .await;

    let shared = referrals.code_of(node).await.answer();
    let expected = Referrer {
        code: Code::prefixes(node).next().unwrap(),
        platform: Some(X),
        platform_id: X.id(),
        handle_node: node,
        primary_handle: Some("referrer_alice".into()),
        chains: vec![CHAIN],
    };
    assert_eq!(shared, expected);
    assert!(shared.code.to_string().len() <= 11, "{}", shared.code);

    let landed = referrals.named(&shared.code.to_string()).await.answer();
    assert_eq!(landed, expected);
    referrals.done();
}

#[tokio::test]
async fn a_handle_nobody_published_as_a_name_keeps_its_text_hidden() {
    let Some(referrals) = Referrals::open().await else {
        return;
    };
    let private = X.handle_node("referrer_private").unwrap();
    let revealed = X.handle_node("referrer_revealed").unwrap();
    referrals
        .suite
        .apply_tx(
            1,
            NamesEvent::bind(HOLDER, X, "72", "referrer_private", 1, false),
        )
        .await;
    let mut disclosed =
        NamesEvent::bind(OTHER_HOLDER, X, "73", "referrer_revealed", 2, false);
    disclosed.push(NamesEvent::revealed(X, "referrer_revealed"));
    referrals.suite.apply_tx(2, disclosed).await;

    for node in [private, revealed] {
        let referrer = referrals.code_of(node).await.answer();
        assert_eq!(referrer.primary_handle, None, "{node}");
        assert_eq!(referrer.handle_node, node);
    }
    referrals.done();
}

#[tokio::test]
async fn the_node_bound_first_keeps_a_shared_code_and_a_later_one_takes_a_longer_one() {
    let Some(referrals) = Referrals::open().await else {
        return;
    };
    let head = [0x5e, 0xf3, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
    // The node bound later sorts first, so only the bind order decides.
    let first = crafted(&head, 0x22);
    let later = crafted(&head, 0x11);
    referrals.bind(OTHER_CHAIN, 3, first, HOLDER).await;
    let shortest = referrals.code_of(first).await.answer().code;
    assert_eq!(shortest.prefix(), head);

    referrals.bind(CHAIN, 5, later, OTHER_HOLDER).await;
    let still = referrals.named(&shortest.to_string()).await.answer();
    assert_eq!(still.handle_node, first);
    assert_eq!(still.chains, vec![OTHER_CHAIN]);

    let longer = referrals.code_of(later).await.answer();
    assert_eq!(longer.code.prefix(), &later[..9]);
    assert_eq!(
        referrals
            .named(&longer.code.to_string())
            .await
            .answer()
            .handle_node,
        later
    );
    assert_eq!(referrals.code_of(first).await.answer().code, shortest);
    referrals.done();
}

#[tokio::test]
async fn a_code_goes_to_the_first_bind_by_time_then_by_chain() {
    let Some(referrals) = Referrals::open().await else {
        return;
    };
    let head = [0x5e, 0xf3, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f];
    let on_other = crafted(&head, 0x33);
    let on_chain = crafted(&head, 0x44);
    // Bound at the same block time: the lower chain id is first.
    referrals.bind(OTHER_CHAIN, 7, on_other, HOLDER).await;
    referrals.bind(CHAIN, 7, on_chain, OTHER_HOLDER).await;

    let code = Code::prefixes(on_chain).next().unwrap().to_string();
    assert_eq!(referrals.named(&code).await.answer().handle_node, on_chain);
    assert_eq!(
        referrals
            .code_of(on_other)
            .await
            .answer()
            .code
            .prefix()
            .len(),
        9
    );

    // A lagging chain's indexer writes the earlier bind last, and the node
    // bound later sorts first: the block time alone decides.
    let head = [0x5e, 0xf3, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f];
    let earlier = crafted(&head, 0x66);
    let later = crafted(&head, 0x55);
    referrals.bind(CHAIN, 9, later, OTHER_HOLDER).await;
    referrals.bind(OTHER_CHAIN, 8, earlier, HOLDER).await;
    let code = Code::prefixes(earlier).next().unwrap().to_string();
    assert_eq!(referrals.named(&code).await.answer().handle_node, earlier);
    referrals.done();
}

#[tokio::test]
async fn a_node_bound_on_two_chains_lists_both_and_a_retired_one_keeps_its_code() {
    let Some(referrals) = Referrals::open().await else {
        return;
    };
    let node = X.handle_node("referrer_twice").unwrap();
    referrals.bind(CHAIN, 1, node, HOLDER).await;
    referrals.bind(OTHER_CHAIN, 2, node, HOLDER).await;
    let both = referrals.code_of(node).await.answer();
    assert_eq!(both.chains, vec![CHAIN, OTHER_CHAIN]);

    for (chain, block) in [(CHAIN, 3), (OTHER_CHAIN, 4)] {
        referrals
            .on(chain)
            .apply(block, NamesEvent::retire(X, "referrer_twice", HOLDER))
            .await;
    }
    let retired = referrals.named(&both.code.to_string()).await.answer();
    assert_eq!(retired.handle_node, node);
    assert_eq!(retired.code, both.code);
    assert!(retired.chains.is_empty(), "{:?}", retired.chains);
    referrals.done();
}

#[tokio::test]
async fn a_prefix_of_all_0xff_and_one_of_leading_zeros_name_their_nodes() {
    let Some(referrals) = Referrals::open().await else {
        return;
    };
    let top = crafted(&[0xff; 8], 0x5e);
    let zeros = crafted(&[0, 0, 0, 0x5e, 0xf3, 0x5e, 0xf3, 0x5e], 0x5e);
    referrals.bind(CHAIN, 1, top, HOLDER).await;
    referrals.bind(CHAIN, 2, zeros, OTHER_HOLDER).await;

    for node in [top, zeros] {
        let code = referrals.code_of(node).await.answer().code;
        assert_eq!(code.prefix(), &node[..8]);
        let text = code.to_string();
        assert_eq!(
            referrals.named(&text).await.answer().handle_node,
            node,
            "{text}"
        );
    }
    let zeros_code = Code::prefixes(zeros).next().unwrap().to_string();
    assert!(zeros_code.starts_with("111"), "{zeros_code}");
    referrals.done();
}

#[tokio::test]
async fn a_code_or_node_nobody_bound_is_refused() {
    let Some(referrals) = Referrals::open().await else {
        return;
    };
    let bound = crafted(&[0x5e, 0xf3, 0xee], 0x55);
    let above = crafted(&[0x5e, 0xf3, 0xf0], 0x55);
    referrals.bind(CHAIN, 1, bound, HOLDER).await;
    referrals.bind(CHAIN, 2, above, HOLDER).await;
    let unbound = crafted(&[0x5e, 0xf3, 0xee], 0x66);
    let sharing = crafted(&[0x5e, 0xf3, 0xee, 0x55, 0x55, 0x55, 0x55, 0x55], 0x77);
    let unbound_code = crafted(&[0x5e, 0xf3, 0xef], 0x66);

    for node in [unbound, sharing] {
        assert_eq!(
            referrals.code_of(node).await.refusal(),
            (StatusCode::NOT_FOUND, ErrorCode::HandleNotBound),
            "{node}"
        );
    }
    let code = Code::prefixes(unbound_code).next().unwrap().to_string();
    assert_eq!(
        referrals.named(&code).await.refusal(),
        (StatusCode::NOT_FOUND, ErrorCode::CodeNotIssued)
    );
    referrals.done();
}
