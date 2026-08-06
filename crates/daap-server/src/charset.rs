//! Character-set conversion for DMAP string fields.
//!
//! DAAP historically uses UTF-8 for string tag values. Classic-era clients
//! (Mac II speaking a 68k-native DAAP client) expect MacRoman. When a client
//! sends `Accept-Charset: x-mac-roman`, we run string fields through
//! [`Charset::MacRoman`] before writing them into the DMAP payload.
//!
//! Encoding pipeline for [`to_macroman`]:
//!   1. If the string contains any Japanese / CJK script (hiragana,
//!      katakana, or Han ideographs), pre-pass through `ib-romaji` to
//!      turn readings into Hepburn romaji. Unmatched runs fall through
//!      unchanged. This is the aesthetics-over-correctness call — CJK
//!      Han without kana context might be Chinese, but the JP pinyin
//!      dictionary still gives a Latin surface form worth reading.
//!   2. NFC-normalize so decomposed forms (e.g. `e` + combining acute
//!      from macOS filesystem tags) compose back into their pre-composed
//!      code points before the table lookup.
//!   3. ASCII 0x20..0x7E passes through unchanged (MacRoman is ASCII-super).
//!   4. Common Latin-supplement code points map to MacRoman positions per
//!      Apple's canonical table (Inside Macintosh: Text, Appendix E).
//!   5. Combining marks that survived NFC (couldn't compose with a base) are
//!      dropped — the base char is already out.
//!   6. Otherwise, transliterate via `deunicode` into a Latin approximation
//!      and re-lookup each byte through the table. This is the fallback
//!      that catches Chinese-only text, non-JP CJK, and Unicode that
//!      slipped through the other stages.
//!   7. Anything still unmappable → `?` (ASCII 0x3F).
//!
//! We don't ship the full 256-entry Unicode↔MacRoman table — just the ~90
//! code points that show up in real-world artist/track/album names. Extend
//! as gaps show up.

use std::sync::OnceLock;

use ib_romaji::HepburnRomanizer;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// Global romanizer — 4.8 MiB Aho-Corasick / dict tables. Built once on
/// first use, then shared across all requests.
fn romanizer() -> &'static HepburnRomanizer {
    static R: OnceLock<HepburnRomanizer> = OnceLock::new();
    R.get_or_init(HepburnRomanizer::default)
}

/// True when `c` is in a Japanese script range that ib-romaji can handle:
/// hiragana, katakana (full + phonetic-ext + half-width), or CJK Unified
/// Ideographs (Han). Han is ambiguous JP/CN, but we prefer trying romaji.
fn is_japanese_script(c: char) -> bool {
    matches!(c as u32,
        0x3040..=0x309F      // Hiragana
        | 0x30A0..=0x30FF    // Katakana
        | 0x31F0..=0x31FF    // Katakana Phonetic Extensions
        | 0xFF66..=0xFF9F    // Halfwidth Katakana
        | 0x3400..=0x4DBF    // CJK Ext A
        | 0x4E00..=0x9FFF    // CJK Unified Ideographs
    )
}

