//! The indexer's help, and the settings it refuses at parse.

use std::time::Duration;

use clap::{
    CommandFactory,
    Parser,
};

use usernames_indexer::Config;

/// clap renders `[env: NAME=value]` in `--help` for every argument bound
/// to a variable, and `.env` is read before parsing, so without
/// `hide_env_values` a secret is one `--help` away from a CI log.
#[test]
fn help_never_prints_a_secret() {
    let command = Config::command();
    for secret in ["database_url", "rpc_url"] {
        let arg = command
            .get_arguments()
            .find(|a| a.get_id() == secret)
            .unwrap_or_else(|| panic!("{secret} is an argument"));
        assert!(
            arg.is_hide_env_values_set(),
            "{secret} shows its value in --help"
        );
    }
}

/// The chain names are parsed by clap into their type, so a platform key
/// or a malformed label stops the process before anything connects.
#[test]
fn chain_names_are_refused_at_parse() {
    let args = |names: &str| {
        Config::try_parse_from([
            "usernames-indexer",
            "--database-url",
            "postgres://localhost/usernames",
            "--rpc-url",
            "http://127.0.0.1:8545",
            "--identity-registry-address",
            "0x0000000000000000000000000000000000000001",
            "--chain-names",
            names,
        ])
    };
    let names: Vec<String> = args("eden,base")
        .expect("two labels")
        .chain_names
        .iter()
        .map(|name| name.as_str().to_string())
        .collect();
    assert_eq!(names, ["eden", "base"]);
    for bad in ["eden,x", "Eden", "eden,"] {
        assert!(args(bad).is_err(), "{bad:?}");
    }
}

/// The configuration of `rpc` and `poll_interval`, as clap reads it.
fn polling(rpc: &str, poll_interval: &str) -> Result<Config, clap::Error> {
    Config::try_parse_from([
        "usernames-indexer",
        "--database-url",
        "postgres://localhost/usernames",
        "--rpc-url",
        rpc,
        "--identity-registry-address",
        "0x0000000000000000000000000000000000000001",
        "--chain-names",
        "eden",
        "--poll-interval",
        poll_interval,
    ])
}

/// The loop polls over HTTP, so a WebSocket URL is a usage error before
/// anything connects.
#[test]
fn a_websocket_rpc_url_is_refused_at_parse() {
    assert!(polling("https://node.example/rpc", "5s").is_ok());
    for rpc in ["wss://node.example/rpc", "ws://127.0.0.1:8546"] {
        let refused = polling(rpc, "5s").expect_err(rpc);
        assert_eq!(
            refused.kind(),
            clap::error::ErrorKind::ValueValidation,
            "{rpc}"
        );
    }
}

/// The store keeps report times in whole seconds, so a sub-second
/// report would expire as it is written.
#[test]
fn a_sub_second_stale_after_is_refused() {
    let config = |stale_after| {
        Config::try_parse_from([
            "usernames-indexer",
            "--database-url",
            "postgres://localhost/usernames",
            "--rpc-url",
            "http://127.0.0.1:8545",
            "--identity-registry-address",
            "0x0000000000000000000000000000000000000001",
            "--chain-names",
            "eden",
            "--poll-interval",
            "100ms",
            "--stale-after",
            stale_after,
        ])
        .expect("parses")
    };
    assert!(config("500ms").report_validity().is_err());
    assert_eq!(
        config("1s").report_validity().ok(),
        Some(Duration::from_secs(1))
    );
}

/// A zero interval would poll and retry back to back.
#[test]
fn a_zero_poll_interval_is_refused() {
    let config = |interval| polling("http://127.0.0.1:8545", interval).expect("parses");
    assert!(config("0s").poll_interval().is_err());
    assert_eq!(
        config("1s").poll_interval().ok(),
        Some(Duration::from_secs(1))
    );
}
