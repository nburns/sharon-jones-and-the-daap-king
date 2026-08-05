# Todos

## Backends to support

Order isn't priority — just a backlog. Each is an implementation of the
`media_source::MediaSource` trait in its own crate.

- [x] **Filesystem** (`media-source-fs`) — scans a directory, reads tags via `lofty`, streams files directly. Reference/dev backend.
- [x] **DLNA/UPnP** (`media-source-dlna`) — SSDP discovery, hand-rolled SOAP Browse, on-disk catalogue cache. Tested against Plex (4848 tracks).
- [ ] **Plex** — call the Plex Media Server HTTP API against an authenticated server, expose the music library sections. Auth via user-supplied X-Plex-Token. Advantage over DLNA: no hierarchy noise, richer metadata (ratings, playlists), artwork URLs.
- [ ] **Jellyfin** — same shape as Plex, against a Jellyfin server. Auth via API key or username/password.
- [x] **Subsonic-API** (`media-source-subsonic`) — Navidrome, Airsonic, Gonic. OpenSubsonic API-key auth (preferred) with legacy user/password fallback.

## Upstream bug reports

- [ ] **OwnTone: transcoded streams strip metadata, breaking iTunes sort/browse**
  When a track is played from an iTunes ≤ some version shared-library view,
  the played track drops to the bottom of the Artist/Album sort and its
  metadata columns go blank. Reproduced against a fresh OwnTone (v29.3) with
  a FLAC source. Root cause: OwnTone's on-the-fly transcode omits ID3 tags
  in the MP3 output, so iTunes re-reads the played stream, sees no tags, and
  clobbers the (correct) DAAP-provided cache with empties. Our server fixes
  this by injecting `-metadata title=... artist=... album=...` into the
  ffmpeg args (crates/daap-server/src/transcode.rs). File upstream:
    https://github.com/owntone/owntone-server/issues

## Cross-cutting

- [x] **Transcoding pipeline** (ffmpeg subprocess). MP3 for iTunes < 4.5, ALAC for iTunes ≥ 4.5 lossless sources. `--transcode-quality low|med|high` presets, concurrency cap, ID3 tags embedded.
- [x] **Range-request support**. Passthrough uses real file/HTTP seek; transcoded uses ffmpeg `-ss` input seek with byte↔time math for CBR MP3.
- [ ] **Long-poll `/update` should hook a real "library changed" signal** instead of hanging for a year — matters once we have mutable backends (Plex/Jellyfin can add/remove).
- [x] **Artwork** — `MediaSource::artwork()` implemented by all three backends. DAAP `/databases/N/items/M/extra_data/artwork` served with Lanczos3 resize + LRU cache. JPEG q=85 by default; content-negotiates `image/x-pict; depth=1|8` for classic Mac clients (Floyd-Steinberg → Mac System Palette; Atkinson → 1-bit) via the in-tree `pict` crate.
- [ ] **Transcode result caching** — repeated plays re-run ffmpeg. Skip until it actually matters.
- [ ] **Startup ffmpeg check** — verify `ffmpeg -version` runs at boot and warn if missing (instead of failing at first transcode).
- [ ] **VBR MP3 transcode option** — currently CBR only because it keeps byte↔time math exact for Range/seek. VBR + Xing TOC is doable but needs cache-first-play for accurate seek.
- [ ] **Content-codes coverage** — we only declare the ~50 tags we actually emit. iTunes tolerates missing entries but some clients (Rhythmbox, older forked-daapd) may barf on unknown tags in extended metadata. Consider dumping the full owntone `dmap_fields.gperf` if compat issues arise.
