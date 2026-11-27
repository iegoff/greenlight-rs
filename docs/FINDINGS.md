# Findings: getting Xbox remote play working in greenlight-rs

This file records what we learned while bringing greenlight-rs from "builds and passes tests" to "streams from a real Xbox to a Steam Deck over the internet".

Each section has the symptom, how we found the cause, and the fix, so nobody has to rediscover it. Commit ids refer to `main`.

The test setup throughout: the Steam Deck was on one home network (FRITZ!Box, `192.168.178.x`, ISP with DS-Lite), and the Xbox Series X on another, in a different home (`192.168.31.x`). That makes it real remote play over the internet, with a ~110 ms round trip. Edge (xbox.com/play) and the JS Greenlight were used as references on the same Deck.

---

## 1. Signalling: what the xHome service actually expects

The xbox.com web client (Edge) is the reference. **Greenlight 2.4.2's request formats are partly outdated:** the service accepts them with `202 Accepted` and then silently ignores parts of them. Captured from Edge's DevTools (HAR file) on 2026-11-27.

### Order of calls (web client)

1. `POST /v5/sessions/home/play` → `202`, body has `sessionPath`.
2. `GET …/state` every second until `Provisioned`. `ReadyToConnect` means POST `…/connect` with the MSAL console-transfer token.
3. **`GET …/configuration`**. Greenlight never made this call. Its response contains:
   - `serverDetails.ipAddress`: the console's public IP;
   - `stunServerAddresses: ["stun:relay.communication.microsoft.com:3478"]`;
   - an SRTP key (keep it out of logs).
4. `POST …/sdp` (the offer), then `GET …/sdp` until `200`. The answer is the `exchangeResponse` JSON string.
5. `POST …/ice` (local candidates), then `GET …/ice` until `200`. The remote candidates are the `exchangeResponse` JSON string.
6. `POST …/keepalive` every 30 s once connected. Before streaming starts it returns `400 SessionInUnexpectedState`, which is harmless.

### `POST …/ice` body: the root cause of "ICE never connects" (`e1f5f5b`)

The web client sends:

```json
{"candidates": [
  "{\"candidate\":\"candidate:1 1 udp 2113937151 192.168.178.49 57684 typ host generation 0 ufrag eSGS network-cost 999\",\"sdpMid\":\"0\",\"sdpMLineIndex\":0,\"usernameFragment\":\"eSGS\"}",
  "…",
  "a=end-of-candidates"
]}
```

- The field is `candidates`, plural.
- Each entry is a **JSON-encoded string** of `RTCIceCandidate.toJSON()`, not an object.
- The list ends with the literal `"a=end-of-candidates"`.

Greenlight sends `{"messageType":"iceCandidate","candidate":[{…},…]}`. The service answers `202` but **never forwards those candidates to the console**. So the console never sends a single connectivity check, and ICE fails on both greenlight-rs and Greenlight 2.4.2.

Implemented in `api::xcloud::ice_post_body` (with a unit test).

### Other signalling details copied from the web client

| What | Value | Commit |
|---|---|---|
| Local candidate string | bare `candidate:…`, without the SDP attribute prefix `a=` | `c21ad1c` |
| `X-MS-Device-Info` | browser client: `clientAppId: play.xbox.com`, `clientAppType: browser`, `browserName: edge`, `os.name: linux`, `sdkType: web` | `d76cf35` |
| `/play` settings | `osName: linux`, a random 22-character `clientSessionId` (Greenlight sent `""`) | `d76cf35` |
| `/sdp` body | numeric `requestId: 1`, `chatStream {1,1}`, `isDataChannelOnly: false` | `e1f5f5b` |
| STUN | `stun:relay.communication.microsoft.com:3478` (also returned by `/configuration`) | `52a3149` |

- Remote candidates come back with Microsoft-added duplicates (e.g. `a=candidate:1 1 UDP 100 192.168.31.43 9002 typ host`). They're harmless.
- The `/sdp` answer for home streaming negotiated `input: 10` for Edge. We offer `maxVersion: 9`, because our input encoder implements the v9 report format.

## 2. The SDP offer: make it look like a browser's

Done in `player.rs` and `sdp.rs`.

