//! HTTP client wrapper for a Subsonic-compatible server. Adds auth params
//! to every request, deserializes the standard `{subsonic-response: ...}`
//! envelope, and surfaces API errors as typed variants.

use bytes::Bytes;
use futures::StreamExt;
use media_source::ByteStream;
use serde::de::DeserializeOwned;
use url::Url;

use crate::auth::{auth_params, Credentials};
use crate::model;

const API_VERSION: &str = "1.16.1";
const CLIENT_ID: &str = "citunes";

#[derive(Debug, thiserror::Error)]
pub enum SubsonicError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("json parse: {0}")]
    Json(#[from] serde_json::Error),
    #[error("url: {0}")]
    Url(#[from] url::ParseError),
    #[error("subsonic api {code}: {message}")]
    Api { code: i32, message: String },
    #[error("subsonic returned status {0} without a valid error envelope")]
    UnexpectedStatus(u16),
    #[error("missing payload for endpoint {0}")]
    MissingPayload(&'static str),
}

pub struct Client {
    base_url: Url,
    creds: Credentials,
    http: reqwest::Client,
}

impl Client {
    pub fn new(base_url: Url, creds: Credentials) -> std::result::Result<Self, SubsonicError> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()?;
        Ok(Self { base_url, creds, http })
    }

    pub fn base_url(&self) -> &Url {
        &self.base_url
    }

    pub fn credentials_identity(&self) -> String {
        self.creds.identity()
    }

    /// Build the full URL for a `/rest/<endpoint>` call with fresh auth
    /// params and any extras. Extras override any collisions (won't happen
    /// in practice since our extra keys don't clash with auth).
    fn request_url(&self, endpoint: &str, extras: &[(&str, &str)]) -> Url {
        let mut url = self
            .base_url
            .join(&format!("rest/{}", endpoint))
            .expect("valid endpoint");
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("v", API_VERSION);
            q.append_pair("c", CLIENT_ID);
            q.append_pair("f", "json");
            for (k, v) in auth_params(&self.creds) {
                q.append_pair(k, &v);
            }
            for (k, v) in extras {
                q.append_pair(k, v);
            }
        }
        url
    }

    /// Perform a GET, deserialize the JSON envelope, and return `payload`
    /// (or Err if the API reported a failure).
    async fn get<T: DeserializeOwned>(
        &self,
        endpoint: &'static str,
        extras: &[(&str, &str)],
    ) -> std::result::Result<T, SubsonicError> {
        let url = self.request_url(endpoint, extras);
        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            // Subsonic servers send 200 for API errors too — non-2xx here
            // is a transport-layer issue (proxy, gateway, etc).
            return Err(SubsonicError::UnexpectedStatus(status.as_u16()));
        }
        let env: model::Envelope<T> = serde_json::from_slice(&bytes)?;
        if env.response.status != "ok" {
            let err = env.response.error.unwrap_or(model::ApiError {
                code: 0,
                message: "unspecified".into(),
            });
            return Err(SubsonicError::Api {
                code: err.code,
                message: err.message,
            });
        }
        env.response
            .payload
            .ok_or(SubsonicError::MissingPayload(endpoint))
    }

    pub async fn ping(&self) -> std::result::Result<PingResult, SubsonicError> {
        // Ping's payload is empty, but we need the envelope metadata (server
        // type + version) — grab it via a raw request.
        let url = self.request_url("ping", &[]);
        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            return Err(SubsonicError::UnexpectedStatus(status.as_u16()));
        }
        let env: model::Envelope<model::Empty> = serde_json::from_slice(&bytes)?;
        if env.response.status != "ok" {
            let err = env.response.error.unwrap_or(model::ApiError {
                code: 0,
                message: "unspecified".into(),
            });
            return Err(SubsonicError::Api {
                code: err.code,
                message: err.message,
            });
        }
        Ok(PingResult {
            server_type: env.response.server_type,
            server_version: env.response.server_version,
        })
    }

    pub async fn get_artists(&self) -> std::result::Result<model::ArtistsPayload, SubsonicError> {
        self.get("getArtists", &[]).await
    }

    pub async fn get_artist(
        &self,
        id: &str,
    ) -> std::result::Result<model::ArtistPayload, SubsonicError> {
        self.get("getArtist", &[("id", id)]).await
    }

    pub async fn get_album(
        &self,
        id: &str,
    ) -> std::result::Result<model::AlbumPayload, SubsonicError> {
        self.get("getAlbum", &[("id", id)]).await
    }

    pub async fn get_playlists(
        &self,
    ) -> std::result::Result<model::PlaylistsPayload, SubsonicError> {
        self.get("getPlaylists", &[]).await
    }

    pub async fn get_playlist(
        &self,
        id: &str,
    ) -> std::result::Result<model::PlaylistPayload, SubsonicError> {
        self.get("getPlaylist", &[("id", id)]).await
    }

    /// GET /rest/getCoverArt — returns the raw image bytes.
    pub async fn get_cover_art(&self, id: &str) -> std::result::Result<bytes::Bytes, SubsonicError> {
        let url = self.request_url("getCoverArt", &[("id", id)]);
        let resp = self.http.get(url).send().await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(SubsonicError::UnexpectedStatus(status.as_u16()));
        }
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        // Subsonic returns the JSON envelope with an error body when
        // something's wrong — Content-Type will be application/json in that
        // case. Bail so callers don't try to decode error text as an image.
        if ct.starts_with("application/json") {
            return Err(SubsonicError::Api {
                code: 70,
                message: "no cover art returned".into(),
            });
        }
        Ok(resp.bytes().await?)
    }

    /// GET /rest/stream — returns (content-type, total-bytes, served-range, body).
    pub async fn open_stream(
        &self,
        id: &str,
        range: Option<(u64, Option<u64>)>,
    ) -> std::result::Result<(&'static str, Option<u64>, Option<(u64, u64)>, ByteStream), SubsonicError> {
        // format=raw prevents server-side transcoding — we'll do our own.
        let url = self.request_url("stream", &[("id", id), ("format", "raw")]);
        let mut req = self.http.get(url);
        if let Some((start, end)) = range {
            let header = match end {
                Some(e) => format!("bytes={}-{}", start, e),
                None => format!("bytes={}-", start),
            };
            req = req.header("Range", header);
        }
        let resp = req.send().await?;
        let status = resp.status();
        if !status.is_success() && status.as_u16() != 206 {
            return Err(SubsonicError::UnexpectedStatus(status.as_u16()));
        }
        // Content-Type comes from server; we leak into 'static so it can go
        // into the StreamHandle field. Bounded per unique MIME.
        let ct_header = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let ct_static: &'static str = Box::leak(ct_header.into_boxed_str());

        let (served_range, total_from_range) = crate::parse_content_range(
            resp.headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|v| v.to_str().ok()),
        );
        let total_bytes = total_from_range.or_else(|| resp.content_length());

        let stream = resp
            .bytes_stream()
            .map(|r| r.map_err(|e| std::io::Error::other(format!("subsonic stream: {e}"))));
        let reader = tokio_util::io::StreamReader::new(stream);
        let body: ByteStream = Box::pin(reader);
        Ok((ct_static, total_bytes, served_range, body))
    }
}

pub struct PingResult {
    pub server_type: String,
    pub server_version: String,
}

// Suppress unused-import warning when Bytes ends up in the reader path only.
#[allow(dead_code)]
fn _keep_bytes_referenced(_: Bytes) {}
