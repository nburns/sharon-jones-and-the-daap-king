//! Subsonic-API MediaSource. Compatible with Navidrome, Airsonic, Gonic,
//! Ampache, and other Subsonic-compatible servers.
//!
//! Two authentication modes:
//!   * OpenSubsonic `apiKey` — preferred. No password on the wire; revokable.
//!   * Legacy `u` + `t` + `s` (MD5-salted token). Universal but ties auth to
//!     the account password.

mod auth;
mod catalogue;
mod client;
mod model;

pub use auth::Credentials;
pub use catalogue::Catalogue;
pub use client::{Client, SubsonicError};

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use media_source::{
    AudioFormat, Database, DatabaseId, MediaSource, Playlist, Result, SourceError, StreamHandle,
    Track, TrackId,
};
use url::Url;

pub struct SubsonicSource {
    database: Database,
    catalogue: Arc<Catalogue>,
    client: Arc<Client>,
}

impl SubsonicSource {
    pub async fn connect(
        base_url: Url,
        creds: Credentials,
    ) -> std::result::Result<Self, SubsonicError> {
        let client = Arc::new(Client::new(base_url, creds)?);
        // Ping first — fail fast on unreachable/misconfigured server so the
        // user sees a real error instead of a slow catalogue build hang.
        let ping = client.ping().await?;
        tracing::info!(
            server_type = %ping.server_type,
            version = %ping.server_version,
            "Subsonic ping OK"
        );

        let catalogue = catalogue::build(&client).await?;
        tracing::info!(
            tracks = catalogue.tracks.len(),
            playlists = catalogue.playlists.len(),
            "Subsonic catalogue built"
        );
        Ok(Self {
            database: Database {
                id: 1,
                name: format!("{} ({})", ping.server_type, ping.server_version),
            },
            catalogue: Arc::new(catalogue),
            client,
        })
    }

    pub fn track_count(&self) -> usize {
        self.catalogue.tracks.len()
    }

    pub fn playlist_count(&self) -> usize {
        self.catalogue.playlists.len()
    }
}

#[async_trait]
impl MediaSource for SubsonicSource {
    async fn databases(&self) -> Result<Vec<Database>> {
        Ok(vec![self.database.clone()])
    }

    async fn tracks(&self, db: DatabaseId) -> Result<Vec<Track>> {
        if db != self.database.id {
            return Err(SourceError::NotFound);
        }
        Ok(self.catalogue.tracks.iter().map(entry_to_track).collect())
    }

    async fn playlists(&self, db: DatabaseId) -> Result<Vec<Playlist>> {
        if db != self.database.id {
            return Err(SourceError::NotFound);
        }
        Ok(self
            .catalogue
            .playlists
            .iter()
            .map(|p| Playlist {
                id: p.id,
                name: p.name.clone(),
                track_ids: p.track_ids.clone(),
            })
            .collect())
    }

    async fn open_stream(&self, db: DatabaseId, track: TrackId) -> Result<StreamHandle> {
        self.stream(db, track, None).await
    }

    async fn open_stream_range(
        &self,
        db: DatabaseId,
        track: TrackId,
        start: u64,
        end: Option<u64>,
    ) -> Result<StreamHandle> {
        self.stream(db, track, Some((start, end))).await
    }

    async fn artwork(&self, db: DatabaseId, track: TrackId) -> Result<Option<Bytes>> {
        if db != self.database.id {
            return Err(SourceError::NotFound);
        }
        let entry = self
            .catalogue
            .track_by_id(track)
            .ok_or(SourceError::NotFound)?;
        let cover_id = match entry.cover_art_id.as_deref() {
            Some(id) => id,
            None => return Ok(None),
        };
        match self.client.get_cover_art(cover_id).await {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) => {
                tracing::debug!(cover_id, ?err, "getCoverArt failed");
                Ok(None)
            }
        }
    }
}

impl SubsonicSource {
    async fn stream(
        &self,
        db: DatabaseId,
        track: TrackId,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<StreamHandle> {
        if db != self.database.id {
            return Err(SourceError::NotFound);
        }
        let entry = self
            .catalogue
            .track_by_id(track)
            .ok_or(SourceError::NotFound)?;
        let (content_type, total_bytes, served_range, body) = self
            .client
            .open_stream(&entry.subsonic_id, range)
            .await
            .map_err(|e| SourceError::Backend(e.to_string()))?;
        Ok(StreamHandle {
            content_type,
            total_bytes,
            range: served_range,
            body,
        })
    }
}

pub(crate) fn entry_to_track(e: &catalogue::TrackEntry) -> Track {
    Track {
        id: e.id,
        title: e.title.clone(),
        artist: e.artist.clone(),
        album: e.album.clone(),
        album_artist: e.album_artist.clone(),
        genre: e.genre.clone(),
        track_number: e.track_number,
        disc_number: e.disc_number,
        year: e.year,
        duration_ms: e.duration_ms,
        bitrate_kbps: e.bitrate_kbps,
        sample_rate: None,
        size_bytes: e.size_bytes,
        format: e.format,
    }
}

/// Parse an HTTP `Content-Range: bytes N-M/TOTAL` header. Returns
/// `((start, end), total)`. Both components are None if parsing fails.
pub(crate) fn parse_content_range(header: Option<&str>) -> (Option<(u64, u64)>, Option<u64>) {
    let s = match header {
        Some(s) => s.trim(),
        None => return (None, None),
    };
    let s = match s.strip_prefix("bytes ") {
        Some(rest) => rest,
        None => return (None, None),
    };
    let (range_part, total_part) = match s.split_once('/') {
        Some(t) => t,
        None => return (None, None),
    };
    let range = range_part
        .split_once('-')
        .and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)));
    let total = if total_part == "*" {
        None
    } else {
        total_part.parse().ok()
    };
    (range, total)
}

pub(crate) fn classify_content_type(ct: Option<&str>) -> AudioFormat {
    match ct.unwrap_or("").to_ascii_lowercase().as_str() {
        "audio/mpeg" | "audio/mp3" => AudioFormat::Mp3,
        "audio/mp4" | "audio/aac" | "audio/x-m4a" | "audio/m4a" => AudioFormat::Aac,
        "audio/flac" | "audio/x-flac" => AudioFormat::Flac,
        "audio/wav" | "audio/x-wav" => AudioFormat::Wav,
        "audio/aiff" | "audio/x-aiff" => AudioFormat::Aiff,
        "audio/ogg" | "application/ogg" | "audio/opus" => AudioFormat::Ogg,
        _ => AudioFormat::Other,
    }
}
