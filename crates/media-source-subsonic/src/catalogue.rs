//! Build a flat track catalogue from a Subsonic server.
//!
//! Walk: getArtists → per artist getArtist (albums) → per album getAlbum
//! (songs). Parallelize the album fetches with a small semaphore since
//! Subsonic servers can handle it but we don't want to hammer them.

use std::collections::HashMap;
use std::sync::Arc;

use futures::{stream, StreamExt};
use media_source::{AudioFormat, TrackId};
use tokio::sync::Semaphore;

use crate::client::{Client, SubsonicError};
use crate::model::{self, Song};

const ALBUM_FETCH_CONCURRENCY: usize = 8;

/// Baked catalogue held in memory. IDs are stable within a single run:
/// tracks get 1..=N, playlists get IDs starting at 2 (id 1 reserved for
/// the synthesized library playlist).
#[derive(Debug, Default)]
pub struct Catalogue {
    pub tracks: Vec<TrackEntry>,
    pub playlists: Vec<PlaylistEntry>,
    /// Map Subsonic song id → our u32 TrackId, used to resolve playlist
    /// entries after all tracks have been assigned ids.
    pub(crate) id_index: HashMap<String, TrackId>,
}

impl Catalogue {
    pub fn track_by_id(&self, id: TrackId) -> Option<&TrackEntry> {
        self.tracks.iter().find(|t| t.id == id)
    }
}

#[derive(Debug, Clone)]
pub struct TrackEntry {
    pub id: TrackId,
    /// Opaque server-side id (used for streaming/artwork).
    pub subsonic_id: String,
    /// Subsonic cover-art id (usually the album's id, occasionally per-song).
    /// None means no artwork advertised for this track.
    pub cover_art_id: Option<String>,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub track_number: Option<u16>,
    pub disc_number: Option<u16>,
    pub year: Option<u16>,
    pub duration_ms: Option<u32>,
    pub bitrate_kbps: Option<u32>,
    pub size_bytes: Option<u64>,
    pub format: AudioFormat,
}

#[derive(Debug, Clone)]
pub struct PlaylistEntry {
    pub id: u32,
    pub name: String,
    pub track_ids: Vec<TrackId>,
}

pub async fn build(client: &Arc<Client>) -> std::result::Result<Catalogue, SubsonicError> {
    let artists = client.get_artists().await?;
    let artist_count: usize = artists
        .artists
        .index
        .iter()
        .map(|i| i.artist.len())
        .sum();
    tracing::info!(artists = artist_count, "Subsonic: fetching artist detail");

    // Fan out per-artist detail fetches to collect album ids.
    let album_ids: Vec<String> = stream::iter(
        artists
            .artists
            .index
            .into_iter()
            .flat_map(|i| i.artist),
    )
    .map(|a| {
        let client = Arc::clone(client);
        async move {
            match client.get_artist(&a.id).await {
                Ok(detail) => detail
                    .artist
                    .album
                    .into_iter()
                    .map(|al| al.id)
                    .collect::<Vec<_>>(),
                Err(err) => {
                    tracing::warn!(artist = %a.name, ?err, "getArtist failed");
                    Vec::new()
                }
            }
        }
    })
    .buffer_unordered(ALBUM_FETCH_CONCURRENCY)
    .flat_map(|ids| stream::iter(ids))
    .collect()
    .await;

    tracing::info!(albums = album_ids.len(), "Subsonic: fetching album detail");

    // Per-album fetches, with an explicit semaphore to bound outstanding
    // requests even if we later collect via a different combinator.
    let sem = Arc::new(Semaphore::new(ALBUM_FETCH_CONCURRENCY));
    let songs: Vec<Song> = stream::iter(album_ids)
        .map(|album_id| {
            let client = Arc::clone(client);
            let sem = Arc::clone(&sem);
            async move {
                let _permit = sem.acquire().await.ok()?;
                match client.get_album(&album_id).await {
                    Ok(payload) => Some(payload.album.song),
                    Err(err) => {
                        tracing::warn!(album = %album_id, ?err, "getAlbum failed");
                        None
                    }
                }
            }
        })
        .buffer_unordered(ALBUM_FETCH_CONCURRENCY)
        .filter_map(|opt| async move { opt })
        .flat_map(|s| stream::iter(s))
        .collect()
        .await;

    // Bake tracks with u32 ids.
    let mut cat = Catalogue::default();
    for (idx, song) in songs.into_iter().enumerate() {
        let id = (idx + 1) as TrackId;
        cat.id_index.insert(song.id.clone(), id);
        cat.tracks.push(song_to_entry(id, song));
    }

    // Playlists — ignore failures per-entry.
    match client.get_playlists().await {
        Ok(pls) => {
            let mut next_pl_id: u32 = 2;
            for summary in pls.playlists.playlist {
                match client.get_playlist(&summary.id).await {
                    Ok(detail) => {
                        let track_ids: Vec<TrackId> = detail
                            .playlist
                            .entries
                            .iter()
                            .filter_map(|s| cat.id_index.get(&s.id).copied())
                            .collect();
                        if !track_ids.is_empty() {
                            cat.playlists.push(PlaylistEntry {
                                id: next_pl_id,
                                name: detail.playlist.name,
                                track_ids,
                            });
                            next_pl_id += 1;
                        }
                    }
                    Err(err) => tracing::warn!(playlist = %summary.name, ?err, "getPlaylist failed"),
                }
            }
        }
        Err(err) => tracing::warn!(?err, "getPlaylists failed; skipping playlist import"),
    }

    Ok(cat)
}

fn song_to_entry(id: TrackId, s: Song) -> TrackEntry {
    let format = crate::classify_content_type(s.content_type.as_deref());
    let duration_ms = s.duration.map(|secs| secs.saturating_mul(1000));
    TrackEntry {
        id,
        subsonic_id: s.id,
        cover_art_id: s.cover_art,
        title: s.title,
        artist: s.artist,
        album: s.album,
        album_artist: s.album_artist,
        genre: s.genre,
        track_number: s.track,
        disc_number: s.disc_number,
        year: s.year,
        duration_ms,
        bitrate_kbps: s.bit_rate,
        size_bytes: s.size,
        format,
    }
}
