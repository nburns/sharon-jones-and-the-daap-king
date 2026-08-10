//! ContentDirectory browsing: recursive traversal from root, DIDL-Lite XML
//! parsing, catalogue assembly.

use std::collections::VecDeque;

use media_source::{AudioFormat, TrackId};
use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use url::Url;

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Catalogue {
    pub items: Vec<AudioItem>,
    pub containers: Vec<AudioContainer>,
}

impl Catalogue {
    pub fn item_by_id(&self, id: TrackId) -> Option<&AudioItem> {
        self.items.iter().find(|i| i.id == id)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AudioItem {
    pub id: TrackId,
    pub dlna_id: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub track_number: Option<u16>,
    pub year: Option<u16>,
    pub duration_ms: Option<u32>,
    pub bitrate_kbps: Option<u32>,
    pub sample_rate: Option<u32>,
    pub size_bytes: Option<u64>,
    pub mime: Option<String>,
    pub format: AudioFormat,
    pub stream_url: Url,
    /// URL to fetch cover art (from `<upnp:albumArtURI>` in DIDL). Absolute.
    #[serde(default)]
    pub album_art_uri: Option<Url>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AudioContainer {
    pub id: u32,
    pub name: String,
    pub track_ids: Vec<TrackId>,
}

#[derive(Debug, thiserror::Error)]
pub enum BrowseError {
    #[error("http: {0}")]
    Http(#[from] reqwest::Error),
    #[error("upnp returned status {status}: {body}")]
    UpnpStatus { status: u16, body: String },
    #[error("xml parse: {0}")]
    Xml(#[from] quick_xml::Error),
    #[error("url: {0}")]
    Url(#[from] url::ParseError),
    #[error("browse response missing Result field")]
    MissingResult,
}

const MAX_DEPTH: u32 = 8;
const PAGE_SIZE: u32 = 200;

/// Container names we skip at depth 0 — these are non-audio media hierarchies
/// exposed by servers like Plex, Jellyfin, Emby. Case-insensitive match on
/// the container title. Empty title (root) always descends.
const SKIP_TOP_LEVEL: &[&str] = &["Video", "Videos", "Photos", "Photo", "Movies", "TV Shows", "TV"];

pub async fn build_catalogue(
    control_url: &Url,
    service_type: &str,
    base_url: &Url,
    http: &reqwest::Client,
    root_object_id: &str,
) -> Result<Catalogue, BrowseError> {
    let mut cat = Catalogue::default();
    let mut queue: VecDeque<(String, String, u32)> =
        VecDeque::from([(root_object_id.to_string(), String::new(), 0u32)]);
    let mut next_track_id: TrackId = 1;
    let mut next_container_id: u32 = 2;
    // Only filter noisy top-level media-type folders when browsing from the
    // real root ("0"). A user-supplied root is presumed intentional.
    let filter_top_level = root_object_id == "0";

    while let Some((object_id, name, depth)) = queue.pop_front() {
        if depth > MAX_DEPTH {
            tracing::warn!(object_id, depth, "skipping container beyond MAX_DEPTH");
            continue;
        }
        let (containers, items) =
            match browse_children(control_url, service_type, &object_id, base_url, http).await {
                Ok(pair) => pair,
                Err(err) => {
                    tracing::warn!(
                        object_id = %object_id,
                        name = %name,
                        depth = depth,
                        ?err,
                        "browse failed; skipping container"
                    );
                    continue;
                }
            };
        tracing::debug!(
            object_id = %object_id,
            name = %name,
            depth = depth,
            children_containers = containers.len(),
            children_items = items.len(),
            "browsed"
        );

        let mut container_track_ids = Vec::new();
        for mut item in items {
            item.id = next_track_id;
            next_track_id += 1;
            container_track_ids.push(item.id);
            cat.items.push(item);
        }
        if depth > 0 && !container_track_ids.is_empty() {
            cat.containers.push(AudioContainer {
                id: next_container_id,
                name: if name.is_empty() { object_id.clone() } else { name.clone() },
                track_ids: container_track_ids,
            });
            next_container_id += 1;
        }

        for (child_id, child_name) in containers {
            // Skip the top-level non-audio subtrees (Video, Photos, etc.) that
            // media servers expose alongside Music.
            if filter_top_level
                && depth == 0
                && SKIP_TOP_LEVEL
                    .iter()
                    .any(|s| s.eq_ignore_ascii_case(&child_name))
            {
                tracing::debug!(child_name, "skipping non-audio top-level container");
                continue;
            }
            queue.push_back((child_id, child_name, depth + 1));
        }
    }

    Ok(cat)
}

async fn browse_children(
    control_url: &Url,
    service_type: &str,
    object_id: &str,
    base_url: &Url,
    http: &reqwest::Client,
) -> Result<(Vec<(String, String)>, Vec<AudioItem>), BrowseError> {
    let mut containers = Vec::new();
    let mut items = Vec::new();
    let mut start: u32 = 0;
    loop {
        let envelope = format!(
            r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
<s:Body>
<u:Browse xmlns:u="{svc}">
<ObjectID>{oid}</ObjectID>
<BrowseFlag>BrowseDirectChildren</BrowseFlag>
<Filter>*</Filter>
<StartingIndex>{start}</StartingIndex>
<RequestedCount>{page}</RequestedCount>
<SortCriteria></SortCriteria>
</u:Browse>
</s:Body>
</s:Envelope>"#,
            svc = service_type,
            oid = xml_escape(object_id),
            start = start,
            page = PAGE_SIZE,
        );

        let response = http
            .post(control_url.as_str())
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPAction", format!("\"{}#Browse\"", service_type))
            .body(envelope)
            .send()
            .await?;

        let status = response.status();
        let body = response.text().await?;
        if !status.is_success() {
            return Err(BrowseError::UpnpStatus {
                status: status.as_u16(),
                body,
            });
        }
        tracing::trace!(object_id, %status, body_len = body.len(), "browse SOAP response");
        tracing::trace!(body = %body, "browse body");

        let (didl, returned, total_matches) = extract_browse_response(&body)?;
        tracing::debug!(
            object_id,
            returned,
            total_matches,
            didl_len = didl.len(),
            "parsed browse response"
        );
        tracing::trace!(didl = %didl, "extracted didl");

        let (mut c, mut i) = parse_didl_lite(&didl, base_url)?;
        containers.append(&mut c);
        items.append(&mut i);

        if returned == 0 {
            break;
        }
        start += returned;
        if start >= total_matches {
            break;
        }
    }
    Ok((containers, items))
}

/// Pull the DIDL-Lite `Result`, `NumberReturned`, `TotalMatches` out of the
/// SOAP envelope returned by a Browse call.
///
/// We use naive string slicing here rather than a real XML parser because the
/// Result field's content is XML-escaped inline (not CDATA), and both
/// quick-xml and roxmltree tend to fire text events per-entity-fragment,
/// which is a nightmare to reassemble. Byte-level substring extraction is
/// perfectly safe for these small SOAP envelopes.
fn extract_browse_response(soap: &str) -> Result<(String, u32, u32), BrowseError> {
    let didl_raw = slice_between(soap, "<Result>", "</Result>")
        .ok_or(BrowseError::MissingResult)?;
    // CDATA-wrapped payloads (some servers) — strip the CDATA envelope.
    let didl_raw = didl_raw
        .strip_prefix("<![CDATA[")
        .and_then(|s| s.strip_suffix("]]>"))
        .unwrap_or(didl_raw);
    let didl = quick_xml::escape::unescape(didl_raw)
        .map(|c| c.into_owned())
        .unwrap_or_else(|_| didl_raw.to_string());
    let returned = slice_between(soap, "<NumberReturned>", "</NumberReturned>")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let total = slice_between(soap, "<TotalMatches>", "</TotalMatches>")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    Ok((didl, returned, total))
}

fn slice_between<'a>(hay: &'a str, start: &str, end: &str) -> Option<&'a str> {
    let a = hay.find(start)? + start.len();
    let b = hay[a..].find(end)? + a;
    Some(&hay[a..b])
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[derive(Default, Debug)]
struct ItemBuilder {
    dlna_id: String,
    title: String,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    genre: Option<String>,
    track_number: Option<u16>,
    track_number_buf: Option<String>,
    year: Option<u16>,
    date_buf: Option<String>,
    upnp_class: String,
    res: Option<ResEntry>,
    album_art_uri: Option<String>,
}

#[derive(Debug)]
struct ResEntry {
    url: String,
    mime: Option<String>,
    duration_ms: Option<u32>,
    bitrate_kbps: Option<u32>,
    sample_rate: Option<u32>,
    size_bytes: Option<u64>,
}

enum Node {
    None,
    Container { id: String, title: String },
    Item(Box<ItemBuilder>),
}

#[derive(Debug, Clone, Copy)]
enum TextTarget {
    ContainerTitle,
    ItemTitle,
    ItemArtist,
    ItemAlbum,
    ItemAlbumArtist,
    ItemGenre,
    ItemClass,
    ItemTrack,
    ItemDate,
    ItemResUrl,
    ItemAlbumArtUri,
}

type ParseDidlResult = Result<(Vec<(String, String)>, Vec<AudioItem>), BrowseError>;

/// Parse a DIDL-Lite XML payload. Returns (child_containers, audio_items).
fn parse_didl_lite(xml: &str, base_url: &Url) -> ParseDidlResult {
    let mut reader = Reader::from_str(xml);
    // trim_text is intentionally OFF: quick-xml 0.38 splits text runs
    // around entity references (`&amp;` etc. are emitted as separate
    // Event::GeneralRef events), and trimming each fragment would drop
    // the whitespace around them - e.g. "Toots & The Maytals" would
    // arrive as ["Toots", "&", "The Maytals"] and concatenate into
    // "Toots&The Maytals". With trim off, we get the whitespace back;
    // final trim happens in finalize_item / container-close below.
    reader.config_mut().trim_text(false);

    let mut containers = Vec::new();
    let mut items = Vec::new();
    let mut node = Node::None;
    let mut text_target: Option<TextTarget> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => handle_start(&e, &mut node, &mut text_target),
            Ok(Event::Text(t)) => {
                let text = t
                    .decode()
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| String::from_utf8_lossy(&t).into_owned());
                if let Some(target) = text_target {
                    apply_text(&mut node, target, text);
                }
            }
            Ok(Event::GeneralRef(r)) => {
                // Entity reference (`&amp;`, `&lt;`, `&#65;`, ...) inside a
                // text run. Resolve to its literal string and append to the
                // active target so it slots in between the Text fragments
                // on either side.
                if let Some(target) = text_target {
                    let name = String::from_utf8_lossy(&r);
                    if let Some(resolved) = resolve_entity(&name) {
                        apply_text(&mut node, target, resolved);
                    }
                }
            }
            Ok(Event::CData(t)) => {
                // CDATA is literal - no entity decoding. Some servers wrap
                // arbitrary field values in CDATA to sidestep entity handling.
                let text = String::from_utf8_lossy(&t).into_owned();
                if let Some(target) = text_target {
                    apply_text(&mut node, target, text);
                }
            }
            Ok(Event::End(e)) => {
                let name = e.name();
                let local = local_name(name.as_ref());
                match local {
                    "container" => {
                        if let Node::Container { id, mut title } =
                            std::mem::replace(&mut node, Node::None)
                        {
                            // Trim edges of the assembled text run; interior
                            // whitespace (which now correctly surrounds any
                            // entity references) is preserved.
                            let trimmed = title.trim();
                            if trimmed.len() != title.len() {
                                title = trimmed.to_string();
                            }
                            containers.push((id, title));
                        }
                    }
                    "item" => {
                        if let Node::Item(b) = std::mem::replace(&mut node, Node::None)
                            && let Some(item) = finalize_item(*b, base_url)
                        {
                            items.push(item);
                        }
                    }
                    _ => {}
                }
                text_target = None;
            }
            Ok(Event::Eof) => break,
            Err(e) => return Err(BrowseError::Xml(e)),
            _ => {}
        }
    }