- **Bundle policy `max-compat`, not `max-bundle`.** With `max-bundle`, webrtcbin offers secondary m-lines as `port 0` plus `a=bundle-only`. A browser (policy "balanced") never does. (`e9ef40c`)
- **Media ids `0`, `1`, `2` on the wire.** webrtcbin names them `audio0`, `video1`, `application2`. **GStreamer ≥ 1.26 rejects a local description whose mids were renamed** ("Transceiver used to created media with mid 0 not found"). So the SDP sent to the console is rewritten to `0/1/2`, the answer is rewritten back before `set-remote-description`, and candidates are posted with `sdpMid` = the m-line index. (`ea417ba`)
- **ICE credentials:** a 4-character `ice-ufrag` and a 24-character password, like Chrome. libnice's defaults are 32 characters with `/` and `+`. Rewriting them in the local SDP *is* accepted by webrtcbin; the loopback test proves it, because ICE wouldn't connect otherwise.
- **Opus:** `stereo=1` only. We used to send `sprop-stereo=1` as well; Greenlight never does, and GStreamer 1.28's stricter answer matching exposed it. (`e1e3e2c`)
- **Transport-wide congestion control:** offer `a=extmap:3 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01` on audio and video (caps field `extmap-3`). The real console answers with it. See §4. (`6e8b624`)
- **Resolution:** the console sent 1920×1080 even when we reported 1280×720, and also when we reported an Android device (`os.name: android`), as the original Greenlight player did. Edge gets 1080p too. Don't spend more time forcing 720p.

## 3. ICE and connectivity

### Network facts (Steam Deck at home A, Xbox at home B)

- **Home streaming is peer-to-peer.** The working pair (from `edge://webrtc-internals`) was server-reflexive ↔ server-reflexive: Deck `89.0.209.88:3962` ↔ Xbox `213.230.78.120:55496`. There's no relay (TURN) server. Edge also didn't need one.
- The Deck's ISP uses **DS-Lite**. Hop 2 is `192.0.0.2` with a 1452-byte path MTU. The two networks aren't routed to each other: pinging `192.168.31.43` from the Deck fails.
- IPv6 was disabled on the Deck's Wi-Fi profile (`ipv6.method=disabled`). It isn't needed: Edge connected over IPv4.
- **The Microsoft region (`UkSouth`, …) doesn't matter for home streaming.** It only hosts signalling; the video flows directly between the two homes. It matters for xCloud.
- **Periodic loss bursts.** The link loses packets in bursts every ~5–10 s, about 0.1% overall. **Edge saw the same bursts**, so it's the link, most likely the Deck's Wi-Fi, not the client.

### libnice and webrtcbin behaviour

- **Without a STUN server the client has no public candidate**, and the console can't reach a Deck behind NAT. Edge used Microsoft's STUN server; we now use it by default.
- **Don't lengthen libnice's check window.** webrtcbin's libnice agent uses *regular* nomination: `nomination-mode` is construct-only, and webrtcbin creates the agent. It only nominates the working pair once **every** pair has finished.
  - We had raised `stun-max-retransmissions` to 8, to imitate the browser's longer checking. The dead pair to the console's private address then took ~64 s to fail, past our connect deadline, while the public pair had succeeded within 100 ms.
  - The default (3 retransmissions, ~2 s) is right. `--ice-timeout` exists, but defaults to 0, meaning libnice's default. (`72dfee6`)
  - libnice also ignored our `stun-initial-timeout` (the first retry still came at 500 ms).
- The diagnosis that cracked it: **"the console never sends a single packet"**, from `NICE_DEBUG=all`. Longer checks didn't change that. Comparing against Edge (§6) showed the console *does* send checks when it receives candidates in the right format.

## 4. Media quality: loss, lag, audio

| Symptom | Cause | Fix | Commit |
|---|---|---|---|
| Laggy | `latency=100` jitter buffer; Edge's video jitter buffer is ~3 ms | video 20 ms (`--latency`) | `44e5b05` |
| Glitches on every loss; resends wasted bandwidth | 3,345 NACKs → 0 repaired packets: with a 110 ms RTT and a 20 ms buffer, a resent packet always arrives too late | NACK off by default (`--nack` to re-enable) | `6e8b624` |
| Browser lowers quality on a bad link, we glitched | no congestion feedback to the console | TWCC header extension (above); the loopback test asserts the sender receives TWCC feedback | `6e8b624` |
| Smeared or broken frames after loss | decoding frames that reference lost data | `rtph264depay wait-for-keyframe=true request-keyframe=true` | `6e8b624` |
| Audio gaps on loss | `opusdec` ignores in-band FEC by default | `use-inband-fec=true plc=true` | `6e8b624` |
| Audio click about once a second | the audio sink "skew"-corrects by skipping or repeating 20 ms (`correct clock skew` warnings) | `slave-method=resample` | `aba851c` |
| Audio underflows | audio shared the 20 ms jitter buffer | separate 60 ms audio jitter buffer (`--audio-latency`), found by payload type 111 | `aba851c` |
| Extra delay | 200 ms PulseAudio ring buffer | `buffer-time=60 ms`, `latency-time=10 ms` | `087ff96` |
| Extra delay; GPU frames copied to the CPU | a CPU `videoconvert` in front of the GUI's GL sink | none for prebuilt sinks (`glsinkbin` uploads dmabuf/VA frames itself) | `087ff96` |
| Frames waiting behind the sink | 3-frame queue | 1-frame leaky queue (newest frame wins) | `087ff96` |
| Software-decoder delay | `avdec_h264` frame threading | `thread-type=slice` | `087ff96` |
| Frames dropped as "too late" | video sink `sync=true` | `sync=false` by default (`--video-sync` restores it) | `47f0e45` |

