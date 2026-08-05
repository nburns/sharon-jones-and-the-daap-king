//! DMAP response builders for endpoints beyond /server-info.

use bytes::BytesMut;
use media_source::{AudioFormat, Database, Playlist, Track};

use crate::charset::Charset;
use crate::dmap::{container, string_field_bytes, u16_field, u32_field, u64_field, u8_field};
use crate::tags;

/// Write a string field, encoding the value into the wire charset.
fn write_string(out: &mut BytesMut, t: crate::dmap::Tag, s: &str, cs: Charset) {
    let bytes = cs.encode(s);
    string_field_bytes(out, t, &bytes);
}

/// Build the `mlog` login response body.
pub fn login(session_id: u32) -> BytesMut {
    let mut out = BytesMut::with_capacity(32);
    container(&mut out, tags::login_response(), |b| {
        u32_field(b, tags::status(), 200);
        u32_field(b, tags::session_id(), session_id);
    });
    out
}

/// Build the `mupd` update response body.
pub fn update(revision: u32) -> BytesMut {
    let mut out = BytesMut::with_capacity(32);
    container(&mut out, tags::update_response(), |b| {
        u32_field(b, tags::status(), 200);
        u32_field(b, tags::server_revision(), revision);
    });
    out
}

/// Build the `avdb` databases-list response.
pub fn databases(
    dbs: &[Database],
    track_count: u32,
    playlist_count: u32,
    charset: Charset,
) -> BytesMut {
    let mut out = BytesMut::with_capacity(256);
    container(&mut out, tags::databases_response(), |b| {
        u32_field(b, tags::status(), 200);
        u8_field(b, tags::update_type(), 0);
        u32_field(b, tags::total_matched(), dbs.len() as u32);
        u32_field(b, tags::returned_count(), dbs.len() as u32);
        container(b, tags::listing(), |list| {
            for db in dbs {
                container(list, tags::listing_item(), |it| {
                    u32_field(it, tags::item_id(), db.id);
                    u64_field(it, tags::persistent_id(), db.id as u64);
                    write_string(it, tags::item_name(), &db.name, charset);
                    u32_field(it, tags::item_count(), track_count);
                    u32_field(it, tags::container_count(), playlist_count);
                });
            }
        });
    });
    out
}

/// Build the `adbs` items (songs) listing.
///
/// `sliced` is the entries to embed in the `mlcl` listing.
/// `total_matched` is the full pre-slice count and goes into `mtco` —
/// clients use it to size scroll viewports even when the response body
/// only carries a page.
pub fn items(sliced: &[Track], total_matched: usize, charset: Charset) -> BytesMut {
    let mut out = BytesMut::with_capacity(1024);
    container(&mut out, tags::items_response(), |b| {
        u32_field(b, tags::status(), 200);
        u8_field(b, tags::update_type(), 0);
        u32_field(b, tags::total_matched(), total_matched as u32);
        u32_field(b, tags::returned_count(), sliced.len() as u32);
        container(b, tags::listing(), |list| {
            for t in sliced {
                container(list, tags::listing_item(), |it| {
                    emit_track_mlit(it, t, None, charset);
                });
            }
        });
    });
    out
}

/// Emit a single track's DMAP fields into an already-opened `mlit`
/// container `it`. When `mpco` is `Some(v)`, writes a
/// `parent_container_id` field with the given 1-based value — used for
/// playlist_songs where entries need a stable position across pages.
fn emit_track_mlit(it: &mut BytesMut, t: &Track, mpco: Option<u32>, charset: Charset) {
    u8_field(it, tags::item_kind(), 2); // 2 = music
    u32_field(it, tags::item_id(), t.id);
    u64_field(it, tags::persistent_id(), t.id as u64);
    write_string(it, tags::item_name(), &t.title, charset);
    if let Some(a) = &t.artist {
        write_string(it, tags::song_artist(), a, charset);
    }
    if let Some(a) = &t.album {
        write_string(it, tags::song_album(), a, charset);
    }
    if let Some(a) = &t.album_artist {
        write_string(it, tags::song_album_artist(), a, charset);
    }
    if let Some(g) = &t.genre {
        write_string(it, tags::song_genre(), g, charset);
    }
    // Format string is always ASCII; skip charset conversion.
    write_string(it, tags::song_format(), format_string(t.format), charset);
    u8_field(it, tags::song_data_kind(), 0); // 0 = local file
    if let Some(n) = t.track_number {
        u16_field(it, tags::song_track_number(), n);
    }
    if let Some(n) = t.disc_number {
        u16_field(it, tags::song_disc_number(), n);
    }
    if let Some(y) = t.year {
        u16_field(it, tags::song_year(), y);
    }
    if let Some(ms) = t.duration_ms {
        u32_field(it, tags::song_time_ms(), ms);
    }
    if let Some(kbps) = t.bitrate_kbps {
        u16_field(it, tags::song_bitrate(), kbps as u16);
    }
    if let Some(sr) = t.sample_rate {
        u32_field(it, tags::song_sample_rate(), sr);
    }
    if let Some(sz) = t.size_bytes {
        u32_field(it, tags::song_size(), sz.min(u32::MAX as u64) as u32);
    }
    if let Some(v) = mpco {
        u32_field(it, tags::parent_container_id(), v);
    }
}

