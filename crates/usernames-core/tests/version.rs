//! The indexer's version: what a build stamps on the chains it opens.

use usernames_core::{
    db::{
        INDEXER_VERSION,
        SHAPE_VERSION,
    },
    nodes::KnownPlatform,
};

/// A build that learns a platform stamps another version, so the chains
/// it opens replay and keep that platform's disclosed text.
#[test]
fn the_version_names_every_platform_the_build_knows() {
    let (shape, platforms) = INDEXER_VERSION
        .split_once('+')
        .expect("the shape, then the platforms");
    assert_eq!(shape, SHAPE_VERSION);
    let named: Vec<&str> = platforms.split(',').collect();
    for platform in KnownPlatform::ALL {
        assert!(named.contains(&platform.key()), "{platform:?}");
    }
}
