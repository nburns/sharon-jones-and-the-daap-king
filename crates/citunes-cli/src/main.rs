//! CLI binary that ties daap-server + a chosen MediaSource + mDNS together.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};
use daap_server::mdns::{Advertisement, TeardownHandle};
use daap_server::artwork;
use daap_server::transcode::{self, Preset};
use daap_server::{Config, Server};
use media_source::MediaSource;
use media_source_dlna::{cache_filename, CacheConfig, DlnaSource};
use media_source_fs::FsSource;
use media_source_subsonic::{Credentials, SubsonicSource};
use url::Url;

/// Process-global teardown handle for the panic hook and signal paths.
static TEARDOWN: OnceLock<Mutex<Option<TeardownHandle>>> = OnceLock::new();

fn global_teardown() -> &'static Mutex<Option<TeardownHandle>> {
    TEARDOWN.get_or_init(|| Mutex::new(None))
}

fn register_teardown(handle: TeardownHandle) {
    let mut guard = global_teardown().lock().unwrap_or_else(|e| e.into_inner());
    *guard = Some(handle);
}

fn clear_teardown() {
    let mut guard = global_teardown().lock().unwrap_or_else(|e| e.into_inner());
    *guard = None;
}

fn run_emergency_stop() {
    let handle = {
        let mut guard = global_teardown().lock().unwrap_or_else(|e| e.into_inner());
        guard.take()
    };
    if let Some(h) = handle {
        h.emergency_stop();
    }
}

#[derive(Parser, Debug)]
#[command(name = "citunes", about = "Serve a music library over DAAP for old iTunes clients")]
struct Args {
    /// Library name advertised over mDNS.
    #[arg(short, long, default_value = "Classic iTunes Streamer")]
    name: String,

    /// Bind address for the DAAP HTTP server.
    #[arg(short, long, default_value = "0.0.0.0:3689")]
    bind: SocketAddr,

    /// Suppress mDNS advertisement (useful for local curl testing).
    #[arg(long)]
    no_mdns: bool,

    /// MP3 quality for transcoded tracks (low=128k, med=192k, high=320k).
    /// Lossless→ALAC path for modern clients isn't affected.
    #[arg(long, value_enum, default_value_t = QualityArg::Med)]
    transcode_quality: QualityArg,

    /// Cap on concurrent ffmpeg subprocesses.
    #[arg(long, default_value_t = 20)]
    transcode_concurrency: usize,

    /// Path to the `ffmpeg` binary.
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: String,

    /// Path to the `ffprobe` binary.
    #[arg(long, default_value = "ffprobe")]
    ffprobe: String,

    /// Disable transcoding entirely. Non-native tracks will fail to play.
    #[arg(long)]
    no_transcode: bool,

    /// LRU capacity for resized artwork (entries; each ~5-100kB).
    #[arg(long, default_value_t = 500)]
    artwork_cache_size: usize,

    #[command(subcommand)]
    source: SourceKind,
}

#[derive(Copy, Clone, Debug, ValueEnum)]
enum QualityArg {
    Low,
    Med,
    High,
}

impl From<QualityArg> for Preset {
    fn from(q: QualityArg) -> Self {
        match q {
            QualityArg::Low => Preset::Low,
            QualityArg::Med => Preset::Med,
            QualityArg::High => Preset::High,
        }
    }
}

