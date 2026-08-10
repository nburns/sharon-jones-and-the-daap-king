//! Server-side sort for /databases/N/items (and container-items with
//! meta=all). Gated on the `SHRF_SORT` capability bit in /server-info.
//!
//! Wire grammar (on the `sort=` query parameter):
//!
//!   sort=<key>[,<key>...]
//!   key = [-]<field>
//!
//! - Leading `-` requests descending order for that key; otherwise ascending.
//! - Fields accept the full DAAP name (e.g. `daap.songartist`) so a client
//!   that already speaks DMAP field names just works. Short aliases
//!   (`artist`, `album`, `title`/`name`, `time`, `track`, `disc`, `year`,
//!   `genre`, `albumartist`, `bitrate`, `size`, `samplerate`, `id`) are
//!   also recognised.
//! - Multi-key: later keys break ties among earlier ones. `id` ascending is
//!   always appended as the final tie-breaker so paginated fetches of the
//!   same URL return a stable slice.
//! - Missing values (`Option::None` on a track) sort *last* regardless of
//!   direction. Otherwise `sort=-year` would flood the top with unknown
//!   years.
//! - Unknown fields and malformed shape both surface as `ParseError` so the
//!   handler returns HTTP 400 and the client stops retrying.

use std::cmp::Ordering;

use media_source::Track;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    Id,
    Title,
    Artist,
    Album,
    AlbumArtist,
    Genre,
    TrackNumber,
    DiscNumber,
    Year,
    Duration,
    Bitrate,
    SampleRate,
    Size,
}

impl Field {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "dmap.itemid" | "id" => Some(Self::Id),
            "dmap.itemname" | "name" | "title" => Some(Self::Title),
            "daap.songartist" | "artist" => Some(Self::Artist),
            "daap.songalbum" | "album" => Some(Self::Album),
            "daap.songalbumartist" | "albumartist" => Some(Self::AlbumArtist),
            "daap.songgenre" | "genre" => Some(Self::Genre),
            "daap.songtracknumber" | "track" => Some(Self::TrackNumber),
            "daap.songdiscnumber" | "disc" => Some(Self::DiscNumber),
            "daap.songyear" | "year" => Some(Self::Year),
            "daap.songtime" | "time" | "duration" => Some(Self::Duration),
            "daap.songbitrate" | "bitrate" => Some(Self::Bitrate),
            "daap.songsamplerate" | "samplerate" => Some(Self::SampleRate),
            "daap.songsize" | "size" => Some(Self::Size),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SortKey {
    pub field: Field,
    pub desc: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Empty,
    EmptyKey,
    BareDirection,
    UnknownField(String),
}

/// Parse the raw (already-URL-decoded) value of the `sort=` parameter.
pub fn parse(raw: &str) -> Result<Vec<SortKey>, ParseError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(ParseError::Empty);
    }
    let mut keys = Vec::new();
    for part in trimmed.split(',') {
        let p = part.trim();
        if p.is_empty() {
            return Err(ParseError::EmptyKey);
        }
        let (desc, name) = if let Some(rest) = p.strip_prefix('-') {
            (true, rest.trim())
        } else if let Some(rest) = p.strip_prefix('+') {
            (false, rest.trim())
        } else {
            (false, p)
        };
        if name.is_empty() {
            return Err(ParseError::BareDirection);
        }
        let field = Field::parse(name).ok_or_else(|| ParseError::UnknownField(name.to_string()))?;
        keys.push(SortKey { field, desc });
    }
    Ok(keys)
}

/// Sort `tracks` in place by the given keys. `id` ascending is appended as
/// an implicit final tie-breaker so pagination stays stable across pages.
pub fn apply(tracks: &mut [Track], keys: &[SortKey]) {
    tracks.sort_by(|a, b| compare(a, b, keys));
}