    Ok((containers, items))
}

fn handle_start(
    e: &BytesStart<'_>,
    node: &mut Node,
    text_target: &mut Option<TextTarget>,
) {
    let local = local_name(e.name().as_ref()).to_string();
    match local.as_str() {
        "container" => {
            let id = attr(e, b"id").unwrap_or_default();
            *node = Node::Container { id, title: String::new() };
        }
        "item" => {
            let b = Box::new(ItemBuilder {
                dlna_id: attr(e, b"id").unwrap_or_default(),
                ..ItemBuilder::default()
            });
            *node = Node::Item(b);
        }
        "title" => {
            *text_target = Some(match node {
                Node::Container { .. } => TextTarget::ContainerTitle,
                _ => TextTarget::ItemTitle,
            })
        }
        "artist" => *text_target = Some(TextTarget::ItemArtist),
        "album" => *text_target = Some(TextTarget::ItemAlbum),
        "albumArtist" => *text_target = Some(TextTarget::ItemAlbumArtist),
        "genre" => *text_target = Some(TextTarget::ItemGenre),
        "class" => *text_target = Some(TextTarget::ItemClass),
        "originalTrackNumber" => *text_target = Some(TextTarget::ItemTrack),
        "date" => *text_target = Some(TextTarget::ItemDate),
        "albumArtURI" => *text_target = Some(TextTarget::ItemAlbumArtUri),
        "res" => {
            if let Node::Item(b) = node
                && b.res.is_none()
            {
                let mime = attr(e, b"protocolInfo")
                    .and_then(|p| p.split(':').nth(2).map(str::to_string));
                let duration_ms = attr(e, b"duration").and_then(|s| parse_duration_ms(&s));
                let bitrate_kbps = attr(e, b"bitrate")
                    .and_then(|s| s.parse::<u32>().ok())
                    .map(|bps| bps.max(1) / 1000)
                    .filter(|&v| v > 0);
                let sample_rate = attr(e, b"sampleFrequency").and_then(|s| s.parse().ok());
                let size_bytes = attr(e, b"size").and_then(|s| s.parse().ok());
                b.res = Some(ResEntry {
                    url: String::new(),
                    mime,
                    duration_ms,
                    bitrate_kbps,
                    sample_rate,
                    size_bytes,
                });
                *text_target = Some(TextTarget::ItemResUrl);
            }
        }
        _ => {}
    }
}

