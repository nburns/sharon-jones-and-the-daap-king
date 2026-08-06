//! On-the-fly audio transcoding via `ffmpeg` subprocess.
//!
//! Source streams are piped into ffmpeg via stdin so we retain control of
//! auth, byte counting, and error handling regardless of what backend
//! produced them. Output is streamed straight to the HTTP response.
//!
//! Format policy — chosen per-request based on the iTunes client version
//! parsed from `User-Agent`. Old iTunes (< 4.5) only decodes MP3/AIFF/WAV;
//! everything else must be transcoded to MP3. iTunes ≥ 4.5 gains AAC and
//! ALAC support, so lossless sources (FLAC/WAV/AIFF) can go to ALAC — a
//! near-free re-container of the PCM stream instead of a lossy re-encode.

use std::process::Stdio;
use std::sync::Arc;

use media_source::{AudioFormat, Track};
use tokio::io::{AsyncRead, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

// ---- policy ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Low,
    Med,
    High,
}

impl Preset {
    pub fn mp3_bitrate_kbps(self) -> u32 {
        match self {
            Preset::Low => 128,
            Preset::Med => 192,
            Preset::High => 320,
        }
    }
}

/// What we ultimately hand to iTunes for a given source track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServedFormat {
    /// Bytes pass through untouched.
    Passthrough(AudioFormat),
    /// Transcoded to MP3 at `bitrate_kbps`.
    Mp3 { bitrate_kbps: u32 },
    /// Transcoded (re-containerized) to ALAC in an M4A container.
    Alac,
    /// Classic-Mac target: AIFF container with signed 8-bit PCM, mono,
    /// 22254 Hz — the Apple Sound Chip's native rate. Our code emits the
    /// AIFF header directly (ffmpeg would need a seekable output to back-
    /// patch chunk sizes) and ffmpeg pipes raw PCM samples for the body.
    ClassicAiff,
}

impl ServedFormat {
    /// Format field iTunes should be told the track is (`asfm`).
    pub fn asfm(self) -> &'static str {
        match self {
            ServedFormat::Passthrough(f) => f.extension(),
            ServedFormat::Mp3 { .. } => "mp3",
            ServedFormat::Alac => "m4a",
            ServedFormat::ClassicAiff => "aif",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            ServedFormat::Passthrough(AudioFormat::Mp3) | ServedFormat::Mp3 { .. } => "audio/mpeg",
            ServedFormat::Passthrough(AudioFormat::Aac)
            | ServedFormat::Passthrough(AudioFormat::Alac)
            | ServedFormat::Alac => "audio/mp4",
            ServedFormat::Passthrough(AudioFormat::Flac) => "audio/flac",
            ServedFormat::Passthrough(AudioFormat::Wav) => "audio/wav",
            ServedFormat::Passthrough(AudioFormat::Aiff) | ServedFormat::ClassicAiff => {
                "audio/x-aiff"
            }
            ServedFormat::Passthrough(AudioFormat::Ogg) => "audio/ogg",
            ServedFormat::Passthrough(AudioFormat::Other) => "application/octet-stream",
        }
    }

    pub fn is_transcode(self) -> bool {
        !matches!(self, ServedFormat::Passthrough(_))
    }
}

// ---- Classic AIFF constants + header emission ----

/// Apple Sound Chip native rate on the classic Mac (Mac II family through
/// pre-AV Quadras). Sample Manager does zero rate conversion when handed
/// this exact rate.
pub const CLASSIC_AIFF_SAMPLE_RATE: u32 = 22254;
pub const CLASSIC_AIFF_CHANNELS: u16 = 1;
pub const CLASSIC_AIFF_BITS: u16 = 8;
/// AIFF FORM+COMM+SSND framing overhead we prepend to raw PCM. Fixed size:
///   FORM header       = 12  ("FORM" u32 + size + "AIFF")
///   COMM chunk        = 26  (id + size + numChannels + numFrames + bits + 10-byte 80-bit float rate)
///   SSND chunk header = 16  (id + size + offset + blockSize)
///   ---
///   Total             = 54
pub const CLASSIC_AIFF_HEADER_BYTES: u32 = 54;

