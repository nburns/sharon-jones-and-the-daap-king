//! Filesystem-backed MediaSource: recursively scans a directory for supported
//! audio files and serves them as a single DAAP database. Metadata is derived
//! from filename + extension only (no ID3 parsing yet) — enough for a mock/dev
//! source that lets iTunes see and stream tracks.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use media_source::{
    AudioFormat, ByteStream, Database, DatabaseId, MediaSource, Playlist, Result, SourceError,
    StreamHandle, Track, TrackId,
};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
use walkdir::WalkDir;

pub struct FsSource {
    database: Database,
    root: PathBuf,
    /// Sorted list of (id, absolute path, format).
    entries: Arc<Vec<Entry>>,
    /// Auto-generated playlists — one per immediate subdirectory of `root`.
    /// Playlist IDs start at 2 to leave room for the library playlist (id 1).
    playlists: Arc<Vec<Playlist>>,
}

struct Entry {
    id: TrackId,
    path: PathBuf,
    format: AudioFormat,
    size: u64,
    meta: Meta,
}

#[derive(Debug, Default, Clone)]
struct Meta {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    genre: Option<String>,
    year: Option<u16>,
    track_number: Option<u16>,
    disc_number: Option<u16>,
    duration_ms: Option<u32>,
    bitrate_kbps: Option<u32>,
    sample_rate: Option<u32>,
}

impl FsSource {
    /// Scan `root` recursively and build the in-memory catalog.
    pub fn scan(name: impl Into<String>, root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        let mut paths: Vec<(PathBuf, AudioFormat, u64)> = WalkDir::new(&root)
            .follow_links(true)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_type().is_file())
            .filter_map(|e| {
                let path = e.path().to_path_buf();
                let format = classify(&path)?;
                let size = e.metadata().ok()?.len();
                Some((path, format, size))
            })
            .collect();
        paths.sort_by(|a, b| a.0.cmp(&b.0));

        let entries: Vec<Entry> = paths
            .into_iter()
            .enumerate()
            .map(|(i, (path, format, size))| {
                let meta = read_metadata(&path).unwrap_or_default();
                Entry {
                    id: (i + 1) as TrackId,
                    path,
                    format,
                    size,
                    meta,
                }
            })
            .collect();

        let playlists = build_subdir_playlists(&root, &entries);

        tracing::info!(
            root = %root.display(),
            count = entries.len(),
            playlists = playlists.len(),
            "FsSource scan complete"
        );

        Ok(Self {
            database: Database {
                id: 1,
                name: name.into(),
            },
            root,
            entries: Arc::new(entries),
            playlists: Arc::new(playlists),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn track_count(&self) -> usize {
        self.entries.len()
    }

    fn find(&self, id: TrackId) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }
}

fn classify(path: &Path) -> Option<AudioFormat> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "mp3" => AudioFormat::Mp3,
        "m4a" | "aac" => AudioFormat::Aac,
        "flac" => AudioFormat::Flac,
        "wav" => AudioFormat::Wav,
        "aiff" | "aif" => AudioFormat::Aiff,
        "ogg" | "oga" => AudioFormat::Ogg,
        _ => return None,
    })
}

fn track_from_entry(e: &Entry) -> Track {
    let fallback_title = e
        .path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    Track {
        id: e.id,
        title: e.meta.title.clone().unwrap_or(fallback_title),
        artist: e.meta.artist.clone(),
        album: e.meta.album.clone(),
        album_artist: e.meta.album_artist.clone(),
        genre: e.meta.genre.clone(),
        track_number: e.meta.track_number,
        disc_number: e.meta.disc_number,
        year: e.meta.year,
        duration_ms: e.meta.duration_ms,
        bitrate_kbps: e.meta.bitrate_kbps,
        sample_rate: e.meta.sample_rate,
        size_bytes: Some(e.size),
        format: e.format,
    }
}