/// Append `text` to whichever field `target` names inside `node`. We APPEND
/// rather than assign because quick-xml can fire multiple Event::Text events
/// for a single element when its content contains entity references (e.g.
/// `It&apos;s Over` arrives as Text("It"), Text("'"), Text("s Over")). An
/// assign-based implementation would silently keep only the last fragment.
fn apply_text(node: &mut Node, target: TextTarget, text: String) {
    match (node, target) {
        (Node::Container { title, .. }, TextTarget::ContainerTitle) => title.push_str(&text),
        (Node::Item(b), TextTarget::ItemTitle) => b.title.push_str(&text),
        (Node::Item(b), TextTarget::ItemArtist) => append_opt(&mut b.artist, &text),
        (Node::Item(b), TextTarget::ItemAlbum) => append_opt(&mut b.album, &text),
        (Node::Item(b), TextTarget::ItemAlbumArtist) => append_opt(&mut b.album_artist, &text),
        (Node::Item(b), TextTarget::ItemGenre) => append_opt(&mut b.genre, &text),
        (Node::Item(b), TextTarget::ItemClass) => b.upnp_class.push_str(&text),
        (Node::Item(b), TextTarget::ItemTrack) => {
            // Track numbers are ASCII digits; entities aren't a concern here,
            // but still fold text in case the reader delivered them chunked.
            let s = b.track_number_buf.get_or_insert_with(String::new);
            s.push_str(&text);
            b.track_number = s.trim().parse().ok();
        }
        (Node::Item(b), TextTarget::ItemDate) => {
            let s = b.date_buf.get_or_insert_with(String::new);
            s.push_str(&text);
            b.year = s.get(..4).and_then(|y| y.parse().ok());
        }
        (Node::Item(b), TextTarget::ItemResUrl) => {
            if let Some(r) = b.res.as_mut() {
                r.url.push_str(&text);
            }
        }
        (Node::Item(b), TextTarget::ItemAlbumArtUri) => {
            let s = b.album_art_uri.get_or_insert_with(String::new);
            s.push_str(&text);
        }
        _ => {}
    }
}

