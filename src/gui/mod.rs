//! GTK4/libadwaita user interface.
//!
//! * Sign-in page: device-code login with a QR code, or import from Greenlight.
//! * Console list (main page): pick a console and stream.
//! * Stream page: video rendered by `gtk4paintablesink` with an overlay
//!   (status, stats, and a menu with Xbox button / stats / fullscreen /
//!   disconnect). The menu opens with Escape or F1, a tap/click on the video,
//!   or by holding View + Menu on the controller for a second.
//!
//! Threads: GTK owns the main thread. Network and streaming run on the tokio
//! runtime; results come back through futures awaited on the GLib main
//! context. GTK objects are only touched on the main thread.

use std::cell::RefCell;
use std::future::Future;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use anyhow::{anyhow, Context, Result};
use gst::prelude::*;
use gtk::{gdk, glib};
use tokio::sync::mpsc;

use crate::api::webapi::Console;
use crate::app::App;
use crate::auth::DeviceCodeResponse;
use crate::channels::ClientConfig;
use crate::player::PlayerConfig;
use crate::session::{self, SessionCommand, SessionEvent, SessionOptions};

const APP_ID: &str = "io.github.iegoff.greenlight_rs";

const CSS: &str = r#"
.stream-bg { background: black; }
.stats {
    font-family: monospace;
    font-size: 0.85em;
    padding: 6px 10px;
    border-radius: 8px;
}
.stream-menu { padding: 10px; border-radius: 14px; }
.stream-menu button { min-width: 120px; min-height: 44px; }
.status-osd { padding: 14px 20px; border-radius: 12px; }
.code { font-family: monospace; font-size: 2em; font-weight: bold; letter-spacing: 4px; }
"#;

/// Starts the GUI. Blocks until the window is closed.
pub fn run(rt: tokio::runtime::Runtime, app: App) -> Result<()> {
    gst::init()?;
    gstgtk4::plugin_register_static().context("registering gtk4paintablesink")?;
    let rt = Arc::new(rt);
    let application = adw::Application::builder().application_id(APP_ID).build();
    application.connect_activate(move |a| {
        let ui = Ui::build(a, rt.clone(), app.clone());
        ui.start();
    });
    // Our own CLI parsing already happened; don't let GTK see the arguments.
    application.run_with_args::<&str>(&[]);
    Ok(())
}

/// Runs `fut` on tokio and calls `done` with its output on the GTK main thread.
fn spawn<F, T>(rt: &tokio::runtime::Runtime, fut: F, done: impl FnOnce(T) + 'static)
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let handle = rt.spawn(fut);
    glib::spawn_future_local(async move {
        if let Ok(v) = handle.await {
            done(v);
        }
    });
}

struct Widgets {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    stack: gtk::Stack,
    // sign-in
    login_qr: gtk::Picture,
    login_code: gtk::Label,
    login_url: gtk::Label,
    login_status: gtk::Label,
    login_retry: gtk::Button,
    // consoles
    console_list: gtk::ListBox,
    consoles_status: adw::StatusPage,
    consoles_scroller: gtk::ScrolledWindow,
    gamertag: gtk::Label,
    // stream
    picture: gtk::Picture,
    status_box: gtk::Box,
    status_label: gtk::Label,
    stats_label: gtk::Label,
    menu: gtk::Revealer,
    menu_first: gtk::Button,
    title_label: gtk::Label,
}

#[derive(Default)]
struct State {
    commands: Option<mpsc::UnboundedSender<SessionCommand>>,
    stats_on: bool,
    streaming: bool,
    auto_fullscreen: bool,
}

#[derive(Clone)]
struct Ui {
    w: Rc<Widgets>,
    state: Rc<RefCell<State>>,
    rt: Arc<tokio::runtime::Runtime>,
    app: App,
}

impl Ui {
    fn build(application: &adw::Application, rt: Arc<tokio::runtime::Runtime>, app: App) -> Self {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        if let Some(display) = gdk::Display::default() {
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
        }

        let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::Crossfade).build();

        // --- loading ---------------------------------------------------------
        let loading = adw::StatusPage::builder()
            .title("Greenlight")
            .description("Loading…")
            .child(&gtk::Spinner::builder().spinning(true).width_request(32).height_request(32).build())
            .build();
        stack.add_named(&page_with_header(&loading, None), Some("loading"));

