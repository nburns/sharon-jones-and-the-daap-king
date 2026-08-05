//! Character-set conversion for DMAP string fields.
//!
//! DAAP historically uses UTF-8 for string tag values. Classic-era clients
//! (Mac II speaking a 68k-native DAAP client) expect MacRoman. When a client
//! sends `Accept-Charset: x-mac-roman`, we run string fields through
//! [`Charset::MacRoman`] before writing them into the DMAP payload.
//!
//! Encoding notes:
//!   * ASCII 0x20..0x7E passes through unchanged (MacRoman is ASCII-superset).
//!   * Common Latin-supplement code points map to MacRoman positions per
//!     Apple's canonical table (Inside Macintosh: Text, Appendix E).
//!   * Anything unmappable → `?` (ASCII 0x3F).
//!
//! We don't ship the full 256-entry Unicode↔MacRoman table — just the ~90
//! code points that show up in real-world artist/track/album names. Extend
//! as gaps show up.

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

/// Lossy UTF-8 → MacRoman transliteration. Unmappable chars become '?'.
pub fn to_macroman(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for c in s.chars() {
        let byte = if (c as u32) < 0x80 {
            c as u8
        } else {
            LATIN_EXT
                .iter()
                .find(|(u, _)| *u == c)
                .map(|(_, b)| *b)
                .unwrap_or(b'?')
        };
        out.push(byte);
    }
    out
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
    fn unmappable_becomes_question_mark() {
        assert_eq!(to_macroman("💯"), b"?");
        assert_eq!(to_macroman("日本語"), b"???");
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