/// Build the `aply` playlists-list response.
///
/// The synthetic "Library" base playlist lives at absolute index 0; extras
/// live at absolute indices 1..N. Callers decide (based on the requested
/// `?index=` range) whether Library is in this page and which slice of
/// `extras` to include, then pass:
///   * `extras_slice` — the extras entries to embed
///   * `include_library` — whether Library is in this page
///   * `total_matched` — full playlist count (1 + extras.len()), for `mtco`
pub fn playlists(
    library_id: u32,
    track_count: u32,
    extras_slice: &[Playlist],
    include_library: bool,
    total_matched: usize,
    charset: Charset,
) -> BytesMut {
    let mut out = BytesMut::with_capacity(256);
    let returned = (include_library as u32) + extras_slice.len() as u32;
    container(&mut out, tags::playlists_response(), |b| {
        u32_field(b, tags::status(), 200);
        u8_field(b, tags::update_type(), 0);
        u32_field(b, tags::total_matched(), total_matched as u32);
        u32_field(b, tags::returned_count(), returned);
        container(b, tags::listing(), |list| {
            if include_library {
                container(list, tags::listing_item(), |it| {
                    u32_field(it, tags::item_id(), library_id);
                    u64_field(it, tags::persistent_id(), library_id as u64);
                    write_string(it, tags::item_name(), "Library", charset);
                    u32_field(it, tags::item_count(), track_count);
                    u8_field(it, tags::base_playlist(), 1);
                });
            }
            for pl in extras_slice {
                container(list, tags::listing_item(), |it| {
                    u32_field(it, tags::item_id(), pl.id);
                    u64_field(it, tags::persistent_id(), pl.id as u64);
                    write_string(it, tags::item_name(), &pl.name, charset);
                    u32_field(it, tags::item_count(), pl.track_ids.len() as u32);
                });
            }
        });
    });
    out
}

/// Build the `apso` playlist-songs response with only `miid` + `mpco` per
/// entry. This is the classic-DAAP shape iTunes 4 expects when it sends
/// `meta=dmap.itemid,dmap.containeritemid` — it already has full metadata
/// from an earlier `/databases/{db}/items` fetch and just wants the
/// per-playlist ordering here.
pub fn playlist_songs(sliced: &[u32], total_matched: usize, slice_offset: usize) -> BytesMut {
    let mut out = BytesMut::with_capacity(256);
    container(&mut out, tags::playlist_songs_response(), |b| {
        u32_field(b, tags::status(), 200);
        u8_field(b, tags::update_type(), 0);
        u32_field(b, tags::total_matched(), total_matched as u32);
        u32_field(b, tags::returned_count(), sliced.len() as u32);
        container(b, tags::listing(), |list| {
            for (idx, tid) in sliced.iter().enumerate() {
                container(list, tags::listing_item(), |it| {
                    u8_field(it, tags::item_kind(), 2);
                    u32_field(it, tags::item_id(), *tid);
                    u32_field(
                        it,
                        tags::parent_container_id(),
                        (slice_offset + idx + 1) as u32,
                    );
                });
            }
        });
    });
    out
}

/// Build the `apso` playlist-songs response with full per-track metadata.
///
/// `resolved` is the tracks to embed (already looked up from ids). Callers
/// pass a possibly-shorter list than the original id slice when some ids
/// don't resolve — `total_matched` should stay the full playlist size so
/// paging math on the client remains correct. `slice_offset` is the
/// 0-based starting index of the id slice this page corresponds to; used
/// with each entry's index to compute `mpco` (container-item-id, 1-based
/// per DAAP convention) so playlist ordering stays stable across pages.
pub fn playlist_songs_full(
    resolved: &[&Track],
    total_matched: usize,
    slice_offset: usize,
    charset: Charset,
) -> BytesMut {
    let mut out = BytesMut::with_capacity(1024);
    container(&mut out, tags::playlist_songs_response(), |b| {
        u32_field(b, tags::status(), 200);
        u8_field(b, tags::update_type(), 0);
        u32_field(b, tags::total_matched(), total_matched as u32);
        u32_field(b, tags::returned_count(), resolved.len() as u32);
        container(b, tags::listing(), |list| {
            for (idx, t) in resolved.iter().enumerate() {
                let mpco = (slice_offset + idx + 1) as u32;
                container(list, tags::listing_item(), |it| {
                    emit_track_mlit(it, t, Some(mpco), charset);
                });
            }
        });
    });
    out
}

