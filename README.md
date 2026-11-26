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

```bash
greenlight login                  # device-code sign in
greenlight consoles               # list consoles (id, power state, streaming enabled)
greenlight home F4000000000000000 # stream from a console
greenlight titles --search halo   # list xCloud titles
greenlight cloud <TITLE_ID>       # stream an xCloud title
```

Useful stream options (`greenlight home --help` lists all of them):

| Option | Description |
|---|---|
| `--profiles high,medium,low` | H.264 profile preference order |
| `--video-bitrate <kbps>` / `--audio-bitrate <kbps>` | Max bitrate (adds `b=AS:` to the offer) |
| `--resolution 1280x720` | Dimensions reported to the server |
| `--video-sink "glimagesink"` | Any GStreamer sink description, e.g. `waylandsink fullscreen=true` |
| `--low-latency` | Render immediately (no A/V sync) |
| `--latency <ms>` | Jitter-buffer size (default 100) |
| `--stun stun://host:port` | STUN server for ICE |
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

Not covered: the HTTP calls to Microsoft's real endpoints, and interoperability with real Xbox/xCloud servers.

## License

MIT, like the original Greenlight. See [LICENSE](LICENSE).
