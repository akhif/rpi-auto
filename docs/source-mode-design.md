# Source mode design note (T02)

Design for **Pi mode** (code name **source mode**): the Pi is the Android Auto mobile device and the head unit (HU) is the sink. This note is the M2 risk gate. It contains no implementation code. It was written from a read of this fork at commit `6157edc`, without hardware access.

Evidence tags used below:

- **[repo]**: read in this fork's code.
- **[proto]**: Android Auto protocol behaviour (aasdk, OpenAuto, the `.proto` files in `src/protos/`) that this repo does not exercise today. Confirm each one against DHU logs in M2 step 2.
- **[spike]**: cannot be settled from the code. It needs a test on the Pi.

## 0. Findings that change the plan

1. **The Pi does not advertise services. The HU does.** The phone sends `ServiceDiscoveryRequest` (icons, label, device name). The HU answers with `ServiceDiscoveryResponse`, which lists every channel it offers. In source mode the Pi *receives* that list and chooses which channels to open. The T02 brief asks "what the Pi advertises in service discovery". The answer is nothing beyond a device name and label. [repo: `MESSAGE_SERVICE_DISCOVERY_RESPONSE` is parsed from the HU in `mitm.rs:3101`; the proto is in `protos.proto` `ServiceDiscoveryRequest/Response`]
2. **The certificate gate is mostly already tested by M0.** In drive mode the HU-facing half already completes the TLS handshake with the HU. `ssl_builder(ProxyType::HeadUnit, ..)` loads `md_cert.pem` and `md_key.pem` and acts as the TLS **server**, like a phone. [repo: `ssl_rustls.rs:435`, `mitm.rs:4319`] Source mode uses the same certificate in the same role. If M0 runs with `mitm = true` and the log shows `SSL init complete` for the `HU` side, the HU has already accepted the certificate. Passthrough mode hides this: the real phone's certificate is used and nothing is tested. Add this check to M0 (see Plan notes).
3. **The image cannot encode today.** `board/common/kernel.config` sets `CONFIG_MEDIA_SUPPORT=n`, `CONFIG_SOUND=n`, `CONFIG_DRM=n` and `CONFIG_INPUT_MISC=n`. That removes V4L2 codecs, ALSA loopback, vc4 KMS and uinput (`CONFIG_INPUT_UINPUT` sits under `INPUT_MISC`). `rpi02w_defconfig` also selects the cut-down firmware (`BR2_PACKAGE_RPI_FIRMWARE_VARIANT_PI_CD=y`, `start_cd.elf`), which has no codecs. `config.txt` has `max_framebuffers=0` and `disable_fw_kms_setup=1`. [repo] M1 must fix these. Step S1 below lists them.
4. **`map_album_art_h264.rs` only decodes H.264.** It turns frames into album-art PNGs. The encoder code is in `overlay.rs` and `v4l2_m2m.rs`. [repo]
5. **Drive-mode teardown already re-enumerates USB.** Every pass of the `main.rs` loop calls `usb.init()`, which removes all gadgets, then `enable_usb_if_present()` builds a new accessory session. [repo: `main.rs:1079`, `main.rs:547`] A mode switch is therefore one extra branch in an existing loop. It is not new machinery.
6. **A user Disconnect is already detected.** `MESSAGE_BYEBYE_REQUEST` with reason `USER_SELECTION` from the phone sets `action_requested = Some(Action::Stop)` when `stop_on_disconnect` is on. [repo: `mitm.rs:2981`] `transfer_monitor` turns that into a session end (`proxy.rs:159`). The Bluetooth handshake is then called with `stopped = true` and waits for the phone to reconnect by hand, then clears Stop (`bluetooth.rs`, log line `User-stop cleared, phone reconnected manually`). Source mode reuses that state.

## 1. Session without a phone

The HU-facing half of `proxy()` already contains most of it. The phone-facing half goes away.

Sequence for the HU side, with the source-mode difference noted at each step. All control frames use channel 0.