        // --- sign-in ---------------------------------------------------------
        let login_qr = gtk::Picture::builder().width_request(220).height_request(220).can_shrink(false).build();
        let login_code = gtk::Label::builder().css_classes(["code"]).selectable(true).build();
        let login_url = gtk::Label::builder().css_classes(["dim-label"]).selectable(true).build();
        let login_status = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).build();
        let login_retry = gtk::Button::builder().label("Get a new code").halign(gtk::Align::Center).visible(false).build();
        let import_btn = gtk::Button::builder()
            .label("Import sign-in from Greenlight")
            .halign(gtk::Align::Center)
            .css_classes(["pill"])
            .build();
        let login_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .halign(gtk::Align::Center)
            .build();
        for w in [
            login_qr.upcast_ref::<gtk::Widget>(),
            login_url.upcast_ref(),
            login_code.upcast_ref(),
            login_status.upcast_ref(),
            login_retry.upcast_ref(),
            import_btn.upcast_ref(),
        ] {
            login_box.append(w);
        }
        let login_page = adw::StatusPage::builder()
            .title("Sign in")
            .description("Scan the code with your phone, sign in with your Microsoft account and enter the code below.")
            .child(&login_box)
            .build();
        stack.add_named(&page_with_header(&login_page, None), Some("login"));

        // --- consoles --------------------------------------------------------
        let console_list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let clamp = adw::Clamp::builder().maximum_size(720).margin_top(24).margin_bottom(24).margin_start(12).margin_end(12).build();
        let list_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
        let gamertag = gtk::Label::builder().halign(gtk::Align::Start).css_classes(["title-2"]).build();
        list_box.append(&gamertag);
        list_box.append(&gtk::Label::builder().label("Your consoles").halign(gtk::Align::Start).css_classes(["heading"]).build());
        list_box.append(&console_list);
        clamp.set_child(Some(&list_box));
        let consoles_scroller = gtk::ScrolledWindow::builder().child(&clamp).vexpand(true).build();
        let consoles_status = adw::StatusPage::builder().icon_name("network-offline-symbolic").vexpand(true).visible(false).build();
        let consoles_content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        consoles_content.append(&consoles_scroller);
        consoles_content.append(&consoles_status);
        let refresh = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text("Refresh").build();
        let sign_out = gtk::Button::builder().label("Sign out").build();
        let header = adw::HeaderBar::new();
        header.pack_start(&refresh);
        header.pack_end(&sign_out);
        stack.add_named(&page_with_header(&consoles_content, Some(header)), Some("consoles"));

        // --- stream ----------------------------------------------------------
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .hexpand(true)
            .vexpand(true)
            .css_classes(["stream-bg"])
            .build();
        let overlay = gtk::Overlay::builder().css_classes(["stream-bg"]).build();
        overlay.set_child(Some(&picture));

        let status_label = gtk::Label::builder().wrap(true).build();
        let status_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .css_classes(["osd", "status-osd"])
            .build();
        status_box.append(&gtk::Spinner::builder().spinning(true).width_request(32).height_request(32).build());
        status_box.append(&status_label);
        overlay.add_overlay(&status_box);

        let stats_label = gtk::Label::builder()
            .halign(gtk::Align::Start)
            .valign(gtk::Align::Start)
            .margin_top(12)
            .margin_start(12)
            .css_classes(["osd", "stats"])
            .visible(false)
            .build();
        overlay.add_overlay(&stats_label);

        let title_label = gtk::Label::builder().css_classes(["heading"]).build();
        let btn = |label: &str, icon: &str| {
            let content = adw::ButtonContent::builder().label(label).icon_name(icon).build();
            gtk::Button::builder().child(&content).build()
        };
        let xbox_btn = btn("Xbox", "go-home-symbolic");
        let stats_btn = btn("Stats", "utilities-system-monitor-symbolic");
        let full_btn = btn("Fullscreen", "view-fullscreen-symbolic");
        let disc_btn = btn("Disconnect", "window-close-symbolic");
        disc_btn.add_css_class("destructive-action");
        let close_btn = btn("Back to game", "go-down-symbolic");
        let buttons = gtk::Box::builder().spacing(8).build();
        for b in [&xbox_btn, &stats_btn, &full_btn, &disc_btn, &close_btn] {
            buttons.append(b);
        }
        let menu_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(8)
            .css_classes(["osd", "stream-menu"])
            .build();
        menu_box.append(&title_label);
        menu_box.append(&buttons);
        menu_box.append(
            &gtk::Label::builder()
                .label("Hold View + Menu, press Esc or tap the screen to open this menu")
                .css_classes(["dim-label", "caption"])
                .build(),
        );
        let menu = gtk::Revealer::builder()
            .transition_type(gtk::RevealerTransitionType::SlideUp)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::End)
            .margin_bottom(24)
            .child(&menu_box)
            .build();
        overlay.add_overlay(&menu);
        stack.add_named(&overlay, Some("stream"));

        let toasts = adw::ToastOverlay::new();
        toasts.set_child(Some(&stack));
        let window = adw::ApplicationWindow::builder()
            .application(application)
            .title("Greenlight")
            .default_width(1280)
            .default_height(800)
            .content(&toasts)
            .build();

        let ui = Ui {
            w: Rc::new(Widgets {
                window,
                toasts,
                stack,
                login_qr,
                login_code,
                login_url,
                login_status,
                login_retry: login_retry.clone(),
                console_list,
                consoles_status,
                consoles_scroller,
                gamertag,
                picture: picture.clone(),
                status_box,
                status_label,
                stats_label,
                menu,
                menu_first: xbox_btn.clone(),
                title_label,
            }),
            state: Rc::new(RefCell::new(State::default())),
            rt,
            app,
        };

        // --- signals ---------------------------------------------------------
        {
            let ui = ui.clone();
            login_retry.connect_clicked(move |_| ui.start_login());
        }
        {
            let ui = ui.clone();
            import_btn.connect_clicked(move |_| ui.import_greenlight());
        }
        {
            let ui = ui.clone();
            refresh.connect_clicked(move |_| ui.load_consoles());
        }
        {
            let ui = ui.clone();
            sign_out.connect_clicked(move |_| {
                let app = ui.app.clone();
                let ui2 = ui.clone();
                spawn(&ui.rt, async move { app.msal.logout().await }, move |_| ui2.start_login());
            });
        }
        {
            let ui = ui.clone();
            xbox_btn.connect_clicked(move |_| {
                ui.send(SessionCommand::PressNexus);
                ui.show_menu(false);
            });
        }
        {
            let ui = ui.clone();
            stats_btn.connect_clicked(move |_| ui.toggle_stats());
        }
        {
            let ui = ui.clone();
            full_btn.connect_clicked(move |_| ui.toggle_fullscreen());
        }
        {
            let ui = ui.clone();
            disc_btn.connect_clicked(move |_| ui.send(SessionCommand::Disconnect));
        }
        {
            let ui = ui.clone();
            close_btn.connect_clicked(move |_| ui.show_menu(false));
        }
        {
            let ui = ui.clone();
            let click = gtk::GestureClick::new();
            click.connect_released(move |_, _, _, _| {
                let open = ui.w.menu.reveals_child();
                ui.show_menu(!open);
            });
            picture.add_controller(click);
        }
        ui.install_keyboard();
        #[cfg(feature = "gamepad")]
        ui.install_gamepad_navigation();
        ui
    }

    fn start(&self) {
        let in_game_mode = std::env::var_os("SteamGamepadUI").is_some()
            || std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY").is_some();
        self.state.borrow_mut().auto_fullscreen = in_game_mode;
        if in_game_mode {
            self.w.window.fullscreen();
        }
        self.w.window.present();

        if let Ok(demo) = std::env::var("GREENLIGHT_DEMO") {
            self.start_demo(&demo);
            return;
        }
        let app = self.app.clone();
        let ui = self.clone();
        spawn(&self.rt, async move { app.msal.has_user_token().await }, move |signed_in| {
            if signed_in {
                ui.load_consoles();
            } else {
                ui.start_login();
            }
        });
    }

    fn toast(&self, text: &str) {
        let toast = adw::Toast::new(text);
        toast.set_timeout(6);
        self.w.toasts.add_toast(toast);
    }

    // --- sign-in -------------------------------------------------------------

    fn start_login(&self) {
        let w = &self.w;
        w.stack.set_visible_child_name("login");
        w.login_retry.set_visible(false);
        w.login_status.set_text("Requesting a sign-in code…");
        w.login_code.set_text("");
        w.login_url.set_text("");
        w.login_qr.set_paintable(None::<&gdk::Paintable>);
        let msal = self.app.msal.clone();
        let ui = self.clone();
        spawn(&self.rt, async move { msal.start_device_code().await }, move |res| match res {
            Ok(code) => ui.show_device_code(code),
            Err(e) => ui.login_failed(&format!("{e:#}")),
        });
    }

    fn show_device_code(&self, code: DeviceCodeResponse) {
        let w = &self.w;
        let url = code.verification_uri_complete.clone().unwrap_or_else(|| code.verification_uri.clone());
        match qr_texture(&url) {
            Ok(t) => w.login_qr.set_paintable(Some(&t)),
            Err(e) => tracing::debug!("QR code: {e}"),
        }
        w.login_url.set_text(&code.verification_uri);
        w.login_code.set_text(&code.user_code);
        w.login_status.set_text("Waiting for you to sign in…");
        let msal = self.app.msal.clone();
        let ui = self.clone();
        spawn(
            &self.rt,
            async move {
                msal.poll_device_code(&code).await?;
                msal.web_token().await.map(|t| t.gamertag().unwrap_or("").to_string())
            },
            move |res| match res {
                Ok(gt) => {
                    ui.toast(&format!("Signed in as {gt}"));
                    ui.load_consoles();
                }
                Err(e) => ui.login_failed(&format!("{e:#}")),
            },
        );
    }

    fn login_failed(&self, err: &str) {
        self.w.login_status.set_text(&format!("Sign-in failed: {err}"));
        self.w.login_retry.set_visible(true);
    }

    fn import_greenlight(&self) {
        let app = self.app.clone();
        let ui = self.clone();
        self.w.login_status.set_text("Importing sign-in from Greenlight…");
        spawn(&self.rt, async move { app.import_greenlight(None).await }, move |res| match res {
            Ok((_, gt)) => {
                ui.toast(&format!("Signed in as {gt}"));
                ui.load_consoles();
            }
            Err(e) => ui.w.login_status.set_text(&format!("{e:#}")),
        });
    }

    // --- consoles ------------------------------------------------------------

    fn load_consoles(&self) {
        let w = &self.w;
        w.stack.set_visible_child_name("loading");
        let app = self.app.clone();
        let ui = self.clone();
        spawn(
            &self.rt,
            async move {
                let web = app.web().await?;
                let profile = web.profile().await.unwrap_or_default();
                let gamertag = profile.into_iter().find(|(k, _)| k == "Gamertag").map(|(_, v)| v);
                Ok::<_, anyhow::Error>((gamertag, web.consoles().await?))
            },
            move |res| match res {
                Ok((gamertag, consoles)) => ui.show_consoles(gamertag, consoles),
                Err(e) => {
                    let msg = format!("{e:#}");
                    if msg.contains("not signed in") || msg.contains("refreshing user token failed") {
                        ui.start_login();
                    } else {
                        ui.show_consoles_error(&msg);
                    }
                }
            },
        );
    }

    fn show_consoles_error(&self, msg: &str) {
        let w = &self.w;
        w.stack.set_visible_child_name("consoles");
        w.consoles_scroller.set_visible(false);
        w.consoles_status.set_visible(true);
        w.consoles_status.set_title("Couldn't load your consoles");
        w.consoles_status.set_description(Some(msg));
    }

    fn show_consoles(&self, gamertag: Option<String>, consoles: Vec<Console>) {
        let w = &self.w;
        w.stack.set_visible_child_name("consoles");
        w.gamertag.set_text(gamertag.as_deref().unwrap_or(""));
        w.gamertag.set_visible(gamertag.is_some());
        while let Some(child) = w.console_list.first_child() {
            w.console_list.remove(&child);
        }
        if consoles.is_empty() {
            w.consoles_scroller.set_visible(false);
            w.consoles_status.set_visible(true);
            w.consoles_status.set_title("No consoles found");
            w.consoles_status.set_description(Some(
                "Enable remote features on your Xbox: Settings → Devices & connections → Remote features.",
            ));
            return;
        }
        w.consoles_status.set_visible(false);
        w.consoles_scroller.set_visible(true);
        let mut first: Option<gtk::Button> = None;
        for c in consoles {
            let streaming = if c.console_streaming_enabled { "remote play on" } else { "remote play off" };
            let row = adw::ActionRow::builder()
                .title(if c.name.is_empty() { &c.id } else { &c.name })
                .subtitle(format!("{} · {} · {}", c.console_type, c.power_state, streaming))
                .build();
            row.add_prefix(&gtk::Image::from_icon_name("video-display-symbolic"));
            let button = gtk::Button::builder()
                .label("Stream")
                .valign(gtk::Align::Center)
                .css_classes(["suggested-action", "pill"])
                .sensitive(c.console_streaming_enabled)
                .build();
            row.add_suffix(&button);
            row.set_activatable_widget(Some(&button));
            let ui = self.clone();
            let (id, name) = (c.id.clone(), c.name.clone());
            button.connect_clicked(move |_| ui.start_stream(&id, &name));
            if first.is_none() && c.console_streaming_enabled {
                first = Some(button.clone());
            }
            w.console_list.append(&row);
        }
        if let Some(b) = first {
            b.grab_focus();
        }
    }

    // --- streaming -----------------------------------------------------------

    /// Creates gtk4paintablesink on the main thread and wraps it so it can
    /// take GL/dmabuf frames (glsinkbin) or system memory (videoconvert).
    fn make_video_sink(&self) -> Result<gst::Element> {
        let gtksink = gst::ElementFactory::make("gtk4paintablesink").build()?;
        let paintable = gtksink.property::<gdk::Paintable>("paintable");
        self.w.picture.set_paintable(Some(&paintable));
        let has_gl = paintable.property::<Option<gdk::GLContext>>("gl-context").is_some();
        if has_gl && gst::ElementFactory::find("glsinkbin").is_some() {
            Ok(gst::ElementFactory::make("glsinkbin").property("sink", &gtksink).build()?)
        } else {
            let bin = gst::Bin::new();
            let convert = gst::ElementFactory::make("videoconvert").build()?;
            bin.add_many([&convert, &gtksink])?;
            convert.link(&gtksink)?;
            let pad = convert.static_pad("sink").ok_or_else(|| anyhow!("videoconvert without sink pad"))?;
            bin.add_pad(&gst::GhostPad::with_target(&pad)?)?;
            Ok(bin.upcast())
        }
    }

    /// Display size to report: 720p on small screens (Steam Deck), else 1080p.
    fn display_size(&self) -> (u32, u32) {
        let small = gdk::Display::default()
            .and_then(|d| d.monitors().item(0))
            .and_then(|m| m.downcast::<gdk::Monitor>().ok())
            .map(|m| m.geometry().height() <= 800)
            .unwrap_or(false);
        if small { (1280, 720) } else { (1920, 1080) }
    }

    fn start_stream(&self, console_id: &str, name: &str) {
        if self.state.borrow().streaming {
            return;
        }
        let sink = match self.make_video_sink() {
            Ok(s) => s,
            Err(e) => {
                self.toast(&format!("Can't create the video output: {e:#}"));
                return;
            }
        };
        let w = &self.w;
        w.stack.set_visible_child_name("stream");
        w.status_box.set_visible(true);
        w.status_label.set_text(&format!("Starting stream to {name}…"));
        w.title_label.set_text(name);
        w.stats_label.set_visible(false);
        self.show_menu(false);
        if self.state.borrow().auto_fullscreen {
            w.window.fullscreen();
        }

        let (width, height) = self.display_size();
        let opts = SessionOptions {
            player: PlayerConfig { video_sink_element: Some(sink), ..Default::default() },
            client: ClientConfig { width, height, ..Default::default() },
            gamepad: true,
            rumble: true,
            deadzone: 0.08,
            keyframe_interval: 0,
            stop_on_exit: true,
        };
        let (ev_tx, mut ev_rx) = mpsc::unbounded_channel();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        {
            let mut st = self.state.borrow_mut();
            st.commands = Some(cmd_tx);
            st.streaming = true;
            st.stats_on = false;
        }
        let app = self.app.clone();
        let target = console_id.to_string();
        self.rt.spawn(async move {
            let api = match app.home_api().await {
                Ok(mut api) => {
                    api.set_display(width, height);
                    api
                }
                Err(e) => {
                    let _ = ev_tx.send(SessionEvent::Ended { error: Some(format!("{e:#}")) });
                    return;
                }
            };
            let _ = session::run(api, app.msal.clone(), &target, opts, ev_tx, cmd_rx).await;
        });

        let ui = self.clone();
        glib::spawn_future_local(async move {
            while let Some(ev) = ev_rx.recv().await {
                if !ui.on_session_event(ev) {
                    break;
                }
            }
        });
    }

    /// Returns false once the session has ended.
    fn on_session_event(&self, ev: SessionEvent) -> bool {
        let w = &self.w;
        match ev {
            SessionEvent::Status(s) => w.status_label.set_text(&s),
            SessionEvent::Queued { estimated_wait_secs } => w.status_label.set_text(&match estimated_wait_secs {
                Some(s) => format!("In queue, about {} min", (s + 59) / 60),
                None => "In queue…".into(),
            }),
            SessionEvent::Connected => w.status_label.set_text("Connected, starting stream…"),
            SessionEvent::StreamStarted => {
                w.status_box.set_visible(false);
                GtkWindowExt::set_focus(&w.window, None::<&gtk::Widget>);
            }
            SessionEvent::VideoSize { .. } => {}
            SessionEvent::Title(t) => w.title_label.set_text(&t),
            SessionEvent::Dialog { title, text } => self.toast(&format!("{title}: {text}")),
            SessionEvent::Stats(s) => w.stats_label.set_text(&format_stats_multiline(&s)),
            SessionEvent::OverlayRequested => {
                let open = w.menu.reveals_child();
                self.show_menu(!open);
            }
            SessionEvent::Ended { error } => {
                {
                    let mut st = self.state.borrow_mut();
                    st.commands = None;
                    st.streaming = false;
                }
                w.picture.set_paintable(None::<&gdk::Paintable>);
                self.show_menu(false);
                if !self.state.borrow().auto_fullscreen && w.window.is_fullscreen() {
                    w.window.unfullscreen();
                }
                if let Some(e) = error.filter(|e| e != "cancelled") {
                    self.toast(&format!("Stream ended: {e}"));
                }
                self.load_consoles();
                return false;
            }
        }
        true
    }

    fn send(&self, cmd: SessionCommand) {
        if let Some(tx) = &self.state.borrow().commands {
            let _ = tx.send(cmd);
        }
    }

    fn show_menu(&self, open: bool) {
        self.w.menu.set_reveal_child(open);
        if open {
            self.w.menu_first.grab_focus();
        } else {
            GtkWindowExt::set_focus(&self.w.window, None::<&gtk::Widget>);
        }
    }

    fn toggle_stats(&self) {
        let on = {
            let mut st = self.state.borrow_mut();
            st.stats_on = !st.stats_on;
            st.stats_on
        };
        self.w.stats_label.set_visible(on);
        if on {
            self.w.stats_label.set_text("Collecting stats…");
        }
        self.send(SessionCommand::SetStats(on));
    }

    fn toggle_fullscreen(&self) {
        let w = &self.w.window;
        if w.is_fullscreen() {
            w.unfullscreen();
        } else {
            w.fullscreen();
        }
    }

    fn on_stream_page(&self) -> bool {
        self.w.stack.visible_child_name().as_deref() == Some("stream")
    }

    /// Keyboard: forwarded to the console while streaming (menu closed).
    fn install_keyboard(&self) {
        let keys = gtk::EventControllerKey::new();
        keys.set_propagation_phase(gtk::PropagationPhase::Capture);
        {
            let ui = self.clone();
            keys.connect_key_pressed(move |_, key, _, _| {
                if !ui.on_stream_page() || !ui.state.borrow().streaming {
                    return glib::Propagation::Proceed;
                }
                match key {
                    gdk::Key::Escape | gdk::Key::F1 => {
                        let open = ui.w.menu.reveals_child();
                        ui.show_menu(!open);
                        return glib::Propagation::Stop;
                    }
                    gdk::Key::F11 => {
                        ui.toggle_fullscreen();
                        return glib::Propagation::Stop;
                    }
                    _ => {}
                }
                if ui.w.menu.reveals_child() {
                    return glib::Propagation::Proceed; // navigate the menu
                }
                if let Some(name) = key.name() {
                    if crate::input::keyboard::client_action(&name).is_some() {
                        if matches!(crate::input::keyboard::client_action(&name), Some(crate::input::keyboard::ClientAction::ToggleStats)) {
                            ui.toggle_stats();
                        }
                    } else {
                        ui.send(SessionCommand::Key { name: name.to_string(), pressed: true });
                    }
                }
                glib::Propagation::Stop
            });
        }
        {
            let ui = self.clone();
            keys.connect_key_released(move |_, key, _, _| {
                if ui.on_stream_page() && ui.state.borrow().streaming {
                    if let Some(name) = key.name() {
                        ui.send(SessionCommand::Key { name: name.to_string(), pressed: false });
                    }
                }
            });
        }
        self.w.window.add_controller(keys);
    }

    /// Controller navigation for the menus (D-pad / left stick to move, A to
    /// activate, B to go back). Inactive while a stream has the controller.
    #[cfg(feature = "gamepad")]
    fn install_gamepad_navigation(&self) {
        use gilrs::{Axis, Button, EventType, Gilrs};
        let Ok(mut gilrs) = Gilrs::new() else { return };
        let ui = self.clone();
        let mut stick_armed = true;
        glib::timeout_add_local(Duration::from_millis(16), move || {
            while let Some(ev) = gilrs.next_event() {
                let streaming_input = ui.on_stream_page() && !ui.w.menu.reveals_child();
                if streaming_input {
                    continue;
                }
                let dir = match ev.event {
                    EventType::ButtonPressed(Button::DPadUp, _) => Some(gtk::DirectionType::Up),
                    EventType::ButtonPressed(Button::DPadDown, _) => Some(gtk::DirectionType::Down),
                    EventType::ButtonPressed(Button::DPadLeft, _) => Some(gtk::DirectionType::Left),
                    EventType::ButtonPressed(Button::DPadRight, _) => Some(gtk::DirectionType::Right),
                    EventType::AxisChanged(axis @ (Axis::LeftStickX | Axis::LeftStickY), v, _) => {
                        if v.abs() < 0.3 {
                            stick_armed = true;
                            None
                        } else if stick_armed && v.abs() > 0.7 {
                            stick_armed = false;
                            Some(match (axis, v > 0.0) {
                                (Axis::LeftStickX, true) => gtk::DirectionType::Right,
                                (Axis::LeftStickX, false) => gtk::DirectionType::Left,
                                (_, true) => gtk::DirectionType::Up,
                                (_, false) => gtk::DirectionType::Down,
                            })
                        } else {
                            None
                        }
                    }
                    EventType::ButtonPressed(Button::South, _) => {
                        if let Some(w) = gtk::prelude::GtkWindowExt::focus(&ui.w.window) {
                            w.activate();
                        }
                        None
                    }
                    EventType::ButtonPressed(Button::East, _) => {
                        if ui.w.menu.reveals_child() {
                            ui.show_menu(false);
                        }
                        None
                    }
                    _ => None,
                };
                if let Some(d) = dir {
                    ui.w.window.child_focus(d);
                }
            }
            glib::ControlFlow::Continue
        });
    }

    // --- demo / screenshots ------------------------------------------------------

    /// `GREENLIGHT_DEMO=login|consoles|stream` shows a page with sample data
    /// (no network); `GREENLIGHT_SCREENSHOT=file.png` saves the window after
    /// a few seconds and quits. Used to check the UI headlessly.
    fn start_demo(&self, which: &str) {
        match which {
            "login" => self.show_device_code_demo(),
            "consoles" => self.show_consoles(
                Some("GreenlightFan".into()),
                vec![
                    demo_console("F4000DD27A5BF144", "Living Room", "XboxSeriesX", "On", true),
                    demo_console("F4000000000000AA", "Bedroom", "XboxOne", "ConnectedStandby", true),
                    demo_console("F4000000000000BB", "Office", "XboxSeriesS", "Off", false),
                ],
            ),
            _ => self.demo_stream(),
        }
        if let Ok(path) = std::env::var("GREENLIGHT_SCREENSHOT") {
            let ui = self.clone();
            glib::timeout_add_local_once(Duration::from_secs(3), move || {
                if let Err(e) = save_screenshot(&ui.w.window, &path) {
                    eprintln!("screenshot failed: {e:#}");
                } else {
                    eprintln!("screenshot saved to {path}");
                }
                ui.w.window.close();
            });
        }
    }

    fn show_device_code_demo(&self) {
        let w = &self.w;
        w.stack.set_visible_child_name("login");
        if let Ok(t) = qr_texture("https://www.microsoft.com/link") {
            w.login_qr.set_paintable(Some(&t));
        }
        w.login_url.set_text("https://www.microsoft.com/link");
        w.login_code.set_text("ABCD1234");
        w.login_status.set_text("Waiting for you to sign in…");
    }

    fn demo_stream(&self) {
        let Ok(sink) = self.make_video_sink() else { return };
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("videotestsrc").property("is-live", true).build().unwrap();
        let caps = gst::ElementFactory::make("capsfilter")
            .property("caps", gst::Caps::builder("video/x-raw").field("width", 1280).field("height", 720).build())
            .build()
            .unwrap();
        pipeline.add_many([&src, &caps, &sink]).unwrap();
        gst::Element::link_many([&src, &caps, &sink]).unwrap();
        let _ = pipeline.set_state(gst::State::Playing);
        // Keep the pipeline alive for the lifetime of the window.
        unsafe { self.w.window.set_data("demo-pipeline", pipeline) };
        let w = &self.w;
        w.stack.set_visible_child_name("stream");
        w.status_box.set_visible(false);
        w.title_label.set_text("Living Room");
        w.stats_label.set_visible(true);
        w.stats_label.set_text(&format_stats_multiline(&session::StreamStats {
            video_mbps: 18.4,
            fps: 60,
            video_size: Some((1920, 1080)),
            packets_lost: Some(90),
            jitter_ms: Some(3.0),
            rtt_ms: None,
            decoder: Some("vah264dec".into()),
            jitter_buffer_ms: 20,
        }));
        self.state.borrow_mut().streaming = true;
        self.show_menu(true);
    }
}

