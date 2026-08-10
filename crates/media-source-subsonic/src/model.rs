//! Serde structs for the subset of Subsonic JSON responses we consume.
//! All Subsonic responses share a `{"subsonic-response": {...}}` envelope.

use serde::Deserialize;

#[derive(Deserialize, Debug)]
pub struct Envelope<T> {
    #[serde(rename = "subsonic-response")]
    pub response: Body<T>,
}

#[derive(Deserialize, Debug)]
pub struct Body<T> {
    pub status: String,
    #[serde(default)]
    #[allow(dead_code)]
    pub version: String,
    #[serde(rename = "type", default)]
    pub server_type: String,
    #[serde(rename = "serverVersion", default)]
    pub server_version: String,
    #[serde(default)]
    pub error: Option<ApiError>,
    #[serde(flatten)]
    pub payload: Option<T>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct ApiError {
    pub code: i32,
    pub message: String,
}

// ---- payload types ----

#[derive(Deserialize, Debug)]
pub struct Empty {}

#[derive(Deserialize, Debug)]
pub struct ArtistsPayload {
    pub artists: ArtistsIndex,
}
#[derive(Deserialize, Debug)]
pub struct ArtistsIndex {
    #[serde(default)]
    pub index: Vec<ArtistIndexEntry>,
}
#[derive(Deserialize, Debug)]
pub struct ArtistIndexEntry {
    #[serde(default)]
    pub artist: Vec<ArtistSummary>,
}
#[derive(Deserialize, Debug, Clone)]
pub struct ArtistSummary {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub album_count: Option<u32>,
}

#[derive(Deserialize, Debug)]
pub struct ArtistPayload {
    pub artist: ArtistDetail,
}
#[derive(Deserialize, Debug)]
pub struct ArtistDetail {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub album: Vec<AlbumSummary>,
}
#[derive(Deserialize, Debug, Clone)]
pub struct AlbumSummary {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub year: Option<u16>,
    #[serde(default)]
    pub genre: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct AlbumPayload {
    pub album: AlbumDetail,
}
#[derive(Deserialize, Debug)]
pub struct AlbumDetail {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub year: Option<u16>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub song: Vec<Song>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct Song {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub artist: Option<String>,
    #[serde(default)]
    pub album: Option<String>,
    #[serde(default, rename = "albumArtist")]
    pub album_artist: Option<String>,
    #[serde(default)]
    pub genre: Option<String>,
    #[serde(default)]
    pub track: Option<u16>,
    #[serde(default, rename = "discNumber")]
    pub disc_number: Option<u16>,
    #[serde(default)]
    pub year: Option<u16>,
    #[serde(default)]
    pub duration: Option<u32>, // seconds
    #[serde(default, rename = "bitRate")]
    pub bit_rate: Option<u32>,
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default, rename = "contentType")]
    pub content_type: Option<String>,
    #[serde(default)]
    pub suffix: Option<String>,
    #[serde(default, rename = "coverArt")]
    pub cover_art: Option<String>,
}

#[derive(Deserialize, Debug)]
pub struct PlaylistsPayload {
    pub playlists: PlaylistsList,
}
#[derive(Deserialize, Debug)]
pub struct PlaylistsList {
    #[serde(default)]
    pub playlist: Vec<PlaylistSummary>,
}
#[derive(Deserialize, Debug, Clone)]
pub struct PlaylistSummary {
    pub id: String,
    pub name: String,
    #[serde(default, rename = "songCount")]
    pub song_count: Option<u32>,
}

#[derive(Deserialize, Debug)]
pub struct PlaylistPayload {
    pub playlist: PlaylistDetail,
}
#[derive(Deserialize, Debug)]
pub struct PlaylistDetail {
    pub id: String,
    pub name: String,
    #[serde(default, rename = "entry")]
    pub entries: Vec<Song>,
}