| # | Step | Today (relay) | Source mode |
| --- | --- | --- | --- |
| 1 | USB: HU runs the AOA handshake, gadget gives bulk pipes | `FfsGadget` → `AccessorySession` [repo `usb_functionfs.rs`] | Unchanged. |
| 2 | HU sends `VERSION_REQUEST` (plain, major/minor) | Forwarded to the phone and the phone's reply is relayed (`mitm.rs:4341`) | Pi answers `VERSION_RESPONSE` itself: same major, minor, status 0. |
| 3 | TLS: HU is the client, Pi is the server | `ssl_builder(HeadUnit)` with `md_*.pem` (`mitm.rs:4372`) | Identical code. The SSL handshake loop is copied or factored out. |
| 4 | Phone sends `AUTH_COMPLETE` (`AuthResponse{status=0}`) | Passes through | Pi sends it right after the handshake. [proto] |
| 5 | Phone sends `SERVICE_DISCOVERY_REQUEST` | Passes through | Pi sends it with `device_name`/`label_text` such as "Kodi" and a small icon. [proto] |
| 6 | HU replies `SERVICE_DISCOVERY_RESPONSE` | Parsed in `pkt_modify_hook` (`mitm.rs:3101`) | Pi parses it into a `HuCaps` struct (see below). |
| 7 | Phone sends `CHANNEL_OPEN_REQUEST{priority, service_id}` on each service channel; HU answers `CHANNEL_OPEN_RESPONSE` | Passes through | Pi opens only the channels in section 2. |
| 8 | Ping both ways, HU-initiated `AUDIO_FOCUS_REQUEST`, `NAV_FOCUS_REQUEST`, `VOICE_SESSION_NOTIFICATION` | Passes through | Handled locally (section 2, control row). |
| 9 | `BYEBYE_REQUEST`/`RESPONSE` | Forwarded | Pi sends it on exit and answers it when the HU sends it. |

The channel number equals the service id from the HU's response. [repo: `build_control_reply_on_channel` and the `transparency:` comment at `mitm.rs:4632`]

`HuCaps`, built from the response:

- video sink service(s): list of `VideoConfiguration` (resolution enum, fps, `width_margin`, `height_margin`, `density`)
- audio sink services by `AudioStreamType` (guidance, system, media, telephony) with their `AudioConfiguration` (rate, bits, channels)
- input source: keycodes supported, `touchscreen[0]` width and height
- sensor source: the sensor types the HU offers
- the set of all other services, for logging only

If the HU lacks a video sink, an `h264` config, or a sensor service with speed, the session ends with a clear log line (`source: HU offers no usable video sink`). Missing speed is a hard stop because of the safety rule in section 2.

New code reuses `Packet`, `endpoint_reader` and `build_control_reply[_on_channel]` from `mitm.rs`. Section 11 lists the visibility changes needed. `pkt_modify_hook` is **not** reused. It assumes two peers (`ProxyType` plus `PacketFlow`) and carries the injection and overlay logic.

## 2. Every channel

Rule: the Pi opens a channel only if it uses it. A channel it never opens never carries traffic. Opening a channel is a Pi decision. The HU's response only offers it. [proto]

| Channel (service in SDR) | Source-mode handling | M2 | Later |
| --- | --- | --- | --- |
| **Control (0)** | Reply to `PING_REQUEST`. Send own ping every 5 s and end the session after 3 missed replies. `AUDIO_FOCUS_REQUEST` from the HU: reply `AUDIO_FOCUS_NOTIFICATION` (GAIN when Kodi plays, LOSS otherwise). `NAV_FOCUS_REQUEST`: reply `NAV_FOCUS_NOTIFICATION` (projected). `VOICE_SESSION_NOTIFICATION`: log and ignore. `CAR_CONNECTED_DEVICES_REQUEST`: reply with an empty list. `BATTERY_STATUS_NOTIFICATION` and `CALL_AVAILABILITY_STATUS`: send a fixed value once if the HU asks. `BYEBYE`: reply and end the session. Unknown message ids: log once at `warn` and keep going. | yes | |
| **Video sink** (`media_sink_service`, H.264) | Section 3. Open the main display only. Ignore auxiliary or cluster displays (`display_type`). | yes | |
| **Media audio** (`AUDIO_STREAM_MEDIA`) | Section 4. | no | M3 |
| **Guidance audio** (`AUDIO_STREAM_GUIDANCE`) | Not opened. Kodi produces no navigation prompts. | no | no |
| **System audio** (`AUDIO_STREAM_SYSTEM_AUDIO`) | Not opened. Optional later: a click sound for touches. | no | stretch |
| **Telephony audio** | Not opened. Calls go over the phone's own Bluetooth link to the RAV4 (SPEC). | no | no |
| **Microphone** (`media_source_service`) | Not opened, so the HU never streams mic data. The Pi never sends `MICROPHONE_REQUEST`. A voice button press (`VOICE_SESSION_NOTIFICATION` or a voice keycode on the input channel) is logged and dropped. | no | no |
| **Input source** | Open. Section 5. Send `KEY_BINDING_REQUEST` for the keycodes Kodi uses (back, enter, d-pad, media keys) and accept the response. In M2, parse `InputReport.touch_event` and log it. | log only | uinput in M3 |
| **Sensor source** | Open. Send `SENSOR_MESSAGE_REQUEST` for `SENSOR_SPEED`, `SENSOR_DRIVING_STATUS_DATA`, `SENSOR_NIGHT_MODE`, `SENSOR_PARKING_BRAKE` and `SENSOR_GEAR` when offered. Add `SENSOR_LOCATION` when the HU offers it and `source_gps` is on. Handle `SENSOR_MESSAGE_BATCH`: `speed_data[].speed_e3` is m/s × 1000. Safety rule below. | speed logged | M3 pause, M6 hardened |
| **Bluetooth** | Not opened. The phone pairs with the RAV4 on its own (SPEC). The Pi's Bluetooth stays dedicated to the phone's AA wireless profile. | no | no |
| **Navigation status** | Not opened. | no | no |
| **Media playback status** | Not opened in M2/M3. Later option: send Kodi's now-playing title and artist so the HU widget shows it. | no | stretch |
| **Phone status** | Not opened. | no | no |
| **Media browser, radio, generic notification, wifi projection** | Not opened. | no | no |
| **Vendor extension** | Not opened. If the HU insists on it, log the service id and payload and decide from the log. [spike] | no | |
| **Any other service id** | Not opened. Logged once at `info` with the raw service id so new protocol areas show up in the log. | no | |