/// Read tags + audio properties from a file. Best-effort — returns None on
/// unreadable/unsupported files.
fn read_metadata(path: &Path) -> Option<Meta> {
    use lofty::file::{AudioFile, TaggedFileExt};
    use lofty::probe::Probe;
    use lofty::tag::{Accessor, ItemKey};

    let tagged = Probe::open(path).ok()?.read().ok()?;
    let props = tagged.properties();

    let duration_ms = {
        let d = props.duration();
        let ms = d.as_millis();
        if ms > 0 { Some(ms.min(u32::MAX as u128) as u32) } else { None }
    };
    let bitrate_kbps = props.audio_bitrate();
    let sample_rate = props.sample_rate();

    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());

    let (title, artist, album, album_artist, genre, year, track_number, disc_number) =
        if let Some(t) = tag {
            (
                t.title().map(|c| c.to_string()),
                t.artist().map(|c| c.to_string()),
                t.album().map(|c| c.to_string()),
                t.get_string(&ItemKey::AlbumArtist).map(str::to_string),
                t.genre().map(|c| c.to_string()),
                t.year().map(|y| y.min(u16::MAX as u32) as u16),
                t.track().map(|n| n.min(u16::MAX as u32) as u16),
                t.disk().map(|n| n.min(u16::MAX as u32) as u16),
            )
        } else {
            (None, None, None, None, None, None, None, None)
        };

    Some(Meta {
        title,
        artist,
        album,
        album_artist,
        genre,
        year,
        track_number,
        disc_number,
        duration_ms,
        bitrate_kbps,
        sample_rate,
    })
}

#[async_trait]
impl MediaSource for FsSource {
    async fn databases(&self) -> Result<Vec<Database>> {
        Ok(vec![self.database.clone()])
    }

    async fn tracks(&self, db: DatabaseId) -> Result<Vec<Track>> {
        if db != self.database.id {
            return Err(SourceError::NotFound);
        }
        Ok(self.entries.iter().map(track_from_entry).collect())
    }

    async fn playlists(&self, db: DatabaseId) -> Result<Vec<Playlist>> {
        if db != self.database.id {
            return Err(SourceError::NotFound);
        }
        Ok((*self.playlists).clone())
    }

    async fn open_stream(&self, db: DatabaseId, track: TrackId) -> Result<StreamHandle> {
        if db != self.database.id {
            return Err(SourceError::NotFound);
        }
        let entry = self.find(track).ok_or(SourceError::NotFound)?;
        let file = File::open(&entry.path).await?;
        let body: ByteStream = Box::pin(file);
        Ok(StreamHandle::full(
            content_type_for(entry.format),
            Some(entry.size),
            body,
        ))
    }

    async fn open_stream_range(
        &self,
        db: DatabaseId,
        track: TrackId,
        start: u64,
        end: Option<u64>,
    ) -> Result<StreamHandle> {
        if db != self.database.id {
            return Err(SourceError::NotFound);
        }
        let entry = self.find(track).ok_or(SourceError::NotFound)?;
        if start >= entry.size {
            return Err(SourceError::Backend(format!(
                "range start {} beyond file size {}",
                start, entry.size
            )));
        }
        let mut file = File::open(&entry.path).await?;
        file.seek(SeekFrom::Start(start)).await?;
        let end_incl = end.map(|e| e.min(entry.size - 1)).unwrap_or(entry.size - 1);
        let take_n = end_incl - start + 1;
        let body: ByteStream = Box::pin(file.take(take_n));
        Ok(StreamHandle {
            content_type: content_type_for(entry.format),
            total_bytes: Some(entry.size),
            range: Some((start, end_incl)),
            body,
        })
    }

    async fn artwork(&self, db: DatabaseId, track: TrackId) -> Result<Option<Bytes>> {
        if db != self.database.id {
            return Err(SourceError::NotFound);
        }
        let entry = self.find(track).ok_or(SourceError::NotFound)?;
        // Try embedded first (APIC / covr / METADATA_BLOCK_PICTURE via lofty),
        // fall back to a folder-level image file (cover.jpg, folder.jpg, etc.)
        // next to the track. Both are best-effort: any error → None.
        if let Some(bytes) = read_embedded_artwork(&entry.path) {
            return Ok(Some(bytes));
        }
        if let Some(bytes) = read_folder_artwork(&entry.path).await {
            return Ok(Some(bytes));
        }
        Ok(None)
    }
}

