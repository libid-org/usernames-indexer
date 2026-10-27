//! A TCP proxy in front of Postgres that can go silent: every socket stays
//! open and nothing crosses it, which is how a half-open connection or a hung
//! server looks to the client.

use std::net::SocketAddr;

use tokio::{
    io::{
        AsyncReadExt,
        AsyncWriteExt,
    },
    net::{
        tcp::{
            OwnedReadHalf,
            OwnedWriteHalf,
        },
        TcpListener,
        TcpStream,
    },
    sync::watch,
};
use url::Url;

/// The proxy's listening end and its switch.
pub struct Proxy {
    url: Url,
    silent: watch::Sender<bool>,
}

impl Proxy {
    /// Listen on a free local port, relaying every connection to the server
    /// `database` names.
    pub async fn start(database: &str) -> Self {
        let mut url: Url = database.parse().expect("a database URL");
        let upstream = format!(
            "{}:{}",
            url.host_str().expect("a database host"),
            url.port().unwrap_or(5432)
        );
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("a local port");
        let addr: SocketAddr = listener.local_addr().expect("the bound address");
        url.set_host(Some("127.0.0.1")).expect("a host");
        url.set_port(Some(addr.port())).expect("a port");
        let (silent, _) = watch::channel(false);
        let switch = silent.clone();
        tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let server = TcpStream::connect(&upstream)
                    .await
                    .expect("the database answers");
                let (from_client, to_client) = client.into_split();
                let (from_server, to_server) = server.into_split();
                tokio::spawn(pump(from_client, to_server, switch.subscribe()));
                tokio::spawn(pump(from_server, to_client, switch.subscribe()));
            }
        });
        Self { url, silent }
    }

    /// The database URL through the proxy.
    pub fn url(&self) -> &str {
        self.url.as_str()
    }

    /// Stop relaying, for good, and keep every socket open.
    pub fn silence(&self) {
        self.silent.send_replace(true);
    }
}

/// Copy one direction until either end closes, and hold both halves open,
/// relaying nothing, once silenced.
async fn pump(
    mut from: OwnedReadHalf,
    mut to: OwnedWriteHalf,
    mut silent: watch::Receiver<bool>,
) {
    let mut buffer = vec![0u8; 8192];
    loop {
        let read = tokio::select! {
            read = from.read(&mut buffer) => read,
            _ = silent.wait_for(|silent| *silent) => break,
        };
        let Ok(read @ 1..) = read else {
            return;
        };
        if *silent.borrow() {
            break;
        }
        if to.write_all(&buffer[..read]).await.is_err() {
            return;
        }
    }
    std::future::pending::<()>().await;
}
