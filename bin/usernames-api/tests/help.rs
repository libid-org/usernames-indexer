//! The server's help.

use clap::CommandFactory;

use usernames_api::Config;

/// clap renders `[env: NAME=value]` in `--help` for every argument bound
/// to a variable, and `.env` is read before parsing, so without
/// `hide_env_values` a secret is one `--help` away from a CI log.
#[test]
fn help_never_prints_a_secret() {
    let command = Config::command();
    for secret in ["database_url", "ens_signer_key"] {
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
