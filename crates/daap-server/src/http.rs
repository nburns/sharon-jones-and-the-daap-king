//! axum wiring: routes, shared handler state, response helpers.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use bytes::BytesMut;
use media_source::{DatabaseId, MediaSource, TrackId};
use serde::Deserialize;
use tokio_util::io::ReaderStream;

use crate::artwork::{self, Artworker, OutputVariant, Prepared};
use crate::charset::{charset_from_accept, Charset};
use crate::content_codes;
use crate::prefix_reader::PrefixReader;
use crate::responses;
use crate::server_info::{self, ClientDialect, ServerInfo};
use crate::session::SessionStore;
use crate::transcode::{self, choose_format, client_supports_modern_codecs, ServedFormat, Transcoder};

pub struct HandlerState<S: MediaSource + 'static> {
    pub name: String,
    pub source: Arc<S>,
    pub sessions: SessionStore,
    pub revision: u32,
    /// Synthesized "Library" playlist id. Must be a small positive value so
    /// iTunes 4 (which treats DMAP ints as signed) round-trips it in URLs
    /// as the same unsigned value. Collides with any real playlist that has
    /// id 1 — caller's responsibility to keep source playlist ids > 1.
    pub library_playlist_id: u32,
    pub transcoder: Arc<Transcoder>,
    pub artworker: Arc<Artworker>,
}

impl<S: MediaSource + 'static> HandlerState<S> {
    pub fn new(name: String, source: Arc<S>) -> Self {
        Self::new_with_transcode(name, source, transcode::Config::default())
    }

    pub fn new_with_transcode(
        name: String,
        source: Arc<S>,
        transcode_cfg: transcode::Config,
    ) -> Self {
        Self::new_full(name, source, transcode_cfg, artwork::Config::default())
    }

    pub fn new_full(
        name: String,
        source: Arc<S>,
        transcode_cfg: transcode::Config,
        artwork_cfg: artwork::Config,
    ) -> Self {
        Self {
            name,
            source,
            sessions: SessionStore::new(),
            revision: 2,
            library_playlist_id: 1,
            transcoder: Arc::new(Transcoder::new(transcode_cfg)),
            artworker: Arc::new(Artworker::new(artwork_cfg)),
        }
    }
}

pub fn router<S: MediaSource + 'static>(state: Arc<HandlerState<S>>) -> Router {
    Router::new()
        .route("/server-info", get(handle_server_info::<S>))
        .route("/content-codes", get(handle_content_codes::<S>))
        .route("/login", get(handle_login::<S>))
        .route("/logout", get(handle_logout::<S>))
        .route("/update", get(handle_update::<S>))
        .route("/databases", get(handle_databases::<S>))
        .route("/databases/{db}/items", get(handle_items::<S>))
        .route("/databases/{db}/containers", get(handle_containers::<S>))
        .route(
            "/databases/{db}/containers/{cid}/items",
            get(handle_container_items::<S>),
        )
        .route(
            "/databases/{db}/items/{track_file}",
            get(handle_stream::<S>),
        )
        .route(
            "/databases/{db}/items/{track_id}/extra_data/artwork",
            get(handle_artwork::<S>),
        )
        .with_state(state)
}

// ---- handlers ----

async fn handle_server_info<S: MediaSource + 'static>(
    State(state): State<Arc<HandlerState<S>>>,
    headers: HeaderMap,
) -> Response {
    let dialect = ClientDialect::from_header(
        headers
            .get("Client-DAAP-Version")
            .and_then(|v| v.to_str().ok()),
    );

    let databases = state.source.databases().await.unwrap_or_default();

    let info = ServerInfo {
        name: &state.name,
        database_count: databases.len() as u32,
        requires_password: false,
        dialect,
    };
    dmap_response(server_info::encode(&info))
}

async fn handle_content_codes<S: MediaSource + 'static>(
    State(_state): State<Arc<HandlerState<S>>>,
) -> Response {
    dmap_response(content_codes::encode())
}

async fn handle_login<S: MediaSource + 'static>(
    State(state): State<Arc<HandlerState<S>>>,
) -> Response {
    let sid = state.sessions.create();
    dmap_response(responses::login(sid))
}

async fn handle_logout<S: MediaSource + 'static>(
    State(state): State<Arc<HandlerState<S>>>,
    Query(SessionQuery { session_id }): Query<SessionQuery>,
) -> StatusCode {
    if let Some(id) = session_id {
        state.sessions.end(id);
    }
    StatusCode::NO_CONTENT
}

#[derive(Deserialize)]
struct UpdateQuery {
    #[serde(rename = "revision-number")]
    revision_number: Option<u32>,
}

async fn handle_update<S: MediaSource + 'static>(
    State(state): State<Arc<HandlerState<S>>>,
    Query(q): Query<UpdateQuery>,
) -> Response {
    // Long-poll semantics: if the client's known revision equals ours, they
    // want to be told when it changes. Since our library never mutates in
    // this MVP, we hang until the client hangs up. Returning immediately
    // makes iTunes think there was an update and re-fetches everything in a
    // tight loop.
    if q.revision_number == Some(state.revision) {
        // Hang for a long time — the client will drop the connection when
        // it navigates away, and axum tears down this future.
        tokio::time::sleep(std::time::Duration::from_secs(60 * 60 * 24 * 365)).await;
    }
    dmap_response(responses::update(state.revision))
}

async fn handle_databases<S: MediaSource + 'static>(
    State(state): State<Arc<HandlerState<S>>>,
    headers: HeaderMap,
) -> Response {
    let cs = charset_from_accept(headers.get(header::ACCEPT_CHARSET).and_then(|v| v.to_str().ok()));
    let dbs = state.source.databases().await.unwrap_or_default();
    let (track_count, playlist_count) = if let Some(db) = dbs.first() {
        let t = state.source.tracks(db.id).await.map(|v| v.len() as u32).unwrap_or(0);
        let p = state.source.playlists(db.id).await.map(|v| v.len() as u32 + 1).unwrap_or(1);
        (t, p)
    } else {
        (0, 0)
    };
    dmap_response_cs(responses::databases(&dbs, track_count, playlist_count, cs), cs)
}

async fn handle_items<S: MediaSource + 'static>(
    State(state): State<Arc<HandlerState<S>>>,
    Path(db): Path<DatabaseId>,
    Query(idx): Query<IndexQuery>,
    headers: HeaderMap,
) -> Response {
    let mut tracks = state.source.tracks(db).await.unwrap_or_default();
    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
    let cs = charset_from_accept(headers.get(header::ACCEPT_CHARSET).and_then(|v| v.to_str().ok()));
    let cfg = state.transcoder.config().clone();
    if cfg.enabled {
        let modern = client_supports_modern_codecs(ua);
        for t in tracks.iter_mut() {
            let served = choose_format(t.format, modern, cfg.preset);
            t.format = match served {
                ServedFormat::Passthrough(f) => f,
                ServedFormat::Mp3 { .. } => media_source::AudioFormat::Mp3,
                ServedFormat::Alac => media_source::AudioFormat::Alac,
                ServedFormat::ClassicAiff => media_source::AudioFormat::Aiff,
            };
            if let (ServedFormat::Mp3 { bitrate_kbps }, Some(dur)) = (served, t.duration_ms) {
                t.size_bytes = Some(state.transcoder.estimate_mp3_size(dur, bitrate_kbps));
                t.bitrate_kbps = Some(bitrate_kbps);
            }
        }
    }
    let total = tracks.len();
    let range = parse_index_range(idx.index.as_deref());
    let sliced = apply_index_range(&tracks, range);
    dmap_response_cs(responses::items(sliced, total, cs), cs)
}