The stats overlay shows **Latency**: a smoothed time from frame arrival to the video queue, measured in `player.rs::add_latency_probe`. RTT shows "n/a" because webrtcbin doesn't report it for a receive-only peer. Don't invent a ping.

**Where browsers still win:**
- Chrome's audio engine (NetEQ) adapts its buffer continuously and time-stretches audio to hide gaps.
- Its video jitter buffer adapts to network jitter and paces frames evenly.
- Ours are fixed buffers. A "smooth" video mode (slightly more buffer, frames paced to the display) is a possible next step.

## 5. Input

- **The stick Y axis was inverted** (`db43bd9`).
  - Frames follow the browser Gamepad API, as in Greenlight: **up = −1**.
  - The packet encoder negates Y, so up goes on the wire as **+32767**.
  - gilrs reports up as **+1**, so we negate at the gilrs boundary (`input/gamepad.rs`).
  - The packet unit test now asserts "stick up → +32767".
- **Steam Deck Desktop Mode** maps the controller to mouse and keyboard (the desktop layout). The app's virtual gamepad then receives nothing. **Test controllers in Game Mode**, or set the shortcut's controller layout to "Gamepad".
  - Triggers become mouse clicks, so the menu's tap gesture is touch-only.
  - A button can map to Escape, so Escape only *closes* the menu.
  - The menu opens with F1, a touch tap, or View+Menu held for 1 s. Both buttons are masked from the console while held.
- **A keyboard receiver can look like a gamepad**: the NuPhy dongle's "System Control" HID interface. Such devices are filtered by name.
- **Input channel:** Edge creates `reliableinput` and `unreliableinput` (protocol `2.0`, `ordered:false, maxRetransmits:0` for the unreliable one) and negotiates input v10. We use Greenlight's `input` channel (`1.0`, reliable and ordered).
  - On a lossy link, one lost input packet delays the following ones by about one RTT.
  - If controls feel late, try `ordered=false, max-retransmits=0` on `input` first. If the console rejects that, capture Edge's payloads, by wrapping `RTCDataChannel.prototype.send` in DevTools, before implementing v10. Don't guess the format.

## 6. Debugging recipes that worked

### Compare with the browser (the decisive technique)

- **`edge://webrtc-internals`:** open it in one tab, stream in another, then use *Create a WebRTC-Internals dump → Download the PeerConnection updates and stats data*. It gives:
  - the ICE servers, every candidate, the **selected candidate pair**, and request/response counts per direction (`requestsReceived` showed the console *does* send checks to Edge);
  - the inbound-rtp series over time (`packetsLost`, `jitterBufferDelay`, `framesDropped`);
  - the full offer and answer.
- **HAR from DevTools → Network** (filter `sessions`): the exact `/play`, `/sdp`, `/ice` request bodies.
  - **The HAR contains login tokens in its headers.** Extract only bodies, redact token-like strings, and delete the file afterwards.

### Logs

| Goal | Environment |
|---|---|
| app flow, SDP, candidates, signalling responses | `GREENLIGHT_LOG=greenlight=debug` |
| GStreamer warnings | `GST_DEBUG=3` (on 1.28 `GST_DEBUG=2` is quieter) |
| ICE pairs and checks (did *anything* come back?) | `GST_DEBUG=3,webrtcnice*:6 G_MESSAGES_DEBUG=all NICE_DEBUG=all`, then grep `Packet received`, pair state changes, `Retransmissions failed` |
| why an answer rejected an m-line | `GST_DEBUG=webrtcbin:6`, grep `reject`, `compatible`, `intersect` |

In a Flatpak, pass environment variables as `flatpak run --env=NAME=value …`.

**libnice debug output contains ICE passwords:** don't share those logs as-is.

### Tests and headless UI checks

- `cargo test`: unit tests plus `tests/loopback.rs`, a fake console made from a second webrtcbin. It covers:
  - the offer shape (mids, ufrag, extmap);
  - ICE, the four data channels and the handshake;
  - input reports, decoded audio and video, and stats;
  - loss-handling settings, per-stream jitter buffers, and TWCC feedback reaching the sender.
  - `LOOPBACK_DEBUG=1 … -- --nocapture` prints details.
