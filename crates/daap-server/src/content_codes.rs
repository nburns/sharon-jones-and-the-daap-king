//! /content-codes handler: a dictionary iTunes uses to know each DMAP tag's
//! long name and value type. iTunes 4 refuses to load a library from a
//! server that doesn't provide this endpoint.
//!
//! We enumerate every tag our response builders can emit, with the correct
//! type code. iTunes only needs entries for tags it will encounter; extras
//! are harmless.

use bytes::BytesMut;

use crate::dmap::{container, string_field, tag, u16_field, u32_field};
use crate::tags;

/// DMAP type codes (mcty). Same numbering the rest of the DAAP world uses.
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DmapType {
    Byte = 1,       // uint8
    UByte = 2,      // int8
    Short = 3,      // int16
    UShort = 4,     // uint16
    Int = 5,        // int32
    UInt = 6,       // uint32
    Long = 7,       // int64
    ULong = 8,      // uint64
    String = 9,
    Date = 10,
    Version = 11,   // packed major.minor int
    List = 12,      // container of nested DMAP fields
}

use DmapType::*;

/// Every tag we emit in any response, with its DMAP metadata. Order isn't
/// meaningful — iTunes builds a lookup table from these.
fn field_table() -> &'static [(fn() -> crate::dmap::Tag, &'static str, DmapType)] {
    &[
        // dmap.*
        (tags::status, "dmap.status", Int),
        (tags::item_id, "dmap.itemid", Int),
        (tags::item_kind, "dmap.itemkind", Byte),
        (tags::item_name, "dmap.itemname", String),
        (tags::persistent_id, "dmap.persistentid", Long),
        (tags::parent_container_id, "dmap.parentcontainerid", Int),
        (tags::container_count, "dmap.containercount", Int),
        (tags::item_count, "dmap.itemcount", Int),
        (tags::login_required, "dmap.loginrequired", Byte),
        (tags::timeout_interval, "dmap.timeoutinterval", Int),
        (tags::supports_autologout, "dmap.supportsautologout", Byte),
        (tags::auth_method, "dmap.authenticationmethod", Byte),
        (tags::supports_update, "dmap.supportsupdate", Byte),
        (tags::supports_persistent_ids, "dmap.supportspersistentids", Byte),
        (tags::supports_extensions, "dmap.supportsextensions", Byte),
        (tags::supports_browse, "dmap.supportsbrowse", Byte),
        (tags::supports_query, "dmap.supportsquery", Byte),
        (tags::supports_index, "dmap.supportsindex", Byte),
        (tags::supports_edit, "dmap.supportsedit", Byte),
        (tags::databases_count, "dmap.databasescount", Int),
        (tags::session_id, "dmap.sessionid", Int),
        (tags::server_revision, "dmap.serverrevision", Int),
        (tags::update_type, "dmap.updatetype", Byte),
        (tags::total_matched, "dmap.specifiedtotalcount", Int),
        (tags::returned_count, "dmap.returnedcount", Int),
        (tags::protocol_version, "dmap.protocolversion", Version),

        // Containers
        (tags::server_info_response, "dmap.serverinforesponse", List),
        (tags::login_response, "dmap.loginresponse", List),
        (tags::update_response, "dmap.updateresponse", List),
        (tags::databases_response, "daap.serverdatabases", List),
        (tags::items_response, "daap.databasesongs", List),
        (tags::playlists_response, "daap.databaseplaylists", List),
        (tags::playlist_songs_response, "daap.playlistsongs", List),
        (tags::listing, "dmap.listing", List),
        (tags::listing_item, "dmap.listingitem", List),

        // daap.*
        (tags::daap_protocol_version, "daap.protocolversion", Version),
        (tags::supports_extradata, "daap.supportsextradata", Short),
        (tags::supports_groups, "daap.supportsgroups", Short),
        (tags::song_album, "daap.songalbum", String),
        (tags::song_artist, "daap.songartist", String),
        (tags::song_album_artist, "daap.songalbumartist", String),
        (tags::song_genre, "daap.songgenre", String),
        (tags::song_format, "daap.songformat", String),
        (tags::song_data_kind, "daap.songdatakind", Byte),
        (tags::song_track_number, "daap.songtracknumber", Short),
        (tags::song_track_count, "daap.songtrackcount", Short),
        (tags::song_disc_number, "daap.songdiscnumber", Short),
        (tags::song_disc_count, "daap.songdisccount", Short),
        (tags::song_year, "daap.songyear", Short),
        (tags::song_time_ms, "daap.songtime", Int),
        (tags::song_bitrate, "daap.songbitrate", Short),
        (tags::song_sample_rate, "daap.songsamplerate", Int),
        (tags::song_size, "daap.songsize", Int),
        (tags::playlist_smart, "daap.baseplaylist", Byte),
        (tags::base_playlist, "daap.baseplaylist", Byte),
    ]
}

