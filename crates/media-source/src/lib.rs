use std::pin::Pin;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::AsyncRead;

pub type DatabaseId = u32;
pub type PlaylistId = u32;
pub type TrackId = u32;

#[derive(Debug, Clone)]
pub struct Database {
    pub id: DatabaseId,
    pub name: String,
}

#[derive(Debug, Clone)]
pub struct Playlist {
    pub id: PlaylistId,
    pub name: String,
    pub track_ids: Vec<TrackId>,
}

#[derive(Debug, Clone)]
pub struct Track {
    pub id: TrackId,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub track_number: Option<u16>,
    pub disc_number: Option<u16>,
    pub year: Option<u16>,
    pub duration_ms: Option<u32>,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
    pub size_bytes: Option<u64>,
    pub format: AudioFormat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AudioFormat {
    Mp3,
    Aac,
    Alac,
    Flac,
    Wav,
    Aiff,
    Ogg,
    Other,
}

impl AudioFormat {
    /// Formats iTunes 4 can play natively without transcoding.
    pub fn is_itunes4_native(self) -> bool {
        matches!(self, Self::Mp3 | Self::Aac | Self::Aiff | Self::Wav)
    }

    /// Container/extension iTunes expects for this format.
    pub fn extension(self) -> &'static str {
        match self {
            Self::Mp3 => "mp3",
            Self::Aac => "m4a",
            Self::Alac => "m4a",
            Self::Flac => "flac",
            Self::Wav => "wav",
            Self::Aiff => "aiff",
            Self::Ogg => "ogg",
            Self::Other => "bin",
        }
    }
}

pub type ByteStream = Pin<Box<dyn AsyncRead + Send + Unpin>>;

pub struct StreamHandle {
    pub content_type: &'static str,
    /// Full size of the underlying resource, when known.
    pub total_bytes: Option<u64>,
    /// Portion of the resource carried in `body`, when this handle represents
    /// a partial (Range) response. Inclusive on both ends.
    pub range: Option<(u64, u64)>,
    pub body: ByteStream,
}

impl StreamHandle {
    pub fn full(content_type: &'static str, total_bytes: Option<u64>, body: ByteStream) -> Self {
        Self {
            content_type,
            total_bytes,
            range: None,
            body,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("not found")]
    NotFound,
    #[error("backend error: {0}")]
    Backend(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, SourceError>;

#[async_trait]
pub trait MediaSource: Send + Sync {
    async fn databases(&self) -> Result<Vec<Database>>;
    async fn tracks(&self, db: DatabaseId) -> Result<Vec<Track>>;
    async fn playlists(&self, db: DatabaseId) -> Result<Vec<Playlist>>;
    async fn open_stream(&self, db: DatabaseId, track: TrackId) -> Result<StreamHandle>;

    /// Serve a byte range from a track. `end` is inclusive; None reads to EOF.
    /// Default implementation is correct but O(start): it opens the full
    /// stream and skips `start` bytes. Backends override to do a real seek
    /// (e.g. `File::seek`, HTTP `Range:` header) for a fast path.
    async fn open_stream_range(
        &self,
        db: DatabaseId,
        track: TrackId,
        start: u64,
        end: Option<u64>,
    ) -> Result<StreamHandle> {
        use tokio::io::AsyncReadExt;
        let mut handle = self.open_stream(db, track).await?;
        if start > 0 {
            let mut skipped = 0u64;
            let mut buf = vec![0u8; 64 * 1024];
            while skipped < start {
                let want = (start - skipped).min(buf.len() as u64) as usize;
                let n = handle.body.read(&mut buf[..want]).await?;
                if n == 0 {
                    break;
                }
                skipped += n as u64;
            }
        }
        let effective_end = end.or(handle.total_bytes.map(|t| t.saturating_sub(1)));
        let body: ByteStream = match effective_end {
            Some(e) if e >= start => {
                let take_n = e - start + 1;
                Box::pin(handle.body.take(take_n))
            }
            _ => handle.body,
        };
        Ok(StreamHandle {
            content_type: handle.content_type,
            total_bytes: handle.total_bytes,
            range: effective_end.map(|e| (start, e)),
            body,
        })
    }

    async fn artwork(&self, db: DatabaseId, track: TrackId) -> Result<Option<Bytes>>;
}