fn append_opt(dst: &mut Option<String>, text: &str) {
    match dst {
        Some(existing) => existing.push_str(text),
        None => *dst = Some(text.to_string()),
    }
}

fn finalize_item(b: ItemBuilder, base_url: &Url) -> Option<AudioItem> {
    if !b.upnp_class.contains("audioItem") {
        return None;
    }
    let res = b.res?;
    let stream_url = resolve_url(base_url, &res.url).ok()?;
    let format = classify_mime(res.mime.as_deref());
    let album_art_uri = b
        .album_art_uri
        .as_deref()
        .and_then(|s| resolve_url(base_url, s.trim()).ok());
    Some(AudioItem {
        id: 0,
        dlna_id: b.dlna_id,
        // Trim edges of assembled text runs. Interior whitespace (which
        // now correctly surrounds any entity references we handled) is
        // preserved.
        title: b.title.trim().to_string(),
        artist: b.artist.map(|s| s.trim().to_string()),
        album: b.album.map(|s| s.trim().to_string()),
        album_artist: b.album_artist.map(|s| s.trim().to_string()),
        genre: b.genre.map(|s| s.trim().to_string()),
        track_number: b.track_number,
        year: b.year,
        duration_ms: res.duration_ms,
        bitrate_kbps: res.bitrate_kbps,
        sample_rate: res.sample_rate,
        size_bytes: res.size_bytes,
        mime: res.mime,
        format,
        stream_url,
        album_art_uri,
    })
}