**Safety rule** (SPEC, "Safety"): the Pi pauses video when the HU reports speed above 0 km/h and keeps audio playing. It never enables `video_in_motion`.

- Pause means: send `VIDEO_FOCUS_NOTIFICATION{NATIVE}` so the HU shows its own screen, stop encoding, and keep the audio session running.
- Resume: speed back to 0 for 2 s, then `VIDEO_FOCUS_NOTIFICATION{PROJECTED}`, force an IDR frame, restart encoding.
- Compare `speed_e3 > 0`. Debounce the resume, not the pause.
- Fail safe: no speed sensor, no speed sample within 3 s of the sensor start, or a `SENSOR_MESSAGE_ERROR` all count as **moving**. Video stays paused. `DRIVING_STATUS` with any restriction bit set also counts as moving.
- Source-mode code never reads `cfg.video_in_motion`. If `source_mode != off` and `video_in_motion = true`, startup logs an error and source mode stays off. The sensor-spoofing code in `pkt_modify_hook` (`mitm.rs:2330` onward) is not in the source-mode path.

## 3. Video path

### 3.1 Negotiation

1. From `HuCaps`, pick a `VideoConfiguration`. Prefer 1280×720 at 30 fps. Fall back to 800×480 at 30 fps, then to the smallest offered. Never pick 60 fps or above 720p on the Zero 2 W (SPEC risk: 512 MB RAM).
2. Open the channel. Send `MEDIA_MESSAGE_SETUP{MEDIA_CODEC_VIDEO_H264_BP}`. Wait for `MEDIA_MESSAGE_CONFIG{STATUS_READY, max_unacked, configuration_indices}`. Choose the index of the config picked above. If the HU does not list it, take the first listed index and log it. [proto]
3. HU may send `VIDEO_FOCUS_REQUEST`. Answer `VIDEO_FOCUS_NOTIFICATION{PROJECTED}`. Also send it unsolicited when the session is ready.
4. Send `MEDIA_MESSAGE_START{session_id, configuration_index}`. The Pi chooses `session_id`.
5. Send `MEDIA_MESSAGE_CODEC_CONFIG` (message id `0x0001`) with SPS+PPS in Annex B. Then send `MEDIA_MESSAGE_DATA` (id `0x0000`) frames. Each is `[00 00][8-byte big-endian timestamp in µs][Annex-B access unit]`. [repo: layout at `overlay.rs:4080`]
6. The first frame must be an IDR. Force another IDR on every START, on a return to PROJECTED focus, on resume after a motion pause, and every 2 s (GOP = 60 at 30 fps).

The HU applies `width_margin`/`height_margin` to the UI area, not the codec size. The Pi encodes the full `codec_resolution` and Kodi's skin must keep content inside the margins. This is a Kodi calibration setting derived from the margins, not an encoder concern.

### 3.2 Encoder