async fn handle_containers<S: MediaSource + 'static>(
    State(state): State<Arc<HandlerState<S>>>,
    Path(db): Path<DatabaseId>,
    Query(idx): Query<IndexQuery>,
    headers: HeaderMap,
) -> Response {
    let cs = charset_from_accept(headers.get(header::ACCEPT_CHARSET).and_then(|v| v.to_str().ok()));
    let tracks = state.source.tracks(db).await.unwrap_or_default();
    let mut extras = state.source.playlists(db).await.unwrap_or_default();
    // Stable case-insensitive alpha order. The paginated 68k source pane
    // fetches pages independently, so joining page N with page N+1 must
    // yield the same order as fetching the full range. Source-defined
    // order (e.g. Subsonic catalogue order) is stable across calls but
    // isn't alphabetical, and isn't a documented contract - pin it here.
    extras.sort_by(|a, b| {
        a.name
            .to_ascii_lowercase()
            .cmp(&b.name.to_ascii_lowercase())
    });

    let total = 1 + extras.len();
    let range = parse_index_range(idx.index.as_deref());
    let (include_library, extras_slice) = slice_playlists(range, &extras);
    dmap_response_cs(
        responses::playlists(
            state.library_playlist_id,
            tracks.len() as u32,
            extras_slice,
            include_library,
            total,
            cs,
        ),
        cs,
    )
}

/// Map an absolute-index `?index=` range onto the (Library at abs 0,
/// extras at abs 1..N) layout. Returns `(include_library, extras_slice)`.
///
/// - `None` → whole listing (Library + all extras)
/// - Range starting past the last playlist → empty page
/// - Otherwise: Library included iff start == 0; extras trimmed so the
///   returned entries cover absolute indices [start, end_incl].
fn slice_playlists<'a>(
    range: Option<(usize, Option<usize>)>,
    extras: &'a [media_source::Playlist],
) -> (bool, &'a [media_source::Playlist]) {
    let total = 1 + extras.len();
    match range {
        None => (true, extras),
        Some((start, _)) if start >= total => (false, &[]),
        Some((start, end)) => {
            let last_idx = total - 1;
            let end_incl = end.map(|e| e.min(last_idx)).unwrap_or(last_idx);
            let include_library = start == 0;
            let extras_start = start.saturating_sub(1);
            // extras[i] lives at abs i+1, so end_incl converts directly to
            // the exclusive slice bound (extras[end_incl-1] is the last).
            let extras_end = end_incl.min(extras.len());
            (include_library, &extras[extras_start..extras_end])
        }
    }
}

async fn handle_container_items<S: MediaSource + 'static>(
    State(state): State<Arc<HandlerState<S>>>,
    Path((db, cid)): Path<(DatabaseId, u32)>,
    Query(idx): Query<IndexQuery>,
    headers: HeaderMap,
) -> Response {
    let cs = charset_from_accept(headers.get(header::ACCEPT_CHARSET).and_then(|v| v.to_str().ok()));
    let full = wants_full_metadata(idx.meta.as_deref());
    // Full-metadata path needs the track table for lookup; ids-only path
    // only needs it for the synthetic Library case, so we conditionally
    // fetch it to keep the hot iTunes path cheap.
    let tracks = if full || cid == state.library_playlist_id {
        state.source.tracks(db).await.unwrap_or_default()
    } else {
        Vec::new()
    };

    let ids: Vec<u32> = if cid == state.library_playlist_id {
        tracks.iter().map(|t| t.id).collect()
    } else {
        state
            .source
            .playlists(db)
            .await
            .unwrap_or_default()
            .into_iter()
            .find(|p| p.id == cid)
            .map(|p| p.track_ids)
            .unwrap_or_default()
    };

    let total = ids.len();
    let range = parse_index_range(idx.index.as_deref());
    let sliced = apply_index_range(&ids, range);
    let offset = range.map(|(s, _)| s).unwrap_or(0);

    if full {
        let by_id: std::collections::HashMap<u32, &media_source::Track> =
            tracks.iter().map(|t| (t.id, t)).collect();
        let resolved: Vec<&media_source::Track> = sliced
            .iter()
            .filter_map(|id| {
                let hit = by_id.get(id).copied();
                if hit.is_none() {
                    tracing::debug!(track_id = id, "playlist entry not in track list; skipping");
                }
                hit
            })
            .collect();
        dmap_response_cs(
            responses::playlist_songs_full(&resolved, total, offset, cs),
            cs,
        )
    } else {
        dmap_response(responses::playlist_songs(sliced, total, offset))
    }
}

#[derive(Deserialize, Default)]
struct ArtworkQuery {
    mw: Option<u32>,
    mh: Option<u32>,
}

#[derive(Deserialize, Default)]
struct IndexQuery {
    /// DAAP-style pagination range on listing endpoints: `A-B` (inclusive,
    /// 0-based) or `A-` (from A to end). Missing or malformed → full listing.
    index: Option<String>,
    /// DAAP `meta=` list. iTunes 4 sends a narrow explicit list here
    /// (`dmap.itemid,dmap.containeritemid`); resource-constrained clients
    /// opt into full per-track metadata by sending `meta=all`.
    meta: Option<String>,
}

/// True when the client's `meta=` value contains `all` (case-insensitive,
/// comma-separated). Everything else — including the absent case and
/// iTunes' narrow list — gets the classic ids-only response.
fn wants_full_metadata(meta: Option<&str>) -> bool {
    match meta {
        None => false,
        Some(s) => s
            .split(',')
            .any(|tok| tok.trim().eq_ignore_ascii_case("all")),
    }
}

/// Parse a DAAP `index=` value. Returns `(start, Some(end))` for `A-B`,
/// `(start, None)` for `A-`. Returns `None` on any parse trouble; callers
/// treat that as "no range applied" (permissive).
fn parse_index_range(raw: Option<&str>) -> Option<(usize, Option<usize>)> {
    let s = raw?.trim();
    let (a, b) = s.split_once('-')?;
    let start: usize = a.trim().parse().ok()?;
    let end = b.trim();
    let end = if end.is_empty() {
        None
    } else {
        let e: usize = end.parse().ok()?;
        if e < start {
            tracing::warn!(raw = s, "index=A-B with B < A; ignoring");
            return None;
        }
        Some(e)
    };
    Some((start, end))
}