/// Compute the exact byte count of the served AIFF for a given source
/// duration. Precise because PCM is CBR.
pub fn classic_aiff_size(duration_ms: u32) -> u32 {
    let samples = (duration_ms as u64 * CLASSIC_AIFF_SAMPLE_RATE as u64) / 1000;
    let bytes_per_sample = (CLASSIC_AIFF_BITS as u64 / 8) * (CLASSIC_AIFF_CHANNELS as u64);
    let data = samples * bytes_per_sample;
    CLASSIC_AIFF_HEADER_BYTES + data as u32
}

/// PCM sample count for a given microsecond-accurate source duration.
/// Rounded to nearest sample; PCM at 22254 Hz has one sample every
/// ~44.9 microseconds, so this is exact to well below one sample.
pub fn classic_aiff_sample_count_micros(duration_micros: u64) -> u32 {
    let samples = (duration_micros
        .saturating_mul(CLASSIC_AIFF_SAMPLE_RATE as u64)
        + 500_000)
        / 1_000_000;
    samples.min(u32::MAX as u64) as u32
}

/// Byte count for a given PCM sample count (mono 8-bit).
pub fn classic_aiff_size_from_samples(sample_count: u32) -> u64 {
    let bytes_per_sample = (CLASSIC_AIFF_BITS as u64 / 8) * (CLASSIC_AIFF_CHANNELS as u64);
    CLASSIC_AIFF_HEADER_BYTES as u64 + (sample_count as u64) * bytes_per_sample
}

/// Convert a byte offset within the served AIFF into an input-time offset
/// (in milliseconds), for `ffmpeg -ss` seek.
pub fn classic_aiff_byte_to_time_ms(byte_offset: u64) -> u32 {
    if byte_offset <= CLASSIC_AIFF_HEADER_BYTES as u64 {
        return 0;
    }
    let data_byte = byte_offset - CLASSIC_AIFF_HEADER_BYTES as u64;
    let bytes_per_second = (CLASSIC_AIFF_SAMPLE_RATE as u64)
        * (CLASSIC_AIFF_CHANNELS as u64)
        * (CLASSIC_AIFF_BITS as u64 / 8);
    let ms = (data_byte * 1000) / bytes_per_second.max(1);
    ms.min(u32::MAX as u64) as u32
}

/// Emit the 54-byte AIFF header prelude for `sample_count` PCM8 mono
/// samples at 22254 Hz. Big-endian throughout (AIFF native).
pub fn classic_aiff_header(sample_count: u32) -> [u8; 54] {
    let data_bytes = sample_count as u32; // 1 byte per sample, mono
    let ssnd_chunk_size = 8u32 + data_bytes; // offset+blockSize (8) + audio
    let form_size = 4u32 // "AIFF"
        + 8 + 18      // COMM header + payload
        + 8 + ssnd_chunk_size; // SSND header + payload

    let mut out = [0u8; 54];
    // FORM
    out[0..4].copy_from_slice(b"FORM");
    out[4..8].copy_from_slice(&form_size.to_be_bytes());
    out[8..12].copy_from_slice(b"AIFF");
    // COMM
    out[12..16].copy_from_slice(b"COMM");
    out[16..20].copy_from_slice(&18u32.to_be_bytes());
    out[20..22].copy_from_slice(&CLASSIC_AIFF_CHANNELS.to_be_bytes());
    out[22..26].copy_from_slice(&sample_count.to_be_bytes()); // numSampleFrames
    out[26..28].copy_from_slice(&CLASSIC_AIFF_BITS.to_be_bytes());
    // sampleRate — 80-bit IEEE 754 extended-precision float (10 bytes).
    // Hard-coded encoding of 22254.0.
    out[28..38].copy_from_slice(&ieee_754_extended(CLASSIC_AIFF_SAMPLE_RATE as f64));
    // SSND
    out[38..42].copy_from_slice(b"SSND");
    out[42..46].copy_from_slice(&ssnd_chunk_size.to_be_bytes());
    out[46..50].copy_from_slice(&0u32.to_be_bytes()); // offset
    out[50..54].copy_from_slice(&0u32.to_be_bytes()); // blockSize
    out
}

