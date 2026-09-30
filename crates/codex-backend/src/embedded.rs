//! Runs this backend inside another process.
//!
//! Clodex starts the server on a loopback port on its own runtime thread
//! instead of spawning a separate proxy executable. Configuration is still
//! read from the `CCP_*` environment variables the server has always used;
//! the embedding process sets them before starting it.

use std::net::TcpListener as StdTcpListener;
use std::thread;

use anyhow::{Context, Result};
use tokio::sync::oneshot;

pub use crate::providers::codex::translate::model_allowlist::{CatalogModel, install_catalog};

/// A running embedded server. Dropping it shuts the server down and waits for
/// in-flight connections to finish draining.
pub struct EmbeddedServer {
    port: u16,
    shutdown: Option<oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<Result<()>>>,
}

impl EmbeddedServer {
    /// Binds an ephemeral loopback port and starts serving on it.
    pub fn start() -> Result<Self> {
        let listener = StdTcpListener::bind(("127.0.0.1", 0))
            .context("could not bind the embedded Codex backend")?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);

        let thread = thread::Builder::new()
            .name("codex-backend".to_string())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .context("could not create the embedded Codex backend runtime")?;
                runtime.block_on(async move {
                    let listener = tokio::net::TcpListener::from_std(listener)
                        .context("could not adopt the embedded Codex backend listener")?;
                    let _ = ready_tx.send(());
                    crate::server::serve_listener(listener, None, async {
                        let _ = shutdown_rx.await;
                    })
                    .await
                })
            })
            .context("could not start the embedded Codex backend thread")?;

        ready_rx
            .recv()
            .context("the embedded Codex backend did not start")?;
        Ok(Self {
            port,
            shutdown: Some(shutdown_tx),
            thread: Some(thread),
        })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Whether the server has stopped on its own, which only happens when it
    /// failed.
    pub fn has_stopped(&self) -> bool {
        self.thread
            .as_ref()
            .is_none_or(std::thread::JoinHandle::is_finished)
    }
}

impl Drop for EmbeddedServer {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};

    use super::*;

    #[test]
    fn serves_health_on_loopback_and_stops_when_dropped() {
        let server = EmbeddedServer::start().unwrap();
        let port = server.port();
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .write_all(b"GET /healthz HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(!server.has_stopped());

        drop(server);
        assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err());
    }
}
