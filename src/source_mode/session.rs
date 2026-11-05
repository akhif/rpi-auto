//! The source-mode session: the Pi plays the Android Auto mobile device toward the head unit (HU).
//!
//! This is design note step S2 (T05): handshake, service discovery, channel opens, pings, sensor
//! and input parsing. No media is streamed. Every message below is listed, with its direction and
//! the evidence for it, in `docs/source-mode-design.md` section 1a. Keep the two in step.
//!
//! The session is one `select!` loop over the HU's byte stream, a 100 ms tick and a cancel token.
//! It does no spawning, so it runs on any runtime and with either I/O backend: it only needs the
//! generic [`IoDeviceTrait`] read and write calls.

use crate::io_backend::IoDevice as IoDeviceTrait;
use crate::mitm::protos::{
    AuthResponse, ByeByeReason, ByeByeRequest, CarConnectedDevices, ChannelOpenRequest,
    ChannelOpenResponse, ControlMessageType, InputMessageId, InputReport, KeyBindingRequest,
    KeyBindingResponse, MediaMessageId, MessageStatus, PingRequest, PingResponse, SensorBatch,
    SensorMessageId, SensorRequest, SensorResponse, SensorType, ServiceDiscoveryRequest,
    ServiceDiscoveryResponse, VideoFocusNotification,
};
use crate::mitm::{
    build_control_reply, build_control_reply_on_channel, ssl_encapsulate, Packet, ENCRYPTED,
    FRAME_TYPE_FIRST, FRAME_TYPE_LAST, FRAME_TYPE_MASK,
};
use crate::ssl::{AaConnection, SslMemBuf, TlsBackend};
use anyhow::{anyhow, bail, Context, Result};
use log::{debug, error, info, warn};
use protobuf::Message;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use super::caps::HuCaps;

/// `_CONTROL` in `mitm.rs` (private there): set on service-channel lifecycle frames.
const FLAG_CONTROL: u8 = 1 << 2;
/// Plain (unencrypted, unfragmented) frame.
const FLAGS_PLAIN: u8 = FRAME_TYPE_FIRST | FRAME_TYPE_LAST;
/// Encrypted, unfragmented data frame on a service channel.
const FLAGS_DATA: u8 = ENCRYPTED | FRAME_TYPE_FIRST | FRAME_TYPE_LAST;

const MSG_VERSION_REQUEST: u16 = ControlMessageType::MESSAGE_VERSION_REQUEST as u16;
const MSG_VERSION_RESPONSE: u16 = ControlMessageType::MESSAGE_VERSION_RESPONSE as u16;
const MSG_SSL: u16 = ControlMessageType::MESSAGE_ENCAPSULATED_SSL as u16;
const MSG_AUTH_COMPLETE: u16 = ControlMessageType::MESSAGE_AUTH_COMPLETE as u16;
const MSG_SDR: u16 = ControlMessageType::MESSAGE_SERVICE_DISCOVERY_RESPONSE as u16;
const MSG_CH_OPEN_RESPONSE: u16 = ControlMessageType::MESSAGE_CHANNEL_OPEN_RESPONSE as u16;
const MSG_CH_CLOSE: u16 = ControlMessageType::MESSAGE_CHANNEL_CLOSE_NOTIFICATION as u16;
const MSG_PING_REQUEST: u16 = ControlMessageType::MESSAGE_PING_REQUEST as u16;
const MSG_PING_RESPONSE: u16 = ControlMessageType::MESSAGE_PING_RESPONSE as u16;
const MSG_NAV_FOCUS_NOTIFICATION: u16 = ControlMessageType::MESSAGE_NAV_FOCUS_NOTIFICATION as u16;
const MSG_BYEBYE_REQUEST: u16 = ControlMessageType::MESSAGE_BYEBYE_REQUEST as u16;
const MSG_BYEBYE_RESPONSE: u16 = ControlMessageType::MESSAGE_BYEBYE_RESPONSE as u16;
const MSG_VOICE_SESSION: u16 = ControlMessageType::MESSAGE_VOICE_SESSION_NOTIFICATION as u16;
const MSG_AUDIO_FOCUS_NOTIFICATION: u16 =
    ControlMessageType::MESSAGE_AUDIO_FOCUS_NOTIFICATION as u16;
const MSG_CAR_DEVICES_REQUEST: u16 =
    ControlMessageType::MESSAGE_CAR_CONNECTED_DEVICES_REQUEST as u16;