- Hardware: `V4l2M2mEncoder::new(path, w, h, qp, gop)`, `encode(&YuvFrame)`, `request_keyframe()`, `header_bytes()` in `v4l2_m2m.rs` (`:1261`, `:1517`, `:1494`, `:1714`). Probe with `probe_v4l2_codec_devices` (`overlay.rs:1149`). [repo]
- Software fallback for desk work: `rusty_h264` Constrained Baseline via `create_baseline_encoder` (`overlay.rs:741`). Needed on a laptop with no V4L2 encoder, so DHU testing works.
- The overlay's per-channel state (`ChannelEncoderState`, `VIDEO_ENCODERS`) is tied to MITM channels. Source mode builds its own small `H264Source` around the already-`pub` `EncoderBackend` enum (`overlay.rs:482`) and makes `create_baseline_encoder` `pub(crate)`.
- SPS/PPS: with the hardware encoder, take `header_bytes()` (or the headers in the first IDR). Never mix a software-built SPS with a hardware stream. The repo already follows this rule (`overlay.rs` `get_sps_pps`). Set `V4L2_CID_MPEG_VIDEO_REPEAT_SEQ_HEADER` if the driver supports it. [spike: `bcm2835-codec` control support on the Zero 2 W]
- Input is a `YuvFrame` (planar YUV420). The encoder dimensions must equal the negotiated config.

### 3.3 Frame source

Two sources, behind one trait `FrameSource { fn next_frame(&mut self) -> Option<YuvFrame> }`.

1. **Test pattern (M2).** Generated in memory at 30 fps: colour bars, a moving box, a frame counter and µs clock burned in (so a photo of the screen shows latency), and a crosshair at the last touch position (so a photo also shows touch mapping). It needs no GPU and works on the desk and in the car.
2. **Kodi (M3). [spike]** Candidates for the Zero 2 W in order of preference. None is verified.
   1. *Kodi on GBM with vc4 KMS plus a forced virtual HDMI connector* (kernel cmdline `video=HDMI-A-1:<w>x<h>@30D`), then capture the CRTC through the **KMS writeback connector** into a dmabuf and feed the encoder. This is the cleanest zero-copy path. Writeback support on the vc4 of the Zero 2 W (bcm2710) must be checked: `modetest -c` should list a `Writeback-1` connector. [spike]
   2. *Kodi on GBM plus a CPU readback* of the scanout framebuffer (DRM `GETFB2`, mmap the dumb buffer, NEON convert to YUV420). Costs about one memcpy and one colour conversion per frame (800×480×4 ≈ 1.5 MB). Tiled buffers (`VC4_T_TILED`) make this fail, in which case force linear scanout. [spike]
   3. *Kodi's own render capture API* (`xbmc.RenderCapture`). Too slow at 30 fps on this CPU. Last resort, only for a slideshow-rate fallback.
   - Not considered: dispmanx snapshot (gone with KMS), X11 or Wayland capture (too heavy for 512 MB).
   - The spike is one script Akhif runs on the Pi: check `/dev/dri`, `modetest -c`, `/dev/video11`. It is listed as M2 step S1 so the answer is known before M3 starts.

Constant 30 fps in M2. For M3, a "send only when the frame changed, plus a 1 s heartbeat" mode can follow once a real source exists.

### 3.4 Keyframes, flow control and acks

- The HU has no keyframe request message. IDR frames come from the Pi's own schedule and the events in 3.1 step 6. `request_keyframe()` is called by the session task.
- The HU sends `MEDIA_MESSAGE_ACK{session_id, ack}` for received frames. The repo's `display.rs` treats `ack` as a cumulative counter (`display.rs:1024`). [repo] Pi code tracks `sent_frames` and `acked`, and treats `sent - acked >= max_unacked` as a full window. Log both interpretations in M2 so the DHU log settles it. [proto]
- **Window full:** do not encode, do not queue. Skip the frame *before* the encoder so reference frames stay valid. If an encoded frame had to be dropped for any reason, force an IDR on the next one. This keeps latency bounded at about one frame.
- **Fragmentation:** an IDR can exceed one frame (`DEFAULT_FIRST_FRAGMENT_PAYLOAD_BYTES = 16_120`). Use the fragmenting helper in `packet_fragment.rs`, as `overlay.rs` does for its injected frames. [repo]
- **Queues:** the existing `MITM_QUEUE_CAPACITY` of 10 comes from the relay. Source mode writes straight to the `IoDevice` from one task and does not need those channels.

## 4. Audio path

Kodi → ALSA loopback → Rust → PCM on the media-audio channel.