fn demo_console(id: &str, name: &str, kind: &str, power: &str, streaming: bool) -> Console {
    serde_json::from_value(serde_json::json!({
        "id": id, "name": name, "consoleType": kind, "powerState": power,
        "consoleStreamingEnabled": streaming,
    }))
    .unwrap()
}

fn page_with_header(content: &impl IsA<gtk::Widget>, header: Option<adw::HeaderBar>) -> adw::ToolbarView {
    let view = adw::ToolbarView::new();
    view.add_top_bar(&header.unwrap_or_default());
    view.set_content(Some(content));
    view
}

fn format_stats_multiline(s: &session::StreamStats) -> String {
    let opt = |v: Option<f64>, unit: &str| v.map(|v| format!("{v:.0} {unit}")).unwrap_or_else(|| "n/a".into());
    format!(
        "Video    {} @ {} fps\nBitrate  {:.1} Mbit/s\nRTT      {}\nJitter   {}\nLost     {}\nBuffer   {} ms\nDecoder  {}",
        s.video_size.map(|(w, h)| format!("{w}×{h}")).unwrap_or_else(|| "?".into()),
        s.fps,
        s.video_mbps,
        opt(s.rtt_ms, "ms"),
        opt(s.jitter_ms, "ms"),
        s.packets_lost.map(|l| format!("{l} packets")).unwrap_or_else(|| "n/a".into()),
        s.jitter_buffer_ms,
        s.decoder.as_deref().unwrap_or("?"),
    )
}

