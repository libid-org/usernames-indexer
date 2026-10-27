//! The signals that ask a service to stop: ctrl-c by hand, and SIGTERM from
//! a container runtime or a supervisor, which kills the process if it has
//! not exited a grace period later.

use tokio::signal::unix::{
    signal,
    Signal,
    SignalKind,
};

/// The stop signals, each listened for from the moment they are taken
/// until this is dropped, so none sent in between is lost.
pub struct StopSignals {
    interrupt: Signal,
    terminate: Signal,
}

impl StopSignals {
    /// Listen for ctrl-c and SIGTERM from now on. A process running as a
    /// container's PID 1 ignores a SIGTERM nobody listens for, so a service
    /// takes these before anything else.
    pub fn listen() -> std::io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    /// Wait for ctrl-c or SIGTERM.
    pub async fn received(&mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
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
}