1. Kernel: `CONFIG_SOUND=y`, `CONFIG_SND=y`, `CONFIG_SND_ALOOP=y` (or `m` and load at boot). Buildroot: `alsa-lib`. Kodi uses `hw:Loopback,0,0` for playback. aa-proxy-rs captures from `hw:Loopback,1,0`. [spike: confirm `snd-aloop` adds no meaningful boot time]
2. Format: take the media `AudioConfiguration` from the SDR. 48000 Hz, 16-bit, 2 channels is typical. [proto] Kodi's output is set to the same rate in `asound.conf` with the `plug` plugin. If the HU offers only a different media format, resample in ALSA (`plug`) and never in Rust.
3. Channel setup: open the media-audio channel, `SETUP{MEDIA_CODEC_AUDIO_PCM}`, wait for `CONFIG`, send unsolicited `AUDIO_FOCUS_NOTIFICATION{GAIN}` on the control channel, `START`.
4. Data: read 20 ms periods (960 frames, 3840 bytes at 48 kHz stereo 16-bit). Each period is one `MEDIA_MESSAGE_DATA` of `[00 00][timestamp µs][PCM s16le interleaved]`. One DATA fits in one frame, so no fragmentation.
5. Flow control: same `max_unacked` accounting as video. When the window is full, drop the oldest period and count it as an underrun in the log. Do not block the capture or Kodi will stall.
6. Focus: the HU's `AUDIO_FOCUS_REQUEST` with LOSS stops the stream. The Pi sends `STOP`, pauses Kodi over JSON-RPC (`Player.PlayPause`) and replies LOSS. GAIN restores.
7. Audio keeps playing when video is paused for motion (SPEC).
8. Fallback if `snd-aloop` is too heavy: ALSA `file` plugin writing raw PCM to a FIFO that Rust reads. Real-time pacing then depends on the reader, so it needs a spike. [spike]

A/V sync: video and audio share one monotonic µs clock (`CLOCK_MONOTONIC`) for their DATA timestamps. Kodi's audio delay setting absorbs any constant offset measured in M3.

## 5. Touch

1. The HU sends `INPUT_MESSAGE_INPUT_REPORT` on the input channel with `touch_event{pointer_data[{x,y,pointer_id}], action_index, action}`. Actions: `DOWN 0`, `UP 1`, `MOVED 2`, `POINTER_DOWN 5`, `POINTER_UP 6`. [repo protos]
2. Coordinates are in the HU's touchscreen space, `InputSourceService.touchscreen[0].width/height` from the SDR. They are **not** necessarily the video resolution. The Pi scales: `x_video = x * video_w / touch_w`, `y` likewise, and applies the margins from 3.1. In M2 the log prints raw and scaled values.
3. Sink: a uinput device created through `/dev/uinput`. Type B multitouch: `ABS_MT_SLOT`, `ABS_MT_TRACKING_ID`, `ABS_MT_POSITION_X/Y`, `BTN_TOUCH`, with `INPUT_PROP_DIRECT`. Axis ranges are the video size, so Kodi (libinput, evdev) maps them with no extra step. One extra keyboard-class device carries the key events from the key-binding request (back, d-pad, enter, media keys).
4. The kernel needs `CONFIG_INPUT_MISC=y`, `CONFIG_INPUT_UINPUT=y`, `CONFIG_INPUT_EVDEV=y`. `INPUT_MISC` is set to `n` today, which also hides `UINPUT`; `INPUT_EVDEV` is not mentioned in the fragment, so check the resulting `.config`. [repo]
5. Implementation style: hand-rolled ioctls via `libc`, matching `v4l2_m2m.rs`. This adds no dependency.
6. `hu_input.rs` handles only media-key interception for the relay. It is not reused. Touch handling is new, in the source-mode module.

## 6. Mode switching

State machine in `main.rs`, active only when `source_mode == on_disconnect`:

```
DRIVE ──(BYEBYE reason=USER_SELECTION from phone)──▶ STOPPING ──▶ PI_MODE
  ▲                                                                  │
  └────────(phone reconnects: BT profile connected)──── TEARDOWN ◀───┘
```

**Drive → Pi mode** [repo for the first three steps]:

1. The phone's Disconnect arrives as `BYEBYE_REQUEST` with `USER_SELECTION` on the phone-facing proxy. The existing branch at `mitm.rs:2981` sets `action_requested = Some(Stop)` when `stop_on_disconnect` is true. Change the condition to `cfg.stop_on_disconnect || cfg.source_mode == OnDisconnect`. One line, no new logic.
2. `transfer_monitor` returns `Err("action request: Stop")` (`proxy.rs:166`). `io_loop` aborts both proxy tasks, closes the TCP stream, disassociates nothing extra, and sends `need_restart.send(Some(Stop))`. The ByeBye has also been forwarded to the HU, which ends its session.
3. The `main.rs` loop (`:1045`) runs `usb.init()` (`:1079`), which re-enumerates the gadget.
4. **New branch:** before the Bluetooth handshake, if `source_mode_enabled && action_requested == Some(Stop)`:
   - `enable_usb_if_present(..)` to get a fresh accessory session (as drive mode does).
   - Start `source_mode::run_session(accessory, cancel)` as a task.
   - **At the same time** run `bluetooth.aa_handshake(.., stopped = true, ..)`. It already waits for the phone to reconnect manually, so no change is needed in `bluetooth.rs`. The Bluetooth chip and the USB gadget are independent hardware.
   - `select!` on the two:
     - Handshake returns Ok (phone is back): cancel the source session (send `BYEBYE`, close pipes), `usb.rearm_for_next_session(cooldown)` (`usb_functionfs.rs:472`), clear `Stop`, and `continue` into the normal drive path. **Drive always wins.**
     - Source session ends on its own (HU closed it, USB lost, ping timeout): `rearm_for_next_session` and re-enter source mode. Stop after 3 quick failures in 60 s and fall back to waiting for the phone, so the Pi never loops against a bad HU.
