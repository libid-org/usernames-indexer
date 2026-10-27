//! The signals that ask a service to stop: ctrl-c by hand, and SIGTERM from
//! a container runtime or a supervisor, which kills the process if it has
//! not exited a grace period later.

use tokio::signal::unix::{
    signal,
    Signal,
    SignalKind,
};

/// The stop signals, listened for from the moment they are taken.
pub struct StopSignals {
    terminate: Signal,
}

impl StopSignals {
    /// Listen for ctrl-c and SIGTERM from now on. A process running as a
    /// container's PID 1 is not stopped by a SIGTERM nobody listens for, so
    /// a service takes these once it can stop cleanly.
    pub fn listen() -> std::io::Result<Self> {
        Ok(Self {
            terminate: signal(SignalKind::terminate())?,
        })
    }

    /// Wait for ctrl-c or SIGTERM.
    pub async fn received(&mut self) -> std::io::Result<()> {
        tokio::select! {
            r = tokio::signal::ctrl_c() => r,
            _ = self.terminate.recv() => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        process::Command,
        time::Duration,
    };

    use super::*;

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
            .expect("the SIGTERM is received")
            .unwrap();
    }
}
