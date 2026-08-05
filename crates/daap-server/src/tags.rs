//! DMAP tag constants.
//!
//! Naming convention: SCREAMING_SNAKE_CASE of the semantic name from the
//! reverse-engineered DAAP spec, with the 4-char tag on the same line for
//! quick greppability. Cross-reference:
//!   owntone/src/dmap_fields.gperf

use crate::dmap::{tag, Tag};

// ---- dmap.* (protocol core) ----
pub fn status() -> Tag { tag("mstt") }                   // int32,  dmap.status
pub fn protocol_version() -> Tag { tag("mpro") }         // version, dmap.protocolversion
pub fn item_name() -> Tag { tag("minm") }                // string, dmap.itemname
pub fn item_id() -> Tag { tag("miid") }                  // int32,  dmap.itemid
pub fn item_kind() -> Tag { tag("mikd") }                // uint8,  dmap.itemkind (2=music)
pub fn persistent_id() -> Tag { tag("mper") }            // int64,  dmap.persistentid
pub fn parent_container_id() -> Tag { tag("mpco") }      // int32,  dmap.parentcontainerid
pub fn container_count() -> Tag { tag("mctc") }          // int32,  dmap.containercount
pub fn item_count() -> Tag { tag("mimc") }               // int32,  dmap.itemcount
pub fn login_required() -> Tag { tag("mslr") }           // uint8,  dmap.loginrequired
pub fn timeout_interval() -> Tag { tag("mstm") }         // int32,  dmap.timeoutinterval
pub fn supports_autologout() -> Tag { tag("msal") }      // uint8,  dmap.supportsautologout
pub fn auth_method() -> Tag { tag("msau") }              // uint8,  dmap.authenticationmethod
pub fn supports_update() -> Tag { tag("msup") }          // uint8,  dmap.supportsupdate
pub fn supports_persistent_ids() -> Tag { tag("mspi") }  // uint8,  dmap.supportspersistentids
pub fn supports_extensions() -> Tag { tag("msex") }      // uint8,  dmap.supportsextensions
pub fn supports_browse() -> Tag { tag("msbr") }          // uint8,  dmap.supportsbrowse
pub fn supports_query() -> Tag { tag("msqy") }           // uint8,  dmap.supportsquery
pub fn supports_index() -> Tag { tag("msix") }           // uint8,  dmap.supportsindex
pub fn supports_edit() -> Tag { tag("msed") }            // uint8,  dmap.supportsedit
pub fn databases_count() -> Tag { tag("msdc") }          // int32,  dmap.databasescount
pub fn session_id() -> Tag { tag("mlid") }               // int32,  dmap.sessionid
pub fn server_revision() -> Tag { tag("musr") }          // int32,  dmap.serverrevision
pub fn update_type() -> Tag { tag("muty") }              // uint8,  dmap.updatetype
pub fn total_matched() -> Tag { tag("mtco") }            // int32,  dmap.specifiedtotalcount
pub fn returned_count() -> Tag { tag("mrco") }           // int32,  dmap.returnedcount

// ---- Container tags ----
pub fn server_info_response() -> Tag { tag("msrv") }     // container, dmap.serverinforesponse
pub fn login_response() -> Tag { tag("mlog") }           // container, dmap.loginresponse
pub fn update_response() -> Tag { tag("mupd") }          // container, dmap.updateresponse
pub fn databases_response() -> Tag { tag("avdb") }       // container, daap.serverdatabases
pub fn listing() -> Tag { tag("mlcl") }                  // container, dmap.listing
pub fn listing_item() -> Tag { tag("mlit") }             // container, dmap.listingitem
pub fn playlists_response() -> Tag { tag("aply") }       // container, daap.databaseplaylists
pub fn items_response() -> Tag { tag("adbs") }           // container, daap.databasesongs
pub fn playlist_songs_response() -> Tag { tag("apso") }  // container, daap.playlistsongs

// ---- daap.* (audio protocol) ----
pub fn daap_protocol_version() -> Tag { tag("apro") }    // version, daap.protocolversion
pub fn supports_extradata() -> Tag { tag("ated") }       // int16,  daap.supportsextradata
pub fn supports_groups() -> Tag { tag("asgr") }          // int16,  daap.supportsgroups
pub fn song_album() -> Tag { tag("asal") }               // string, daap.songalbum
pub fn song_artist() -> Tag { tag("asar") }              // string, daap.songartist
pub fn song_album_artist() -> Tag { tag("asaa") }        // string, daap.songalbumartist
pub fn song_genre() -> Tag { tag("asgn") }               // string, daap.songgenre
pub fn song_format() -> Tag { tag("asfm") }              // string, daap.songformat  (e.g. "mp3")
pub fn song_data_kind() -> Tag { tag("asdk") }           // uint8,  daap.songdatakind
pub fn song_track_number() -> Tag { tag("astn") }        // int16,  daap.songtracknumber
pub fn song_track_count() -> Tag { tag("astc") }         // int16,  daap.songtrackcount
pub fn song_disc_number() -> Tag { tag("asdn") }         // int16,  daap.songdiscnumber
pub fn song_disc_count() -> Tag { tag("asdc") }          // int16,  daap.songdisccount
pub fn song_year() -> Tag { tag("asyr") }                // int16,  daap.songyear
pub fn song_time_ms() -> Tag { tag("astm") }             // int32,  daap.songtime
pub fn song_bitrate() -> Tag { tag("asbr") }             // int16,  daap.songbitrate
pub fn song_sample_rate() -> Tag { tag("assr") }         // int32,  daap.songsamplerate
pub fn song_size() -> Tag { tag("assz") }                // int32,  daap.songsize
pub fn playlist_smart() -> Tag { tag("apsm") }           // uint8,  daap.baseplaylist? / smart flag
pub fn base_playlist() -> Tag { tag("abpl") }            // uint8,  daap.baseplaylist (1=main library)

/// Encode a DMAP `version` int: major.minor packed as (major<<16) | minor.
pub const fn version(major: u16, minor: u16) -> u32 {
    ((major as u32) << 16) | (minor as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tag_bytes_are_ascii_lowercase() {
        for t in [status(), item_name(), server_info_response(), login_response(),
                  update_response(), databases_response(), items_response(),
                  playlists_response(), playlist_songs_response(), song_artist()]
        {
            for b in t {
                assert!(b.is_ascii_lowercase() || b.is_ascii_uppercase() || b.is_ascii_digit(),
                    "tag byte {b:?} not ASCII alnum");
            }
        }
    }

    #[test]
    fn version_packs_correctly() {
        assert_eq!(version(1, 0), 0x00010000);
        assert_eq!(version(3, 12), 0x0003000C);
    }
}