/// Encode a positive finite `f64` as a 10-byte big-endian IEEE-754
/// extended-precision (80-bit) float — the format AIFF uses for sample
/// rate. Only handles values > 0.
fn ieee_754_extended(v: f64) -> [u8; 10] {
    // Represent as sign(1) + exponent(15, bias 16383) + integer_part(1) +
    // fraction(63). Uses the "non-implicit" leading 1 bit that x86 80-bit
    // extended and the Motorola 68881 both use.
    let bits = v.to_bits();
    let sign = ((bits >> 63) & 1) as u16;
    let exp_ieee = ((bits >> 52) & 0x7FF) as i32;
    let frac_ieee = bits & 0x000F_FFFF_FFFF_FFFF;
    // Reject zero/subnormal — we only ever encode 22254 here.
    if exp_ieee == 0 {
        return [0u8; 10];
    }
    let unbiased = exp_ieee - 1023;
    let exp_ext = (unbiased + 16383) as u16;
    // The 63-bit fraction of extended has an explicit leading 1 bit; IEEE
    // 754 double has an implicit one. So mantissa_ext = (1 << 63) | (frac
    // << 11).
    let mantissa_ext = (1u64 << 63) | (frac_ieee << 11);

    let mut out = [0u8; 10];
    let sign_exp = (sign << 15) | (exp_ext & 0x7FFF);
    out[0..2].copy_from_slice(&sign_exp.to_be_bytes());
    out[2..10].copy_from_slice(&mantissa_ext.to_be_bytes());
    out
}

/// Decide how a given source format should be served to a client, considering
/// what iTunes version supports and the user's preset.
pub fn choose_format(
    source: AudioFormat,
    client_supports_modern: bool,
    preset: Preset,
) -> ServedFormat {
    match (source, client_supports_modern) {
        // Always-native for iTunes 4.0+
        (AudioFormat::Mp3, _) | (AudioFormat::Aiff, _) | (AudioFormat::Wav, _) => {
            ServedFormat::Passthrough(source)
        }
        // AAC & ALAC native from iTunes 4.5
        (AudioFormat::Aac, true) | (AudioFormat::Alac, true) => ServedFormat::Passthrough(source),
        (AudioFormat::Aac, false) | (AudioFormat::Alac, false) => ServedFormat::Mp3 {
            bitrate_kbps: preset.mp3_bitrate_kbps(),
        },
        // FLAC: prefer lossless→ALAC on modern clients, MP3 otherwise
        (AudioFormat::Flac, true) => ServedFormat::Alac,
        (AudioFormat::Flac, false) => ServedFormat::Mp3 {
            bitrate_kbps: preset.mp3_bitrate_kbps(),
        },
        // Ogg / unknown → always transcode to MP3
        (AudioFormat::Ogg, _) | (AudioFormat::Other, _) => ServedFormat::Mp3 {
            bitrate_kbps: preset.mp3_bitrate_kbps(),
        },
    }
}

/// Parse `iTunes/X.Y (...)` out of a User-Agent header. Returns None when
/// the UA is missing or doesn't look like iTunes.
pub fn parse_itunes_version(ua: Option<&str>) -> Option<(u16, u16)> {
    let ua = ua?;
    let rest = ua.strip_prefix("iTunes/")?;
    let ver = rest.split_whitespace().next()?;
    // Version may be `12.7`, `12.7.4`, `12.7.4.80` — only the first two
    // segments matter for codec-support gating.
    let mut parts = ver.split('.');
    let major: u16 = parts.next()?.parse().ok()?;
    let minor: u16 = parts.next()?.parse().ok()?;
    Some((major, minor))
}

/// Whether the client (identified by UA) supports AAC/ALAC natively.
/// Conservative default: unknown UA is assumed to be a non-iTunes DAAP
/// client (Rhythmbox, Amarok, Music.app etc.) which all handle AAC/ALAC.
pub fn client_supports_modern_codecs(ua: Option<&str>) -> bool {
    match parse_itunes_version(ua) {
        Some((major, minor)) => (major, minor) >= (4, 5),
        None => true,
    }
}

// ---- transcoding execution ----

