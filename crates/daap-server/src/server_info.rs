//! /server-info handler: returns the top-level server capability block iTunes
//! reads on first contact.

use bytes::BytesMut;

use crate::dmap::{container, string_field, u8_field, u16_field, u32_field};
use crate::tags;

/// Which DAAP dialect to respond in. iTunes sends a Client-DAAP-Version
/// header to negotiate; we return matching protocol versions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientDialect {
    /// iTunes 4.0 - 4.1
    V1,
    /// iTunes 4.2 - 4.5
    V2,
    /// iTunes 4.6+, Music.app
    V3,
}

impl ClientDialect {
    /// Parse the Client-DAAP-Version header value.
    pub fn from_header(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            Some("1.0") => Self::V1,
            Some("2.0") => Self::V2,
            _ => Self::V3,
        }
    }

    fn mpro(self) -> u32 {
        match self {
            Self::V1 => tags::version(1, 0),
            Self::V2 => tags::version(1, 0),
            Self::V3 => tags::version(2, 10),
        }
    }

    fn apro(self) -> u32 {
        match self {
            Self::V1 => tags::version(1, 0),
            Self::V2 => tags::version(2, 0),
            Self::V3 => tags::version(3, 12),
        }
    }
}

pub struct ServerInfo<'a> {
    pub name: &'a str,
    pub database_count: u32,
    pub requires_password: bool,
    pub dialect: ClientDialect,
}

