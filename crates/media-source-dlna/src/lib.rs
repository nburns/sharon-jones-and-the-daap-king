//! DLNA/UPnP MediaSource: discovers (or connects to a configured) UPnP
//! MediaServer, browses its ContentDirectory for audio items, and proxies
//! their HTTP audio URLs through as our own streams.
//!
//! Structure:
//!   * `discover`: SSDP search for MediaServer:1 devices on the LAN.
//!   * `connect`: build a DlnaSource from a specific device (either
//!     discovered or a user-supplied device-description URL).
//!   * On construction we perform a full recursive Browse from root and
//!     build a flat catalogue of audio items indexed by our own u32 TrackId.

mod browse;

pub use browse::{AudioItem, Catalogue};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use media_source::{
    Database, DatabaseId, MediaSource, Playlist, Result, SourceError, StreamHandle, Track, TrackId,
};
use rupnp::ssdp::{SearchTarget, URN};
use rupnp::Device;
use url::Url;

pub const MEDIA_SERVER_URN: URN = URN::device("schemas-upnp-org", "MediaServer", 1);
const CONTENT_DIRECTORY_URN: URN = URN::service("schemas-upnp-org", "ContentDirectory", 1);

#[derive(Debug, thiserror::Error)]
pub enum DlnaError {
    #[error("no MediaServer:1 devices found on LAN")]
    NoServersDiscovered,
    #[error("named server not found: {0}")]
    NamedServerNotFound(String),
    #[error("device has no ContentDirectory service")]
    NoContentDirectory,
    #[error("upnp: {0}")]
    Upnp(#[from] rupnp::Error),
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("browse: {0}")]
    Browse(#[from] browse::BrowseError),
    #[error("url: {0}")]
    Url(#[from] url::ParseError),
    #[error("uri: {0}")]
    Uri(#[from] http::uri::InvalidUri),
}

/// A UPnP MediaServer seen on the LAN.
#[derive(Debug, Clone)]
pub struct DiscoveredServer {
    pub friendly_name: String,
    /// The device-description URL (`http://host:port/description.xml`)
    /// suitable for later passing to `DlnaSource::connect`.
    pub description_url: Url,
}

/// Perform an SSDP search for UPnP MediaServer:1 devices on the LAN.
/// Results are deduplicated by description URL — SSDP replies naturally arrive
/// multiple times per device.
pub async fn discover(timeout: Duration) -> std::result::Result<Vec<DiscoveredServer>, DlnaError> {
    use std::collections::HashSet;
    let search_target = SearchTarget::URN(MEDIA_SERVER_URN);
    let stream = rupnp::discover(&search_target, timeout, None).await?;
    tokio::pin!(stream);

    let mut seen: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    while let Some(res) = stream.next().await {
        match res {
            Ok(device) => {
                let url = uri_to_url(device.url())?;
                if seen.insert(url.to_string()) {
                    out.push(DiscoveredServer {
                        friendly_name: device.friendly_name().to_string(),
                        description_url: url,
                    });
                }
            }
            Err(err) => tracing::debug!(?err, "discovery entry error"),
        }
    }
    tracing::info!(count = out.len(), "SSDP discovery complete");
    Ok(out)
}

fn uri_to_url(uri: &http::Uri) -> std::result::Result<Url, url::ParseError> {
    Url::parse(&uri.to_string())
}

fn url_to_uri(url: &Url) -> std::result::Result<http::Uri, http::uri::InvalidUri> {
    url.as_str().parse::<http::Uri>()
}

pub struct DlnaSource {
    database: Database,
    catalogue: Arc<Catalogue>,
    http: reqwest::Client,
}

/// Optional persistent cache for a DLNA source's built catalogue.
/// If `path` exists on connect, load and skip the full browse.
/// After a fresh browse, the catalogue is written to `path`.
#[derive(Debug, Clone)]
pub struct CacheConfig {
    pub path: PathBuf,
}

impl DlnaSource {
    /// Connect to a specific UPnP MediaServer by its device-description URL.
    /// Browsing starts from the ContentDirectory root ("0"), no disk cache.
    pub async fn connect(description_url: &Url) -> std::result::Result<Self, DlnaError> {
        Self::connect_from(description_url, "0", None).await
    }

    /// Connect and start browsing from a specific ContentDirectory ObjectID
    /// (skips the usual `"0"` root). Optionally uses a disk cache: if the
    /// cache file exists, load the catalogue from it; otherwise browse and
    /// write it back. Useful for jumping straight into an audio-only subtree
    /// on servers whose root hierarchy is noisy — e.g. Plex's `All Artists`
    /// container under Music.
    pub async fn connect_from(
        description_url: &Url,
        root_object_id: &str,
        cache: Option<CacheConfig>,
    ) -> std::result::Result<Self, DlnaError> {
        let uri = url_to_uri(description_url)?;
        let device = Device::from_url(uri).await?;
        Self::from_device(device, root_object_id, cache).await
    }

    /// Convenience: run discovery, pick the first server whose friendly name
    /// contains `name_substring` (case-insensitive), and connect.
    pub async fn connect_named(
        name_substring: &str,
        timeout: Duration,
        root_object_id: &str,
        cache: Option<CacheConfig>,
    ) -> std::result::Result<Self, DlnaError> {
        let servers = discover(timeout).await?;
        let picked = servers
            .into_iter()
            .find(|s| {
                s.friendly_name
                    .to_lowercase()
                    .contains(&name_substring.to_lowercase())
            })
            .ok_or_else(|| DlnaError::NamedServerNotFound(name_substring.to_string()))?;
        Self::connect_from(&picked.description_url, root_object_id, cache).await
    }

    async fn from_device(
        device: Device,
        root_object_id: &str,
        cache: Option<CacheConfig>,
    ) -> std::result::Result<Self, DlnaError> {
        let _service = device
            .find_service(&CONTENT_DIRECTORY_URN)
            .ok_or(DlnaError::NoContentDirectory)?;

        let friendly_name = device.friendly_name().to_string();
        let base_url = uri_to_url(device.url())?;

        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()?;

        // rupnp keeps controlURL private, so fetch + parse the device
        // description XML ourselves to find the ContentDirectory control URL.
        let description = http.get(base_url.as_str()).send().await?.text().await?;
        let control_rel = find_control_url(&description, "ContentDirectory")
            .ok_or(DlnaError::NoContentDirectory)?;
        let control_url = base_url.join(&control_rel)?;

        tracing::info!(server = %friendly_name, url = %device.url(), control = %control_url, "connected to MediaServer");

        let catalogue = load_or_build_catalogue(
            cache.as_ref(),
            &control_url,
            &CONTENT_DIRECTORY_URN.to_string(),
            &base_url,
            &http,
            root_object_id,
        )
        .await?;
        tracing::info!(
            tracks = catalogue.items.len(),
            containers = catalogue.containers.len(),
            "DLNA catalogue built"
        );

        Ok(Self {
            database: Database {
                id: 1,
                name: friendly_name,
            },
            catalogue: Arc::new(catalogue),
            http,
        })
    }

    pub fn track_count(&self) -> usize {
        self.catalogue.items.len()
    }

    pub fn container_count(&self) -> usize {
        self.catalogue.containers.len()
    }
}

#[async_trait]
impl MediaSource for DlnaSource {
    async fn databases(&self) -> Result<Vec<Database>> {
        Ok(vec![self.database.clone()])
    }

    async fn tracks(&self, db: DatabaseId) -> Result<Vec<Track>> {
        if db != self.database.id {
            return Err(SourceError::NotFound);
        }
        Ok(self.catalogue.items.iter().map(item_to_track).collect())
    }

    async fn playlists(&self, db: DatabaseId) -> Result<Vec<Playlist>> {
        if db != self.database.id {
            return Err(SourceError::NotFound);
        }
        Ok(self
            .catalogue
            .containers
            .iter()
            .map(|c| Playlist {
                id: c.id,
                name: c.name.clone(),
                track_ids: c.track_ids.clone(),
            })
            .collect())
    }

    async fn open_stream(&self, db: DatabaseId, track: TrackId) -> Result<StreamHandle> {
        self.fetch_dlna_stream(db, track, None).await
    }

    async fn open_stream_range(
        &self,
        db: DatabaseId,
        track: TrackId,
        start: u64,
        end: Option<u64>,
    ) -> Result<StreamHandle> {
        self.fetch_dlna_stream(db, track, Some((start, end))).await
    }

    async fn artwork(&self, db: DatabaseId, track: TrackId) -> Result<Option<Bytes>> {
        if db != self.database.id {
            return Err(SourceError::NotFound);
        }
        let item = self
            .catalogue
            .item_by_id(track)
            .ok_or(SourceError::NotFound)?;
        let url = match &item.album_art_uri {
            Some(u) => u,
            None => return Ok(None),
        };
        match self.http.get(url.as_str()).send().await {
            Ok(resp) if resp.status().is_success() => match resp.bytes().await {
                Ok(bytes) => Ok(Some(bytes)),
                Err(err) => {
                    tracing::debug!(?err, %url, "artwork body read failed");
                    Ok(None)
                }
            },
            Ok(resp) => {
                tracing::debug!(status = %resp.status(), %url, "artwork fetch non-200");
                Ok(None)
            }
            Err(err) => {
                tracing::debug!(?err, %url, "artwork fetch failed");
                Ok(None)
            }
        }
    }
}

impl DlnaSource {
    async fn fetch_dlna_stream(
        &self,
        db: DatabaseId,
        track: TrackId,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<StreamHandle> {
        if db != self.database.id {
            return Err(SourceError::NotFound);
        }
        let item = self
            .catalogue
            .item_by_id(track)
            .ok_or(SourceError::NotFound)?;

        let mut req = self.http.get(item.stream_url.as_str());
        if let Some((start, end)) = range {
            let header = match end {
                Some(e) => format!("bytes={}-{}", start, e),
                None => format!("bytes={}-", start),
            };
            req = req.header("Range", header);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| SourceError::Backend(format!("dlna stream fetch: {e}")))?;
        if !resp.status().is_success() {
            return Err(SourceError::Backend(format!(
                "dlna stream returned {}",
                resp.status()
            )));
        }

        // Parse Content-Range if the server honored our Range request.
        let (served_range, total_bytes_from_range) = if range.is_some() {
            parse_content_range(resp.headers().get("Content-Range").and_then(|v| v.to_str().ok()))
        } else {
            (None, None)
        };
        let total_bytes = total_bytes_from_range.or_else(|| resp.content_length());
        let content_type = item.mime.as_deref().unwrap_or("application/octet-stream");
        // Leaking a String → &'static str keeps the trait signature ergonomic;
        // acceptable because content_type strings are bounded per unique MIME.
        let content_type_static: &'static str =
            Box::leak(content_type.to_string().into_boxed_str());

        let stream = resp
            .bytes_stream()
            .map(|r| r.map_err(|e| std::io::Error::other(format!("dlna stream: {e}"))));
        let reader = tokio_util::io::StreamReader::new(stream);
        Ok(StreamHandle {
            content_type: content_type_static,
            total_bytes,
            range: served_range,
            body: Box::pin(reader),
        })
    }
}

/// Parse an HTTP `Content-Range: bytes N-M/TOTAL` header. Returns
/// `((start, end), total)`. Both components are None if parsing fails.
fn parse_content_range(header: Option<&str>) -> (Option<(u64, u64)>, Option<u64>) {
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

#[cfg(test)]
mod content_range_tests {
    use super::*;

    #[test]
    fn parses_ordinary_content_range() {
        assert_eq!(
            parse_content_range(Some("bytes 1000-1999/5000")),
            (Some((1000, 1999)), Some(5000))
        );
    }

    #[test]
    fn parses_unknown_total() {
        assert_eq!(
            parse_content_range(Some("bytes 0-99/*")),
            (Some((0, 99)), None)
        );
    }

    #[test]
    fn none_on_missing_or_garbage() {
        assert_eq!(parse_content_range(None), (None, None));
        assert_eq!(parse_content_range(Some("garbage")), (None, None));
    }
}

async fn load_or_build_catalogue(
    cache: Option<&CacheConfig>,
    control_url: &Url,
    service_type: &str,
    base_url: &Url,
    http: &reqwest::Client,
    root_object_id: &str,
) -> std::result::Result<browse::Catalogue, DlnaError> {
    if let Some(cfg) = cache {
        match load_catalogue_from(&cfg.path) {
            Ok(cat) => {
                tracing::info!(
                    path = %cfg.path.display(),
                    tracks = cat.items.len(),
                    containers = cat.containers.len(),
                    "loaded DLNA catalogue from cache"
                );
                return Ok(cat);
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::info!(path = %cfg.path.display(), "cache miss — building fresh catalogue");
            }
            Err(err) => {
                tracing::warn!(path = %cfg.path.display(), ?err, "cache read failed — rebuilding");
            }
        }
    }

    let cat =
        browse::build_catalogue(control_url, service_type, base_url, http, root_object_id).await?;

    if let Some(cfg) = cache {
        if let Err(err) = save_catalogue_to(&cfg.path, &cat) {
            tracing::warn!(path = %cfg.path.display(), ?err, "failed to write catalogue cache");
        } else {
            tracing::info!(path = %cfg.path.display(), "wrote DLNA catalogue cache");
        }
    }
    Ok(cat)
}

fn load_catalogue_from(path: &Path) -> std::io::Result<browse::Catalogue> {
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

fn save_catalogue_to(path: &Path, cat: &browse::Catalogue) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(cat)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Derive a stable cache filename from server URL + root object id, so
/// different servers (or the same server with a different browse root) don't
/// clobber each other.
pub fn cache_filename(description_url: &Url, root_object_id: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    description_url.as_str().hash(&mut h);
    root_object_id.hash(&mut h);
    format!("dlna-catalogue-{:016x}.json", h.finish())
}

/// Find the `controlURL` element for a service whose `serviceType` contains
/// the given substring (e.g. "ContentDirectory"). Very small regex-free parse.
fn find_control_url(description_xml: &str, service_type_substr: &str) -> Option<String> {
    // We look for `<service>...<serviceType>...ContentDirectory...</serviceType>
    // ...<controlURL>PATH</controlURL>...</service>` — split into blocks and
    // scan each. Cheap enough; device descriptions are small.
    let lower = description_xml.to_ascii_lowercase();
    let sub = service_type_substr.to_ascii_lowercase();

    let mut pos = 0;
    while let Some(start_rel) = lower[pos..].find("<service") {
        let start = pos + start_rel;
        let end_rel = lower[start..].find("</service>")?;
        let end = start + end_rel;
        let block = &description_xml[start..end];
        let block_lower = &lower[start..end];
        if block_lower.contains(&sub) {
            if let Some(url) = extract_tag(block, "controlURL") {
                return Some(url);
            }
        }
        pos = end + "</service>".len();
    }
    None
}

fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}>", tag);
    let close = format!("</{}>", tag);
    let a = xml.find(&open)? + open.len();
    let b = xml[a..].find(&close)? + a;
    Some(xml[a..b].trim().to_string())
}

#[cfg(test)]
mod description_tests {
    use super::*;

    const EXAMPLE_DEVICE_XML: &str = r#"<?xml version="1.0"?>
<root>
  <device>
    <serviceList>
      <service>
        <serviceType>urn:microsoft.com:service:X_MS_MediaReceiverRegistrar:1</serviceType>
        <controlURL>/foo/control.xml</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ContentDirectory:1</serviceType>
        <controlURL>/ContentDirectory/abc/control.xml</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <controlURL>/ConnectionManager/abc/control.xml</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    #[test]
    fn finds_control_url_for_matching_service() {
        assert_eq!(
            find_control_url(EXAMPLE_DEVICE_XML, "ContentDirectory"),
            Some("/ContentDirectory/abc/control.xml".to_string())
        );
        assert_eq!(
            find_control_url(EXAMPLE_DEVICE_XML, "ConnectionManager"),
            Some("/ConnectionManager/abc/control.xml".to_string())
        );
    }

    #[test]
    fn returns_none_when_service_absent() {
        assert_eq!(find_control_url(EXAMPLE_DEVICE_XML, "MissingService"), None);
    }

    #[test]
    fn extract_tag_pulls_inner_text() {
        assert_eq!(extract_tag("<foo>bar</foo>", "foo"), Some("bar".to_string()));
        assert_eq!(extract_tag("<foo> spaced </foo>", "foo"), Some("spaced".to_string()));
        assert_eq!(extract_tag("no match", "foo"), None);
    }
}

fn item_to_track(item: &AudioItem) -> Track {
    Track {
        id: item.id,
        title: item.title.clone(),
        artist: item.artist.clone(),
        album: item.album.clone(),
        album_artist: item.album_artist.clone(),
        genre: item.genre.clone(),
        track_number: item.track_number,
        disc_number: None,
        year: item.year,
        duration_ms: item.duration_ms,
        bitrate_kbps: item.bitrate_kbps,
        sample_rate: item.sample_rate,
        size_bytes: item.size_bytes,
        format: item.format,
    }
}