/// Apply a parsed `index=` range to `all`, clamping to bounds. `end` = None
/// means "to the last element."
fn apply_index_range<'a, T>(
    all: &'a [T],
    range: Option<(usize, Option<usize>)>,
) -> &'a [T] {
    match range {
        None => all,
        Some((start, _)) if start >= all.len() => &[],
        Some((start, end)) => {
            let last_idx = all.len().saturating_sub(1);
            let end_incl = end.map(|e| e.min(last_idx)).unwrap_or(last_idx);
            &all[start..=end_incl]
        }
    }
}

async fn handle_artwork<S: MediaSource + 'static>(
    State(state): State<Arc<HandlerState<S>>>,
    Path((db, track_id)): Path<(DatabaseId, TrackId)>,
    Query(q): Query<ArtworkQuery>,
    headers: HeaderMap,
) -> Response {
    let variant = variant_from_accept(headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()));
    match state.source.artwork(db, track_id).await {
        Ok(Some(raw)) => {
            let prepared = state.artworker.prepare(track_id, raw, q.mw, q.mh, variant);
            let (bytes, content_type): (bytes::Bytes, &'static str) = match prepared {
                Prepared::Encoded { bytes, content_type } => (bytes, content_type),
                Prepared::Original { bytes, content_type } => (bytes, content_type),
            };
            let mut resp = Response::new(Body::from(bytes));
            let h = resp.headers_mut();
            h.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
            *resp.status_mut() = StatusCode::OK;
            resp
        }
        Ok(None) => (StatusCode::NOT_FOUND, "no artwork").into_response_stub(),
        Err(err) => {
            tracing::debug!(track_id, ?err, "source.artwork failed");
            (StatusCode::NOT_FOUND, "artwork lookup failed").into_response_stub()
        }
    }
}

/// Pick an OutputVariant based on the client's Accept header. Recognises:
///   image/x-pict; depth=1  → Pict1
///   image/x-pict; depth=8  → Pict8
///   image/x-pict           → Pict8 (default depth)
/// Falls back to Jpeg for anything else (or missing header).
fn variant_from_accept(accept: Option<&str>) -> OutputVariant {
    let a = match accept {
        Some(s) => s.to_ascii_lowercase(),
        None => return OutputVariant::Jpeg,
    };
    if a.contains("image/x-pict") {
        // Find the MIME params after image/x-pict — poor-man's parse: look
        // for `depth=` anywhere in the string (Accept can list multiple
        // types so this can pick up an unrelated `depth=` in an obscure
        // MIME extension, but no other type we care about uses that param).
        if a.contains("depth=1") {
            return OutputVariant::Pict1;
        }
        return OutputVariant::Pict8;
    }
    OutputVariant::Jpeg
}

async fn handle_stream<S: MediaSource + 'static>(
    State(state): State<Arc<HandlerState<S>>>,
    Path((db, track_file)): Path<(DatabaseId, String)>,
    headers: HeaderMap,
) -> Response {
    // Track path segment looks like "42.mp3" — split off the id.
    let track_id: TrackId = match track_file.split('.').next().and_then(|s| s.parse().ok()) {
        Some(id) => id,
        None => return (StatusCode::BAD_REQUEST, "invalid track id").into_response_stub(),
    };

    let track = match state.source.tracks(db).await.ok().and_then(|ts| {
        ts.into_iter().find(|t| t.id == track_id)
    }) {
        Some(t) => t,
        None => return (StatusCode::NOT_FOUND, "not found").into_response_stub(),
    };

    let ua = headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok());
    let accept = headers.get(header::ACCEPT).and_then(|v| v.to_str().ok());
    let cfg = state.transcoder.config().clone();

    // Accept header takes precedence over UA-derived format selection:
    // this is how classic (Mac II era) clients request AIFF PCM directly.
    let served = if let Some(f) = format_from_accept(accept) {
        f
    } else if cfg.enabled {
        choose_format(
            track.format,
            client_supports_modern_codecs(ua),
            cfg.preset,
        )
    } else {
        ServedFormat::Passthrough(track.format)
    };

    let range = parse_range_header(headers.get(header::RANGE).and_then(|v| v.to_str().ok()));

    if served.is_transcode() {
        serve_transcoded(state, db, track_id, &track, served, range).await
    } else {
        serve_passthrough(state, db, track_id, range).await
    }
}

/// Very small Accept-header parser: looks for known audio types and returns
/// the matching ServedFormat. Ignores quality weights and MIME params for
/// MVP — we only distinguish presence of `audio/x-aiff` right now.
fn format_from_accept(accept: Option<&str>) -> Option<ServedFormat> {
    let a = accept?.to_ascii_lowercase();
    if a.contains("audio/x-aiff") || a.contains("audio/aiff") {
        return Some(ServedFormat::ClassicAiff);
    }
    None
}

async fn serve_passthrough<S: MediaSource + 'static>(
    state: Arc<HandlerState<S>>,
    db: DatabaseId,
    track_id: TrackId,
    range: Option<(u64, Option<u64>)>,
) -> Response {
    let handle_result = match range {
        Some((start, end)) => state.source.open_stream_range(db, track_id, start, end).await,
        None => state.source.open_stream(db, track_id).await,
    };
    let handle = match handle_result {
        Ok(h) => h,
        Err(_) => return (StatusCode::NOT_FOUND, "source open failed").into_response_stub(),
    };

    let mut resp = Response::new(Body::from_stream(ReaderStream::new(handle.body)));
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(handle.content_type));
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

    match handle.range {
        Some((start, end)) => {
            let total = handle
                .total_bytes
                .map(|t| t.to_string())
                .unwrap_or_else(|| "*".to_string());
            let content_range = format!("bytes {}-{}/{}", start, end, total);
            if let Ok(v) = HeaderValue::from_str(&content_range) {
                h.insert(header::CONTENT_RANGE, v);
            }
            let len = end - start + 1;
            if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
                h.insert(header::CONTENT_LENGTH, v);
            }
            *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
        }
        None => {
            if let Some(len) = handle.total_bytes {
                if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
                    h.insert(header::CONTENT_LENGTH, v);
                }
            }
            *resp.status_mut() = StatusCode::OK;
        }
    }
    resp
}

