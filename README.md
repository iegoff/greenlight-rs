# greenlight-rs

A Rust port of [Greenlight](https://github.com/unknownskl/greenlight), the open-source xCloud and Xbox home streaming client. It replaces the Electron/Chromium WebRTC stack with **GStreamer `webrtcbin`**. The result is a single native binary with a CLI and a GStreamer video window.

_Disclaimer: not affiliated with Microsoft, Xbox, Greenlight or Moonlight. All trademarks belong to their owners._

> **Status: experimental.** The protocol code is a straight port of Greenlight and `xbox-xcloud-player`. It is covered by unit tests and a WebRTC loopback test (see [Testing](#testing)). It has **not yet been tested against real Xbox / xCloud servers**: no account or console was available while it was written. Reports and fixes are very welcome.

## Features

- Microsoft account sign-in with the device-code flow (port of `xal-node`'s MSAL flow). Tokens are stored in a JSON file compatible with `xal-node`.
- **Home streaming** from your Xbox One / Series console, including the console-transfer token (`/connect`) step.
- **xCloud** streaming, with queue/wait-time handling and region selection.
- WebRTC through `webrtcbin`:
  - Opus audio (stereo) and H.264 video, with a configurable profile preference (main / constrained baseline / baseline).
  - The four xCloud data channels: `message`, `control`, `input`, `chat`.
- Binary input protocol: gamepad reports, video-frame latency metadata, keyboard and mouse frames. The client also parses rumble and server-metadata reports.
- Physical gamepads and **rumble** through [`gilrs`](https://crates.io/crates/gilrs).
- Keyboard controls, captured from the video window through GStreamer navigation events.
- Teredo candidate expansion for home streaming without IPv6, plus optional STUN.
- Profile, friends list, console list and xCloud title list, with names from the Game Pass catalog.

## Install

### Flatpak (recommended, works on Steam Deck)

Download `greenlight-rs.flatpak` from the [releases page](https://github.com/iegoff/greenlight-rs/releases), then:

```bash
flatpak install --user greenlight-rs.flatpak
```

The bundle points at Flathub, so the `org.freedesktop.Platform` 26.08 runtime is installed automatically. That runtime includes GStreamer 1.28 with WebRTC, VA-API hardware decoding and Wayland/GL sinks. Run the app with:

```bash
flatpak run io.github.iegoff.greenlight_rs --help
```

To build the Flatpak yourself:

```bash
flatpak install --user flathub org.freedesktop.Sdk//26.08 org.freedesktop.Sdk.Extension.rust-stable//26.08
flatpak-builder --user --install --force-clean build-dir flatpak/io.github.iegoff.greenlight_rs.yml
```

Crates are vendored through `flatpak/cargo-sources.json`, so the build runs offline, as Flathub requires. After changing dependencies, regenerate it:

```bash
python3 flatpak/gen-cargo-sources.py Cargo.lock > flatpak/cargo-sources.json
```

### Steam Deck

1. In **Desktop Mode**, open Konsole and install the Flatpak as above.
2. Sign in and find your console ID:
   ```bash
   flatpak run io.github.iegoff.greenlight_rs login
   flatpak run io.github.iegoff.greenlight_rs consoles
   ```
   `login` shows a QR code: scan it with your phone, sign in, and enter the code shown on the Deck. If Greenlight is already signed in on the Deck, run `import-greenlight` instead of `login`.
3. Open Steam → **Games → Add a Non-Steam Game**, tick **Greenlight (Rust)**, and add it.
4. Launch it from Game Mode. It opens fullscreen on the console list; pick your console with the controller and press A.
   To skip the list and stream straight away, append a command to the shortcut's **Launch options**, after the Flatpak arguments Steam pre-fills, for example `home F4XXXXXXXXXXXXXX`.
5. While streaming, hold **View + Menu** for a second to open the overlay menu. Steam Input exposes the Deck's controls as an Xbox controller. The Steam button opens Steam's menu, so map a back button (e.g. L4) to the keyboard key **N** for the Xbox button.

If there's no video, add `--video-sink waylandsink` or `--video-sink glimagesink` to the launch options. For logs, run it from Konsole: `flatpak run --env=GREENLIGHT_LOG=greenlight=debug io.github.iegoff.greenlight_rs home F4XXXXXXXXXXXXXX`.

### From source

Requirements:

- Rust 1.80 or newer.
- GStreamer 1.22 or newer, with the `base`, `good`, `bad` (webrtc, sctp, dtls, srtp) and `libav` plugins, and `libnice`.

Debian/Ubuntu:

```bash
sudo apt install libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev libgstreamer-plugins-bad1.0-dev \
  gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-libav \
  gstreamer1.0-nice libudev-dev pkg-config
```

Fedora:

```bash
sudo dnf install gstreamer1-devel gstreamer1-plugins-base-devel gstreamer1-plugins-bad-free-devel \
  gstreamer1-plugins-good gstreamer1-libav libnice-gstreamer1 systemd-devel
```

Then build:

```bash
cargo install --path .
```

Run `cargo build --no-default-features` to build without gamepad support (no `libudev` dependency).

## Usage

### Graphical interface

Run `greenlight` without a command (or start **Greenlight (Rust)** from your app menu or Steam):

- **Sign in:** scan the QR code with your phone and enter the code shown, or import an existing Greenlight sign-in.
- **Your consoles:** pick a console and press **Stream**.
- **While streaming:** open the menu with **Esc**/**F1**, a tap or click on the video, or by **holding View + Menu for a second** on the controller. From there:
  - **Xbox:** press the Xbox (guide) button;
  - **Stats:** show resolution, frame rate, bitrate, jitter, lost packets and the decoder;
  - **Fullscreen** (or **F11**);
  - **Disconnect.**
- The menus can be driven with a controller: D-pad or left stick to move, A to select, B to go back.
- Under Steam's Game Mode the window is fullscreen automatically.

### Command line


```bash
greenlight login                  # device-code sign in (with a QR code)
greenlight consoles               # list consoles (id, power state, streaming enabled)
greenlight home F4000000000000000 # stream from a console
greenlight titles --search halo   # list xCloud titles
greenlight cloud <TITLE_ID>       # stream an xCloud title
```

### Signing in

| Command | What it does |
|---|---|
| `greenlight login` | Device-code sign in. Shows a URL, a code and a QR code of the URL, so you can sign in from your phone. Add `--no-qr` to hide the QR code. |
| `greenlight import-greenlight` | Reuses the sign-in of an installed [Greenlight](https://github.com/unknownskl/greenlight) desktop app, native or Flatpak. Use `--from <path>` to point at its `config.json` yourself. Only the sign-in current Greenlight versions use can be imported; tokens from Greenlight's older XAL sign-in can't be reused (Greenlight asks you to sign in again in that case; after that, the import works). If the imported sign-in doesn't work, your previous one is kept. |
| `greenlight logout` | Removes the stored sign-in. |

Greenlight keeps its own copy of the sign-in after an import. If one of the two apps later reports that you're signed out, sign in to that one again.

The QR code is drawn for a dark terminal. On a light background some phone scanners can't read it; use `--no-qr` and open the URL by hand.

Useful stream options (`greenlight home --help` lists all of them):

| Option | Description |
|---|---|
| `--profiles high,medium,low` | H.264 profile preference order |
| `--video-bitrate <kbps>` / `--audio-bitrate <kbps>` | Max bitrate (adds `b=AS:` to the offer) |
| `--resolution 1280x720` | Dimensions reported to the server. Lower values reduce bandwidth and lag over the internet |
| `--video-sink "glimagesink"` | Any GStreamer sink description, e.g. `waylandsink fullscreen=true` |
| `--fullscreen` | Fullscreen window where the sink supports it (e.g. `waylandsink`); Steam Game Mode is always fullscreen |
| `--video-sync` | Sync video to the clock (smoother, more latency). By default frames are shown as soon as they're decoded |
| `--low-latency` | Also play audio without clock sync |
| `--latency <ms>` | Jitter-buffer size (default 100) |
| `--stun stun://host:port` | STUN server for ICE. Defaults to Microsoft's (`relay.communication.microsoft.com:3478`), as xbox.com uses; `none` disables it |
| `--no-gamepad`, `--no-rumble`, `--deadzone 0.1` | Input tweaks |
| `--keep-session` | Don't stop the remote session on exit |

Global options:

- `--force-region-ip <ip>`: sent as `X-Forwarded-For`, to pick another region (same as Greenlight's setting).
- `--locale en-US`

Settings can also be stored in `settings.json` inside the config directory, using the keys `force_region_ip`, `locale` and `xcloud_region`. The config directory is `~/.config/greenlight` on Linux; override it with `GREENLIGHT_CONFIG_DIR`. Set `GREENLIGHT_LOG=greenlight=debug` for verbose logs, including the SDP.

### Keyboard controls

These match Greenlight's defaults:

```
D-pad: arrow keys    A: A / Enter    B: B / Backspace    X: X    Y: Y
Nexus: N             LB: [           RB: ]               LT: -   RT: =
View: V              Menu: M         LS: L               RS: R
~ : toggle stats     F5: request keyframe                F12: quit
```

## Architecture

| Module | Ported from |
|---|---|
| `auth` | `xal-node` `Msal` / `TokenStore` (device code → XSTS → GSSV → streaming tokens) |
| `api::xcloud` | `desktop/main/helpers/xcloudapi.ts`, `player/src/server` (play, state, SDP/ICE exchange, connect, keepalive) |
| `api::webapi`, `api::catalog` | `xbox-webapi` providers, `titlemanager.ts` |
| `session` | `streammanager.ts` and the stream page's signalling flow |
| `channels` | `player/src/client/lib/channel/*` (message handshake, control auth, transactions) |
| `input::packet` | `channel/input/packet.ts` (binary input reports) |
| `player` | `RTCPeerConnection` + `<video>` → `webrtcbin` + `decodebin` + auto sinks |
| `teredo`, `sdp` | `teredo.ts`, `ice.ts`, `sdp.ts` |

The data-channel protocol (`channels`) doesn't depend on the transport, so it can be unit-tested without WebRTC.

## Testing

```bash
cargo test
```

- **Unit tests** cover:
  - input packet layout, checked against the layout of Greenlight's TypeScript encoder;
  - rumble and server-metadata parsing;
  - Teredo decoding (RFC 4380 vector);
  - SDP munging and ICE candidate JSON;
  - token store compatibility, plus token and API response parsing;
  - the message-channel handshake and transactions.
- **`tests/loopback.rs`** plays a fake "console": a second `webrtcbin` that answers the client's offer, streams H.264 + Opus, and runs the server side of the message handshake. It checks that:
  - ICE connects and all four data channels open with the right protocols;
  - the handshake triggers the control authorization and the client-metadata input report;
  - gamepad/metadata input reports flow;
  - **decoded** audio and video reach the sinks.

  The loopback test passes both on the host's GStreamer 1.24 and inside the Flatpak runtime's GStreamer 1.28.

Not covered: the HTTP calls to Microsoft's real endpoints, and interoperability with real Xbox/xCloud servers.

## License

MIT, like the original Greenlight. See [LICENSE](LICENSE).
