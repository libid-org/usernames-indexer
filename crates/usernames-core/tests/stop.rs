//! A ctrl-c or SIGTERM asks the service to stop, from the moment the
//! listeners are held.

use std::{
    process::Command,
    time::Duration,
};

use usernames_core::stop::StopSignals;

/// A signal sent before the wait starts is still received: the
/// listeners are held from `listen` on.
#[tokio::test]
async fn a_ctrl_c_before_the_wait_asks_the_service_to_stop() {
    let mut signals = StopSignals::listen().unwrap();
    let sent = Command::new("kill")
        .args(["-INT", &std::process::id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());
    tokio::time::timeout(Duration::from_secs(5), signals.received())
        .await
        .expect("the ctrl-c is received");
}

#[tokio::test]
async fn a_sigterm_asks_the_service_to_stop() {
    let mut signals = StopSignals::listen().unwrap();
    let sent = Command::new("kill")
        .args(["-TERM", &std::process::id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());
    tokio::time::timeout(Duration::from_secs(5), signals.received())
        .await
        .expect("the SIGTERM is received");
}