async fn serve_transcoded<S: MediaSource + 'static>(
    state: Arc<HandlerState<S>>,
    db: DatabaseId,
    track_id: TrackId,
    track: &media_source::Track,
    served: ServedFormat,
    range: Option<(u64, Option<u64>)>,
) -> Response {
    // Compute per-request info first: seek time, estimated total transcoded
    // size, and the Content-Range/Content-Length for a partial response.
    let est_total = match served {
        ServedFormat::Mp3 { bitrate_kbps } => track
            .duration_ms
            .map(|d| state.transcoder.estimate_mp3_size(d, bitrate_kbps)),
        ServedFormat::Alac => track.size_bytes,
        ServedFormat::ClassicAiff => track
            .duration_ms
            .map(|d| transcode::classic_aiff_size(d) as u64),
        _ => None,
    };
    let (start, end_incl, seek_time_ms) = match (range, est_total) {
        (Some((s, e)), Some(total)) if s < total => {
            let end = e.unwrap_or(total - 1).min(total - 1);
            let seek_ms = match served {
                ServedFormat::Mp3 { bitrate_kbps } => {
                    Some(transcode::bytes_to_time_ms(s, bitrate_kbps))
                }
                ServedFormat::Alac => track.duration_ms.map(|d| {
                    // ALAC is VBR — approximate by proportional-time-of-total.
                    let ratio = s as f64 / total as f64;
                    (ratio * d as f64).round().min(u32::MAX as f64) as u32
                }),
                ServedFormat::ClassicAiff => Some(transcode::classic_aiff_byte_to_time_ms(s)),
                _ => None,
            };
            (s, Some(end), seek_ms)
        }
        _ => (0, est_total.map(|t| t.saturating_sub(1)), None),
    };

    let handle = match state.source.open_stream(db, track_id).await {
        Ok(h) => h,
        Err(_) => return (StatusCode::NOT_FOUND, "source open failed").into_response_stub(),
    };

    let transcode_handle = match state
        .transcoder
        .spawn(served, track, handle.body, seek_time_ms)
        .await
    {
        Ok(h) => h,
        Err(err) => {
            tracing::warn!(?err, "failed to spawn ffmpeg");
            return (StatusCode::INTERNAL_SERVER_ERROR, "transcode failed").into_response_stub();
        }
    };

    // For ClassicAiff, prepend our own AIFF header — ffmpeg emits raw PCM
    // (no seekable output means it can't back-patch chunk sizes itself).
    // Header goes only on the first response fragment; range-partial
    // responses skip it since we're already in the data region.
    let mut resp = if matches!(served, ServedFormat::ClassicAiff) && !seek_time_ms.is_some() {
        let sample_count = track
            .duration_ms
            .map(|d| (d as u64 * transcode::CLASSIC_AIFF_SAMPLE_RATE as u64 / 1000) as u32)
            .unwrap_or(0);
        let header = transcode::classic_aiff_header(sample_count).to_vec();
        let prefixed = PrefixReader::new(header, transcode_handle);
        Response::new(Body::from_stream(ReaderStream::new(prefixed)))
    } else {
        Response::new(Body::from_stream(ReaderStream::new(transcode_handle)))
    };
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(served.content_type()),
    );
    // Advertise Range support even for transcoded — clients can request
    // seeks and we'll spin up a fresh ffmpeg with -ss.
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

    let is_partial = seek_time_ms.is_some();
    if is_partial {
        if let (Some(end), Some(total)) = (end_incl, est_total) {
            let content_range = format!("bytes {}-{}/{}", start, end, total);
            if let Ok(v) = HeaderValue::from_str(&content_range) {
                h.insert(header::CONTENT_RANGE, v);
            }
            let len = end - start + 1;
            if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
                h.insert(header::CONTENT_LENGTH, v);
            }
        }
        *resp.status_mut() = StatusCode::PARTIAL_CONTENT;
    } else {
        if let Some(total) = est_total {
            if let Ok(v) = HeaderValue::from_str(&total.to_string()) {
                h.insert(header::CONTENT_LENGTH, v);
            }
        }
        *resp.status_mut() = StatusCode::OK;
    }
    resp
}

// ---- helpers ----

#[derive(Deserialize)]
struct SessionQuery {
    #[serde(rename = "session-id")]
    session_id: Option<u32>,
}

/// Parse a simple `bytes=start-end` Range header.
fn parse_range_header(v: Option<&str>) -> Option<(u64, Option<u64>)> {
    let v = v?.strip_prefix("bytes=")?;
    let (s, e) = v.split_once('-')?;
    let start: u64 = s.parse().ok()?;
    let end: Option<u64> = if e.is_empty() { None } else { e.parse().ok() };
    Some((start, end))
}

/// Standard DAAP response: application/x-dmap-tagged with Accept-Ranges.
fn dmap_response(body: BytesMut) -> Response {
    dmap_response_cs(body, Charset::Utf8)
}

