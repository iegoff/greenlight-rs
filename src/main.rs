use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};

use greenlight::api::catalog;
use greenlight::api::xcloud::XCloudApi;
use greenlight::app::App;
use greenlight::auth::Msal;
use greenlight::channels::ClientConfig;
use greenlight::player::{H264Profile, PlayerConfig};
use greenlight::session::{self, SessionOptions};

#[derive(Parser)]
#[command(name = "greenlight", version, about = "xCloud and Xbox home streaming client (Rust + GStreamer webrtcbin)")]
struct Cli {
    /// IP address sent as X-Forwarded-For to select another region.
    #[arg(long, global = true, env = "GREENLIGHT_FORCE_REGION_IP")]
    force_region_ip: Option<String>,

    /// Game locale.
    #[arg(long, global = true, env = "GREENLIGHT_LOCALE")]
    locale: Option<String>,

    /// Without a command, the graphical interface starts.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Sign in with a Microsoft account (device code flow).
    Login {
        /// Don't print a QR code of the login URL.
        #[arg(long)]
        no_qr: bool,
    },
    /// Reuse the sign-in of an installed Greenlight desktop app (incl. its Flatpak).
    ImportGreenlight {
        /// Greenlight `config.json` (or a `tokens.json`); searched automatically if omitted.
        #[arg(long)]
        from: Option<std::path::PathBuf>,
    },
    /// Remove stored credentials.
    Logout,
    /// Show the signed-in profile.
    Profile,
    /// List consoles available for home streaming.
    Consoles,
    /// List online friends.
    Friends,
    /// List xCloud titles.
    Titles {
        /// Only recently played titles.
        #[arg(long)]
        recent: bool,
        /// Filter by name (case insensitive).
        #[arg(long)]
        search: Option<String>,
    },
    /// List xCloud regions available to the account.
    Regions,
    /// List active streaming sessions.
    Sessions,
    /// Stream from an Xbox console on your network (`F4000...` server id).
    Home {
        /// Console id, see `greenlight consoles`.
        console_id: String,
        #[command(flatten)]
        stream: StreamArgs,
    },
    /// Stream an xCloud title.
    Cloud {
        /// xCloud title id, see `greenlight titles`.
        title_id: String,
        /// xCloud region name (see `greenlight regions`).
        #[arg(long)]
        region: Option<String>,
        #[command(flatten)]
        stream: StreamArgs,
    },
}

#[derive(Args, Clone)]
struct StreamArgs {
    /// H.264 profile preference, comma separated (high = main 4d, medium = 42e, low = 420).
    #[arg(long, value_delimiter = ',', default_value = "high,medium,low")]
    profiles: Vec<H264Profile>,
    /// Max video bitrate in kbps (0 = server default).
    #[arg(long, default_value_t = 0)]
    video_bitrate: u32,
    /// Max audio bitrate in kbps (0 = server default).
    #[arg(long, default_value_t = 0)]
    audio_bitrate: u32,
    /// Force mono audio.
    #[arg(long)]
    mono: bool,
    /// Disable audio playback.
    #[arg(long)]
    no_audio: bool,
    /// Request retransmission of lost packets (only useful with --latency above the round-trip time).
    #[arg(long)]
    nack: bool,
    /// Jitter buffer latency in ms.
    #[arg(long, default_value_t = 20)]
    latency: u32,
    /// Also play audio as soon as it arrives (no clock sync at all).
    #[arg(long)]
    low_latency: bool,
    /// Fullscreen video window (where the sink supports it, e.g. waylandsink).
    #[arg(long)]
    fullscreen: bool,
    /// Sync video frames to the clock (smoother, but adds latency).
    #[arg(long)]
    video_sync: bool,
    /// Custom GStreamer video sink, e.g. "glimagesink" or "waylandsink fullscreen=true".
    #[arg(long)]
    video_sink: Option<String>,
    /// Custom GStreamer audio sink, e.g. "pulsesink".
    #[arg(long)]
    audio_sink: Option<String>,
    /// STUN server (the Xbox web client's by default); "none" to disable.
    #[arg(long, default_value = "stun://relay.communication.microsoft.com:3478")]
    stun: String,
    /// How long ICE keeps trying each connection path, in seconds (0 = libnice's ~2 s).
    /// Longer values delay connecting, because ICE waits for every path to finish.
    #[arg(long, default_value_t = 0)]
    ice_timeout: u32,
    /// Requested stream resolution reported to the server.
    #[arg(long, default_value = "1920x1080", value_parser = parse_resolution)]
    resolution: (u32, u32),
    /// Disable physical gamepads (keyboard only).
    #[arg(long)]
    no_gamepad: bool,
    /// Disable controller rumble.
    #[arg(long)]
    no_rumble: bool,
    /// Analog stick deadzone.
    #[arg(long, default_value_t = 0.08)]
    deadzone: f32,
    /// Request a keyframe every N seconds (0 = never).
    #[arg(long, default_value_t = 0)]
    keyframe_interval: u64,
    /// Keep the remote session running when the client exits.
    #[arg(long)]
    keep_session: bool,
}