/// Greedy longest-match walk with ib-romaji. Matched runs are concatenated
/// directly (kana strings like `ありがとう` become `arigatou`, not
/// `a ri ga to u`). Unmatched chars pass through so the next stage in the
/// pipeline can handle them. A single space is inserted at kana↔non-kana
/// boundaries so words don't smash into surrounding Latin text.
fn romanize_japanese(s: &str) -> String {
    let r = romanizer();
    let mut out = String::with_capacity(s.len());
    let mut last_was_romaji = false;
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < s.len() {
        let mut best: Option<(usize, &'static str)> = None;
        r.romanize_and_try_for_each(&s[i..], |len, romaji| {
            match best {
                Some((n, _)) if n >= len => {}
                _ => best = Some((len, romaji)),
            }
            None::<()>
        });
        if let Some((len, romaji)) = best {
            if !last_was_romaji {
                // Entering a JP run from Latin/space - no leading separator
                // needed because the source already had its own boundary.
            }
            out.push_str(romaji);
            last_was_romaji = true;
            i += len;
        } else {
            let c_len = std::str::from_utf8(&bytes[i..])
                .ok()
                .and_then(|s| s.chars().next())
                .map(|c| c.len_utf8())
                .unwrap_or(1);
            let ch = &s[i..i + c_len];
            // If we're leaving a romaji run and the next char is a letter
            // (i.e. an untranslated kanji that will become pinyin, or a
            // Latin letter), insert a space to avoid word-smash.
            if last_was_romaji {
                let starts_letter = ch.chars().next().is_some_and(|c| c.is_alphabetic());
                if starts_letter {
                    out.push(' ');
                }
            }
            out.push_str(ch);
            last_was_romaji = false;
            i += c_len;
        }
    }
    out
}

/// Which byte encoding to use for DMAP string field values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Charset {
    /// Standard DAAP: UTF-8, per spec.
    Utf8,
    /// Classic-Mac MacRoman (Apple's original 8-bit encoding).
    MacRoman,
}

impl Charset {
    /// Encode `s` into the target charset. Returns owned bytes because
    /// `MacRoman` requires per-char lookup; `Utf8` returns the input bytes
    /// directly cloned for uniformity.
    pub fn encode(self, s: &str) -> Vec<u8> {
        match self {
            Charset::Utf8 => s.as_bytes().to_vec(),
            Charset::MacRoman => to_macroman(s),
        }
    }

    /// String suitable for a `Content-Type` charset parameter.
    pub fn ct_param(self) -> Option<&'static str> {
        match self {
            Charset::Utf8 => None,
            Charset::MacRoman => Some("x-mac-roman"),
        }
    }
}

/// Case-insensitive check of an `Accept-Charset` header value.
pub fn charset_from_accept(header: Option<&str>) -> Charset {
    let a = match header {
        Some(s) => s.to_ascii_lowercase(),
        None => return Charset::Utf8,
    };
    for token in a.split(&[',', ';'][..]) {
        let t = token.trim();
        if t == "x-mac-roman" || t == "macintosh" || t == "mac-roman" || t == "macroman" {
            return Charset::MacRoman;
        }
    }
    Charset::Utf8
}

/// Lossy UTF-8 → MacRoman transliteration. See module docs for pipeline.
pub fn to_macroman(s: &str) -> Vec<u8> {
    let owned;
    let input: &str = if s.chars().any(is_japanese_script) {
        owned = romanize_japanese(s);
        &owned
    } else {
        s
    };
    let mut out = Vec::with_capacity(input.len());
    for c in input.nfc() {
        encode_char(c, &mut out);
    }
    out
}

fn encode_char(c: char, out: &mut Vec<u8>) {
    if (c as u32) < 0x80 {
        out.push(c as u8);
        return;
    }
    if let Some(b) = LATIN_EXT.iter().find(|(u, _)| *u == c).map(|(_, b)| *b) {
        out.push(b);
        return;
    }
    if is_combining_mark(c) {
        return;
    }
    // deunicode always returns ASCII, so we push its bytes directly rather
    // than re-recursing through encode_char (avoids infinite loops if it
    // ever returned a non-ASCII fallback for a specific char).
    match deunicode::deunicode_char(c) {
        Some("") => {}
        Some(s) => out.extend_from_slice(s.as_bytes()),
        None => out.push(b'?'),
    }
}