fn format_string(f: AudioFormat) -> &'static str {
    match f {
        AudioFormat::Mp3 => "mp3",
        AudioFormat::Aac | AudioFormat::Alac => "m4a",
        AudioFormat::Flac => "flac",
        AudioFormat::Wav => "wav",
        AudioFormat::Aiff => "aiff",
        AudioFormat::Ogg => "ogg",
        AudioFormat::Other => "bin",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expect_tag_at(body: &[u8], offset: usize, tag: &[u8; 4]) {
        assert_eq!(&body[offset..offset + 4], tag, "expected tag {:?} at offset {}", std::str::from_utf8(tag).unwrap(), offset);
    }

    fn container_body_len(body: &[u8]) -> usize {
        u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize
    }

    #[test]
    fn login_encodes_session_id() {
        let body = login(0x1234_5678);
        expect_tag_at(&body, 0, b"mlog");
        assert_eq!(body.len(), 8 + 24);
        assert_eq!(container_body_len(&body), 24);
        // second field (after mstt=12 bytes) should be mlid
        expect_tag_at(&body, 8 + 12, b"mlid");
    }

    #[test]
    fn update_encodes_revision() {
        let body = update(2);
        expect_tag_at(&body, 0, b"mupd");
        assert_eq!(container_body_len(&body), 24);
    }

    #[test]
    fn databases_lists_one_db() {
        let dbs = vec![Database { id: 1, name: "Main".into() }];
        let body = databases(&dbs, 10, 3, Charset::Utf8);
        expect_tag_at(&body, 0, b"avdb");
        assert_eq!(container_body_len(&body), body.len() - 8);
    }

    #[test]
    fn items_encodes_track_metadata() {
        let tracks = vec![Track {
            id: 1,
            title: "Hello".into(),
            artist: Some("Adele".into()),
            album: Some("25".into()),
            album_artist: None,
            genre: Some("Pop".into()),
            track_number: Some(1),
            disc_number: Some(1),
            year: Some(2015),
            duration_ms: Some(295000),
            bitrate_kbps: Some(256),
            sample_rate: Some(44100),
            size_bytes: Some(9_500_000),
            format: AudioFormat::Mp3,
        }];
        let body = items(&tracks, tracks.len(), Charset::Utf8);
        expect_tag_at(&body, 0, b"adbs");
        assert!(body.windows(5).any(|w| w == b"Adele"));
        assert!(body.windows(5).any(|w| w == b"Hello"));
    }

    #[test]
    fn items_macroman_transliterates_accented_chars() {
        let tracks = vec![Track {
            id: 1,
            title: "Café".into(),
            artist: Some("Björk".into()),
            album: None, album_artist: None,
            genre: None, track_number: None, disc_number: None,
            year: None, duration_ms: None, bitrate_kbps: None,
            sample_rate: None, size_bytes: None,
            format: AudioFormat::Mp3,
        }];
        let body = items(&tracks, tracks.len(), Charset::MacRoman);
        // MacRoman é = 0x8E, ö = 0x9A
        assert!(body.windows(4).any(|w| w == [b'C', b'a', b'f', 0x8E]));
        assert!(body.windows(5).any(|w| w == [b'B', b'j', 0x9A, b'r', b'k']));
        // And the raw UTF-8 bytes should NOT appear.
        assert!(!body.windows(4).any(|w| w == "Café".as_bytes()));
    }

    #[test]
    fn playlists_full_response_includes_library() {
        let body = playlists(1, 42, &[], true, 1, Charset::Utf8);
        expect_tag_at(&body, 0, b"aply");
        assert!(body.windows(7).any(|w| w == b"Library"));
    }

    #[test]
    fn playlists_slice_omits_library_when_range_starts_past_zero() {
        let extras = vec![
            Playlist { id: 2, name: "Alpha".into(), track_ids: vec![] },
            Playlist { id: 3, name: "Beta".into(),  track_ids: vec![] },
        ];
        // Page = extras[0..2] with Library excluded; full total = 1 + 2 = 3.
        let body = playlists(1, 42, &extras, false, 3, Charset::Utf8);
        assert!(!body.windows(7).any(|w| w == b"Library"));
        assert!(body.windows(5).any(|w| w == b"Alpha"));
        assert!(body.windows(4).any(|w| w == b"Beta"));
        // mrco should be 2, mtco should be 3.
        let mrco_pos = find_field(&body, b"mrco");
        let mtco_pos = find_field(&body, b"mtco");
        assert_eq!(u32::from_be_bytes(body[mrco_pos..mrco_pos + 4].try_into().unwrap()), 2);
        assert_eq!(u32::from_be_bytes(body[mtco_pos..mtco_pos + 4].try_into().unwrap()), 3);
    }

    fn dummy_track(id: u32, title: &str) -> Track {
        Track {
            id,
            title: title.into(),
            artist: Some(format!("Artist{id}")),
            album: Some(format!("Album{id}")),
            album_artist: None,
            genre: None,
            track_number: None,
            disc_number: None,
            year: None,
            duration_ms: Some(1000 + id),
            bitrate_kbps: Some(192),
            sample_rate: Some(44100),
            size_bytes: Some(10_000),
            format: AudioFormat::Mp3,
        }
    }

    #[test]
    fn playlist_songs_full_encodes_all_metadata() {
        let t1 = dummy_track(10, "One");
        let t2 = dummy_track(20, "Two");
        let refs: Vec<&Track> = vec![&t1, &t2];
        let body = playlist_songs_full(&refs, refs.len(), 0, Charset::Utf8);
        expect_tag_at(&body, 0, b"apso");
        assert!(body.windows(3).any(|w| w == b"One"));
        assert!(body.windows(7).any(|w| w == b"Artist10"[..7].as_ref()));
        assert!(body.windows(4).any(|w| w == b"asar")); // song_artist tag
        assert!(body.windows(4).any(|w| w == b"asal")); // song_album tag
        assert!(body.windows(4).any(|w| w == b"astm")); // song_time_ms tag
    }

    #[test]
    fn playlist_songs_full_slice_reports_mpco() {
        // Full playlist has 100 items; we're sending a page of 3 starting at
        // index 42. Expected: mtco=100, mrco=3, mpco values 43/44/45.
        let t1 = dummy_track(500, "A");
        let t2 = dummy_track(501, "B");
        let t3 = dummy_track(502, "C");
        let refs: Vec<&Track> = vec![&t1, &t2, &t3];
        let body = playlist_songs_full(&refs, 100, 42, Charset::Utf8);
        let mtco_offset = find_field(&body, b"mtco");
        let mrco_offset = find_field(&body, b"mrco");
        assert_eq!(u32::from_be_bytes(body[mtco_offset..mtco_offset + 4].try_into().unwrap()), 100);
        assert_eq!(u32::from_be_bytes(body[mrco_offset..mrco_offset + 4].try_into().unwrap()), 3);
        let mpco_positions: Vec<u32> = body
            .windows(4)
            .enumerate()
            .filter_map(|(i, w)| {
                if w == b"mpco" {
                    Some(u32::from_be_bytes(body[i + 8..i + 12].try_into().unwrap()))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(mpco_positions, vec![43, 44, 45]);
    }

    #[test]
    fn playlist_songs_full_mtco_reflects_full_id_count_even_when_some_missing() {
        // Simulate: playlist has 5 ids, only 3 resolved (2 stale).
        // Handler passes `total = ids.len() = 5`, `resolved = 3 tracks`.
        // mtco should be 5, mrco should be 3.
        let t1 = dummy_track(1, "one");
        let t2 = dummy_track(2, "two");
        let t3 = dummy_track(3, "three");
        let refs: Vec<&Track> = vec![&t1, &t2, &t3];
        let body = playlist_songs_full(&refs, 5, 0, Charset::Utf8);
        let mtco_offset = find_field(&body, b"mtco");
        let mrco_offset = find_field(&body, b"mrco");
        assert_eq!(u32::from_be_bytes(body[mtco_offset..mtco_offset + 4].try_into().unwrap()), 5);
        assert_eq!(u32::from_be_bytes(body[mrco_offset..mrco_offset + 4].try_into().unwrap()), 3);
    }

    fn find_field(body: &[u8], tag: &[u8; 4]) -> usize {
        // Naive scan: look for the 4-byte tag literal followed by a plausible length.
        let mut i = 0;
        while i + 8 <= body.len() {
            if &body[i..i + 4] == tag {
                return i + 8;
            }
            i += 1;
        }
        panic!("tag {:?} not found", std::str::from_utf8(tag).unwrap());
    }
}