#[derive(Subcommand, Debug)]
enum SourceKind {
    /// Serve a directory of audio files.
    Fs {
        /// Root directory to scan.
        #[arg(short, long)]
        music: PathBuf,
    },
    /// Serve a DLNA/UPnP MediaServer's audio content.
    Dlna {
        /// Substring of the server's friendly name to auto-discover on the LAN.
        /// If not given, first server found is used.
        #[arg(short = 's', long)]
        server: Option<String>,

        /// Explicit device-description URL (bypasses SSDP discovery).
        #[arg(short = 'u', long, conflicts_with = "server")]
        url: Option<Url>,

        /// SSDP discovery timeout (seconds).
        #[arg(long, default_value_t = 3)]
        discover_timeout: u64,

        /// ContentDirectory ObjectID to start browsing from. Defaults to root
        /// ("0"). Useful for servers with noisy hierarchies - e.g. on Plex,
        /// pass the "All Artists" object id under Music to skip Video/Photos
        /// and the duplicate By-Album/By-Genre views.
        #[arg(long, default_value = "0")]
        root: String,

        /// Directory to write cached catalogues into. On subsequent starts
        /// the cached catalogue is loaded instantly instead of re-browsing.
        /// Cache files are named per (server URL, root) so different targets
        /// don't collide.
        #[arg(long, default_value = "/tmp/citunes-dlna-cache")]
        cache_dir: PathBuf,

        /// Disable the on-disk catalogue cache; always browse fresh.
        #[arg(long)]
        no_cache: bool,
    },
    /// SSDP-discover DLNA MediaServers on the LAN and list them.
    DlnaList {
        /// How long to wait for SSDP replies (seconds).
        #[arg(long, default_value_t = 3)]
        timeout: u64,
    },
    /// Serve a Subsonic-compatible server (Navidrome, Airsonic, Gonic, ...).
    Subsonic {
        /// Base server URL, e.g. http://navidrome.local:4533
        #[arg(short = 'u', long)]
        url: Url,

        /// OpenSubsonic API key (preferred). Set via env: SUBSONIC_API_KEY.
        #[arg(long, env = "SUBSONIC_API_KEY")]
        api_key: Option<String>,

        /// Legacy Subsonic username. Requires --password or SUBSONIC_PASSWORD.
        #[arg(long)]
        user: Option<String>,

        /// Legacy Subsonic password. Preferably set via env: SUBSONIC_PASSWORD.
        #[arg(long, env = "SUBSONIC_PASSWORD")]
        password: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Install panic hook before anything else so it fires on any panic path.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        run_emergency_stop();
        prev_hook(info);
    }));

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info,daap_server=info,media_source=info".into()),
        )
        .init();

    let args = Args::parse();

    let transcode_cfg = transcode::Config {
        ffmpeg_path: args.ffmpeg.clone(),
        ffprobe_path: args.ffprobe.clone(),
        preset: args.transcode_quality.into(),
        concurrency: args.transcode_concurrency,
        enabled: !args.no_transcode,
    };
    let artwork_cfg = artwork::Config {
        cache_size: args.artwork_cache_size,
        jpeg_quality: 85,
    };
    let make_opts = |name: String| ServeOpts {
        name,
        bind: args.bind,
        no_mdns: args.no_mdns,
        transcode: transcode_cfg.clone(),
        artwork: artwork_cfg.clone(),
    };

    match args.source {
        SourceKind::Fs { music } => {
            let source = FsSource::scan(&args.name, &music)?;
            tracing::info!(count = source.track_count(), "fs source ready");
            serve(make_opts(args.name.clone()), source).await
        }
        SourceKind::Dlna {
            server,
            url,
            discover_timeout,
            root,
            cache_dir,
            no_cache,
        } => {
            let cache_for = |target_url: &Url| -> Option<CacheConfig> {
                if no_cache {
                    None
                } else {
                    Some(CacheConfig {
                        path: cache_dir.join(cache_filename(target_url, &root)),
                    })
                }
            };

            let source = if let Some(u) = url {
                let cache = cache_for(&u);
                DlnaSource::connect_from(&u, &root, cache).await?
            } else if let Some(name) = server {
                // For named-discovery, cache is keyed by the resolved URL
                // once discovery completes - do the discovery ourselves so
                // we know that URL up front.
                let servers =
                    media_source_dlna::discover(Duration::from_secs(discover_timeout)).await?;
                let picked = servers
                    .into_iter()
                    .find(|s| {
                        s.friendly_name
                            .to_lowercase()
                            .contains(&name.to_lowercase())
                    })
                    .ok_or_else(|| {
                        Box::<dyn std::error::Error + Send + Sync>::from(format!(
                            "no DLNA MediaServer matching {name:?}"
                        ))
                    })?;
                let cache = cache_for(&picked.description_url);
                DlnaSource::connect_from(&picked.description_url, &root, cache).await?
            } else {
                // Pick first discovered.
                let servers =
                    media_source_dlna::discover(Duration::from_secs(discover_timeout)).await?;
                let first = servers.into_iter().next().ok_or_else(|| {
                    Box::<dyn std::error::Error + Send + Sync>::from(
                        "no DLNA MediaServers found on LAN",
                    )
                })?;
                tracing::info!(server = %first.friendly_name, url = %first.description_url, "using first-discovered server");
                let cache = cache_for(&first.description_url);
                DlnaSource::connect_from(&first.description_url, &root, cache).await?
            };
            tracing::info!(
                tracks = source.track_count(),
                playlists = source.container_count(),
                "dlna source ready"
            );
            serve(make_opts(args.name.clone()), source).await
        }
        SourceKind::Subsonic { url, api_key, user, password } => {
            let creds = match (api_key, user, password) {
                (Some(k), _, _) => Credentials::ApiKey(k),
                (None, Some(u), Some(p)) => Credentials::UserPassword { user: u, password: p },
                (None, Some(_), None) => {
                    return Err("subsonic --user requires --password or SUBSONIC_PASSWORD".into());
                }
                (None, None, _) => {
                    return Err(
                        "subsonic backend needs either --api-key/SUBSONIC_API_KEY or --user + --password/SUBSONIC_PASSWORD"
                            .into(),
                    );
                }
            };
            let source = SubsonicSource::connect(url, creds).await?;
            tracing::info!(
                tracks = source.track_count(),
                playlists = source.playlist_count(),
                "subsonic source ready"
            );
            serve(make_opts(args.name.clone()), source).await
        }
        SourceKind::DlnaList { timeout } => {
            let servers = media_source_dlna::discover(Duration::from_secs(timeout)).await?;
            if servers.is_empty() {
                println!("(no MediaServers discovered)");
            } else {
                for s in servers {
                    println!("{}", s.friendly_name);
                    println!("    {}", s.description_url);
                }
            }
            Ok(())
        }
    }
}