/// Extract cover art embedded in the audio file's own metadata.
fn read_embedded_artwork(path: &Path) -> Option<Bytes> {
    use lofty::file::TaggedFileExt;
    use lofty::picture::PictureType;
    use lofty::probe::Probe;

    let tagged = Probe::open(path).ok()?.read().ok()?;
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag())?;
    // Prefer the CoverFront picture; fall back to anything present.
    let pic = tag
        .pictures()
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or_else(|| tag.pictures().first())?;
    Some(Bytes::copy_from_slice(pic.data()))
}

/// Look for common album-art filenames alongside the audio file. Returns
/// bytes of the first hit.
async fn read_folder_artwork(audio_path: &Path) -> Option<Bytes> {
    let dir = audio_path.parent()?;
    const CANDIDATES: &[&str] = &[
        "cover.jpg", "Cover.jpg", "cover.jpeg",
        "cover.png", "Cover.png",
        "folder.jpg", "Folder.jpg",
        "folder.png",
        "AlbumArt.jpg", "AlbumArtSmall.jpg",
        "album.jpg", "Album.jpg",
        "front.jpg", "Front.jpg",
    ];
    for name in CANDIDATES {
        let p = dir.join(name);
        if let Ok(bytes) = tokio::fs::read(&p).await {
            return Some(Bytes::from(bytes));
        }
    }
    None
}

/// One playlist per immediate subdirectory of `root`, containing every track
/// found under that subdirectory. Playlist IDs start at 2 (id 1 is reserved
/// for the synthesized library playlist).
fn build_subdir_playlists(root: &Path, entries: &[Entry]) -> Vec<Playlist> {
    use std::collections::BTreeMap;
    let mut by_subdir: BTreeMap<String, Vec<TrackId>> = BTreeMap::new();
    for e in entries {
        let rel = match e.path.strip_prefix(root) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let first = match rel.components().next() {
            Some(c) => c.as_os_str().to_string_lossy().into_owned(),
            None => continue,
        };
        // Only bucket entries whose first component is a directory (i.e. the
        // file lives at least one level below root).
        if rel.components().count() < 2 {
            continue;
        }
        by_subdir.entry(first).or_default().push(e.id);
    }
    by_subdir
        .into_iter()
        .enumerate()
        .map(|(i, (name, track_ids))| Playlist {
            id: (i as u32) + 2,
            name,
            track_ids,
        })
        .collect()
}