fn parse_resolution(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s.split_once(['x', 'X']).ok_or("expected WIDTHxHEIGHT")?;
    Ok((w.parse().map_err(|_| "bad width")?, h.parse().map_err(|_| "bad height")?))
}

impl StreamArgs {
    fn options(&self) -> SessionOptions {
        SessionOptions {
            player: PlayerConfig {
                profiles: self.profiles.clone(),
                stereo: !self.mono,
                video_kbps: self.video_bitrate,
                audio_kbps: self.audio_bitrate,
                stun_server: (!self.stun.eq_ignore_ascii_case("none") && !self.stun.is_empty())
                    .then(|| self.stun.clone()),
                ice_check_secs: self.ice_timeout,
                latency_ms: self.latency,
                video_sink: self.video_sink.clone(),
                audio_sink: self.audio_sink.clone(),
                low_latency: self.low_latency,
                fullscreen: self.fullscreen,
                video_sync: self.video_sync,
                video_sink_element: None,
                nack: self.nack,
                audio: !self.no_audio,
            },
            client: ClientConfig { width: self.resolution.0, height: self.resolution.1, ..Default::default() },
            gamepad: !self.no_gamepad,
            rumble: !self.no_rumble,
            deadzone: self.deadzone,
            keyframe_interval: self.keyframe_interval,
            stop_on_exit: !self.keep_session,
        }
    }
}

/// Runs a session in the terminal: prints its events, ctrl-c disconnects.
async fn run_cli_session(api: XCloudApi, msal: Arc<Msal>, target: &str, opts: SessionOptions) -> Result<()> {
    use greenlight::session::{SessionCommand, SessionEvent};
    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(ev) = ev_rx.recv().await {
            match ev {
                SessionEvent::Status(s) => eprintln!("{s}"),
                SessionEvent::Queued { estimated_wait_secs } => match estimated_wait_secs {
                    Some(s) => eprintln!("In queue. Estimated wait: {s}s"),
                    None => eprintln!("In queue, waiting for a server..."),
                },
                SessionEvent::Connected => eprintln!("Connected."),
                SessionEvent::StreamStarted => eprintln!("Stream started. Press ~ for stats, F12 to quit."),
                SessionEvent::VideoSize { .. } | SessionEvent::Title(_) | SessionEvent::OverlayRequested => {}
                SessionEvent::Dialog { title, text } => eprintln!("[console] {title}: {text}"),
                SessionEvent::Stats(s) => eprintln!("[stats] {}", greenlight::session::format_stats(&s)),
                SessionEvent::Ended { .. } => break,
            }
        }
    });
    {
        let cmd_tx = cmd_tx.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                eprintln!("Stopping stream...");
                let _ = cmd_tx.send(SessionCommand::Disconnect);
            }
        });
    }
    session::run(api, msal, target, opts, ev_tx, cmd_rx).await
}

/// Renders `data` as a QR code using Unicode half blocks (two rows per line).
fn qr_text(data: &str) -> Result<String> {
    let code = qrcode::QrCode::new(data.as_bytes())?;
    Ok(code
        .render::<qrcode::render::unicode::Dense1x2>()
        .dark_color(qrcode::render::unicode::Dense1x2::Light)
        .light_color(qrcode::render::unicode::Dense1x2::Dark)
        .quiet_zone(true)
        .build())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("GREENLIGHT_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn,greenlight=info")),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();
    let rt = tokio::runtime::Runtime::new()?;
    let app = {
        let _guard = rt.enter();
        App::new(cli.force_region_ip.clone(), cli.locale.clone())?
    };
    match cli.command {
        Some(command) => rt.block_on(run_command(command, app)),
        None => run_gui(rt, app),
    }
}

#[cfg(feature = "gui")]
fn run_gui(rt: tokio::runtime::Runtime, app: App) -> Result<()> {
    greenlight::gui::run(rt, app)
}

