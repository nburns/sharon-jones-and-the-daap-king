//! DAAP-style query parser + matcher for /databases/N/items?query=…
//!
//! Wire grammar (produced by cooperating clients like iRunes):
//!
//!   ('field:*substring*','field:*substring*',…)
//!
//! - Outer parens wrap a single top-level group. Comma inside the group is
//!   OR — a track matches when any clause matches.
//! - Each clause is single-quoted `field:pattern`.
//! - Pattern is `*substring*` — asterisk on both ends, case-insensitive
//!   substring match. No mid-string `*` or `?` in v1.
//! - Recognised fields: `dmap.itemname`, `daap.songartist`, `daap.songalbum`.
//!   Unknown fields are parsed successfully but never match anything, so
//!   older servers stay forward-compatible with client-side additions.
//!
//! Parsing is intentionally strict on shape (matched parens, matched
//! quotes, `*X*` wrap on patterns) — malformed input becomes `ParseError`
//! so the handler can return HTTP 400 and the client stops retrying that
//! keystroke.

use media_source::Track;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    ItemName,
    SongArtist,
    SongAlbum,
    /// Unknown field name — parses successfully but never matches. Keeps
    /// the door open for clients to send additional fields we haven't
    /// wired up yet.
    Unknown,
}

impl Field {
    fn parse(s: &str) -> Self {
        match s {
            "dmap.itemname" => Field::ItemName,
            "daap.songartist" => Field::SongArtist,
            "daap.songalbum" => Field::SongAlbum,
            _ => Field::Unknown,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clause {
    pub field: Field,
    /// Lowercased substring to look for. Empty (`**`) means "match all".
    pub needle: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    /// OR'd together — a track matches when any clause matches.
    pub clauses: Vec<Clause>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    MissingOuterParens,
    EmptyGroup,
    UnquotedClause,
    MissingColon,
    UnwrappedPattern,
}

/// Parse the raw (already-URL-decoded) value of the `query=` parameter.
pub fn parse(raw: &str) -> Result<Query, ParseError> {
    let inner = raw
        .trim()
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .ok_or(ParseError::MissingOuterParens)?;
    if inner.trim().is_empty() {
        return Err(ParseError::EmptyGroup);
    }
    let mut clauses = Vec::new();
    for part in inner.split(',') {
        let p = part.trim();
        let unquoted = p
            .strip_prefix('\'')
            .and_then(|s| s.strip_suffix('\''))
            .ok_or(ParseError::UnquotedClause)?;
        let (field, pat) = unquoted.split_once(':').ok_or(ParseError::MissingColon)?;
        let needle = pat
            .strip_prefix('*')
            .and_then(|s| s.strip_suffix('*'))
            .ok_or(ParseError::UnwrappedPattern)?;
        clauses.push(Clause {
            field: Field::parse(field),
            needle: needle.to_lowercase(),
        });
    }
    Ok(Query { clauses })
}

/// True if `track` matches any clause in `query`.
pub fn matches(query: &Query, track: &Track) -> bool {
    query.clauses.iter().any(|c| clause_matches(c, track))
}

fn clause_matches(c: &Clause, track: &Track) -> bool {
    let haystack: Option<&str> = match c.field {
        Field::ItemName => Some(track.title.as_str()),
        Field::SongArtist => track.artist.as_deref(),
        Field::SongAlbum => track.album.as_deref(),
        Field::Unknown => None,
    };
    match haystack {
        Some(h) => contains_ci(h, &c.needle),
        None => false,
    }
}

/// ASCII-case-insensitive substring containment. Empty needle matches
/// anything (per grammar `**` is a valid "match-all" pattern).
fn contains_ci(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    // to_lowercase() allocates but this is per-track, not hot enough to
    // matter for LAN-scale libraries. If it ever does, swap in a
    // borrow-friendly case-insensitive substring search.
    haystack.to_lowercase().contains(needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use media_source::{AudioFormat, Track};

    fn track(id: u32, title: &str, artist: Option<&str>, album: Option<&str>) -> Track {
        Track {
            id,
            title: title.into(),
            artist: artist.map(str::to_string),
            album: album.map(str::to_string),
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
    fn parses_single_clause() {
        let q = parse("('dmap.itemname:*love*')").unwrap();
        assert_eq!(q.clauses.len(), 1);
        assert_eq!(q.clauses[0].field, Field::ItemName);
        assert_eq!(q.clauses[0].needle, "love");
    }

    #[test]
    fn parses_three_way_or() {
        let raw =
            "('dmap.itemname:*beatles*','daap.songartist:*beatles*','daap.songalbum:*beatles*')";
        let q = parse(raw).unwrap();
        assert_eq!(q.clauses.len(), 3);
        assert_eq!(
            q.clauses.iter().map(|c| c.field).collect::<Vec<_>>(),
            vec![Field::ItemName, Field::SongArtist, Field::SongAlbum]
        );
        assert!(q.clauses.iter().all(|c| c.needle == "beatles"));
    }

    #[test]
    fn parses_lowercases_needle() {
        let q = parse("('daap.songartist:*Beatles*')").unwrap();
        assert_eq!(q.clauses[0].needle, "beatles");
    }

    #[test]
    fn parses_tolerates_whitespace_between_clauses() {
        let q = parse("('daap.songartist:*a*', 'daap.songalbum:*b*')").unwrap();
        assert_eq!(q.clauses.len(), 2);
    }

    #[test]
    fn parses_unknown_field_without_error() {
        let q = parse("('daap.songcomposer:*bach*')").unwrap();
        assert_eq!(q.clauses[0].field, Field::Unknown);
    }

    #[test]
    fn rejects_missing_outer_parens() {
        assert_eq!(
            parse("'daap.songartist:*x*'"),
            Err(ParseError::MissingOuterParens)
        );
    }

    #[test]
    fn rejects_unquoted_clause() {
        assert_eq!(
            parse("(daap.songartist:*x*)"),
            Err(ParseError::UnquotedClause)
        );
    }

    #[test]
    fn rejects_missing_colon() {
        assert_eq!(
            parse("('daap.songartist*x*')"),
            Err(ParseError::MissingColon)
        );
    }

    #[test]
    fn rejects_pattern_without_wildcards() {
        assert_eq!(
            parse("('daap.songartist:beatles')"),
            Err(ParseError::UnwrappedPattern)
        );
        assert_eq!(
            parse("('daap.songartist:*beatles')"),
            Err(ParseError::UnwrappedPattern)
        );
        assert_eq!(
            parse("('daap.songartist:beatles*')"),
            Err(ParseError::UnwrappedPattern)
        );
    }

    #[test]
    fn rejects_empty_group() {
        assert_eq!(parse("()"), Err(ParseError::EmptyGroup));
        assert_eq!(parse("(   )"), Err(ParseError::EmptyGroup));
    }

    #[test]
    fn matches_hits_any_clause() {
        let q = parse("('dmap.itemname:*love*','daap.songartist:*love*','daap.songalbum:*love*')")
            .unwrap();
        // Title hit.
        assert!(matches(
            &q,
            &track(1, "Love Song", Some("Nobody"), Some("Album X"))
        ));
        // Artist hit.
        assert!(matches(&q, &track(2, "X", Some("Love Battery"), Some("Y"))));
        // Album hit.
        assert!(matches(&q, &track(3, "X", Some("Y"), Some("Love Supreme"))));
        // No hit anywhere.
        assert!(!matches(&q, &track(4, "X", Some("Y"), Some("Z"))));
    }

    #[test]
    fn matches_case_insensitive() {
        let q = parse("('daap.songartist:*Beatles*')").unwrap();
        assert!(matches(&q, &track(1, "T", Some("THE BEATLES"), None)));
        assert!(matches(&q, &track(2, "T", Some("the beatles"), None)));
        assert!(matches(&q, &track(3, "T", Some("The Beatles"), None)));
    }

    #[test]
    fn matches_missing_field_is_not_a_hit() {
        // Track has no artist. A songartist-only query must miss.
        let q = parse("('daap.songartist:*x*')").unwrap();
        assert!(!matches(&q, &track(1, "any title", None, None)));
    }

    #[test]
    fn matches_unknown_field_never_hits() {
        let q = parse("('daap.songcomposer:*bach*')").unwrap();
        assert!(!matches(&q, &track(1, "Bach", Some("Bach"), Some("Bach"))));
    }

    #[test]
    fn matches_empty_needle_matches_everything() {
        let q = parse("('daap.songartist:**')").unwrap();
        assert!(matches(&q, &track(1, "T", Some("anyone"), None)));
    }
}