/// Keycodes Kodi uses (Android `KEYCODE_*`): back, d-pad, enter, media keys.
const KODI_KEYCODES: [i32; 12] = [4, 19, 20, 21, 22, 23, 66, 85, 87, 88, 126, 127];

#[derive(Debug, Clone)]
pub struct SessionParams {
    /// Directory holding `md_cert.pem` and `md_key.pem`.
    pub keys_path: String,
    pub tls_backend: TlsBackend,
    pub ping_interval: Duration,
    /// Unanswered pings in a row that end the session.
    pub ping_max_missed: u32,
    /// How long to wait for `AUTH_COMPLETE` before sending the service discovery request anyway.
    pub auth_wait: Duration,
    /// Limit for each setup step (version, handshake, discovery, channel opens).
    pub step_timeout: Duration,
    /// How long to wait for `BYEBYE_RESPONSE` after sending our own `BYEBYE_REQUEST`.
    pub bye_wait: Duration,
    pub device_name: String,
    pub source_gps: bool,
}

impl SessionParams {
    pub fn new(keys_path: &str, tls_backend: TlsBackend, source_gps: bool) -> Self {
        Self {
            keys_path: keys_path.to_string(),
            tls_backend,
            ping_interval: Duration::from_secs(5),
            ping_max_missed: 3,
            auth_wait: Duration::from_secs(3),
            step_timeout: Duration::from_secs(10),
            bye_wait: Duration::from_secs(1),
            device_name: "Kodi".to_string(),
            source_gps,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndReason {
    /// The HU sent `BYEBYE_REQUEST`; we answered it.
    HuByeBye,
    /// We were cancelled and said goodbye.
    Cancelled,
    /// The HU did not answer `ping_max_missed` pings in a row.
    PingTimeout,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub end: EndReason,
    pub caps: HuCaps,
    /// Channels the HU accepted, in open order.
    pub opened: Vec<u8>,
    pub auth_complete_seen: bool,
    pub pings_sent: u32,
    pub pings_answered: u32,
    pub hu_pings_answered: u32,
    pub speed_samples: u32,
    pub last_speed_e3: Option<i32>,
    pub sensor_batches: u32,
    pub touch_events: u32,
    pub key_binding_ok: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum Kind {
    Video,
    Sensor,
    Input,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    AwaitVersion,
    Handshake,
    AwaitAuth {
        since: Instant,
    },
    AwaitSdr,
    Opening,
    Running,
    /// We sent `BYEBYE_REQUEST` and wait for the answer.
    Closing {
        since: Instant,
    },
}

/// A complete (reassembled, decrypted) message.
struct Msg {
    channel: u8,
    flags: u8,
    payload: Vec<u8>,
}

impl Msg {
    fn id(&self) -> Option<u16> {
        self.payload
            .get(0..2)
            .map(|b| u16::from_be_bytes([b[0], b[1]]))
    }

    fn body(&self) -> &[u8] {
        self.payload.get(2..).unwrap_or(&[])
    }
}

fn now_us() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_micros() as i64
}

fn with_id(id: u16, body: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(2 + body.len());
    v.extend_from_slice(&id.to_be_bytes());
    v.extend_from_slice(body);
    v
}

/// Encrypted data message on a service channel (sensor, input, media, ...).
fn data_packet(channel: u8, id: u16, body: &[u8]) -> Packet {
    Packet {
        channel,
        flags: FLAGS_DATA,
        final_length: None,
        payload: with_id(id, body),
    }
}

fn kmh(speed_e3: i32) -> f64 {
    speed_e3 as f64 * 3.6 / 1000.0
}

struct Session<R: IoDeviceTrait, W: IoDeviceTrait> {
    reader: R,
    writer: W,
    ssl: AaConnection,
    mem: SslMemBuf,
    params: SessionParams,
    /// Bytes read from the HU, not yet parsed into frames.
    rbuf: Vec<u8>,
    /// Partial fragmented messages by channel: (flags of first frame, decrypted bytes).
    frags: HashMap<u8, (u8, Vec<u8>)>,
    phase: Phase,
    phase_deadline: Instant,
    caps: HuCaps,
    pending_opens: HashMap<u8, Kind>,
    channels: HashMap<Kind, u8>,
    warned: HashSet<(u8, u16)>,
    ping_outstanding: u32,
    next_ping: Instant,
    summary: SessionSummary,
}

impl<R: IoDeviceTrait, W: IoDeviceTrait> Session<R, W> {
    async fn send(&mut self, mut pkt: Packet) -> Result<()> {
        pkt.encrypt_payload(&mut self.mem, &mut self.ssl)
            .await
            .map_err(|e| anyhow!("{e}"))?;
        let expected = 4 + pkt.final_length.map(|_| 4).unwrap_or(0) + pkt.payload.len();
        let written = pkt
            .transmit(&mut self.writer)
            .await
            .context("write to HU")?;
        if written != expected {
            bail!("short write to HU: {written} of {expected} bytes");
        }
        Ok(())
    }

    async fn read_more(&mut self) -> Result<()> {
        let mut chunk = vec![0u8; 16 * 1024];
        let n = self
            .reader
            .read_data(&mut chunk)
            .await
            .context("read from HU")?;
        if n == 0 {
            bail!("HU closed the connection");
        }
        self.rbuf.extend_from_slice(&chunk[..n]);
        Ok(())
    }

    fn parse_frame(&mut self) -> Option<Packet> {
        let b = &self.rbuf;
        if b.len() < 4 {
            return None;
        }
        let channel = b[0];
        let flags = b[1];
        let len = u16::from_be_bytes([b[2], b[3]]) as usize;
        let first_only = flags & FRAME_TYPE_MASK == FRAME_TYPE_FIRST;
        let header = if first_only { 8 } else { 4 };
        if b.len() < header + len {
            return None;
        }
        let final_length = first_only.then(|| u32::from_be_bytes([b[4], b[5], b[6], b[7]]));
        let payload = b[header..header + len].to_vec();
        self.rbuf.drain(..header + len);
        Some(Packet {
            channel,
            flags,
            final_length,
            payload,
        })
    }

    /// Next complete message, if the buffered bytes hold one.
    async fn next_message(&mut self) -> Result<Option<Msg>> {
        while let Some(mut pkt) = self.parse_frame() {
            pkt.decrypt_payload(&mut self.mem, &mut self.ssl)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            match pkt.flags & FRAME_TYPE_MASK {
                FRAME_TYPE_MASK => {
                    return Ok(Some(Msg {
                        channel: pkt.channel,
                        flags: pkt.flags,
                        payload: pkt.payload,
                    }))
                }
                FRAME_TYPE_FIRST => {
                    self.frags.insert(pkt.channel, (pkt.flags, pkt.payload));
                }
                0 => {
                    if let Some((_, buf)) = self.frags.get_mut(&pkt.channel) {
                        buf.extend_from_slice(&pkt.payload);
                    }
                }
                _ => {
                    // LAST
                    if let Some((flags, mut buf)) = self.frags.remove(&pkt.channel) {
                        buf.extend_from_slice(&pkt.payload);
                        return Ok(Some(Msg {
                            channel: pkt.channel,
                            flags,
                            payload: buf,
                        }));
                    }
                }
            }
        }
        Ok(None)
    }

    fn set_phase(&mut self, phase: Phase) {
        self.phase = phase;
        self.phase_deadline = Instant::now() + self.params.step_timeout;
    }

    fn warn_once(&mut self, channel: u8, id: u16, what: &str) {
        if self.warned.insert((channel, id)) {
            warn!(
                "source: {what}: channel {channel:#04x} message {id:#06x}, ignored (logged once)"
            );
        }
    }

    // ---- setup steps ------------------------------------------------------------------------

    async fn on_version_request(&mut self, msg: &Msg) -> Result<()> {
        let body = msg.body();
        if body.len() < 4 {
            bail!("VERSION_REQUEST too short ({} bytes)", body.len());
        }
        let major = u16::from_be_bytes([body[0], body[1]]);
        let minor = u16::from_be_bytes([body[2], body[3]]);
        info!("source: HU requests protocol version {major}.{minor}; accepting it");
        // Plain frame: id, major, minor, status 0 (match).
        let mut reply = with_id(MSG_VERSION_RESPONSE, &major.to_be_bytes());
        reply.extend_from_slice(&minor.to_be_bytes());
        reply.extend_from_slice(&0u16.to_be_bytes());
        self.send(Packet {
            channel: 0,
            flags: FLAGS_PLAIN,
            final_length: None,
            payload: reply,
        })
        .await?;
        self.set_phase(Phase::Handshake);
        Ok(())
    }

    async fn on_handshake_message(&mut self, msg: &Msg) -> Result<()> {
        if msg.id() != Some(MSG_SSL) {
            bail!("expected TLS handshake data, got message {:?}", msg.id());
        }
        self.mem.feed_incoming(msg.body());
        let still = self.ssl.process(&mut self.mem).map_err(|e| {
            anyhow!("source: SSL/certificate problem during handshake ({e}); certificate rejected")
        })?;
        info!("source: SSL handshake: {}", self.ssl.state_string());
        let outgoing = self.mem.drain_outgoing();
        if !outgoing.is_empty() {
            self.send(ssl_encapsulate(outgoing)).await?;
        }
        if !still && !self.ssl.is_handshaking() {
            info!(
                "source: SSL init complete, negotiated cipher: {}",
                self.ssl.cipher_suite()
            );
            self.set_phase(Phase::AwaitAuth {
                since: Instant::now(),
            });
        }
        Ok(())
    }

    async fn send_service_discovery_request(&mut self) -> Result<()> {
        let mut req = ServiceDiscoveryRequest::new();
        req.set_device_name(self.params.device_name.clone());
        req.set_label_text(self.params.device_name.clone());
        let pkt = build_control_reply(
            ControlMessageType::MESSAGE_SERVICE_DISCOVERY_REQUEST,
            req.write_to_bytes()?,
        );
        self.send(pkt).await?;
        self.set_phase(Phase::AwaitSdr);
        info!(
            "source: sent SERVICE_DISCOVERY_REQUEST as \"{}\"",
            self.params.device_name
        );
        Ok(())
    }

    fn on_auth_complete(&mut self, msg: &Msg) -> Result<()> {
        let status = AuthResponse::parse_from_bytes(msg.body())
            .map(|a| a.status())
            .context("parse AUTH_COMPLETE")?;
        self.summary.auth_complete_seen = true;
        if status != 0 {
            bail!("HU sent AUTH_COMPLETE with status {status}");
        }
        info!("source: AUTH_COMPLETE received from HU, status 0");
        Ok(())
    }

    async fn on_sdr(&mut self, msg: &Msg) -> Result<()> {
        let sdr = ServiceDiscoveryResponse::parse_from_bytes(msg.body())
            .context("parse SERVICE_DISCOVERY_RESPONSE")?;
        let caps = HuCaps::from_sdr(&sdr);
        info!(
            "source: HU offers {} services (video sinks {}, audio sinks {}, input {}, sensors {}, other {})",
            sdr.services.len(),
            caps.video_sinks.len(),
            caps.audio_sinks.len(),
            caps.input.is_some(),
            caps.sensors.is_some(),
            caps.other.len()
        );
        for line in caps.describe() {
            info!("source: channel list: {line}");
        }
        let Some(video) = caps.main_video() else {
            bail!("source: HU offers no usable video sink (no main display with an H.264 config)");
        };
        let video_id = video.service_id;
        if !caps.has_sensor(SensorType::SENSOR_SPEED) {
            // The pause rule needs speed. No media flows in this task, so keep the session for
            // the logs; the media tasks must treat this as "moving" (design note, section 2).
            warn!("source: HU offers no speed sensor; video must stay paused once it is streamed");
        }
        let input_id = caps.input.as_ref().map(|i| i.service_id);
        let sensor_id = caps.sensors.as_ref().map(|s| s.service_id);
        self.caps = caps.clone();
        self.summary.caps = caps;

        let mut wanted = vec![(Kind::Video, video_id)];
        if let Some(id) = input_id {
            wanted.push((Kind::Input, id));
        }
        if let Some(id) = sensor_id {
            wanted.push((Kind::Sensor, id));
        }
        for (kind, id) in wanted {
            let mut req = ChannelOpenRequest::new();
            req.set_priority(0);
            req.set_service_id(id as i32);
            let pkt = build_control_reply_on_channel(
                id,
                ControlMessageType::MESSAGE_CHANNEL_OPEN_REQUEST,
                req.write_to_bytes()?,
            );
            self.send(pkt).await?;
            self.pending_opens.insert(id, kind);
            info!("source: opening channel {id:#04x} ({kind:?})");
        }
        self.set_phase(Phase::Opening);
        Ok(())
    }

    async fn on_channel_open_response(&mut self, msg: &Msg) -> Result<()> {
        let Some(kind) = self.pending_opens.remove(&msg.channel) else {
            self.warn_once(
                msg.channel,
                MSG_CH_OPEN_RESPONSE,
                "unsolicited CHANNEL_OPEN_RESPONSE",
            );
            return Ok(());
        };
        let status = ChannelOpenResponse::parse_from_bytes(msg.body())
            .map(|r| r.status())
            .context("parse CHANNEL_OPEN_RESPONSE")?;
        if status != MessageStatus::STATUS_SUCCESS {
            bail!(
                "HU refused to open channel {:#04x} ({kind:?}): {status:?}",
                msg.channel
            );
        }
        info!("source: channel opened: {:#04x} ({kind:?})", msg.channel);
        self.channels.insert(kind, msg.channel);
        self.summary.opened.push(msg.channel);
        match kind {
            Kind::Sensor => self.request_sensors(msg.channel).await?,
            Kind::Input => self.request_key_bindings(msg.channel).await?,
            // Video setup, focus and frames belong to the media tasks.
            Kind::Video => {}
        }
        if self.pending_opens.is_empty() && self.phase == Phase::Opening {
            self.set_phase(Phase::Running);
            self.next_ping = Instant::now() + self.params.ping_interval;
            info!(
                "source: session up, {} channels open: {:?}",
                self.summary.opened.len(),
                self.summary.opened
            );
        }
        Ok(())
    }

    async fn request_sensors(&mut self, channel: u8) -> Result<()> {
        let mut wanted = vec![
            SensorType::SENSOR_SPEED,
            SensorType::SENSOR_DRIVING_STATUS_DATA,
            SensorType::SENSOR_NIGHT_MODE,
            SensorType::SENSOR_PARKING_BRAKE,
            SensorType::SENSOR_GEAR,
        ];
        if self.params.source_gps {
            wanted.push(SensorType::SENSOR_LOCATION);
        }
        for sensor in wanted {
            if !self.caps.has_sensor(sensor) {
                info!("source: sensor {sensor:?} not offered by the HU");
                continue;
            }
            let mut req = SensorRequest::new();
            req.set_type(sensor);
            req.set_min_update_period(0);
            let id = SensorMessageId::SENSOR_MESSAGE_REQUEST as u16;
            self.send(data_packet(channel, id, &req.write_to_bytes()?))
                .await?;
            info!("source: requested sensor {sensor:?}");
        }
        Ok(())
    }

    async fn request_key_bindings(&mut self, channel: u8) -> Result<()> {
        let offered = self
            .caps
            .input
            .as_ref()
            .map(|i| i.keycodes.clone())
            .unwrap_or_default();
        let keys: Vec<i32> = KODI_KEYCODES
            .iter()
            .copied()
            .filter(|k| offered.is_empty() || offered.contains(k))
            .collect();
        if keys.is_empty() {
            info!("source: HU offers none of the keycodes Kodi uses; no key binding request");
            return Ok(());
        }
        let mut req = KeyBindingRequest::new();
        req.keycodes = keys.clone();
        let id = InputMessageId::INPUT_MESSAGE_KEY_BINDING_REQUEST as u16;
        self.send(data_packet(channel, id, &req.write_to_bytes()?))
            .await?;
        info!("source: sent KEY_BINDING_REQUEST for keycodes {keys:?}");
        Ok(())
    }

    // ---- running ----------------------------------------------------------------------------

    /// Handles one message. Returns `Some(reason)` when the session is over.
    async fn on_message(&mut self, msg: Msg) -> Result<Option<EndReason>> {
        match self.phase {
            Phase::AwaitVersion => {
                if msg.channel == 0 && msg.id() == Some(MSG_VERSION_REQUEST) {
                    self.on_version_request(&msg).await?;
                    return Ok(None);
                }
                bail!("expected VERSION_REQUEST, got message {:?}", msg.id());
            }
            Phase::Handshake => {
                self.on_handshake_message(&msg).await?;
                return Ok(None);
            }
            _ => {}
        }

        let Some(id) = msg.id() else {
            return Ok(None);
        };

        if msg.channel == 0 {
            return self.on_control(msg, id).await;
        }
        if msg.flags & FLAG_CONTROL != 0 {
            match id {
                MSG_CH_OPEN_RESPONSE => self.on_channel_open_response(&msg).await?,
                MSG_CH_CLOSE => {
                    bail!("HU closed channel {:#04x}", msg.channel);
                }
                _ => self.warn_once(msg.channel, id, "unknown channel control message"),
            }
            return Ok(None);
        }
        self.on_channel_data(&msg, id).await?;
        Ok(None)
    }

    async fn on_control(&mut self, msg: Msg, id: u16) -> Result<Option<EndReason>> {
        match id {
            MSG_AUTH_COMPLETE => {
                self.on_auth_complete(&msg)?;
                if matches!(self.phase, Phase::AwaitAuth { .. }) {
                    self.send_service_discovery_request().await?;
                }
            }
            MSG_SDR => {
                if self.phase == Phase::AwaitSdr {
                    self.on_sdr(&msg).await?;
                } else {
                    self.warn_once(0, id, "SERVICE_DISCOVERY_RESPONSE outside discovery");
                }
            }
            MSG_PING_REQUEST => {
                let req =
                    PingRequest::parse_from_bytes(msg.body()).context("parse PING_REQUEST")?;
                let mut resp = PingResponse::new();
                resp.set_timestamp(req.timestamp());
                let pkt = build_control_reply(
                    ControlMessageType::MESSAGE_PING_RESPONSE,
                    resp.write_to_bytes()?,
                );
                self.send(pkt).await?;
                self.summary.hu_pings_answered += 1;
                debug!("source: answered HU ping");
            }
            MSG_PING_RESPONSE => {
                self.ping_outstanding = 0;
                self.summary.pings_answered += 1;
                debug!("source: HU answered our ping");
            }
            MSG_BYEBYE_REQUEST => {
                let reason = ByeByeRequest::parse_from_bytes(msg.body())
                    .map(|r| format!("{:?}", r.reason()))
                    .unwrap_or_else(|_| "unparsable".into());
                info!(
                    "source: HU sent BYEBYE_REQUEST ({reason}); answering and ending the session"
                );
                let pkt = build_control_reply(ControlMessageType::MESSAGE_BYEBYE_RESPONSE, vec![]);
                self.send(pkt).await?;
                return Ok(Some(EndReason::HuByeBye));
            }
            MSG_BYEBYE_RESPONSE => {
                if matches!(self.phase, Phase::Closing { .. }) {
                    info!("source: HU answered our BYEBYE_REQUEST");
                    return Ok(Some(EndReason::Cancelled));
                }
                info!("source: unsolicited BYEBYE_RESPONSE from HU; ending the session");
                return Ok(Some(EndReason::HuByeBye));
            }
            MSG_NAV_FOCUS_NOTIFICATION | MSG_AUDIO_FOCUS_NOTIFICATION => {
                // Answers to focus requests the Pi sends from T06/T07 on. Not sent in T05.
                info!("source: focus notification {id:#06x} from HU (logged only)");
            }
            MSG_VOICE_SESSION => {
                info!(
                    "source: VOICE_SESSION_NOTIFICATION from HU, ignored (no microphone channel)"
                );
            }
            MSG_CAR_DEVICES_REQUEST => {
                let list = CarConnectedDevices::new();
                let pkt = build_control_reply(
                    ControlMessageType::MESSAGE_CAR_CONNECTED_DEVICES_RESPONSE,
                    list.write_to_bytes()?,
                );
                self.send(pkt).await?;
                info!("source: answered CAR_CONNECTED_DEVICES_REQUEST with an empty list");
            }
            other => self.warn_once(0, other, "unhandled control message"),
        }
        Ok(None)
    }

    async fn on_channel_data(&mut self, msg: &Msg, id: u16) -> Result<()> {
        let kind = self
            .channels
            .iter()
            .find(|(_, ch)| **ch == msg.channel)
            .map(|(k, _)| *k);
        match kind {
            Some(Kind::Sensor) => self.on_sensor_message(msg, id),
            Some(Kind::Input) => self.on_input_message(msg, id),
            Some(Kind::Video) => {
                self.on_video_message(msg, id);
                Ok(())
            }
            None => {
                self.warn_once(msg.channel, id, "data on a channel the Pi did not open");
                Ok(())
            }
        }
    }

    fn on_sensor_message(&mut self, msg: &Msg, id: u16) -> Result<()> {
        if id == SensorMessageId::SENSOR_MESSAGE_RESPONSE as u16 {
            let status = SensorResponse::parse_from_bytes(msg.body())
                .map(|r| r.status())
                .context("parse SENSOR_MESSAGE_RESPONSE")?;
            info!("source: sensor request answered: {status:?}");
        } else if id == SensorMessageId::SENSOR_MESSAGE_BATCH as u16 {
            let batch =
                SensorBatch::parse_from_bytes(msg.body()).context("parse SENSOR_MESSAGE_BATCH")?;
            self.summary.sensor_batches += 1;
            for s in &batch.speed_data {
                self.summary.speed_samples += 1;
                self.summary.last_speed_e3 = Some(s.speed_e3());
                // Logged only. The motion pause arrives with T07.
                info!(
                    "source: sensor speed {:.1} km/h (speed_e3 {})",
                    kmh(s.speed_e3()),
                    s.speed_e3()
                );
            }
            for d in &batch.driving_status_data {
                info!("source: sensor driving status {:#x}", d.status());
            }
            for n in &batch.night_mode_data {
                info!("source: sensor night mode {}", n.night_mode());
            }
            for p in &batch.parking_brake_data {
                info!("source: sensor parking brake {}", p.parking_brake());
            }
            for g in &batch.gear_data {
                info!("source: sensor gear {:?}", g.gear());
            }
            if self.params.source_gps {
                for l in &batch.location_data {
                    info!(
                        "source: sensor location lat {:.5} lon {:.5}",
                        l.latitude_e7() as f64 / 1e7,
                        l.longitude_e7() as f64 / 1e7
                    );
                }
            }
        } else if id == SensorMessageId::SENSOR_MESSAGE_ERROR as u16 {
            warn!(
                "source: SENSOR_MESSAGE_ERROR from HU: {} bytes",
                msg.body().len()
            );
        } else {
            self.warn_once(msg.channel, id, "unknown sensor message");
        }
        Ok(())
    }

    fn on_input_message(&mut self, msg: &Msg, id: u16) -> Result<()> {
        if id == InputMessageId::INPUT_MESSAGE_KEY_BINDING_RESPONSE as u16 {
            let status = KeyBindingResponse::parse_from_bytes(msg.body())
                .map(|r| r.status())
                .context("parse KEY_BINDING_RESPONSE")?;
            self.summary.key_binding_ok = Some(status == 0);
            info!("source: key binding answered: status {status}");
        } else if id == InputMessageId::INPUT_MESSAGE_INPUT_REPORT as u16 {
            let report = InputReport::parse_from_bytes(msg.body()).context("parse INPUT_REPORT")?;
            if let Some(t) = report.touch_event.as_ref() {
                self.summary.touch_events += 1;
                // Raw HU touch coordinates. Scaling to the video size comes with the video task.
                let pointers: Vec<String> = t
                    .pointer_data
                    .iter()
                    .map(|p| format!("#{} ({}, {})", p.pointer_id(), p.x(), p.y()))
                    .collect();
                info!("source: touch {:?} {}", t.action(), pointers.join(" "));
            }
            if let Some(k) = report.key_event.as_ref() {
                for key in &k.keys {
                    info!(
                        "source: key {} {}",
                        key.keycode(),
                        if key.down() { "down" } else { "up" }
                    );
                }
            }
        } else {
            self.warn_once(msg.channel, id, "unknown input message");
        }
        Ok(())
    }

    fn on_video_message(&mut self, msg: &Msg, id: u16) {
        if id == MediaMessageId::MEDIA_MESSAGE_VIDEO_FOCUS_NOTIFICATION as u16 {
            let focus = VideoFocusNotification::parse_from_bytes(msg.body())
                .map(|f| format!("{:?}", f.focus()))
                .unwrap_or_else(|_| "unparsable".into());
            info!("source: video focus notification from HU: {focus} (logged only)");
        } else if id == MediaMessageId::MEDIA_MESSAGE_CONFIG as u16
            || id == MediaMessageId::MEDIA_MESSAGE_ACK as u16
        {
            info!("source: video channel message {id:#06x} from HU (logged only)");
        } else {
            self.warn_once(msg.channel, id, "unhandled video channel message");
        }
    }

    async fn send_ping(&mut self) -> Result<()> {
        let mut req = PingRequest::new();
        req.set_timestamp(now_us());
        let pkt = build_control_reply(
            ControlMessageType::MESSAGE_PING_REQUEST,
            req.write_to_bytes()?,
        );
        self.send(pkt).await?;
        self.ping_outstanding += 1;
        self.summary.pings_sent += 1;
        Ok(())
    }

    async fn send_byebye(&mut self) -> Result<()> {
        let mut req = ByeByeRequest::new();
        req.set_reason(ByeByeReason::USER_SELECTION);
        let pkt = build_control_reply(
            ControlMessageType::MESSAGE_BYEBYE_REQUEST,
            req.write_to_bytes()?,
        );
        self.send(pkt).await?;
        info!("source: sent BYEBYE_REQUEST to HU");
        Ok(())
    }

    /// Deadlines and pings, called on every tick.
    async fn on_tick(&mut self) -> Result<Option<EndReason>> {
        let now = Instant::now();
        match self.phase {
            Phase::AwaitAuth { since } => {
                if now.duration_since(since) >= self.params.auth_wait {
                    warn!(
                        "source: no AUTH_COMPLETE from HU within {:?}; sending service discovery anyway",
                        self.params.auth_wait
                    );
                    self.send_service_discovery_request().await?;
                }
            }
            Phase::Running => {
                if now >= self.next_ping {
                    if self.ping_outstanding >= self.params.ping_max_missed {
                        error!(
                            "source: HU missed {} pings in a row; ending the session",
                            self.ping_outstanding
                        );
                        return Ok(Some(EndReason::PingTimeout));
                    }
                    self.send_ping().await?;
                    self.next_ping = now + self.params.ping_interval;
                }
            }
            Phase::Closing { since } => {
                if now.duration_since(since) >= self.params.bye_wait {
                    info!("source: no BYEBYE_RESPONSE from HU; closing anyway");
                    return Ok(Some(EndReason::Cancelled));
                }
            }
            Phase::AwaitVersion | Phase::Handshake | Phase::AwaitSdr | Phase::Opening => {
                if now >= self.phase_deadline {
                    bail!("timed out in setup phase {:?}", self.phase);
                }
            }
        }
        Ok(None)
    }
}

/// Runs one source session to its end.
///
/// `reader` and `writer` are the two halves of the HU link (the USB accessory pipes, or the DHU's
/// TCP stream). Setup problems return `Err` with a log line that says which stage failed, so the
/// car test triage (design note, S4) can read it. A clean end returns the summary.
pub async fn run_session<R: IoDeviceTrait, W: IoDeviceTrait>(
    reader: R,
    writer: W,
    params: SessionParams,
    cancel: CancellationToken,
) -> Result<SessionSummary> {
    let (ssl, mem) = crate::ssl::ssl_builder(
        params.tls_backend,
        crate::mitm::ProxyType::HeadUnit,
        &params.keys_path,
    )
    .map_err(|e| anyhow!("source: SSL/certificate problem ({e})"))?;

    let step_timeout = params.step_timeout;
    let mut s = Session {
        reader,
        writer,
        ssl,
        mem,
        params,
        rbuf: Vec::new(),
        frags: HashMap::new(),
        phase: Phase::AwaitVersion,
        phase_deadline: Instant::now() + step_timeout,
        caps: HuCaps::default(),
        pending_opens: HashMap::new(),
        channels: HashMap::new(),
        warned: HashSet::new(),
        ping_outstanding: 0,
        next_ping: Instant::now(),
        summary: SessionSummary {
            end: EndReason::Cancelled,
            caps: HuCaps::default(),
            opened: Vec::new(),
            auth_complete_seen: false,
            pings_sent: 0,
            pings_answered: 0,
            hu_pings_answered: 0,
            speed_samples: 0,
            last_speed_e3: None,
            sensor_batches: 0,
            touch_events: 0,
            key_binding_ok: None,
        },
    };

    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut cancelled = false;

    info!("source: session started, waiting for the HU's VERSION_REQUEST");
    let end = 'session: loop {
        while let Some(msg) = s.next_message().await? {
            if let Some(end) = s.on_message(msg).await? {
                break 'session end;
            }
        }
        tokio::select! {
            r = s.read_more() => r?,
            _ = tick.tick() => {
                if let Some(end) = s.on_tick().await? {
                    break 'session end;
                }
            }
            _ = cancel.cancelled(), if !cancelled => {
                cancelled = true;
                if s.phase == Phase::Running || s.phase == Phase::Opening {
                    s.send_byebye().await?;
                    s.set_phase(Phase::Closing { since: Instant::now() });
                } else {
                    info!("source: cancelled during setup; closing without BYEBYE");
                    break 'session EndReason::Cancelled;
                }
            }
        }
    };
    s.summary.end = end;
    info!("source: session ended: {end:?}");
    Ok(s.summary)
}