#[cfg(not(feature = "gui"))]
fn run_gui(_rt: tokio::runtime::Runtime, _app: App) -> Result<()> {
    use clap::CommandFactory;
    Cli::command().print_help()?;
    Ok(())
}

async fn run_command(command: Command, app: App) -> Result<()> {
    let settings = app.settings.clone();
    let http = app.http.clone();
    match command {
        Command::Login { no_qr } => {
            let code = app.msal.start_device_code().await?;
            eprintln!("{}", code.message);
            let url = code.verification_uri_complete.as_deref().unwrap_or(&code.verification_uri);
            if !no_qr {
                match qr_text(url) {
                    Ok(qr) => eprintln!("\nScan to open the login page:\n{qr}"),
                    Err(e) => tracing::debug!("QR code: {e}"),
                }
            }
            eprintln!("\n  URL:  {}\n  Code: {}\n", code.verification_uri, code.user_code);
            app.msal.poll_device_code(&code).await?;
            let web = app.msal.web_token().await.context("fetching Xbox Live token")?;
            println!("Signed in as {}", web.gamertag().unwrap_or("<unknown>"));
        }
        Command::ImportGreenlight { from } => {
            let (path, gamertag) = app.import_greenlight(from).await?;
            eprintln!("Imported sign-in from {}", path.display());
            println!("Signed in as {gamertag}");
        }
        Command::Logout => {
            app.msal.logout().await?;
            println!("Signed out.");
        }
        Command::Profile => {
            for (k, v) in app.web().await?.profile().await? {
                println!("{k:>18}: {v}");
            }
        }
        Command::Consoles => {
            let consoles = app.web().await?.consoles().await?;
            if consoles.is_empty() {
                println!("No consoles found.");
            }
            for c in consoles {
                println!(
                    "{}  {:<20} {:<16} power={:<10} streaming={}",
                    c.id,
                    c.name,
                    c.console_type,
                    c.power_state,
                    if c.console_streaming_enabled { "enabled" } else { "disabled" }
                );
            }
        }
        Command::Friends => {
            let mut friends = app.web().await?.friends().await?;
            friends.retain(|f| f.presence_state != "Offline");
            if friends.is_empty() {
                println!("No friends online.");
            }
            for f in friends {
                println!("{:<20} {}", f.gamertag, f.presence_text);
            }
        }
        Command::Regions => {
            let (_, token) = app.cloud_api(None).await?;
            for r in &token.offering_settings.regions {
                println!("{:<20} {}{}", r.name, r.base_uri, if r.is_default { "  (default)" } else { "" });
            }
        }
        Command::Titles { recent, search } => {
            let region = settings.xcloud_region.as_deref();
            let (api, token) = app.cloud_api(region).await?;
            let raw = if recent { api.recent_titles().await? } else { api.titles().await? };
            let mut titles = catalog::parse_titles(&raw);
            let ids: Vec<String> = titles.iter().filter_map(|t| t.product_id.clone()).collect();
            let products = catalog::resolve_products(&http, &ids, &token.market, &app.locale)
                .await
                .unwrap_or_default();
            for t in &mut titles {
                if let Some(p) = t.product_id.as_ref().and_then(|id| products.get(id)) {
                    t.name = Some(p.product_title.clone());
                }
            }
            titles.sort_by(|a, b| a.name.cmp(&b.name));
            for t in titles {
                let name = t.name.unwrap_or_else(|| "?".into());
                if let Some(s) = &search {
                    if !name.to_lowercase().contains(&s.to_lowercase()) {
                        continue;
                    }
                }
                println!("{:<40} {}{}", t.title_id, name, if t.has_entitlement { "" } else { "  (not entitled)" });
            }
        }
        Command::Sessions => {
            let home = app.home_api().await?;
            println!("home: {}", serde_json::to_string_pretty(&home.active_sessions().await?)?);
            if let Ok((cloud, _)) = app.cloud_api(None).await {
                println!("cloud: {}", serde_json::to_string_pretty(&cloud.active_sessions().await?)?);
            }
        }
        Command::Home { console_id, stream } => {
            let mut api = app.home_api().await?;
            api.set_display(stream.resolution.0, stream.resolution.1);
            run_cli_session(api, app.msal.clone(), &console_id, stream.options()).await?;
        }
        Command::Cloud { title_id, region, stream } => {
            let region = region.or(settings.xcloud_region.clone());
            let (mut api, _) = app.cloud_api(region.as_deref()).await?;
            api.set_display(stream.resolution.0, stream.resolution.1);
            run_cli_session(api, app.msal.clone(), &title_id, stream.options()).await?;
        }
    }
    Ok(())
}
