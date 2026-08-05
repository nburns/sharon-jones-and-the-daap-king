pub mod artwork;
pub mod charset;
pub mod content_codes;
pub mod dmap;
pub mod http;
pub mod mdns;
pub mod prefix_reader;
pub mod responses;
pub mod server_info;
pub mod session;
pub mod tags;
pub mod transcode;

use std::net::SocketAddr;
use std::sync::Arc;

use media_source::MediaSource;
use tokio::net::TcpListener;

use crate::http::{router, HandlerState};

#[derive(Debug, Clone)]
pub struct Config {
    pub name: String,
    pub bind: SocketAddr,
    pub transcode: transcode::Config,
    pub artwork: artwork::Config,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: "Classic iTunes Streamer".to_string(),
            bind: "0.0.0.0:3689".parse().unwrap(),
            transcode: transcode::Config::default(),
            artwork: artwork::Config::default(),
        }
    }
}

pub struct Server<S: MediaSource + 'static> {
    config: Config,
    source: Arc<S>,
}

impl<S: MediaSource + 'static> Server<S> {
    pub fn new(config: Config, source: S) -> Self {
        Self {
            config,
            source: Arc::new(source),
        }
    }

    pub async fn run(self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let state = Arc::new(HandlerState::new_full(
            self.config.name.clone(),
            self.source,
            self.config.transcode.clone(),
            self.config.artwork.clone(),
        ));
        let app = router(state).layer(axum::middleware::from_fn(log_request));
        let listener = TcpListener::bind(self.config.bind).await?;
        tracing::info!("daap-server listening on {}", self.config.bind);
        axum::serve(listener, app).await?;
        Ok(())
    }
}

async fn log_request(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let ua = req
        .headers()
        .get(axum::http::header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("-")
        .to_string();
    let started = std::time::Instant::now();
    let response = next.run(req).await;
    tracing::info!(
        status = response.status().as_u16(),
        method = %method,
        uri = %uri,
        ua = %ua,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "handled"
    );
    response
}