5. Start the USB teardown when the Bluetooth profile connects, not when TCP is accepted. Gadget switching takes about 2-3 s (`proxy.rs:54` comment) and the phone is still doing Wi-Fi setup, so this hides most of it. [spike: measure in M4]
6. On leaving Pi mode, pause Kodi (JSON-RPC) so audio does not play into nothing. Kodi itself keeps running. It is not tied to the AA session.

**HU behaviour on ByeBye and re-enumeration is unknown.** This is the SPEC's open question. Some HUs return to their own UI and wait for the next USB attach. Others reset the port. The design above assumes the first. If the HU ignores the new session, the fallback is an *in-session takeover*: do not forward the ByeBye, keep the HU session alive, and have the Pi continue as the phone on the already-open channels (it knows every channel and session id from the relayed SDR and START messages). This avoids re-enumeration but needs the Pi to adopt state it did not create. It is more work and is only worth it if M4 measurements show the plain path failing. [spike]

**Phone setup assumption** (from the SPEC): the phone stays on the Pi's Wi-Fi and Android Auto stays installed but disconnected. A user reconnect is the only drive-mode trigger.

## 7. Config

All keys are flat and default to the stock behaviour. They are added next to `stop_on_disconnect` in `AppConfig` (`config.rs:763`), `Default` (`:1208`) and the TOML writer (`:1577`), grouped as one block per place so upstream merges stay clean.

| Key | Default | Meaning |
| --- | --- | --- |
| `source_mode` | `"off"` | `"off"`: stock aa-proxy-rs. `"on_disconnect"`: Pi mode after a user Disconnect (M4). `"always"`: bring-up and test mode, starts the source session as soon as the USB accessory is up, skips the phone entirely (M2/DHU). |
| `source_fps` | `30` | Frame rate. Capped at 30. |
| `source_encoder` | `"auto"` | `auto`, `v4l2`, `software`. `auto` probes V4L2 and falls back to software. |
| `source_video` | `"test_pattern"` | `test_pattern` or `kodi` (M3). |
| `source_audio_device` | `"hw:Loopback,1,0"` | ALSA capture device (M3). |
| `source_touch_uinput` | `false` | `false`: log touches only (M2). `true`: create the uinput devices (M3). |
| `source_gps` | `false` | Request `SENSOR_LOCATION` and expose it on the web socket. |

Guards at startup:

- `source_mode != off` with `video_in_motion = true`: error, source mode stays off.
- `source_mode != off` with `mitm = false` or `runtime_mitm_failed`: error. Source mode needs the TLS stack.
- With the `io-uring` feature on a real USB gadget the existing refusal in `main.rs:1507` applies. Source mode with `dhu = true` works under either feature set because it is written against the generic `IoDeviceTrait` read and write calls. The Buildroot package builds without `io-uring` anyway.

Keys and certificates stay on the device. Config files in the repo never contain them.

## 8. Desk testing with DHU, no phone

1. On a Linux x86_64 dev machine, build with `cargo build --release`. Config: `dhu = true`, `mitm = true`, `source_mode = "always"`, `source_video = "test_pattern"`, `source_encoder = "software"`.
2. Place `md_cert.pem` and `md_key.pem` in the base config directory by hand (copied from the Pi, never committed). DHU exercises the protocol and the TLS stack but does **not** prove that the Toyota HU accepts the certificate. Only the car does that, and section 0, item 2 explains how M0 covers it.
3. Start `aa-proxy-rs`. With `dhu = true`, `io_loop` waits on the DHU TCP listener (port 5277, `config.rs:29`). In `always` mode there is no phone to wait for, so the source branch skips the Bluetooth and MD steps and goes straight to the HU session.
4. Start `desktop-head-unit` with no arguments (README, "Connecting to Desktop Head Unit (DHU)", step 5).
5. Check, in the log (`/var/log/aa-proxy-rs.log` on the Pi, console on the desk): `SSL init complete`, the SDR dump, one `channel opened` line per channel in section 2, `video: SETUP/CONFIG/START`, the first IDR, steady `acked` counters, and touch lines when clicking in the DHU window. The DHU window should show the moving test pattern with the crosshair following the mouse.
6. DHU's console (type `help`) can toggle day/night and, per its documentation, simulate driving state. Use it to confirm the pause and resume rule from section 2. Confirm the exact commands, since they differ between DHU versions. [spike]
7. **Automated test without DHU:** a `cargo test` that starts the session against a fake HU on loopback. The fake HU is a rustls client with a self-signed certificate generated at test time, replying with a canned `ServiceDiscoveryResponse`. It checks the handshake, channel opens, the video message sequence, ack accounting, window-full drops and speed-based pause. No key material is stored in the repo.
8. DHU cannot test: USB re-enumeration, the real encoder, uinput into Kodi, Toyota's certificate check, or the HU's reaction to ByeBye. Those are on-device tests.