struct ServeOpts {
    name: String,
    bind: SocketAddr,
    no_mdns: bool,
    transcode: transcode::Config,
    artwork: artwork::Config,
}

/// Awaits any signal that should trigger a clean shutdown.
async fn wait_for_shutdown() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut sighup = signal(SignalKind::hangup()).expect("SIGHUP handler");
        let mut sigquit = signal(SignalKind::quit()).expect("SIGQUIT handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => { tracing::info!("received SIGINT, shutting down"); }
            _ = sigterm.recv() => { tracing::info!("received SIGTERM, shutting down"); }
            _ = sighup.recv() => { tracing::info!("received SIGHUP, shutting down"); }
            _ = sigquit.recv() => { tracing::info!("received SIGQUIT, shutting down"); }
        }
    }
    #[cfg(windows)]
    {
        use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_shutdown};
        let mut cb = ctrl_break().expect("ctrl_break handler");
        let mut cc = ctrl_close().expect("ctrl_close handler");
        let mut cs = ctrl_shutdown().expect("ctrl_shutdown handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => { tracing::info!("received Ctrl-C, shutting down"); }
            _ = cb.recv() => { tracing::info!("received Ctrl-Break, shutting down"); }
            _ = cc.recv() => { tracing::info!("received Ctrl-Close, shutting down"); }
            _ = cs.recv() => { tracing::info!("received Ctrl-Shutdown, shutting down"); }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        tokio::signal::ctrl_c().await.ok();
        tracing::info!("shutting down");
    }
}

async fn serve<S: MediaSource + 'static>(
    opts: ServeOpts,
    source: S,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut mdns: Option<Advertisement> = if opts.no_mdns {
        None
    } else {
        let adv = Advertisement::start(&opts.name, opts.bind.port())?;
        register_teardown(adv.teardown_handle());
        Some(adv)
    };

    let config = Config {
        name: opts.name,
        bind: opts.bind,
        transcode: opts.transcode,
        artwork: opts.artwork,
    };
    let server = Server::new(config, source);

    let result = tokio::select! {
        res = server.run() => res,
        () = wait_for_shutdown() => Ok(()),
    };

    // Always attempt a clean goodbye before propagating any error.
    clear_teardown();
    if let Some(adv) = mdns.take()
        && let Err(e) = adv.stop().await
    {
        tracing::warn!("mDNS goodbye failed: {e}");
    }

    result?;
    Ok(())
}