- **Run the tests in all three GStreamer versions** (host 1.24, GNOME 50 runtime 1.26, freedesktop 26.08 runtime 1.28). Their webrtcbin behaviour differs; see §7. Inside the Flatpak, build with `--keep-build-dirs` and run:
  ```bash
  flatpak build --filesystem=$B --env=CARGO_HOME=$B/cargo --env=PATH=/usr/lib/sdk/rust-stable/bin:/app/bin:/usr/bin $BUILD sh -c "cd $B && cargo --offline test --locked"
  ```
- **GUI screenshots without a display:**
  ```bash
  GDK_BACKEND=x11 GREENLIGHT_DEMO=login|consoles|stream|stream-h264 GREENLIGHT_SCREENSHOT=out.png xvfb-run -a greenlight
  ```
  `stream-h264` runs real H.264 decoding into the GTK sink, which is the path that can go black.
- `flatpak run` in a container needs a system D-Bus (`dbus-daemon --system --fork`, after removing a stale `/run/dbus/system_bus_socket`) plus `dbus-run-session`.

## 7. GStreamer version gotchas

- **webrtcbin answer matching got stricter in 1.26/1.28.** The answerer intersects the offer with the transceiver's codec preferences *and* the actual pad caps, including `profile-level-id` and `sprop-stereo`. Our test fixture needed:
  - explicit codec preferences, keyed by pad name (the transceiver kind is unknown until caps flow);
  - a `capssetter` to advertise the offered H.264 level;
  - `format=I420` input, because without it x264 picks High 4:4:4 10-bit;
  - `extmap-3` in its preferences to act as a TWCC sender.
- **1.26+ rejects renamed local mids** (§2).
- **Fixture crashes** (`assertion failed: (trans->stream)` / `_connect_rtpfunnel`) came from pad/m-line order mismatches in the *fake console*, not from the client. Request the answerer's sink pads in m-line order (audio, then video).
- **The video sink must accept what the decoder outputs.** Without `videoconvert`, `glsinkbin` can't take 10-bit 4:4:4. The Xbox sends 8-bit 4:2:0 H.264 Main, which works.
- **`autovideosink` picked `glimagesink`**, which can't go fullscreen. On Wayland use `waylandsink`, or the GUI's `gtk4paintablesink`.

## 8. Steam Deck and Flatpak

- **Runtime:** `org.gnome.Platform//50`, for GTK 4.22 and libadwaita. It includes GStreamer 1.26 with webrtc, nice, dtls, srtp, sctp, va and libav. The Rust extension must match its freedesktop base: `rust-stable//25.08`.
- **Permissions:**
  - `--device=all` and `--filesystem=/run/udev:ro`: gilrs needs udev's database to recognise joysticks. Greenlight's own Flatpak does the same.
  - `--device=dri`;
  - read-only access to Greenlight's settings folders, for `import-greenlight`.
- **Offline cargo build:** crates are vendored via `flatpak/cargo-sources.json`. Regenerate it with `flatpak/gen-cargo-sources.py` after any dependency change. (Running flatpak-builder-tools' downloaded script was blocked in this environment.)
- **H.264 decoding:** hardware via VA-API (`vah264dec`) on the Deck. Patented codecs for the software fallback come from the auto-downloaded `org.freedesktop.Platform.codecs-extra`.
- **Game Mode:** detected via `SteamGamepadUI` or `GAMESCOPE_WAYLAND_DISPLAY`; the GUI goes fullscreen.
- In Game Mode the Steam button never reaches the app, hence the View+Menu hold.
- The Steam Virtual Gamepad reports no force feedback, so rumble may not work through it.

## 9. Sign-in

- **Device-code login:** client id `1f907974-e22b-4810-a9de-d9647380c97e`, scope `xboxlive.signin openid profile offline_access`. The response has no `verification_uri_complete`, so the QR code only opens the page and the user still types the code.
- **Importing from Greenlight:**
  - Greenlight stores its sign-in in electron-store `Greenlight/config.json`, under `user.tokenstore`, as a JSON *string*. On Linux that's in `~/.config`; for its Flatpak, in `~/.var/app/io.github.unknownskl.greenlight/config`.
  - Only tokens with the `XboxLive.signin` scope can be reused; old XAL tokens can't.
  - A failed import restores the previous sign-in.

## 10. Open items

- Input lag on lossy links: unreliable input channel (§5).
- Optional smooth video pacing (§4).
- Remember the last console; wake a console that's off (smartglass power-on, still to be researched); reconnect after a brief drop; a settings page.
- Rumble through Steam Input.
