use axum::body::Bytes;
use std::time::Duration;
use tracing::{info, warn};

use crate::metrics::{UPSTREAM_FORWARD_ERRORS, UPSTREAM_FORWARDS};

/// Marker header attached to every relayed request. If an incoming request
/// already carries it, the gateway is (directly or indirectly) receiving its
/// own relay — e.g. the host's DNS for www.nepviewer.net is also spoofed back
/// to us — and the handler must not forward it again.
pub const RELAY_MARKER_HEADER: &str = "x-nep-gw-relay";

/// The real NEP cloud endpoint the microinverter was originally posting to.
pub const DEFAULT_UPSTREAM_URL: &str = "http://www.nepviewer.net/i.php";

/// The vhost the NEP cloud expects. Sent explicitly so that pinning the
/// upstream URL to a raw IP (when this host's own DNS is spoofed too) still
/// reaches the right site.
const UPSTREAM_HOST: &str = "www.nepviewer.net";

#[derive(Clone, Debug)]
pub struct UpstreamConfig {
    pub url: String,
}

/// Relays raw inverter POSTs to the real NEP cloud (dual-delivery mode).
///
/// Cheap to clone: `reqwest::Client` is an `Arc` internally.
#[derive(Clone)]
pub struct UpstreamForwarder {
    client: reqwest::Client,
    url: String,
}

impl UpstreamForwarder {
    pub fn new(config: UpstreamConfig) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to build upstream HTTP client");
        Self {
            client,
            url: config.url,
        }
    }

    /// Relay one raw inverter payload upstream. Fire-and-forget: callers spawn
    /// this so the inverter's time-sync response never waits on the cloud, and
    /// a cloud outage cannot affect local operation.
    pub async fn forward(&self, body: Bytes) {
        let size = body.len();
        let result = self
            .client
            .post(&self.url)
            .header(reqwest::header::HOST, UPSTREAM_HOST)
            .header(RELAY_MARKER_HEADER, "1")
            .body(body)
            .send()
            .await;

        match result {
            Ok(response) => {
                let status = response.status();
                let reply = response.text().await.unwrap_or_default();
                if status.is_success() {
                    UPSTREAM_FORWARDS.inc();
                    info!(
                        "Relayed {} bytes to upstream {} -> {} (reply: {:?})",
                        size, self.url, status, reply
                    );
                } else {
                    UPSTREAM_FORWARD_ERRORS.inc();
                    warn!(
                        "Upstream {} rejected relayed packet: {} (reply: {:?})",
                        self.url, status, reply
                    );
                }
            }
            Err(err) => {
                UPSTREAM_FORWARD_ERRORS.inc();
                warn!("Failed to relay packet to upstream {}: {}", self.url, err);
            }
        }
    }

    /// Deliver a raw inverter payload to the real cloud at `path`, byte-for-byte
    /// like the inverter itself.
    ///
    /// The NEP `/t.php` endpoint is picky: it accepts the inverter's *bare*
    /// request (Host + Connection: close + Content-Length, nothing else) but
    /// REJECTS a normal HTTP client's request -- with reqwest's default headers
    /// the server RSTs without ever acking the body (confirmed by packet
    /// capture). So we write the exact minimal request over a raw TCP socket.
    /// The endpoint sends no HTTP response, so we deliver and return whatever
    /// (usually nothing) comes back. Loop-safe because the container resolves
    /// `UPSTREAM_HOST` via a public resolver, never back to this gateway.
    pub async fn proxy(&self, path: &str, body: Bytes) -> Option<Vec<u8>> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpStream;

        let addr = format!("{}:80", UPSTREAM_HOST);
        let mut stream = match tokio::time::timeout(
            Duration::from_secs(5),
            TcpStream::connect(&addr),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                UPSTREAM_FORWARD_ERRORS.inc();
                warn!("Failed to connect to upstream {}: {}", addr, e);
                return None;
            }
            Err(_) => {
                UPSTREAM_FORWARD_ERRORS.inc();
                warn!("Timed out connecting to upstream {}", addr);
                return None;
            }
        };

        let head = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n",
            path,
            UPSTREAM_HOST,
            body.len()
        );
        if let Err(e) = stream.write_all(head.as_bytes()).await {
            UPSTREAM_FORWARD_ERRORS.inc();
            warn!("Failed to send request head to {}: {}", addr, e);
            return None;
        }
        if let Err(e) = stream.write_all(&body).await {
            UPSTREAM_FORWARD_ERRORS.inc();
            warn!("Failed to send body to {}: {}", addr, e);
            return None;
        }
        let _ = stream.flush().await;
        UPSTREAM_FORWARDS.inc();

        let mut buf = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut buf)).await;
        info!(
            "Delivered {} bytes to {}{} ({} resp bytes)",
            body.len(),
            UPSTREAM_HOST,
            path,
            buf.len()
        );
        Some(buf)
    }
}