/// Same as [`dmap_response`] but echoes the negotiated charset in the
/// Content-Type header so the client knows how to decode string tags.
fn dmap_response_cs(body: BytesMut, cs: Charset) -> Response {
    let mut resp = Response::new(Body::from(body.freeze()));
    let h = resp.headers_mut();
    let ct = match cs.ct_param() {
        Some(param) => {
            let s = format!("application/x-dmap-tagged; charset={}", param);
            HeaderValue::from_str(&s).unwrap()
        }
        None => HeaderValue::from_static("application/x-dmap-tagged"),
    };
    h.insert(header::CONTENT_TYPE, ct);
    h.insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    h.insert(
        "DAAP-Server",
        HeaderValue::from_static(concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"))),
    );
    *resp.status_mut() = StatusCode::OK;
    resp
}

trait IntoStubResponse {
    fn into_response_stub(self) -> Response;
}
impl<T: AsRef<str>> IntoStubResponse for (StatusCode, T) {
    fn into_response_stub(self) -> Response {
        let mut resp = Response::new(Body::from(self.1.as_ref().to_string()));
        *resp.status_mut() = self.0;
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use axum::body::to_bytes;
    use axum::http::Request;
    use bytes::Bytes;
    use media_source::{
        AudioFormat, Database, DatabaseId, MediaSource, Playlist, Result as MSResult, StreamHandle,
        Track, TrackId,
    };
    use tower::ServiceExt;

    struct MemSource;

    #[async_trait]
    impl MediaSource for MemSource {
        async fn databases(&self) -> MSResult<Vec<Database>> {
            Ok(vec![Database { id: 1, name: "Test".into() }])
        }
        async fn tracks(&self, _db: DatabaseId) -> MSResult<Vec<Track>> {
            Ok(vec![Track {
                id: 100,
                title: "One".into(),
                artist: Some("Artist".into()),
                album: Some("Album".into()),
                album_artist: None,
                genre: None,
                track_number: Some(1),
                disc_number: None,
                year: None,
                duration_ms: Some(1000),
                bitrate_kbps: Some(128),
                sample_rate: Some(44100),
                size_bytes: Some(1234),
                format: AudioFormat::Mp3,
            }])
        }
        async fn playlists(&self, _db: DatabaseId) -> MSResult<Vec<Playlist>> {
            Ok(vec![])
        }
        async fn open_stream(&self, _db: DatabaseId, _tid: TrackId) -> MSResult<StreamHandle> {
            unimplemented!()
        }
        async fn artwork(&self, _: DatabaseId, _: TrackId) -> MSResult<Option<Bytes>> {
            Ok(None)
        }
    }

    fn app() -> Router {
        router(Arc::new(HandlerState::new(
            "Under Test".to_string(),
            Arc::new(MemSource),
        )))
    }

    async fn body_of(response: Response) -> Vec<u8> {
        to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()
    }

    #[tokio::test]
    async fn server_info_returns_dmap_response() {
        let r = app()
            .oneshot(Request::builder().uri("/server-info").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let body = body_of(r).await;
        assert_eq!(&body[0..4], b"msrv");
    }

    #[tokio::test]
    async fn login_issues_incrementing_session() {
        let app = app();
        let r = app
            .clone()
            .oneshot(Request::builder().uri("/login").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let body = body_of(r).await;
        assert_eq!(&body[0..4], b"mlog");
    }

    #[tokio::test]
    async fn update_returns_current_revision() {
        let r = app()
            .oneshot(Request::builder().uri("/update?revision-number=1").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let body = body_of(r).await;
        assert_eq!(&body[0..4], b"mupd");
    }

    #[tokio::test]
    async fn databases_lists_our_db() {
        let r = app()
            .oneshot(Request::builder().uri("/databases").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_of(r).await;
        assert_eq!(&body[0..4], b"avdb");
        assert!(body.windows(4).any(|w| w == b"Test"));
    }

    #[tokio::test]
    async fn items_includes_track_metadata() {
        let r = app()
            .oneshot(Request::builder().uri("/databases/1/items").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_of(r).await;
        assert_eq!(&body[0..4], b"adbs");
        assert!(body.windows(6).any(|w| w == b"Artist"));
    }

    #[tokio::test]
    async fn containers_includes_library_playlist() {
        let r = app()
            .oneshot(Request::builder().uri("/databases/1/containers").body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = body_of(r).await;
        assert_eq!(&body[0..4], b"aply");
        assert!(body.windows(7).any(|w| w == b"Library"));
    }

    #[tokio::test]
    async fn library_container_items_returns_all_tracks() {
        let r = app()
            .oneshot(
                Request::builder()
                    .uri("/databases/1/containers/1/items")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_of(r).await;
        assert_eq!(&body[0..4], b"apso");
    }

    #[test]
    fn range_header_parses() {
        assert_eq!(parse_range_header(Some("bytes=0-99")), Some((0, Some(99))));
        assert_eq!(parse_range_header(Some("bytes=1024-")), Some((1024, None)));
        assert_eq!(parse_range_header(Some("bytes=")), None);
        assert_eq!(parse_range_header(None), None);
    }

    // ---- ?index= parsing + slicing helpers ----

    #[test]
    fn parse_index_range_valid() {
        assert_eq!(parse_index_range(Some("0-99")), Some((0, Some(99))));
        assert_eq!(parse_index_range(Some("100-100")), Some((100, Some(100))));
        assert_eq!(parse_index_range(Some("0 - 99")), Some((0, Some(99))));
    }

    #[test]
    fn parse_index_range_open_end() {
        assert_eq!(parse_index_range(Some("42-")), Some((42, None)));
        assert_eq!(parse_index_range(Some("0-")), Some((0, None)));
    }

    #[test]
    fn parse_index_range_none_and_malformed() {
        assert_eq!(parse_index_range(None), None);
        assert_eq!(parse_index_range(Some("")), None);
        assert_eq!(parse_index_range(Some("garbage")), None);
        assert_eq!(parse_index_range(Some("-99")), None); // bare from-end not supported
        assert_eq!(parse_index_range(Some("50-10")), None); // B < A → ignored
    }

    #[test]
    fn apply_index_range_slices_and_clamps() {
        let all: Vec<u32> = (0..100).collect();
        assert_eq!(apply_index_range(&all, None), &all[..]);
        assert_eq!(apply_index_range(&all, Some((10, Some(19)))), &all[10..=19]);
        assert_eq!(apply_index_range(&all, Some((90, Some(999)))), &all[90..=99]); // clamp end
        assert_eq!(apply_index_range(&all, Some((42, None))), &all[42..=99]);      // open end
        assert!(apply_index_range(&all, Some((200, Some(300)))).is_empty());       // out of range
    }

    // ---- /databases/1/items ?index= integration ----

    struct BigMemSource {
        n: usize,
        /// Number of extra (non-Library) playlists to expose.
        extra_playlists: usize,
    }

    #[async_trait]
    impl MediaSource for BigMemSource {
        async fn databases(&self) -> MSResult<Vec<Database>> {
            Ok(vec![Database { id: 1, name: "Big".into() }])
        }
        async fn tracks(&self, _db: DatabaseId) -> MSResult<Vec<Track>> {
            Ok((0..self.n)
                .map(|i| Track {
                    id: (i + 1) as TrackId,
                    title: format!("t{i}"),
                    artist: None, album: None, album_artist: None,
                    genre: None, track_number: None, disc_number: None,
                    year: None, duration_ms: None, bitrate_kbps: None,
                    sample_rate: None, size_bytes: None,
                    format: AudioFormat::Mp3,
                })
                .collect())
        }
        async fn playlists(&self, _db: DatabaseId) -> MSResult<Vec<Playlist>> {
            // First extra playlist has 30 tracks so /containers/2/items tests
            // have something to page over; the remainder are named playlists
            // for /containers ?index= tests.
            let mut out = Vec::with_capacity(self.extra_playlists);
            if self.extra_playlists >= 1 {
                out.push(Playlist {
                    id: 2,
                    name: "P".into(),
                    track_ids: (1..=30).collect(),
                });
            }
            for i in 1..self.extra_playlists {
                out.push(Playlist {
                    id: (2 + i) as u32,
                    name: format!("pl{i}"),
                    track_ids: vec![],
                });
            }
            Ok(out)
        }
        async fn open_stream(&self, _: DatabaseId, _: TrackId) -> MSResult<StreamHandle> {
            unimplemented!()
        }
        async fn artwork(&self, _: DatabaseId, _: TrackId) -> MSResult<Option<Bytes>> {
            Ok(None)
        }
    }

    fn app_with_tracks(n: usize) -> Router {
        app_with_tracks_and_playlists(n, 1)
    }

    fn app_with_tracks_and_playlists(n: usize, extra_playlists: usize) -> Router {
        // Disable transcoding to keep track.format unchanged (test expectations
        // don't care about format munging).
        let mut cfg = crate::transcode::Config::default();
        cfg.enabled = false;
        router(Arc::new(HandlerState::new_with_transcode(
            "Big".into(),
            Arc::new(BigMemSource { n, extra_playlists }),
            cfg,
        )))
    }

    /// Locate a top-level DMAP field's value bytes by tag inside a listing
    /// response. Naive search — fine for tests where the tag is unique.
    fn find_field_u32(body: &[u8], tag: &[u8; 4]) -> u32 {
        let mut i = 0;
        while i + 12 <= body.len() {
            if &body[i..i + 4] == tag {
                let len = u32::from_be_bytes(body[i + 4..i + 8].try_into().unwrap());
                if len == 4 {
                    return u32::from_be_bytes(body[i + 8..i + 12].try_into().unwrap());
                }
            }
            i += 1;
        }
        panic!("tag {:?} not found", std::str::from_utf8(tag).unwrap());
    }

    fn count_mlit(body: &[u8]) -> usize {
        body.windows(4).filter(|w| *w == b"mlit").count()
    }

    #[tokio::test]
    async fn items_index_slices_and_reports_full_total() {
        let r = app_with_tracks(1000)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/items?index=100-149")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let body = body_of(r).await;
        assert_eq!(find_field_u32(&body, b"mtco"), 1000);
        assert_eq!(find_field_u32(&body, b"mrco"), 50);
        assert_eq!(count_mlit(&body), 50);
    }

    #[tokio::test]
    async fn items_index_out_of_range_returns_empty_listing() {
        let r = app_with_tracks(50)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/items?index=100-200")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        let body = body_of(r).await;
        assert_eq!(find_field_u32(&body, b"mtco"), 50);
        assert_eq!(find_field_u32(&body, b"mrco"), 0);
        assert_eq!(count_mlit(&body), 0);
    }

    #[tokio::test]
    async fn items_index_end_clamps_to_total() {
        let r = app_with_tracks(50)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/items?index=40-999")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        let body = body_of(r).await;
        assert_eq!(find_field_u32(&body, b"mrco"), 10); // 40..=49
    }

    #[tokio::test]
    async fn items_open_end_range_reads_to_last() {
        let r = app_with_tracks(50)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/items?index=45-")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        let body = body_of(r).await;
        assert_eq!(find_field_u32(&body, b"mtco"), 50);
        assert_eq!(find_field_u32(&body, b"mrco"), 5); // 45..=49
    }

    #[tokio::test]
    async fn items_no_index_returns_all_tracks() {
        let r = app_with_tracks(50)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/items")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        let body = body_of(r).await;
        assert_eq!(find_field_u32(&body, b"mtco"), 50);
        assert_eq!(find_field_u32(&body, b"mrco"), 50);
    }

    #[tokio::test]
    async fn items_malformed_index_is_permissive() {
        let r = app_with_tracks(50)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/items?index=garbage")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let body = body_of(r).await;
        assert_eq!(find_field_u32(&body, b"mtco"), 50);
        assert_eq!(find_field_u32(&body, b"mrco"), 50);
    }

    // ---- /databases/1/containers/2/items ?index= integration ----

    #[tokio::test]
    async fn container_items_index_slices_playlist() {
        let r = app_with_tracks(1000)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/containers/2/items?index=5-14")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        let body = body_of(r).await;
        assert_eq!(&body[0..4], b"apso");
        assert_eq!(find_field_u32(&body, b"mtco"), 30);
        assert_eq!(find_field_u32(&body, b"mrco"), 10);
        assert_eq!(count_mlit(&body), 10);
    }

    #[tokio::test]
    async fn container_items_index_offsets_mpco_across_pages() {
        // First page: items 0..=4 → mpco values 1..=5
        // Second page: items 5..=9 → mpco values 6..=10 (offset carries through)
        let r = app_with_tracks(1000)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/containers/2/items?index=5-9")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        let body = body_of(r).await;
        // Extract all mpco values in order
        let mut mpcos = Vec::new();
        let mut i = 0;
        while i + 12 <= body.len() {
            if &body[i..i + 4] == b"mpco" {
                mpcos.push(u32::from_be_bytes(body[i + 8..i + 12].try_into().unwrap()));
            }
            i += 1;
        }
        assert_eq!(mpcos, vec![6, 7, 8, 9, 10]);
    }

    #[tokio::test]
    async fn container_items_out_of_range_returns_empty() {
        let r = app_with_tracks(1000)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/containers/2/items?index=100-200")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        let body = body_of(r).await;
        assert_eq!(find_field_u32(&body, b"mtco"), 30);
        assert_eq!(find_field_u32(&body, b"mrco"), 0);
    }

    #[tokio::test]
    async fn container_items_no_index_returns_all() {
        let r = app_with_tracks(1000)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/containers/2/items")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        let body = body_of(r).await;
        assert_eq!(find_field_u32(&body, b"mtco"), 30);
        assert_eq!(find_field_u32(&body, b"mrco"), 30);
    }

    #[tokio::test]
    async fn container_items_endpoint_returns_full_metadata_when_meta_all() {
        // Resource-constrained clients opt in with meta=all and get real
        // per-track metadata so they can render the playlist without a
        // second fetch.
        let r = app_with_tracks(1000)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/containers/2/items?index=0-2&meta=all")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        let body = body_of(r).await;
        assert_eq!(&body[0..4], b"apso");
        // Metadata tags that only appear with full-encoding, not the
        // old ids-only shape.
        assert!(body.windows(4).any(|w| w == b"minm"), "expected item_name field");
        assert!(body.windows(4).any(|w| w == b"asfm"), "expected song_format field");
        assert!(body.windows(2).any(|w| w == b"t0"));
        assert!(body.windows(4).any(|w| w == b"mpco"));
    }

    #[tokio::test]
    async fn container_items_default_response_is_ids_only_for_itunes_compat() {
        // No meta=all → iTunes-shaped compact response: mlit contains only
        // mikd/miid/mpco. No minm/asfm/etc. This keeps iTunes 4 happy.
        let r = app_with_tracks(1000)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/containers/2/items?index=0-2")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        let body = body_of(r).await;
        assert_eq!(&body[0..4], b"apso");
        assert!(body.windows(4).any(|w| w == b"miid"));
        assert!(body.windows(4).any(|w| w == b"mpco"));
        assert!(!body.windows(4).any(|w| w == b"minm"), "ids-only shape must not carry minm");
        assert!(!body.windows(4).any(|w| w == b"asfm"), "ids-only shape must not carry asfm");
        assert!(!body.windows(4).any(|w| w == b"asar"), "ids-only shape must not carry asar");
    }

    #[tokio::test]
    async fn container_items_itunes_narrow_meta_still_ids_only() {
        // iTunes 4 explicitly requests just dmap.itemid + dmap.containeritemid.
        // We must NOT interpret that as "give me everything."
        let r = app_with_tracks(1000)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/containers/2/items?meta=dmap.itemid,dmap.containeritemid&index=0-2")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        let body = body_of(r).await;
        assert!(!body.windows(4).any(|w| w == b"minm"));
        assert!(!body.windows(4).any(|w| w == b"asfm"));
    }

    #[test]
    fn wants_full_metadata_gate() {
        assert!(!wants_full_metadata(None));
        assert!(!wants_full_metadata(Some("")));
        assert!(!wants_full_metadata(Some("dmap.itemid,dmap.containeritemid")));
        assert!(wants_full_metadata(Some("all")));
        assert!(wants_full_metadata(Some("ALL")));
        assert!(wants_full_metadata(Some("dmap.itemid,all")));
        assert!(wants_full_metadata(Some(" all ")));
        assert!(!wants_full_metadata(Some("allow"))); // must be exact token
    }

    /// Media source whose playlist references some track ids that don't
    /// exist in `tracks()`. Used to verify the handler drops stale entries
    /// rather than emitting half-populated mlits.
    struct StaleIdSource;

    #[async_trait]
    impl MediaSource for StaleIdSource {
        async fn databases(&self) -> MSResult<Vec<Database>> {
            Ok(vec![Database { id: 1, name: "Stale".into() }])
        }
        async fn tracks(&self, _db: DatabaseId) -> MSResult<Vec<Track>> {
            // Real tracks: ids 1, 2, 3 only.
            Ok((1..=3)
                .map(|i| Track {
                    id: i, title: format!("t{i}"),
                    artist: None, album: None, album_artist: None,
                    genre: None, track_number: None, disc_number: None,
                    year: None, duration_ms: None, bitrate_kbps: None,
                    sample_rate: None, size_bytes: None,
                    format: AudioFormat::Mp3,
                })
                .collect())
        }
        async fn playlists(&self, _db: DatabaseId) -> MSResult<Vec<Playlist>> {
            // Playlist references ids 1..=5 but only 1,2,3 exist.
            Ok(vec![Playlist {
                id: 2, name: "Mixed".into(),
                track_ids: vec![1, 2, 3, 4, 5],
            }])
        }
        async fn open_stream(&self, _: DatabaseId, _: TrackId) -> MSResult<StreamHandle> {
            unimplemented!()
        }
        async fn artwork(&self, _: DatabaseId, _: TrackId) -> MSResult<Option<Bytes>> {
            Ok(None)
        }
    }

    #[tokio::test]
    async fn container_items_skips_missing_ids() {
        let mut cfg = crate::transcode::Config::default();
        cfg.enabled = false;
        let app = router(Arc::new(HandlerState::new_with_transcode(
            "Stale".into(),
            Arc::new(StaleIdSource),
            cfg,
        )));
        let r = app
            .oneshot(
                Request::builder()
                    .uri("/databases/1/containers/2/items?meta=all")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        let body = body_of(r).await;
        // Full-metadata path resolves ids against the track table and drops
        // stale entries. mtco reflects the ORIGINAL playlist size (5),
        // mrco = resolvable (3).
        assert_eq!(find_field_u32(&body, b"mtco"), 5);
        assert_eq!(find_field_u32(&body, b"mrco"), 3);
        assert_eq!(count_mlit(&body), 3);
    }

    // ---- /databases/1/containers ?index= integration ----

    #[test]
    fn slice_playlists_covers_all_shapes() {
        let extras: Vec<Playlist> = (0..20)
            .map(|i| Playlist { id: (i + 2) as u32, name: format!("p{i}"), track_ids: vec![] })
            .collect();

        // No range → include Library, full extras.
        let (lib, sl) = slice_playlists(None, &extras);
        assert!(lib);
        assert_eq!(sl.len(), 20);

        // start=0, end=0 → Library only.
        let (lib, sl) = slice_playlists(Some((0, Some(0))), &extras);
        assert!(lib);
        assert_eq!(sl.len(), 0);

        // start=0, end=4 → Library + extras[0..=3] (abs 0..=4).
        let (lib, sl) = slice_playlists(Some((0, Some(4))), &extras);
        assert!(lib);
        assert_eq!(sl.len(), 4);
        assert_eq!(sl[0].name, "p0");
        assert_eq!(sl[3].name, "p3");

        // start=5, end=9 → no Library, extras[4..=8] (abs 5..=9).
        let (lib, sl) = slice_playlists(Some((5, Some(9))), &extras);
        assert!(!lib);
        assert_eq!(sl.len(), 5);
        assert_eq!(sl[0].name, "p4");
        assert_eq!(sl[4].name, "p8");

        // Open-end: start=15 → extras[14..=19] (abs 15..=20).
        let (lib, sl) = slice_playlists(Some((15, None)), &extras);
        assert!(!lib);
        assert_eq!(sl.len(), 6);

        // End clamps to total.
        let (lib, sl) = slice_playlists(Some((0, Some(999))), &extras);
        assert!(lib);
        assert_eq!(sl.len(), 20);

        // Start past end → empty page.
        let (lib, sl) = slice_playlists(Some((100, Some(200))), &extras);
        assert!(!lib);
        assert_eq!(sl.len(), 0);
    }

    struct NamedPlaylistsSource {
        names: Vec<String>,
    }

    #[async_trait]
    impl MediaSource for NamedPlaylistsSource {
        async fn databases(&self) -> MSResult<Vec<Database>> {
            Ok(vec![Database { id: 1, name: "N".into() }])
        }
        async fn tracks(&self, _db: DatabaseId) -> MSResult<Vec<Track>> {
            Ok(vec![])
        }
        async fn playlists(&self, _db: DatabaseId) -> MSResult<Vec<Playlist>> {
            Ok(self
                .names
                .iter()
                .enumerate()
                .map(|(i, n)| Playlist {
                    id: (i + 2) as u32,
                    name: n.clone(),
                    track_ids: vec![],
                })
                .collect())
        }
        async fn open_stream(&self, _: DatabaseId, _: TrackId) -> MSResult<StreamHandle> {
            unimplemented!()
        }
        async fn artwork(&self, _: DatabaseId, _: TrackId) -> MSResult<Option<Bytes>> {
            Ok(None)
        }
    }

    fn app_with_playlist_names(names: Vec<String>) -> Router {
        let mut cfg = crate::transcode::Config::default();
        cfg.enabled = false;
        router(Arc::new(HandlerState::new_with_transcode(
            "N".into(),
            Arc::new(NamedPlaylistsSource { names }),
            cfg,
        )))
    }

    /// Extract every `minm` (item_name) string value from a DAAP body in
    /// encounter order. Each `minm` is: 4-byte tag, 4-byte BE length,
    /// then UTF-8 bytes.
    fn collect_minm_values(body: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        let mut i = 0;
        while i + 8 <= body.len() {
            if &body[i..i + 4] == b"minm" {
                let len = u32::from_be_bytes(body[i + 4..i + 8].try_into().unwrap()) as usize;
                let start = i + 8;
                let end = start + len;
                if end <= body.len() {
                    out.push(String::from_utf8_lossy(&body[start..end]).into_owned());
                }
                i = end;
            } else {
                i += 1;
            }
        }
        out
    }

    #[tokio::test]
    async fn containers_extras_returned_alphabetical() {
        let r = app_with_playlist_names(vec!["Zebra".into(), "apple".into(), "Middle".into()])
            .oneshot(
                Request::builder()
                    .uri("/databases/1/containers")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        let body = body_of(r).await;
        let names = collect_minm_values(&body);
        // Library first (synthetic, abs 0), then alpha extras.
        assert_eq!(names, vec!["Library", "apple", "Middle", "Zebra"]);
    }

    #[tokio::test]
    async fn containers_paged_join_equals_full() {
        // 30 extras in a jumbled non-alphabetical order. Two pages that
        // together cover the full range must join into the same order as
        // one request for the full range.
        let names: Vec<String> = (0..30)
            .map(|i| {
                // Deterministic scramble: reverse decimal digits so
                // adjacent indices don't cluster alphabetically.
                let scrambled: String = format!("{:02}", i).chars().rev().collect();
                format!("pl-{}", scrambled)
            })
            .collect();

        let full = {
            let r = app_with_playlist_names(names.clone())
                .oneshot(
                    Request::builder()
                        .uri("/databases/1/containers?index=0-29")
                        .body(Body::empty()).unwrap()
                ).await.unwrap();
            collect_minm_values(&body_of(r).await)
        };
        let page_a = {
            let r = app_with_playlist_names(names.clone())
                .oneshot(
                    Request::builder()
                        .uri("/databases/1/containers?index=0-14")
                        .body(Body::empty()).unwrap()
                ).await.unwrap();
            collect_minm_values(&body_of(r).await)
        };
        let page_b = {
            let r = app_with_playlist_names(names.clone())
                .oneshot(
                    Request::builder()
                        .uri("/databases/1/containers?index=15-29")
                        .body(Body::empty()).unwrap()
                ).await.unwrap();
            collect_minm_values(&body_of(r).await)
        };
        let joined: Vec<String> = page_a.into_iter().chain(page_b).collect();
        assert_eq!(joined, full);
        assert_eq!(joined.len(), 30);
        assert_eq!(joined[0], "Library");
    }

    #[tokio::test]
    async fn containers_index_range_slices_and_reports_full_total() {
        // 20 extras + Library = 21 total.
        let r = app_with_tracks_and_playlists(5, 20)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/containers?index=5-9")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let body = body_of(r).await;
        assert_eq!(&body[0..4], b"aply");
        assert_eq!(find_field_u32(&body, b"mtco"), 21);
        assert_eq!(find_field_u32(&body, b"mrco"), 5);
        assert_eq!(count_mlit(&body), 5);
        // Library shouldn't appear on this page.
        assert!(!body.windows(7).any(|w| w == b"Library"));
    }

    #[tokio::test]
    async fn containers_index_zero_includes_library_base_playlist() {
        let r = app_with_tracks_and_playlists(5, 20)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/containers?index=0-0")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        let body = body_of(r).await;
        assert_eq!(find_field_u32(&body, b"mtco"), 21);
        assert_eq!(find_field_u32(&body, b"mrco"), 1);
        assert_eq!(count_mlit(&body), 1);
        assert!(body.windows(7).any(|w| w == b"Library"));
        // abpl (base_playlist) marker should be present.
        assert!(body.windows(4).any(|w| w == b"abpl"));
    }

    #[tokio::test]
    async fn containers_index_open_end() {
        let r = app_with_tracks_and_playlists(5, 20)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/containers?index=15-")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        let body = body_of(r).await;
        assert_eq!(find_field_u32(&body, b"mtco"), 21);
        // abs 15..=20 = 6 entries.
        assert_eq!(find_field_u32(&body, b"mrco"), 6);
    }

    #[tokio::test]
    async fn containers_index_out_of_range() {
        let r = app_with_tracks_and_playlists(5, 20)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/containers?index=100-200")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        let body = body_of(r).await;
        assert_eq!(find_field_u32(&body, b"mtco"), 21);
        assert_eq!(find_field_u32(&body, b"mrco"), 0);
    }

    #[tokio::test]
    async fn containers_no_index_returns_all_backwards_compat() {
        let r = app_with_tracks_and_playlists(5, 20)
            .oneshot(
                Request::builder()
                    .uri("/databases/1/containers")
                    .body(Body::empty()).unwrap()
            ).await.unwrap();
        let body = body_of(r).await;
        assert_eq!(find_field_u32(&body, b"mtco"), 21);
        assert_eq!(find_field_u32(&body, b"mrco"), 21);
        assert!(body.windows(7).any(|w| w == b"Library"));
    }

    // ---- Range integration for passthrough ----

    struct RangeSource {
        /// pretend-file contents, indexed by track id
        contents: Vec<u8>,
    }

    #[async_trait]
    impl MediaSource for RangeSource {
        async fn databases(&self) -> MSResult<Vec<Database>> {
            Ok(vec![Database { id: 1, name: "R".into() }])
        }
        async fn tracks(&self, _db: DatabaseId) -> MSResult<Vec<Track>> {
            Ok(vec![Track {
                id: 42,
                title: "T".into(),
                artist: None, album: None, album_artist: None,
                genre: None, track_number: None, disc_number: None,
                year: None, duration_ms: Some(10_000),
                bitrate_kbps: Some(192), sample_rate: None,
                size_bytes: Some(self.contents.len() as u64),
                format: AudioFormat::Mp3,
            }])
        }
        async fn playlists(&self, _db: DatabaseId) -> MSResult<Vec<Playlist>> {
            Ok(vec![])
        }
        async fn open_stream(&self, _: DatabaseId, _: TrackId) -> MSResult<StreamHandle> {
            let body: media_source::ByteStream = Box::pin(std::io::Cursor::new(self.contents.clone()));
            Ok(StreamHandle::full("audio/mpeg", Some(self.contents.len() as u64), body))
        }
        // Default open_stream_range implementation is fine for this test — it
        // exercises the default trait method's skip-based fallback.
        async fn artwork(&self, _: DatabaseId, _: TrackId) -> MSResult<Option<Bytes>> {
            Ok(None)
        }
    }

    fn range_app() -> Router {
        // Disable transcoding so MP3 stays as passthrough.
        let mut cfg = crate::transcode::Config::default();
        cfg.enabled = false;
        let state = Arc::new(HandlerState::new_with_transcode(
            "R".into(),
            Arc::new(RangeSource { contents: (b'A'..=b'Z').cycle().take(100).collect() }),
            cfg,
        ));
        router(state)
    }

    #[tokio::test]
    async fn passthrough_range_returns_206_with_content_range() {
        let r = range_app()
            .oneshot(
                Request::builder()
                    .uri("/databases/1/items/42.mp3")
                    .header("Range", "bytes=10-19")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(r.headers().get(header::CONTENT_RANGE).unwrap(), "bytes 10-19/100");
        assert_eq!(r.headers().get(header::CONTENT_LENGTH).unwrap(), "10");
        assert_eq!(r.headers().get(header::ACCEPT_RANGES).unwrap(), "bytes");
        let body = body_of(r).await;
        assert_eq!(body.len(), 10);
    }

    #[tokio::test]
    async fn passthrough_no_range_returns_200_with_full_body() {
        let r = range_app()
            .oneshot(
                Request::builder()
                    .uri("/databases/1/items/42.mp3")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers().get(header::CONTENT_LENGTH).unwrap(), "100");
        assert!(r.headers().get(header::CONTENT_RANGE).is_none());
        let body = body_of(r).await;
        assert_eq!(body.len(), 100);
    }

    #[tokio::test]
    async fn passthrough_open_ended_range_reaches_eof() {
        let r = range_app()
            .oneshot(
                Request::builder()
                    .uri("/databases/1/items/42.mp3")
                    .header("Range", "bytes=90-")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(r.headers().get(header::CONTENT_RANGE).unwrap(), "bytes 90-99/100");
        let body = body_of(r).await;
        assert_eq!(body.len(), 10);
    }
}