/// Build the DMAP-encoded body of a /server-info response.
pub fn encode(info: &ServerInfo<'_>) -> BytesMut {
    let mut out = BytesMut::with_capacity(256);
    container(&mut out, tags::server_info_response(), |b| {
        u32_field(b, tags::status(), 200);
        u32_field(b, tags::protocol_version(), info.dialect.mpro());
        string_field(b, tags::item_name(), info.name);

        u32_field(b, tags::daap_protocol_version(), info.dialect.apro());
        u16_field(b, tags::supports_extradata(), 7);
        u16_field(b, tags::supports_groups(), 3);

        u8_field(b, tags::supports_edit(), 0);
        u8_field(b, tags::login_required(), 1);
        u32_field(b, tags::timeout_interval(), 1800);
        u8_field(b, tags::supports_autologout(), 1);
        u8_field(
            b,
            tags::auth_method(),
            if info.requires_password { 2 } else { 0 },
        );

        u8_field(b, tags::supports_update(), 1);
        u8_field(b, tags::supports_persistent_ids(), 1);
        u8_field(b, tags::supports_extensions(), 1);
        u8_field(b, tags::supports_browse(), 1);
        u8_field(b, tags::supports_query(), 1);
        u8_field(b, tags::supports_index(), 1);

        u32_field(b, tags::databases_count(), info.database_count);

        // Sharon-jones custom capabilities. Absent from stock DAAP; iRunes
        // and other cooperating clients look for `shrf` to gate features
        // before sending non-standard params like `query=` on items.
        u32_field(
            b,
            tags::sharon_features(),
            tags::SHRF_QUERY | tags::SHRF_SORT,
        );
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find_field(body: &[u8], t: &[u8; 4]) -> Option<(u32, usize)> {
        // Naive linear scan over top-level DMAP fields inside the msrv container.
        // Assumes body starts with `msrv <len>` header — strip it first.
        assert_eq!(&body[0..4], b"msrv", "expected msrv container");
        let mut i = 8; // skip container header
        while i + 8 <= body.len() {
            let tag = &body[i..i + 4];
            let len = u32::from_be_bytes(body[i + 4..i + 8].try_into().unwrap()) as usize;
            if tag == t {
                return Some((len as u32, i + 8));
            }
            i += 8 + len;
        }
        None
    }

    fn read_u32(body: &[u8], t: &[u8; 4]) -> u32 {
        let (len, offset) = find_field(body, t).expect("field present");
        assert_eq!(len, 4);
        u32::from_be_bytes(body[offset..offset + 4].try_into().unwrap())
    }

    fn read_u8(body: &[u8], t: &[u8; 4]) -> u8 {
        let (len, offset) = find_field(body, t).expect("field present");
        assert_eq!(len, 1);
        body[offset]
    }

    fn read_string<'a>(body: &'a [u8], t: &[u8; 4]) -> &'a str {
        let (len, offset) = find_field(body, t).expect("field present");
        std::str::from_utf8(&body[offset..offset + len as usize]).unwrap()
    }

    #[test]
    fn encodes_status_200() {
        let info = ServerInfo {
            name: "Test",
            database_count: 1,
            requires_password: false,
            dialect: ClientDialect::V3,
        };
        let body = encode(&info);
        assert_eq!(read_u32(&body, b"mstt"), 200);
    }

    #[test]
    fn encodes_server_name() {
        let info = ServerInfo {
            name: "My Music",
            database_count: 1,
            requires_password: false,
            dialect: ClientDialect::V3,
        };
        let body = encode(&info);
        assert_eq!(read_string(&body, b"minm"), "My Music");
    }

    #[test]
    fn auth_method_reflects_password_flag() {
        let with = ServerInfo {
            name: "x",
            database_count: 0,
            requires_password: true,
            dialect: ClientDialect::V3,
        };
        let without = ServerInfo {
            name: "x",
            database_count: 0,
            requires_password: false,
            dialect: ClientDialect::V3,
        };
        assert_eq!(read_u8(&encode(&with), b"msau"), 2);
        assert_eq!(read_u8(&encode(&without), b"msau"), 0);
    }

    #[test]
    fn databases_count_propagates() {
        let info = ServerInfo {
            name: "x",
            database_count: 42,
            requires_password: false,
            dialect: ClientDialect::V3,
        };
        assert_eq!(read_u32(&encode(&info), b"msdc"), 42);
    }

    #[test]
    fn dialect_v1_reports_protocol_1_0() {
        let info = ServerInfo {
            name: "x",
            database_count: 1,
            requires_password: false,
            dialect: ClientDialect::V1,
        };
        let body = encode(&info);
        assert_eq!(read_u32(&body, b"mpro"), 0x0001_0000);
        assert_eq!(read_u32(&body, b"apro"), 0x0001_0000);
    }

    #[test]
    fn dialect_v3_reports_modern_version() {
        let info = ServerInfo {
            name: "x",
            database_count: 1,
            requires_password: false,
            dialect: ClientDialect::V3,
        };
        let body = encode(&info);
        assert_eq!(read_u32(&body, b"mpro"), 0x0002_000A);
        assert_eq!(read_u32(&body, b"apro"), 0x0003_000C);
    }

    #[test]
    fn container_length_matches_body() {
        let info = ServerInfo {
            name: "x",
            database_count: 0,
            requires_password: false,
            dialect: ClientDialect::V3,
        };
        let body = encode(&info);
        let declared = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
        assert_eq!(
            declared,
            body.len() - 8,
            "container length should equal body size"
        );
    }

    #[test]
    fn advertises_sharon_query_capability() {
        let info = ServerInfo {
            name: "x",
            database_count: 1,
            requires_password: false,
            dialect: ClientDialect::V3,
        };
        let body = encode(&info);
        let bits = read_u32(&body, b"shrf");
        assert_eq!(bits & tags::SHRF_QUERY, tags::SHRF_QUERY);
        assert_eq!(bits & tags::SHRF_SORT, tags::SHRF_SORT);
    }

    #[test]
    fn client_dialect_parses_header_values() {
        assert_eq!(ClientDialect::from_header(Some("1.0")), ClientDialect::V1);
        assert_eq!(ClientDialect::from_header(Some("2.0")), ClientDialect::V2);
        assert_eq!(ClientDialect::from_header(Some("3.0")), ClientDialect::V3);
        assert_eq!(ClientDialect::from_header(Some(" 1.0 ")), ClientDialect::V1);
        assert_eq!(ClientDialect::from_header(None), ClientDialect::V3);
    }
}