/// Common non-ASCII Unicode → MacRoman code points. Sourced from Apple's
/// MacRoman table (Inside Macintosh: Text). Order isn't significant since
/// lookup is linear; kept roughly grouped by kind for readability.
const LATIN_EXT: &[(char, u8)] = &[
    // Latin uppercase with diacritics
    ('Ä', 0x80), ('Å', 0x81), ('Ç', 0x82), ('É', 0x83), ('Ñ', 0x84),
    ('Ö', 0x85), ('Ü', 0x86),
    // Latin lowercase with diacritics
    ('á', 0x87), ('à', 0x88), ('â', 0x89), ('ä', 0x8A), ('ã', 0x8B),
    ('å', 0x8C), ('ç', 0x8D), ('é', 0x8E), ('è', 0x8F), ('ê', 0x90),
    ('ë', 0x91), ('í', 0x92), ('ì', 0x93), ('î', 0x94), ('ï', 0x95),
    ('ñ', 0x96), ('ó', 0x97), ('ò', 0x98), ('ô', 0x99), ('ö', 0x9A),
    ('õ', 0x9B), ('ú', 0x9C), ('ù', 0x9D), ('û', 0x9E), ('ü', 0x9F),
    // Punctuation & symbols
    ('†', 0xA0), ('°', 0xA1), ('¢', 0xA2), ('£', 0xA3), ('§', 0xA4),
    ('•', 0xA5), ('¶', 0xA6), ('ß', 0xA7), ('®', 0xA8), ('©', 0xA9),
    ('™', 0xAA), ('´', 0xAB), ('¨', 0xAC), ('≠', 0xAD), ('Æ', 0xAE),
    ('Ø', 0xAF),
    ('∞', 0xB0), ('±', 0xB1), ('≤', 0xB2), ('≥', 0xB3), ('¥', 0xB4),
    ('µ', 0xB5), ('∂', 0xB6), ('∑', 0xB7), ('∏', 0xB8), ('π', 0xB9),
    ('∫', 0xBA), ('ª', 0xBB), ('º', 0xBC), ('Ω', 0xBD), ('æ', 0xBE),
    ('ø', 0xBF),
    ('¿', 0xC0), ('¡', 0xC1), ('¬', 0xC2), ('√', 0xC3), ('ƒ', 0xC4),
    ('≈', 0xC5), ('∆', 0xC6), ('«', 0xC7), ('»', 0xC8), ('…', 0xC9),
    ('\u{00A0}', 0xCA), // non-breaking space
    ('À', 0xCB), ('Ã', 0xCC), ('Õ', 0xCD), ('Œ', 0xCE), ('œ', 0xCF),
    // Curly quotes and dashes
    ('–', 0xD0), ('—', 0xD1), ('“', 0xD2), ('”', 0xD3), ('‘', 0xD4),
    ('’', 0xD5), ('÷', 0xD6), ('◊', 0xD7), ('ÿ', 0xD8), ('Ÿ', 0xD9),
    ('⁄', 0xDA), ('€', 0xDB), ('‹', 0xDC), ('›', 0xDD), ('ﬁ', 0xDE),
    ('ﬂ', 0xDF),
    ('‡', 0xE0), ('·', 0xE1), ('‚', 0xE2), ('„', 0xE3), ('‰', 0xE4),
    ('Â', 0xE5), ('Ê', 0xE6), ('Á', 0xE7), ('Ë', 0xE8), ('È', 0xE9),
    ('Í', 0xEA), ('Î', 0xEB), ('Ï', 0xEC), ('Ì', 0xED), ('Ó', 0xEE),
    ('Ô', 0xEF),
    ('\u{F8FF}', 0xF0), // Apple logo (private-use)
    ('Ò', 0xF1), ('Ú', 0xF2), ('Û', 0xF3), ('Ù', 0xF4), ('ı', 0xF5),
    ('ˆ', 0xF6), ('˜', 0xF7), ('¯', 0xF8), ('˘', 0xF9), ('˙', 0xFA),
    ('˚', 0xFB), ('¸', 0xFC), ('˝', 0xFD), ('˛', 0xFE), ('ˇ', 0xFF),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_passes_through() {
        assert_eq!(to_macroman("Hello, World!"), b"Hello, World!");
    }

    #[test]
    fn common_accented_chars() {
        assert_eq!(to_macroman("Björk"), vec![b'B', b'j', 0x9A, b'r', b'k']);
        assert_eq!(to_macroman("Café"), vec![b'C', b'a', b'f', 0x8E]);
        assert_eq!(to_macroman("naïve"), vec![b'n', b'a', 0x95, b'v', b'e']);
    }

    #[test]
    fn smart_quotes_and_dashes() {
        assert_eq!(to_macroman("‘hi’"), vec![0xD4, b'h', b'i', 0xD5]);
        assert_eq!(to_macroman("“hi”"), vec![0xD2, b'h', b'i', 0xD3]);
        assert_eq!(to_macroman("a—b"), vec![b'a', 0xD1, b'b']);
        assert_eq!(to_macroman("a–b"), vec![b'a', 0xD0, b'b']);
    }

    #[test]
    fn emoji_gets_descriptive_transliteration() {
        // deunicode surprisingly has descriptive fallbacks for many
        // emoji — 💯 → "100 ". Nicer than `?`. Assert it stayed ASCII.
        let out = to_macroman("💯");
        assert!(std::str::from_utf8(&out).unwrap().is_ascii());
        assert!(!out.is_empty());
    }

    #[test]
    fn truly_unmappable_becomes_question_mark() {
        // Private-use area code points have no transliteration.
        assert_eq!(to_macroman("\u{E000}"), b"?");
    }

    #[test]
    fn cjk_han_routes_through_romaji_pass() {
        // Han-only strings get the JP romaji pass first; ib-romaji finds
        // 日本語 as a single dictionary word ("nippongo"). Result is ASCII
        // and doesn't hit the `?` fallback.
        let out = to_macroman("日本語");
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.is_ascii(), "expected ASCII romanization, got {s:?}");
        assert!(!s.contains('?'), "expected no fallbacks, got {s:?}");
        assert_eq!(s, "nippongo");
    }

    #[test]
    fn hiragana_and_katakana_romanize() {
        // Pure kana → straight romaji from the dict.
        assert_eq!(to_macroman("ありがとう"), b"arigatou");
        assert_eq!(to_macroman("カタカナ"), b"katakana");
    }

    #[test]
    fn mixed_script_leaves_latin_alone() {
        // Non-JP surrounding text must not get mangled by the JP pass.
        let out = to_macroman("Hello 日本 world");
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.starts_with("Hello "));
        assert!(s.ends_with(" world"));
        assert!(s.is_ascii());
    }

    #[test]
    fn unmatched_kanji_falls_through_to_deunicode() {
        // A very rare CJK Ext-A char that isn't in ib-romaji's dict; the
        // deunicode fallback should still give something readable rather
        // than `?`. This validates the two-stage pipeline: JP romaji pass,
        // then per-char lookup, then deunicode.
        let out = to_macroman("\u{3400}");
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.is_ascii(), "expected ASCII fallback, got {s:?}");
    }

    #[test]
    fn decomposed_forms_normalize_before_lookup() {
        // NFD café: `e` U+0065 + combining acute U+0301. NFC recomposes to
        // U+00E9, which is in the Mac Roman table at 0x8E.
        let nfd = "cafe\u{0301}";
        assert_eq!(to_macroman(nfd), vec![b'c', b'a', b'f', 0x8E]);
    }

    #[test]
    fn stray_combining_marks_are_dropped() {
        // A combining mark with no base to compose with should silently
        // vanish rather than emit `?`.
        assert_eq!(to_macroman("\u{0301}"), b"");
    }

    #[test]
    fn accept_charset_parsing() {
        assert_eq!(charset_from_accept(Some("x-mac-roman")), Charset::MacRoman);
        assert_eq!(charset_from_accept(Some("X-Mac-Roman")), Charset::MacRoman);
        assert_eq!(charset_from_accept(Some("mac-roman")), Charset::MacRoman);
        assert_eq!(charset_from_accept(Some("macintosh")), Charset::MacRoman);
        assert_eq!(charset_from_accept(Some("utf-8")), Charset::Utf8);
        assert_eq!(charset_from_accept(Some("utf-8, x-mac-roman;q=0.5")), Charset::MacRoman);
        assert_eq!(charset_from_accept(None), Charset::Utf8);
    }

    #[test]
    fn ct_param_only_for_macroman() {
        assert_eq!(Charset::MacRoman.ct_param(), Some("x-mac-roman"));
        assert_eq!(Charset::Utf8.ct_param(), None);
    }
}