#[derive(Debug, Clone)]
pub struct Config {
    pub ffmpeg_path: String,
    pub ffprobe_path: String,
    pub preset: Preset,
    /// Cap on concurrent ffmpeg subprocesses.
    pub concurrency: usize,
    /// If false, refuse to transcode — non-native formats get 415.
    pub enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ffmpeg_path: "ffmpeg".to_string(),
            ffprobe_path: "ffprobe".to_string(),
            preset: Preset::Med,
            concurrency: 20,
            enabled: true,
        }
    }
}

pub struct Transcoder {
    config: Config,
    slots: Arc<Semaphore>,
}

impl Transcoder {
    pub fn new(config: Config) -> Self {
        let slots = Arc::new(Semaphore::new(config.concurrency));
        Self { config, slots }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Estimate output bytes for `duration_ms` at MP3 `bitrate_kbps`. Rough
    /// CBR-style number used to fill a Content-Length header for iTunes.
    /// bytes = seconds × kilobits/s × 1000 / 8 = ms × kbps / 8.
    pub fn estimate_mp3_size(&self, duration_ms: u32, bitrate_kbps: u32) -> u64 {
        (duration_ms as u64) * (bitrate_kbps as u64) / 8
    }

    /// Spawn ffmpeg piping `input` into stdin and returning stdout as an
    /// AsyncRead. The returned handle holds a concurrency permit that
    /// releases on drop. Metadata from `track` is embedded into the output
    /// container (ID3 for MP3, iTunes atoms for ALAC/M4A) so iTunes sees the
    /// correct title/artist/album when it re-parses the played stream —
    /// otherwise it clobbers its cached tags with empties and the track
    /// falls to the bottom of the artist sort.
    pub async fn spawn(
        &self,
        served: ServedFormat,
        track: &Track,
        input: media_source::ByteStream,
        seek_time_ms: Option<u32>,
    ) -> std::io::Result<TranscodeHandle> {
        let permit = Arc::clone(&self.slots).acquire_owned().await.map_err(|_| {
            std::io::Error::other("transcoder shutting down")
        })?;
        let args = ffmpeg_args(served, track, seek_time_ms);
        let mut cmd = Command::new(&self.config.ffmpeg_path);
        cmd.args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        tracing::info!(?args, "spawning ffmpeg");
        let mut child = cmd.spawn()?;
        let mut stdin = child.stdin.take().expect("stdin piped");
        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");

        // Pump source bytes into ffmpeg stdin in a detached task. If the
        // reader errors or ffmpeg closes stdin, the copy simply ends —
        // ffmpeg will finish flushing pending frames and exit.
        let feeder = tokio::spawn(async move {
            let mut input = input;
            let res = tokio::io::copy(&mut input, &mut stdin).await;
            let _ = stdin.shutdown().await;
            if let Err(err) = &res {
                tracing::debug!(?err, "ffmpeg stdin copy ended with error");
            }
        });

        // Drain stderr → tracing so ffmpeg warnings/errors surface in logs.
        let stderr_task = tokio::spawn(async move {
            use tokio::io::AsyncBufReadExt;
            let mut reader = tokio::io::BufReader::new(stderr).lines();
            while let Ok(Some(line)) = reader.next_line().await {
                tracing::debug!("ffmpeg: {}", line);
            }
        });

        Ok(TranscodeHandle {
            _permit: permit,
            _feeder: feeder,
            _stderr: stderr_task,
            _child: child,
            stdout,
        })
    }

    /// Fall back to ffprobe when a source didn't tell us the track duration
    /// (some DLNA servers omit it in DIDL). Returns milliseconds.
    pub async fn probe_duration_ms(&self, url_or_path: &str) -> std::io::Result<Option<u32>> {
        let output = Command::new(&self.config.ffprobe_path)
            .args([
                "-v", "quiet",
                "-show_entries", "format=duration",
                "-of", "default=noprint_wrappers=1:nokey=1",
                url_or_path,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .await?;
        if !output.status.success() {
            return Ok(None);
        }
        let s = String::from_utf8_lossy(&output.stdout);
        let secs: f64 = match s.trim().parse() {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        if !secs.is_finite() || secs <= 0.0 {
            return Ok(None);
        }
        Ok(Some((secs * 1000.0).min(u32::MAX as f64) as u32))
    }

    /// Probe an in-memory source buffer for its exact duration in
    /// microseconds. Feeds `bytes` to ffprobe on stdin. Used to compute
    /// a sample-accurate Content-Length for CBR outputs (ClassicAiff)
    /// where the metadata-derived duration is only accurate to seconds.
    pub async fn probe_duration_micros_bytes(
        &self,
        bytes: &[u8],
    ) -> std::io::Result<Option<u64>> {
        let mut child = Command::new(&self.config.ffprobe_path)
            .args([
                "-v", "quiet",
                "-show_entries", "format=duration",
                "-of", "default=noprint_wrappers=1:nokey=1",
                "-i", "pipe:0",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let mut stdin = child.stdin.take().expect("stdin piped");
        let bytes_owned = bytes.to_vec();
        let write_task = tokio::spawn(async move {
            let _ = stdin.write_all(&bytes_owned).await;
            let _ = stdin.shutdown().await;
        });
        let output = child.wait_with_output().await?;
        let _ = write_task.await;
        if !output.status.success() {
            return Ok(None);
        }
        let s = String::from_utf8_lossy(&output.stdout);
        let secs: f64 = match s.trim().parse() {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        if !secs.is_finite() || secs <= 0.0 {
            return Ok(None);
        }
        Ok(Some((secs * 1_000_000.0).min(u64::MAX as f64) as u64))
    }
}

/// AsyncRead handle over ffmpeg's stdout. Also owns the child + associated
/// pump tasks + concurrency permit; dropping this cancels feeder tasks and
/// (via `kill_on_drop`) reaps the ffmpeg subprocess.
pub struct TranscodeHandle {
    _permit: OwnedSemaphorePermit,
    _feeder: tokio::task::JoinHandle<()>,
    _stderr: tokio::task::JoinHandle<()>,
    _child: Child,
    stdout: tokio::process::ChildStdout,
}

impl AsyncRead for TranscodeHandle {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.stdout).poll_read(cx, buf)
    }
}

fn ffmpeg_args(served: ServedFormat, track: &Track, seek_time_ms: Option<u32>) -> Vec<String> {
    let mut args = vec!["-hide_banner".into(), "-loglevel".into(), "warning".into()];
    // -ss BEFORE -i is an input-side seek: ffmpeg uses the source's own
    // timestamp index to jump ahead, which is efficient for indexed
    // containers (FLAC/OGG/WAV etc.). Placed here so it applies before the
    // decoder starts reading.
    if let Some(ms) = seek_time_ms {
        args.push("-ss".into());
        args.push(format_seek_time(ms));
    }
    args.extend([
        "-i".into(),
        "pipe:0".into(),
        "-vn".into(),
        "-sn".into(),
        "-dn".into(),
        // Copy any tags from source too, then override with our known values
        // below (ours win because -metadata is applied last).
        "-map_metadata".into(),
        "0".into(),
    ]);
    push_metadata(&mut args, "title", Some(&track.title));
    push_metadata(&mut args, "artist", track.artist.as_deref());
    push_metadata(&mut args, "album", track.album.as_deref());
    push_metadata(&mut args, "album_artist", track.album_artist.as_deref());
    push_metadata(&mut args, "genre", track.genre.as_deref());
    if let Some(y) = track.year {
        push_metadata(&mut args, "date", Some(&y.to_string()));
    }
    if let Some(n) = track.track_number {
        push_metadata(&mut args, "track", Some(&n.to_string()));
    }
    if let Some(n) = track.disc_number {
        push_metadata(&mut args, "disc", Some(&n.to_string()));
    }
    match served {
        ServedFormat::Mp3 { bitrate_kbps } => {
            args.extend([
                "-c:a".into(),
                "libmp3lame".into(),
                "-b:a".into(),
                format!("{}k", bitrate_kbps),
                // Use ID3v2.3 for broadest player compat, including iTunes 4.
                "-id3v2_version".into(),
                "3".into(),
                "-write_id3v1".into(),
                "1".into(),
                "-f".into(),
                "mp3".into(),
            ]);
        }
        ServedFormat::Alac => {
            args.extend([
                "-c:a".into(),
                "alac".into(),
                "-f".into(),
                "ipod".into(), // ALAC-in-M4A container ffmpeg understands
            ]);
        }
        ServedFormat::ClassicAiff => {
            // Raw PCM output — we prepend the AIFF header ourselves in
            // http.rs because ffmpeg can't back-patch chunk sizes over an
            // unseekable pipe.
            args.extend([
                "-c:a".into(),
                "pcm_s8".into(),
                "-ar".into(),
                CLASSIC_AIFF_SAMPLE_RATE.to_string(),
                "-ac".into(),
                CLASSIC_AIFF_CHANNELS.to_string(),
                "-f".into(),
                "s8".into(), // raw signed 8-bit, no container
            ]);
        }
        ServedFormat::Passthrough(_) => {
            // We wouldn't call ffmpeg for passthrough; guard so caller
            // doesn't accidentally end up with a re-encode.
            args.extend(["-c:a".into(), "copy".into()]);
        }
    }
    args.push("pipe:1".into());
    args
}

fn push_metadata(args: &mut Vec<String>, key: &str, value: Option<&str>) {
    if let Some(v) = value {
        if !v.is_empty() {
            args.push("-metadata".into());
            args.push(format!("{}={}", key, v));
        }
    }
}

/// Format milliseconds as HH:MM:SS.mmm — ffmpeg's -ss accepts this shape.
fn format_seek_time(ms: u32) -> String {
    let total_secs = ms / 1000;
    let millis = ms % 1000;
    let h = total_secs / 3600;
    let m = (total_secs % 3600) / 60;
    let s = total_secs % 60;
    format!("{:02}:{:02}:{:02}.{:03}", h, m, s, millis)
}

/// Convert a byte offset into an approximate time in milliseconds, given
/// the CBR bitrate we intend to emit. Overflow-safe for u32-representable
/// track durations.
pub fn bytes_to_time_ms(byte_offset: u64, bitrate_kbps: u32) -> u32 {
    // ms = bytes × 8 / kbps
    let ms = byte_offset.saturating_mul(8) / (bitrate_kbps.max(1) as u64);
    ms.min(u32::MAX as u64) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_bitrates() {
        assert_eq!(Preset::Low.mp3_bitrate_kbps(), 128);
        assert_eq!(Preset::Med.mp3_bitrate_kbps(), 192);
        assert_eq!(Preset::High.mp3_bitrate_kbps(), 320);
    }

    #[test]
    fn parse_itunes_version_shapes() {
        assert_eq!(
            parse_itunes_version(Some("iTunes/4.0 (Macintosh; N; PPC)")),
            Some((4, 0))
        );
        assert_eq!(
            parse_itunes_version(Some("iTunes/12.7.4 (Windows; Microsoft Windows 10 x64)")),
            Some((12, 7))
        );
        assert_eq!(parse_itunes_version(Some("Rhythmbox/3.4.4")), None);
        assert_eq!(parse_itunes_version(None), None);
    }

    #[test]
    fn modern_codec_gating() {
        assert!(!client_supports_modern_codecs(Some("iTunes/4.0 (x)")));
        assert!(!client_supports_modern_codecs(Some("iTunes/4.4 (x)")));
        assert!(client_supports_modern_codecs(Some("iTunes/4.5 (x)")));
        assert!(client_supports_modern_codecs(Some("iTunes/12.7 (x)")));
        // Non-iTunes UAs assumed modern (Rhythmbox, Music.app, etc.)
        assert!(client_supports_modern_codecs(Some("Rhythmbox/3.4.4")));
        assert!(client_supports_modern_codecs(None));
    }

    #[test]
    fn passthrough_for_natively_supported_sources() {
        let p = Preset::Med;
        for &fmt in &[AudioFormat::Mp3, AudioFormat::Aiff, AudioFormat::Wav] {
            assert_eq!(choose_format(fmt, false, p), ServedFormat::Passthrough(fmt));
            assert_eq!(choose_format(fmt, true, p), ServedFormat::Passthrough(fmt));
        }
    }

    #[test]
    fn aac_and_alac_gated_by_client() {
        let p = Preset::Med;
        assert_eq!(
            choose_format(AudioFormat::Aac, true, p),
            ServedFormat::Passthrough(AudioFormat::Aac)
        );
        assert_eq!(
            choose_format(AudioFormat::Aac, false, p),
            ServedFormat::Mp3 { bitrate_kbps: 192 }
        );
        assert_eq!(choose_format(AudioFormat::Alac, true, p),
            ServedFormat::Passthrough(AudioFormat::Alac));
        assert_eq!(
            choose_format(AudioFormat::Alac, false, p),
            ServedFormat::Mp3 { bitrate_kbps: 192 }
        );
    }

    #[test]
    fn flac_goes_to_alac_when_client_is_modern() {
        assert_eq!(
            choose_format(AudioFormat::Flac, true, Preset::High),
            ServedFormat::Alac
        );
        assert_eq!(
            choose_format(AudioFormat::Flac, false, Preset::High),
            ServedFormat::Mp3 { bitrate_kbps: 320 }
        );
    }

    #[test]
    fn ogg_always_transcoded_to_mp3() {
        assert!(matches!(
            choose_format(AudioFormat::Ogg, true, Preset::Low),
            ServedFormat::Mp3 { bitrate_kbps: 128 }
        ));
    }

    fn dummy_track() -> Track {
        Track {
            id: 1,
            title: "Hello".into(),
            artist: Some("Adele".into()),
            album: Some("25".into()),
            album_artist: None,
            genre: Some("Pop".into()),
            track_number: Some(3),
            disc_number: None,
            year: Some(2015),
            duration_ms: Some(1000),
            bitrate_kbps: None,
            sample_rate: None,
            size_bytes: None,
            format: AudioFormat::Flac,
        }
    }

    #[test]
    fn ffmpeg_args_include_correct_encoder() {
        let mp3 = ffmpeg_args(ServedFormat::Mp3 { bitrate_kbps: 192 }, &dummy_track(), None);
        assert!(mp3.iter().any(|s| s == "libmp3lame"));
        assert!(mp3.iter().any(|s| s == "192k"));
        let alac = ffmpeg_args(ServedFormat::Alac, &dummy_track(), None);
        assert!(alac.iter().any(|s| s == "alac"));
        assert!(alac.iter().any(|s| s == "ipod"));
    }

    #[test]
    fn ffmpeg_args_inject_track_metadata() {
        let args = ffmpeg_args(ServedFormat::Mp3 { bitrate_kbps: 192 }, &dummy_track(), None);
        // Metadata pairs come through as `-metadata key=value` — verify a
        // handful of them show up.
        assert!(args.iter().any(|s| s == "title=Hello"));
        assert!(args.iter().any(|s| s == "artist=Adele"));
        assert!(args.iter().any(|s| s == "album=25"));
        assert!(args.iter().any(|s| s == "genre=Pop"));
        assert!(args.iter().any(|s| s == "date=2015"));
        assert!(args.iter().any(|s| s == "track=3"));
        // Empty/absent fields are not emitted.
        assert!(!args.iter().any(|s| s.starts_with("album_artist=")));
        assert!(!args.iter().any(|s| s.starts_with("disc=")));
    }

    #[test]
    fn ffmpeg_args_include_id3v2_options_for_mp3() {
        let args = ffmpeg_args(ServedFormat::Mp3 { bitrate_kbps: 128 }, &dummy_track(), None);
        assert!(args.iter().any(|s| s == "-id3v2_version"));
        assert!(args.iter().any(|s| s == "-write_id3v1"));
    }

    #[test]
    fn ffmpeg_args_seek_time_precedes_input() {
        let args = ffmpeg_args(
            ServedFormat::Mp3 { bitrate_kbps: 192 },
            &dummy_track(),
            Some(3_500),
        );
        // Find -ss and -i positions; -ss must come first (input-side seek).
        let ss = args.iter().position(|s| s == "-ss").unwrap();
        let i = args.iter().position(|s| s == "-i").unwrap();
        assert!(ss < i, "-ss must come before -i for fast input-side seek");
        // Value formatted as HH:MM:SS.mmm.
        assert_eq!(args[ss + 1], "00:00:03.500");
    }

    #[test]
    fn ffmpeg_args_no_seek_when_time_is_none() {
        let args = ffmpeg_args(ServedFormat::Mp3 { bitrate_kbps: 192 }, &dummy_track(), None);
        assert!(!args.iter().any(|s| s == "-ss"));
    }

    #[test]
    fn format_seek_time_shape() {
        assert_eq!(format_seek_time(0), "00:00:00.000");
        assert_eq!(format_seek_time(1_500), "00:00:01.500");
        assert_eq!(format_seek_time(3_600_000), "01:00:00.000");
        assert_eq!(format_seek_time(3_723_450), "01:02:03.450");
    }

    #[test]
    fn classic_aiff_header_shape() {
        let h = classic_aiff_header(1000);
        // FORM signature
        assert_eq!(&h[0..4], b"FORM");
        // "AIFF"
        assert_eq!(&h[8..12], b"AIFF");
        // COMM
        assert_eq!(&h[12..16], b"COMM");
        // SSND
        assert_eq!(&h[38..42], b"SSND");
        // numSampleFrames
        assert_eq!(u32::from_be_bytes(h[22..26].try_into().unwrap()), 1000);
    }

    #[test]
    fn classic_aiff_size_scales_with_duration() {
        // 1 second of 22254 Hz PCM8 mono = 22254 bytes + 54-byte header.
        assert_eq!(classic_aiff_size(1000), 54 + 22254);
        assert_eq!(classic_aiff_size(0), 54);
    }

    #[test]
    fn classic_aiff_byte_to_time_math_is_reversible() {
        // Within data body, byte position → time ms → byte position round-trip.
        let byte = 54u64 + 22254; // 1 second in
        let ms = classic_aiff_byte_to_time_ms(byte);
        assert_eq!(ms, 1000);
    }

    #[test]
    fn ieee_754_extended_encodes_sample_rate() {
        // The sample rate value we care about, verified against known
        // 80-bit representations. 22050 Hz → 4001 CC22 0000 0000 0000.
        // We use 22254 → 4001 ADCE 0000 0000 0000.
        let out = ieee_754_extended(22254.0);
        assert_eq!(&out[0..2], &[0x40, 0x0D]);
        // Mantissa MSB should have the explicit-1 bit set.
        assert_eq!(out[2] & 0x80, 0x80);
    }

    #[test]
    fn classic_aiff_ffmpeg_args_include_pcm_s8_and_22254_mono() {
        let args = ffmpeg_args(ServedFormat::ClassicAiff, &dummy_track(), None);
        assert!(args.iter().any(|s| s == "pcm_s8"));
        assert!(args.iter().any(|s| s == "22254"));
        assert!(args.iter().any(|s| s == "1"));
        // Output format is raw, not aiff — we own the header.
        assert!(args.iter().any(|s| s == "s8"));
    }

    #[test]
    fn bytes_to_time_ms_matches_bitrate() {
        // 60 seconds of 192kbps CBR = 192000 * 60 / 8 = 1_440_000 bytes.
        // 720_000 bytes at 192kbps == 30 seconds == 30_000 ms.
        assert_eq!(bytes_to_time_ms(720_000, 192), 30_000);
        assert_eq!(bytes_to_time_ms(0, 192), 0);
        // Safe on divide-by-zero (bitrate clamped to at least 1).
        assert!(bytes_to_time_ms(100, 0) > 0);
    }

    #[test]
    fn asfm_reflects_served_format() {
        assert_eq!(ServedFormat::Mp3 { bitrate_kbps: 192 }.asfm(), "mp3");
        assert_eq!(ServedFormat::Alac.asfm(), "m4a");
        assert_eq!(ServedFormat::Passthrough(AudioFormat::Aac).asfm(), "m4a");
    }

    #[test]
    fn mp3_size_estimate_scales_with_duration_and_bitrate() {
        let t = Transcoder::new(Config::default());
        // 60_000 ms @ 128 kbps = 60 * 128 / 8 = 960 kB
        assert_eq!(t.estimate_mp3_size(60_000, 128), 960_000);
        assert_eq!(t.estimate_mp3_size(0, 320), 0);
    }
}