fn mccr_container() -> crate::dmap::Tag { tag("mccr") } // content-codes response
fn mdcl_container() -> crate::dmap::Tag { tag("mdcl") } // dictionary entry
fn mcnm_field() -> crate::dmap::Tag { tag("mcnm") }     // 4-char tag name
fn mcna_field() -> crate::dmap::Tag { tag("mcna") }     // long name
fn mcty_field() -> crate::dmap::Tag { tag("mcty") }     // type code

pub fn encode() -> BytesMut {
    let table = field_table();
    let mut out = BytesMut::with_capacity(4096);
    container(&mut out, mccr_container(), |b| {
        u32_field(b, tags::status(), 200);
        for (tag_fn, long_name, ty) in table {
            container(b, mdcl_container(), |entry| {
                // mcnm value is the 4-byte tag itself.
                let t = tag_fn();
                let bytes: &[u8] = &t;
                let s = std::str::from_utf8(bytes).expect("tags are ASCII");
                string_field(entry, mcnm_field(), s);
                string_field(entry, mcna_field(), long_name);
                u16_field(entry, mcty_field(), *ty as u16);
            });
        }
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn response_starts_with_mccr_and_status_200() {
        let body = encode();
        assert_eq!(&body[0..4], b"mccr");
        // The msrv payload begins with mstt (status=200).
        assert_eq!(&body[8..12], b"mstt");
        assert_eq!(u32::from_be_bytes(body[16..20].try_into().unwrap()), 200);
    }

    #[test]
    fn container_length_matches_body() {
        let body = encode();
        let declared = u32::from_be_bytes(body[4..8].try_into().unwrap()) as usize;
        assert_eq!(declared, body.len() - 8);
    }

    #[test]
    fn declares_all_common_tags() {
        let body = encode();
        for expected in [b"minm", b"mstt", b"miid", b"asal", b"asar", b"asfm", b"mlit", b"mlcl"] {
            assert!(
                body.windows(4).any(|w| w == expected),
                "missing tag {}",
                std::str::from_utf8(expected).unwrap()
            );
        }
    }

    #[test]
    fn type_codes_are_correct_for_key_tags() {
        // Spot check: minm should be declared as String (type 9).
        let body = encode();
        // Find the mcnm=b"minm" occurrence and step back to check its mcty.
        // The layout inside each mdcl: mcnm(4)-string, mcna(varlen)-string, mcty(2)-u16.
        let mut i = 0;
        while i + 12 <= body.len() {
            if &body[i..i + 4] == b"mcnm"
                && u32::from_be_bytes(body[i + 4..i + 8].try_into().unwrap()) == 4
                && &body[i + 8..i + 12] == b"minm"
            {
                // Skip mcnm header+value (8+4), skip mcna header (8) + its value (variable).
                let mcna_off = i + 8 + 4;
                let mcna_len = u32::from_be_bytes(body[mcna_off + 4..mcna_off + 8].try_into().unwrap()) as usize;
                let mcty_off = mcna_off + 8 + mcna_len;
                assert_eq!(&body[mcty_off..mcty_off + 4], b"mcty");
                let ty = u16::from_be_bytes(body[mcty_off + 8..mcty_off + 10].try_into().unwrap());
                assert_eq!(ty, DmapType::String as u16);
                return;
            }
            i += 1;
        }
        panic!("minm entry not found");
    }
}