/// Resolve a DIDL entity reference (the name between `&` and `;`) to its
/// literal string form. Supports the five XML built-ins and numeric char
/// refs (`#65`, `#x41`). Unknown names return `None`, matching how a
/// lenient consumer would treat a stray entity - drop rather than error.
fn resolve_entity(name: &str) -> Option<String> {
    match name {
        "amp" => Some("&".to_string()),
        "lt" => Some("<".to_string()),
        "gt" => Some(">".to_string()),
        "quot" => Some("\"".to_string()),
        "apos" => Some("'".to_string()),
        n if n.starts_with('#') => {
            let rest = &n[1..];
            let code = if let Some(hex) = rest.strip_prefix('x').or_else(|| rest.strip_prefix('X')) {
                u32::from_str_radix(hex, 16).ok()?
            } else {
                rest.parse::<u32>().ok()?
            };
            char::from_u32(code).map(|c| c.to_string())
        }
        _ => None,
    }
}

fn local_name(name: &[u8]) -> &str {
    let s = std::str::from_utf8(name).unwrap_or("");
    match s.rfind(':') {
        Some(i) => &s[i + 1..],
        None => s,
    }
}

fn attr(e: &BytesStart<'_>, key: &[u8]) -> Option<String> {
    e.attributes()
        .with_checks(false)
        .flatten()
        .find(|a| a.key.as_ref() == key)
        .and_then(|a| a.unescape_value().ok().map(|c| c.into_owned()))
}

/// Parse a DIDL duration string like "0:03:32.500" or "0:03:32" into milliseconds.
fn parse_duration_ms(s: &str) -> Option<u32> {
    let mut parts = s.splitn(3, ':');
    let h: u64 = parts.next()?.parse().ok()?;
    let m: u64 = parts.next()?.parse().ok()?;
    let sec_part = parts.next()?;
    let (whole, frac) = sec_part.split_once('.').unwrap_or((sec_part, "0"));
    let s: u64 = whole.parse().ok()?;
    let ms_frac: u64 = {
        // e.g. "500" → 500ms, "5" → 500ms; scale to 3 digits.
        let mut buf = frac.to_string();
        while buf.len() < 3 {
            buf.push('0');
        }
        buf[..3].parse().unwrap_or(0)
    };
    let total_ms = ((h * 3600 + m * 60 + s) * 1000 + ms_frac).min(u32::MAX as u64);
    Some(total_ms as u32)
}

fn resolve_url(base: &Url, s: &str) -> Result<Url, url::ParseError> {
    if let Ok(abs) = Url::parse(s) {
        Ok(abs)
    } else {
        base.join(s)
    }
}