fn content_type_for(f: AudioFormat) -> &'static str {
    match f {
        AudioFormat::Mp3 => "audio/mpeg",
        AudioFormat::Aac | AudioFormat::Alac => "audio/mp4",
        AudioFormat::Flac => "audio/flac",
        AudioFormat::Wav => "audio/wav",
        AudioFormat::Aiff => "audio/aiff",
        AudioFormat::Ogg => "audio/ogg",
        AudioFormat::Other => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn scans_supported_extensions_only() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.mp3"), b"xx").unwrap();
        fs::write(dir.path().join("b.m4a"), b"yy").unwrap();
        fs::write(dir.path().join("readme.txt"), b"skip me").unwrap();
        let src = FsSource::scan("Test", dir.path()).unwrap();
        assert_eq!(src.track_count(), 2);
        let tracks = src.tracks(1).await.unwrap();
        assert_eq!(tracks.len(), 2);
        assert!(tracks.iter().any(|t| t.title == "a"));
        assert!(tracks.iter().any(|t| t.title == "b"));
        assert!(!tracks.iter().any(|t| t.title == "readme"));
    }

    #[tokio::test]
    async fn ids_are_stable_across_scans() {
        let dir = tempfile::tempdir().unwrap();
        for f in ["c.mp3", "a.mp3", "b.mp3"] {
            fs::write(dir.path().join(f), b"x").unwrap();
        }
        let s1 = FsSource::scan("T", dir.path()).unwrap();
        let s2 = FsSource::scan("T", dir.path()).unwrap();
        let t1 = s1.tracks(1).await.unwrap();
        let t2 = s2.tracks(1).await.unwrap();
        // sorted paths => same order => same IDs
        let ids1: Vec<_> = t1.iter().map(|t| (t.id, t.title.clone())).collect();
        let ids2: Vec<_> = t2.iter().map(|t| (t.id, t.title.clone())).collect();
        assert_eq!(ids1, ids2);
    }

    #[tokio::test]
    async fn open_stream_returns_file_contents() {
        use tokio::io::AsyncReadExt;
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("z.mp3"), b"MP3-DATA").unwrap();
        let src = FsSource::scan("T", dir.path()).unwrap();
        let tracks = src.tracks(1).await.unwrap();
        let mut handle = src.open_stream(1, tracks[0].id).await.unwrap();
        let mut buf = Vec::new();
        handle.body.read_to_end(&mut buf).await.unwrap();
        assert_eq!(&buf, b"MP3-DATA");
        assert_eq!(handle.total_bytes, Some(8));
        assert_eq!(handle.content_type, "audio/mpeg");
    }

    #[tokio::test]
    async fn subdirs_become_playlists() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("Rock")).unwrap();
        fs::create_dir(dir.path().join("Jazz")).unwrap();
        fs::write(dir.path().join("Rock/a.mp3"), b"x").unwrap();
        fs::write(dir.path().join("Rock/b.mp3"), b"x").unwrap();
        fs::write(dir.path().join("Jazz/c.mp3"), b"x").unwrap();
        fs::write(dir.path().join("top.mp3"), b"x").unwrap(); // root-level, no playlist

        let src = FsSource::scan("T", dir.path()).unwrap();
        let pls = src.playlists(1).await.unwrap();
        assert_eq!(pls.len(), 2);
        let jazz = pls.iter().find(|p| p.name == "Jazz").unwrap();
        assert_eq!(jazz.track_ids.len(), 1);
        let rock = pls.iter().find(|p| p.name == "Rock").unwrap();
        assert_eq!(rock.track_ids.len(), 2);
        // Ids must be >= 2 to avoid clashing with the synthetic library playlist.
        assert!(pls.iter().all(|p| p.id >= 2));
    }

    #[tokio::test]
    async fn open_stream_range_seeks_and_bounds() {
        use tokio::io::AsyncReadExt;
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("z.mp3"), b"0123456789ABCDEF").unwrap();
        let src = FsSource::scan("T", dir.path()).unwrap();
        let tracks = src.tracks(1).await.unwrap();
        let id = tracks[0].id;

        // [4, 9] inclusive = "456789"
        let mut h = src.open_stream_range(1, id, 4, Some(9)).await.unwrap();
        assert_eq!(h.range, Some((4, 9)));
        assert_eq!(h.total_bytes, Some(16));
        let mut buf = Vec::new();
        h.body.read_to_end(&mut buf).await.unwrap();
        assert_eq!(&buf, b"456789");

        // Open-ended [10, ..] = "ABCDEF"
        let mut h = src.open_stream_range(1, id, 10, None).await.unwrap();
        assert_eq!(h.range, Some((10, 15)));
        let mut buf = Vec::new();
        h.body.read_to_end(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ABCDEF");
    }

    #[tokio::test]
    async fn open_stream_range_start_past_end_errors() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("x.mp3"), b"tiny").unwrap();
        let src = FsSource::scan("T", dir.path()).unwrap();
        let tracks = src.tracks(1).await.unwrap();
        match src.open_stream_range(1, tracks[0].id, 1000, None).await {
            Err(SourceError::Backend(_)) => {}
            other => panic!("expected Backend err, got {:?}", other.map(|_| "ok")),
        }
    }

    #[tokio::test]
    async fn open_stream_of_unknown_id_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let src = FsSource::scan("T", dir.path()).unwrap();
        match src.open_stream(1, 999).await {
            Err(SourceError::NotFound) => {}
            other => panic!("expected NotFound, got {:?}", other.map(|_| "Ok(_)")),
        }
    }
}