## 9. Risks and unknowns, ranked

| # | Risk | Why it matters | How it gets settled |
| --- | --- | --- | --- |
| 1 | **The Toyota HU rejects or restricts a phone-less session.** The TLS certificate part is mostly covered by M0 (section 0, item 2). What remains is whether the HU trusts a device that sends a service discovery request with no Google Play Services behind it, and whether it demands channels (Bluetooth, vendor extension) before showing video. | Kills Pi mode. Drive mode is unaffected. | M2 car test (S4). Log the HU's SDR to learn its channel list first. |
| 2 | **HU reaction to ByeBye and USB re-enumeration** (SPEC open question). | A bad reaction needs the in-session takeover design, which is a large change. | M2 can test it cheaply: start `always` mode, kill the process, restart, see what the HU does. M4 measures the real switch. |
| 3 | **No known-good frame capture path on the Zero 2 W's vc4** (writeback or scanout readback). | Blocks M3. Does not block M2, which uses the test pattern. | Spike script in S1; answer before M3. |
| 4 | **Image lacks codecs, sound, uinput** (section 0, item 3). | Blocks the hardware encoder, audio and touch. | Kernel and firmware changes in S1. Cheap to do, but each one needs a boot test. |
| 5 | **Hardware encoder compatibility.** `bcm2835-codec` profile, repeated SPS/PPS and latency on the Zero 2 W are untested here. The HU decoder may reject output the DHU accepts. | A stream the car cannot decode shows a black screen. | Software Baseline encoder at M2 in the car as a control (it is known-good for the overlay); then hardware. |
| 6 | **512 MB RAM** with Kodi, encoder buffers, `wasmtime` (default feature) and the proxy. | Stutter or OOM. | Build the Pi image with `--no-default-features` (drop `wasm-scripting`) if memory is tight; measure in M3. |
| 7 | **Ack semantics** (cumulative counter or per-frame). | A wrong window calculation stalls or floods the HU. | M2 logs both against DHU. |
| 8 | **Audio underruns and A/V sync** through `snd-aloop`. | Choppy audio. | M3 tuning of period size and Kodi audio delay. |
| 9 | **Concurrent Bluetooth handshake and source session** in the `main.rs` loop. | Mode switch races. | M4; the design keeps both under one `select!` and one cancellation token. |
| 10 | Safety rule gaps: HU reports speed late, or never. | Video playing while moving. | Fail-safe rule in section 2 (no sample counts as moving). Tested with DHU and in M6. |

## 10. M2 implementation steps

Four task-sized steps for the planner. S1 and S2 can run in parallel. S3 needs S2. S4 needs S1 and S3.

### S1: Image prerequisites and hardware spike (may overlap M1)

- Enable in `board/common/kernel.config` for the `rpi02w` build: `CONFIG_MEDIA_SUPPORT`, V4L2 mem2mem and `bcm2835-codec`, `CONFIG_SOUND` + `CONFIG_SND_ALOOP`, `CONFIG_INPUT_MISC` + `CONFIG_INPUT_UINPUT` + `CONFIG_INPUT_EVDEV`, and the vc4 KMS driver if the spike picks the KMS route. Keep every other board's config unchanged.
- In `rpi02w_defconfig` switch to the full firmware variant (remove `..._PI_CD`) and raise `gpu_mem_512` in `board/rpi/config.txt` for the codec.
- Add `board/rpi/scripts/source-spike.sh` that prints: `/dev/video*` and their capabilities, `/dev/dri`, `modetest -c` connector list (looking for `Writeback`), `/dev/uinput`, `aplay -l` (looking for `Loopback`), free memory, and boot time.
- **Done when:** the image boots on the Zero 2 W, drive mode still connects the phone, the script output is pasted into the PR, and `/dev/video11` (or the right encoder node), `/dev/uinput` and the Loopback card exist. The PR states which of the 3.3 capture routes the hardware allows.

