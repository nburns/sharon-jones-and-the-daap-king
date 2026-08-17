# sharon-jones-and-the-daap-king

Serve a music library over DAAP so classic iTunes clients (iTunes 4.x on
System 7 through modern iTunes) can browse and stream it over the LAN.
The server advertises itself over mDNS/Bonjour and appears under
**Shared** in the sidebar.

Multiple backends implement the `MediaSource` trait; the CLI wires one of
them into the DAAP server:

| Backend    | Crate                       | Notes                                      |
| ---------- | --------------------------- | ------------------------------------------ |
| Filesystem | `media-source-fs`           | Scans a directory, tags via `lofty`.       |
| DLNA/UPnP  | `media-source-dlna`         | SSDP discovery, on-disk catalogue cache.   |
| Subsonic   | `media-source-subsonic`     | Navidrome, Airsonic, Gonic, etc.           |

## Quick start

```
# Filesystem
cargo run -p citunes-cli --release -- --hostname music-box fs --music ~/Music

# DLNA — auto-discover a UPnP MediaServer on the LAN
cargo run -p citunes-cli --release -- --hostname music-box dlna

# Subsonic
SUBSONIC_API_KEY=... cargo run -p citunes-cli --release -- \
  --hostname music-box subsonic -u http://navidrome.local:4533
```

## Naming: `--name` vs `--hostname`

These are two different DNS-SD fields and the CLI keeps them separate:

- `--name` is the **service instance name** — what iTunes shows under
  **Shared**. It is free-form UTF-8 (RFC 6763 §4.1.1), so
  `--name "Nick's Music"` is fine.
- `--hostname` is the **DNS label** the SRV record points at, published
  as `<hostname>.local.`. It must be ASCII alphanumeric plus `-`.

Everywhere except macOS the server publishes its own host record, so
`--hostname` is required (unless you pass `--no-mdns`). Pick something
unique to this server: if you reuse a name another responder on the box
already defends — most importantly the system hostname, which avahi
owns — the two will fight over it and one will get renamed.

On macOS the system mDNSResponder supplies the host record and there is
no way to override it, so `--hostname` is accepted and ignored there.

`ffmpeg` / `ffprobe` must be on `PATH` for transcoding (MP3 for iTunes
< 4.5, ALAC for lossless sources on newer clients).

## Album art

Backends surface raw image bytes via `MediaSource::artwork(...)`; the
server owns decode, resize, and format negotiation:

- **Modern clients** get JPEG (Lanczos3 resize to the client-requested
  `mw × mh`, q=85), cached in-process by `(track_id, w, h)`.
- **Classic Mac clients** that advertise `Accept: image/x-pict` get a
  PICT v2 response instead — 8-bit indexed against the Mac System
  Palette with Floyd-Steinberg dither, or 1-bit with Atkinson dither
  when `depth=1` is requested. Encoded by the in-tree `pict` crate (no
  runtime deps, big-endian PICT output). This is what lets iTunes 4.x
  on System 7 actually render album art in the shared-library view.

## DLNA: picking a good root

Many DLNA servers expose a noisy top-level hierarchy — multiple parallel
views of the same library (All Artists, By Album, By Genre, By Folder,
Recently Added, …). If you point at the default root you'll see every
album appear once per view in the source list, plus non-music
containers like Photos and Video.

Pass `--root <ObjectID>` to jump straight into a single audio subtree.
On Plex under Music, good choices are:

- **`By Album`** — flat list of albums, one entry each, no dupes. Best
  match for the iTunes-4 "shared library" UX where the sidebar is a
  browsable album list.
- **`All Artists`** — one entry per artist. Fewer sidebar rows but
  changes the mental model from albums to artists.

### Finding a good root ID for your server

1. List the servers on your LAN and note the ContentDirectory URL:

   ```
   cargo run -p citunes-cli --release -- dlna-list
   ```

2. Browse from the root (`ObjectID=0`) with a raw SOAP `Browse` and
   look at what containers your server exposes. Replace the URL with
   the ContentDirectory `control.xml` for your server (visible via
   `curl` on the device-description URL) and repeat with successively
   deeper `ObjectID`s until you land on an audio-only subtree:

   ```sh
   curl -s -X POST 'http://<server>:<port>/ContentDirectory/.../control.xml' \
     -H 'Content-Type: text/xml; charset="utf-8"' \
     -H 'SOAPAction: "urn:schemas-upnp-org:service:ContentDirectory:1#Browse"' \
     --data-binary '<?xml version="1.0"?>
       <s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"
                   s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
         <s:Body>
           <u:Browse xmlns:u="urn:schemas-upnp-org:service:ContentDirectory:1">
             <ObjectID>0</ObjectID>
             <BrowseFlag>BrowseDirectChildren</BrowseFlag>
             <Filter>*</Filter>
             <StartingIndex>0</StartingIndex>
             <RequestedCount>0</RequestedCount>
             <SortCriteria></SortCriteria>
           </u:Browse>
         </s:Body>
       </s:Envelope>'
   ```

   Each result is a `<container id="...">` with a `<dc:title>`. Pick the
   id of a container that holds only what you want, then start the
   server with `--root <that-id>`.

3. If you change `--root`, delete the on-disk cache (default
   `/tmp/citunes-dlna-cache/`) or pass `--no-cache` — the cache is
   keyed per (server URL, root) so a fresh id triggers a rebrowse
   automatically, but stale entries can accumulate.

## Development

```
cargo build
cargo test
```

See `todos.md` for the backlog.