fn classify_mime(mime: Option<&str>) -> AudioFormat {
    match mime.unwrap_or("").to_ascii_lowercase().as_str() {
        "audio/mpeg" | "audio/mp3" => AudioFormat::Mp3,
        "audio/mp4" | "audio/aac" | "audio/x-m4a" => AudioFormat::Aac,
        "audio/flac" | "audio/x-flac" => AudioFormat::Flac,
        "audio/wav" | "audio/x-wav" => AudioFormat::Wav,
        "audio/aiff" | "audio/x-aiff" => AudioFormat::Aiff,
        "audio/ogg" | "application/ogg" => AudioFormat::Ogg,
        _ => AudioFormat::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_duration_ms_with_fractional_seconds() {
        assert_eq!(parse_duration_ms("0:00:01.500"), Some(1500));
        assert_eq!(parse_duration_ms("0:03:32.000"), Some(3 * 60_000 + 32_000));
        assert_eq!(parse_duration_ms("1:02:03"), Some((3600 + 120 + 3) * 1000));
        assert_eq!(parse_duration_ms("0:00:00.5"), Some(500));
    }

    #[test]
    fn parses_duration_ms_returns_none_on_garbage() {
        assert_eq!(parse_duration_ms("nope"), None);
        assert_eq!(parse_duration_ms(""), None);
    }

    #[test]
    fn classify_mime_maps_common_formats() {
        assert_eq!(classify_mime(Some("audio/mpeg")), AudioFormat::Mp3);
        assert_eq!(classify_mime(Some("audio/mp4")), AudioFormat::Aac);
        assert_eq!(classify_mime(Some("audio/flac")), AudioFormat::Flac);
        assert_eq!(classify_mime(Some("audio/x-wav")), AudioFormat::Wav);
        assert_eq!(classify_mime(None), AudioFormat::Other);
    }

    #[test]
    fn local_name_strips_namespace() {
        assert_eq!(local_name(b"dc:title"), "title");
        assert_eq!(local_name(b"upnp:albumArtURI"), "albumArtURI");
        assert_eq!(local_name(b"container"), "container");
    }

    #[test]
    fn resolve_url_handles_relative_and_absolute() {
        let base = Url::parse("http://server:1234/desc.xml").unwrap();
        assert_eq!(
            resolve_url(&base, "http://other/stream.mp3").unwrap().as_str(),
            "http://other/stream.mp3"
        );
        assert_eq!(
            resolve_url(&base, "/path/file.mp3").unwrap().as_str(),
            "http://server:1234/path/file.mp3"
        );
    }

    #[test]
    fn parses_minimal_didl_item() {
        let xml = r#"
<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"
           xmlns:dc="http://purl.org/dc/elements/1.1/"
           xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
  <item id="song1" parentID="album1">
    <dc:title>Test Song</dc:title>
    <upnp:artist>Test Artist</upnp:artist>
    <upnp:album>Test Album</upnp:album>
    <upnp:class>object.item.audioItem.musicTrack</upnp:class>
    <res protocolInfo="http-get:*:audio/mpeg:*"
         duration="0:03:32.500"
         size="1234567"
         bitrate="320000"
         sampleFrequency="44100">http://server:8200/stream/song1.mp3</res>
  </item>
</DIDL-Lite>"#;
        let base = Url::parse("http://server:8200/description.xml").unwrap();
        let (containers, items) = parse_didl_lite(xml, &base).unwrap();
        assert_eq!(containers.len(), 0);
        assert_eq!(items.len(), 1);
        let it = &items[0];
        assert_eq!(it.title, "Test Song");
        assert_eq!(it.artist.as_deref(), Some("Test Artist"));
        assert_eq!(it.album.as_deref(), Some("Test Album"));
        assert_eq!(it.duration_ms, Some(3 * 60_000 + 32_500));
        assert_eq!(it.size_bytes, Some(1_234_567));
        assert_eq!(it.bitrate_kbps, Some(320));
        assert_eq!(it.sample_rate, Some(44100));
        assert_eq!(it.mime.as_deref(), Some("audio/mpeg"));
        assert_eq!(it.format, AudioFormat::Mp3);
        assert_eq!(it.stream_url.as_str(), "http://server:8200/stream/song1.mp3");
    }

    #[test]
    fn parses_didl_container() {
        let xml = r#"
<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"
           xmlns:dc="http://purl.org/dc/elements/1.1/"
           xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
  <container id="albums" parentID="0">
    <dc:title>Albums</dc:title>
    <upnp:class>object.container</upnp:class>
  </container>
</DIDL-Lite>"#;
        let base = Url::parse("http://server:8200/description.xml").unwrap();
        let (containers, items) = parse_didl_lite(xml, &base).unwrap();
        assert_eq!(items.len(), 0);
        assert_eq!(containers, vec![("albums".to_string(), "Albums".to_string())]);
    }

    #[test]
    fn parses_title_with_apostrophe_entity() {
        // Real Plex/DIDL wraps apostrophes as &apos; which quick-xml delivers
        // across multiple Text events. Regression for the "s Over" bug where
        // "It's Over" was truncated to "s Over".
        let xml = r#"
<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"
           xmlns:dc="http://purl.org/dc/elements/1.1/"
           xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
  <item id="1" parentID="0">
    <dc:title>It's Over</dc:title>
    <upnp:artist>I've Waited</upnp:artist>
    <upnp:class>object.item.audioItem.musicTrack</upnp:class>
    <res protocolInfo="http-get:*:audio/mpeg:*">http://x/s.mp3</res>
  </item>
</DIDL-Lite>"#
            // simulate what parse_didl_lite sees after unescape from SOAP:
            // the &apos; is now a literal '
            ;
        let base = Url::parse("http://x/").unwrap();
        let (_, items) = parse_didl_lite(xml, &base).unwrap();
        assert_eq!(items.len(), 1, "item should parse");
        assert_eq!(items[0].title, "It's Over");
        assert_eq!(items[0].artist.as_deref(), Some("I've Waited"));
    }

    #[test]
    fn parses_title_with_ampersand_entity() {
        // Regression: real DIDL escapes literal `&` as `&amp;`. Text events
        // must be `.unescape()`d during parsing or the ampersand (plus
        // whatever whitespace surrounds it after fragment stitching) gets
        // dropped, turning "Toots & The Maytals" into "TootsThe Maytals".
        let xml = r#"
<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"
           xmlns:dc="http://purl.org/dc/elements/1.1/"
           xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
  <container id="c1" parentID="0">
    <dc:title>Toots &amp; The Maytals - Reggae Got Soul (1976)</dc:title>
    <upnp:class>object.container</upnp:class>
  </container>
  <item id="i1" parentID="c1">
    <dc:title>Rock &amp; Roll &lt;live&gt;</dc:title>
    <upnp:artist>G. Love &amp; Special Sauce</upnp:artist>
    <upnp:album>Yeah, It&apos;s That Easy</upnp:album>
    <upnp:class>object.item.audioItem.musicTrack</upnp:class>
    <res protocolInfo="http-get:*:audio/mpeg:*">http://x/s.mp3</res>
  </item>
</DIDL-Lite>"#;
        let base = Url::parse("http://x/").unwrap();
        let (containers, items) = parse_didl_lite(xml, &base).unwrap();
        assert_eq!(
            containers,
            vec![("c1".into(), "Toots & The Maytals - Reggae Got Soul (1976)".into())]
        );
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Rock & Roll <live>");
        assert_eq!(items[0].artist.as_deref(), Some("G. Love & Special Sauce"));
        assert_eq!(items[0].album.as_deref(), Some("Yeah, It's That Easy"));
    }

    #[test]
    fn extract_browse_response_pulls_escaped_result() {
        let soap = r#"<?xml version="1.0" encoding="UTF-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
<s:Body>
<u:BrowseResponse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
<Result>&lt;DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"&gt;&lt;container id="a"&gt;&lt;/container&gt;&lt;/DIDL-Lite&gt;</Result>
<NumberReturned>1</NumberReturned>
<TotalMatches>1</TotalMatches>
<UpdateID>1</UpdateID>
</u:BrowseResponse>
</s:Body>
</s:Envelope>"#;
        let (didl, returned, total) = extract_browse_response(soap).unwrap();
        assert_eq!(returned, 1);
        assert_eq!(total, 1);
        assert!(didl.starts_with("<DIDL-Lite"), "didl not unescaped: {didl}");
        assert!(didl.contains("<container id=\"a\">"));
    }

    #[test]
    fn extract_browse_response_pulls_cdata_result() {
        let soap = r#"<Result><![CDATA[<DIDL-Lite><item id="x"/></DIDL-Lite>]]></Result>
<NumberReturned>1</NumberReturned>
<TotalMatches>1</TotalMatches>"#;
        let (didl, _, _) = extract_browse_response(soap).unwrap();
        assert_eq!(didl, "<DIDL-Lite><item id=\"x\"/></DIDL-Lite>");
    }

    #[test]
    fn non_audio_items_are_skipped() {
        let xml = r#"
<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/"
           xmlns:dc="http://purl.org/dc/elements/1.1/"
           xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/">
  <item id="movie1" parentID="0">
    <dc:title>A Movie</dc:title>
    <upnp:class>object.item.videoItem</upnp:class>
    <res protocolInfo="http-get:*:video/mp4:*">http://server/movie.mp4</res>
  </item>
</DIDL-Lite>"#;
        let base = Url::parse("http://server/x").unwrap();
        let (_, items) = parse_didl_lite(xml, &base).unwrap();
        assert_eq!(items.len(), 0);
    }
}