/// QR code as a black-on-white texture (8 px per module, 4-module margin).
fn qr_texture(data: &str) -> Result<gdk::MemoryTexture> {
    let code = qrcode::QrCode::new(data.as_bytes())?;
    let n = code.width();
    let colors = code.to_colors();
    let (scale, margin) = (8usize, 4usize);
    let size = (n + 2 * margin) * scale;
    let mut rgb = vec![255u8; size * size * 3];
    for y in 0..n {
        for x in 0..n {
            if colors[y * n + x] == qrcode::Color::Dark {
                for dy in 0..scale {
                    let row = (y + margin) * scale + dy;
                    for dx in 0..scale {
                        let col = (x + margin) * scale + dx;
                        let i = (row * size + col) * 3;
                        rgb[i..i + 3].copy_from_slice(&[0, 0, 0]);
                    }
                }
            }
        }
    }
    Ok(gdk::MemoryTexture::new(
        size as i32,
        size as i32,
        gdk::MemoryFormat::R8g8b8,
        &glib::Bytes::from_owned(rgb),
        size * 3,
    ))
}

fn save_screenshot(window: &adw::ApplicationWindow, path: &str) -> Result<()> {
    let (w, h) = (window.width(), window.height());
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, w as f64, h as f64);
    let node = snapshot.to_node().ok_or_else(|| anyhow!("empty snapshot"))?;
    let renderer = window.renderer().ok_or_else(|| anyhow!("no renderer"))?;
    let texture = renderer.render_texture(&node, Some(&gtk::graphene::Rect::new(0.0, 0.0, w as f32, h as f32)));
    texture.save_to_png(path)?;
    Ok(())
}