/// Compare two tracks against the ordered key list. Public so callers with
/// borrowed refs (`Vec<&Track>`, as in the container-items full-metadata
/// path) can sort without copying.
pub fn compare(a: &Track, b: &Track, keys: &[SortKey]) -> Ordering {
    for k in keys {
        let ord = cmp_field(a, b, k.field, k.desc);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a.id.cmp(&b.id)
}

fn cmp_field(a: &Track, b: &Track, f: Field, desc: bool) -> Ordering {
    match f {
        // `id` has no missing case, so plain reversal is correct.
        Field::Id => {
            let o = a.id.cmp(&b.id);
            if desc { o.reverse() } else { o }
        }
        Field::Title => cmp_str_ci(Some(a.title.as_str()), Some(b.title.as_str()), desc),
        Field::Artist => cmp_str_ci(a.artist.as_deref(), b.artist.as_deref(), desc),
        Field::Album => cmp_str_ci(a.album.as_deref(), b.album.as_deref(), desc),
        Field::AlbumArtist => {
            cmp_str_ci(a.album_artist.as_deref(), b.album_artist.as_deref(), desc)
        }
        Field::Genre => cmp_str_ci(a.genre.as_deref(), b.genre.as_deref(), desc),
        Field::TrackNumber => cmp_opt(a.track_number, b.track_number, desc),
        Field::DiscNumber => cmp_opt(a.disc_number, b.disc_number, desc),
        Field::Year => cmp_opt(a.year, b.year, desc),
        Field::Duration => cmp_opt(a.duration_ms, b.duration_ms, desc),
        Field::Bitrate => cmp_opt(a.bitrate_kbps, b.bitrate_kbps, desc),
        Field::SampleRate => cmp_opt(a.sample_rate, b.sample_rate, desc),
        Field::Size => cmp_opt(a.size_bytes, b.size_bytes, desc),
    }
}

/// Missing (`None`) sorts *after* any present value regardless of `desc`.
/// `desc` only reverses the Some/Some case, so "unknown year" never floods
/// the top of a descending sort.
fn cmp_opt<T: Ord>(a: Option<T>, b: Option<T>, desc: bool) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => {
            if desc { y.cmp(&x) } else { x.cmp(&y) }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// Case-insensitive string compare with the same missing-last policy as
/// `cmp_opt`. Empty strings are treated as present (they came from the
/// source that way).
fn cmp_str_ci(a: Option<&str>, b: Option<&str>, desc: bool) -> Ordering {
    match (a, b) {
        (Some(x), Some(y)) => {
            let xi = x.chars().flat_map(char::to_lowercase);
            let yi = y.chars().flat_map(char::to_lowercase);
            let o = xi.cmp(yi);
            if desc { o.reverse() } else { o }
        }
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use media_source::AudioFormat;

    fn t(id: u32) -> Track {
        Track {
            id,
            title: format!("t{id}"),
            artist: None,
            album: None,
            album_artist: None,
            genre: None,
            track_number: None,
            disc_number: None,
            year: None,
            duration_ms: None,
            bitrate_kbps: None,
            sample_rate: None,
            size_bytes: None,
            format: AudioFormat::Mp3,
        }
    }

    #[test]
    fn parses_short_alias_asc() {
        let k = parse("artist").unwrap();
        assert_eq!(k, vec![SortKey { field: Field::Artist, desc: false }]);
    }

    #[test]
    fn parses_daap_field_name() {
        let k = parse("daap.songartist").unwrap();
        assert_eq!(k, vec![SortKey { field: Field::Artist, desc: false }]);
    }

    #[test]
    fn parses_dash_prefix_as_desc() {
        let k = parse("-year").unwrap();
        assert_eq!(k, vec![SortKey { field: Field::Year, desc: true }]);
    }

    #[test]
    fn parses_multi_key() {
        let k = parse("artist,album,disc,track").unwrap();
        assert_eq!(
            k,
            vec![
                SortKey { field: Field::Artist, desc: false },
                SortKey { field: Field::Album, desc: false },
                SortKey { field: Field::DiscNumber, desc: false },
                SortKey { field: Field::TrackNumber, desc: false },
            ]
        );
    }

    #[test]
    fn parses_tolerates_whitespace() {
        let k = parse(" -artist , album ").unwrap();
        assert_eq!(
            k,
            vec![
                SortKey { field: Field::Artist, desc: true },
                SortKey { field: Field::Album, desc: false },
            ]
        );
    }

    #[test]
    fn rejects_empty() {
        assert_eq!(parse(""), Err(ParseError::Empty));
        assert_eq!(parse("   "), Err(ParseError::Empty));
    }

    #[test]
    fn rejects_empty_key_in_list() {
        assert_eq!(parse("artist,,album"), Err(ParseError::EmptyKey));
        assert_eq!(parse("artist,"), Err(ParseError::EmptyKey));
    }

    #[test]
    fn rejects_bare_direction() {
        assert_eq!(parse("-"), Err(ParseError::BareDirection));
        assert_eq!(parse("artist,-"), Err(ParseError::BareDirection));
    }

    #[test]
    fn rejects_unknown_field() {
        assert_eq!(
            parse("composer"),
            Err(ParseError::UnknownField("composer".into()))
        );
        assert_eq!(
            parse("artist,daap.songcomposer"),
            Err(ParseError::UnknownField("daap.songcomposer".into()))
        );
    }

    #[test]
    fn sort_by_title_case_insensitive() {
        let mut ts = vec![t(1), t(2), t(3)];
        ts[0].title = "banana".into();
        ts[1].title = "Apple".into();
        ts[2].title = "cherry".into();
        apply(&mut ts, &parse("title").unwrap());
        let titles: Vec<_> = ts.iter().map(|t| t.title.clone()).collect();
        assert_eq!(titles, vec!["Apple", "banana", "cherry"]);
    }

    #[test]
    fn sort_missing_values_go_last_asc() {
        let mut ts = vec![t(1), t(2), t(3)];
        ts[0].year = Some(1999);
        ts[1].year = None;
        ts[2].year = Some(1970);
        apply(&mut ts, &parse("year").unwrap());
        let ids: Vec<_> = ts.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![3, 1, 2]); // 1970, 1999, missing
    }

    #[test]
    fn sort_missing_values_go_last_desc() {
        let mut ts = vec![t(1), t(2), t(3)];
        ts[0].year = Some(1999);
        ts[1].year = None;
        ts[2].year = Some(1970);
        apply(&mut ts, &parse("-year").unwrap());
        let ids: Vec<_> = ts.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![1, 3, 2]); // 1999, 1970, missing (still last)
    }

    #[test]
    fn sort_ties_break_on_id() {
        let mut ts = vec![t(3), t(1), t(2)];
        for x in ts.iter_mut() {
            x.artist = Some("same".into());
        }
        apply(&mut ts, &parse("artist").unwrap());
        let ids: Vec<_> = ts.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }

    #[test]
    fn sort_multi_key_artist_then_disc_then_track() {
        let mut a = t(1);
        a.artist = Some("A".into());
        a.disc_number = Some(1);
        a.track_number = Some(2);
        let mut b = t(2);
        b.artist = Some("A".into());
        b.disc_number = Some(1);
        b.track_number = Some(1);
        let mut c = t(3);
        c.artist = Some("A".into());
        c.disc_number = Some(2);
        c.track_number = Some(1);
        let mut d = t(4);
        d.artist = Some("B".into());
        d.disc_number = Some(1);
        d.track_number = Some(1);
        let mut ts = vec![c.clone(), d.clone(), a.clone(), b.clone()];
        apply(&mut ts, &parse("artist,disc,track").unwrap());
        let ids: Vec<_> = ts.iter().map(|t| t.id).collect();
        assert_eq!(ids, vec![2, 1, 3, 4]);
    }

    #[test]
    fn sort_desc_reverses_string_order() {
        let mut ts = vec![t(1), t(2)];
        ts[0].artist = Some("alpha".into());
        ts[1].artist = Some("bravo".into());
        apply(&mut ts, &parse("-artist").unwrap());
        assert_eq!(ts[0].artist.as_deref(), Some("bravo"));
        assert_eq!(ts[1].artist.as_deref(), Some("alpha"));
    }
}