### S2: Source session core (no media)

- New module `src/source_mode/` (`mod.rs`, `session.rs`, `caps.rs`), registered in `lib.rs`. Config keys from section 7, with the guards. Implements section 1 and the control channel and sensor rows of section 2. Opens and parses all channels but streams no media. `source_mode = "always"` is wired into `main.rs` and `proxy.rs` as the smallest branch that reaches `run_session`.
- Visibility edits in `mitm.rs`: see section 11.
- Fake-HU loopback test from section 8, item 7.
- **Done when:** with `source_mode = "off"` all existing tests pass and behaviour is byte-identical to upstream. With `always` and DHU, the session stays up for 5 minutes, answers pings, logs the HU's channel list and sensor data, and ends cleanly with `BYEBYE` from either side. The fake-HU test passes in CI.

### S3: Video test pattern, touch log (desk-complete)

- `src/source_mode/video.rs` (negotiation, `H264Source`, test pattern, send loop, window accounting, fragmentation, IDR rules from section 3) and `src/source_mode/input.rs` (input channel open, key-binding request, touch parsing and scaling, log only).
- **Done when:** on the desk with DHU and the software encoder, the moving test pattern shows at 30 fps for 10 minutes with no stall, the window never exceeds `max_unacked`, the log shows the first IDR and steady ack counters, mouse clicks in DHU log raw and scaled coordinates, and the crosshair follows them. On the Pi (hardware encoder, DHU over USB as in the README), the same test runs and the encoder path logs `V4L2 M2M hardware encoder`.

### S4: Car bring-up and the M2 gate

- Add the motion-pause rule (sensor handling and focus switching from section 2) so the car test is safe to run. Add an `on-device test plan` for the RAV4 and a triage table: no TLS → "certificate rejected", TLS ok but no SDR → "session rejected", video setup refused, black screen with acks flowing → "decoder rejects stream" (retry with the software encoder).
- **Done when:** the pattern shows on the RAV4 screen, touches are logged with coordinates, the speed sensor is read, and the pattern pauses once the car moves and resumes once it stops. Or the PR records, with the log, which stage the HU refused. A refusal ends Pi mode and leaves drive mode as-is (SPEC M2 exit).

## 11. Files and functions to change

Existing files (small hooks only; everything else is new code in `src/source_mode/`):

| File | Change |
| --- | --- |
| `src/lib.rs` | `pub mod source_mode;` |
| `src/mitm.rs:153` | `KEYS_PATH` → `pub(crate)` |
| `src/mitm.rs:1063,1077,1091,1113` | `Packet::{encrypt_payload, decrypt_payload, transmit, ssl_decapsulate_write}` → `pub(crate)` |
| `src/mitm.rs:4129,4229,4242` | `ssl_encapsulate`, `build_control_reply`, `build_control_reply_on_channel` → `pub(crate)` |
| `src/mitm.rs:2982` | ByeBye condition: `stop_on_disconnect \|\| source_mode == OnDisconnect` |
| `src/overlay.rs:741` | `create_baseline_encoder` → `pub(crate)` |
| `src/config.rs:763,1208,1577` | `source_*` keys (section 7) and the startup guards |
| `src/main.rs:1045-1120` | Source branch in the loop (section 6, step 4). `always` mode skips the Bluetooth handshake. |
| `src/proxy.rs` | None in M2. `io_loop` stays the drive-mode path. |
| `board/common/kernel.config`, `configs/rpi02w_defconfig`, `board/rpi/config.txt` | S1 |

Reused unchanged: `ssl_rustls::ssl_builder` and `AaConnection`, `endpoint_reader` (`mitm.rs:4156`), `usb_functionfs.rs` (`FfsGadget`, `AccessorySession`, `rearm_for_next_session`), `v4l2_m2m.rs` (`probe_v4l2_codec_devices`, `V4l2M2mEncoder`), `packet_fragment.rs`, the generated protobuf types.

New files (names are proposals):

- `src/source_mode/mod.rs`: config glue, `run_session` entry, supervisor used by `main.rs`.
- `src/source_mode/session.rs`: version reply, TLS server handshake, auth, SDR, channel opens, control-channel dispatcher, ping.
- `src/source_mode/caps.rs`: `HuCaps` parsing from the SDR.
- `src/source_mode/video.rs`: negotiation, `FrameSource`, test pattern, `H264Source`, send loop.
- `src/source_mode/audio.rs` (M3): ALSA capture, PCM channel.
- `src/source_mode/input.rs`: touch and key parsing; uinput in M3.
- `src/source_mode/sensors.rs`: sensor requests, speed state, motion pause.
- `tests/source_session.rs`: fake-HU loopback test.
