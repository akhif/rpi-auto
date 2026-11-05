//! Fake head unit (HU) loopback test for the source-mode session (design note, section 8, item 7).
//!
//! The fake HU is a rustls client on one end of an in-memory pipe; the Pi's `run_session` is on the
//! other. The certificates are generated here at test time into a temp directory, so no key
//! material is stored in the repo. The fake HU frames and encrypts its own messages with
//! independent code, so a framing bug in the session cannot cancel itself out.

use aa_proxy_rs::io_backend::IoDevice;
use aa_proxy_rs::mitm::protos::*;
use aa_proxy_rs::mitm::ProxyType;
use aa_proxy_rs::source_mode::session::{run_session, EndReason, SessionParams};
use aa_proxy_rs::source_mode::{StreamReader, StreamWriter};
use aa_proxy_rs::ssl::{ssl_builder, AaConnection, SslMemBuf, TlsBackend};
use openssl::asn1::Asn1Time;
use openssl::hash::MessageDigest;
use openssl::pkey::PKey;
use openssl::rsa::Rsa;
use openssl::x509::{X509NameBuilder, X509};
use protobuf::Message;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{duplex, split, AsyncReadExt, AsyncWriteExt, DuplexStream, ReadHalf, WriteHalf};
use tokio_util::sync::CancellationToken;

const FLAGS_PLAIN: u8 = 0x03;
const FLAGS_DATA: u8 = 0x0b;
const FLAGS_CONTROL: u8 = 0x0f;
const ENCRYPTED: u8 = 0x08;

// ---- throwaway certificates -----------------------------------------------------------------

struct KeyDir(PathBuf);

impl KeyDir {
    fn new() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("aa-source-test-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for prefix in ["md", "hu"] {
            let key = PKey::from_rsa(Rsa::generate(2048).unwrap()).unwrap();
            let mut name = X509NameBuilder::new().unwrap();
            name.append_entry_by_text("CN", &format!("test-{prefix}"))
                .unwrap();
            let name = name.build();
            let mut b = X509::builder().unwrap();
            b.set_version(2).unwrap();
            b.set_subject_name(&name).unwrap();
            b.set_issuer_name(&name).unwrap();
            b.set_pubkey(&key).unwrap();
            b.set_not_before(&Asn1Time::days_from_now(0).unwrap())
                .unwrap();
            b.set_not_after(&Asn1Time::days_from_now(2).unwrap())
                .unwrap();
            b.sign(&key, MessageDigest::sha256()).unwrap();
            let cert = b.build();
            std::fs::write(
                dir.join(format!("{prefix}_cert.pem")),
                cert.to_pem().unwrap(),
            )
            .unwrap();
            std::fs::write(
                dir.join(format!("{prefix}_key.pem")),
                key.private_key_to_pem_pkcs8().unwrap(),
            )
            .unwrap();
        }
        KeyDir(dir)
    }

    fn path(&self) -> &str {
        self.0.to_str().unwrap()
    }
}

impl Drop for KeyDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---- fake HU --------------------------------------------------------------------------------

struct Frame {
    channel: u8,
    flags: u8,
    payload: Vec<u8>,
}

impl Frame {
    fn id(&self) -> u16 {
        u16::from_be_bytes([self.payload[0], self.payload[1]])
    }

    fn body(&self) -> &[u8] {
        &self.payload[2..]
    }
}

struct FakeHu {
    rd: ReadHalf<DuplexStream>,
    wr: WriteHalf<DuplexStream>,
    ssl: AaConnection,
    mem: SslMemBuf,
    buf: Vec<u8>,
}

fn msg(id: u16, body: &[u8]) -> Vec<u8> {
    let mut v = id.to_be_bytes().to_vec();
    v.extend_from_slice(body);
    v
}

impl FakeHu {
    fn new(stream: DuplexStream, keys: &KeyDir) -> Self {
        let (ssl, mem) =
            ssl_builder(TlsBackend::Rustls, ProxyType::MobileDevice, keys.path()).unwrap();
        let (rd, wr) = split(stream);
        Self {
            rd,
            wr,
            ssl,
            mem,
            buf: Vec::new(),
        }
    }

    async fn send_raw(&mut self, channel: u8, flags: u8, payload: &[u8]) {
        let mut f = vec![channel, flags];
        f.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        f.extend_from_slice(payload);
        self.wr.write_all(&f).await.unwrap();
    }

    async fn send_plain(&mut self, channel: u8, payload: &[u8]) {
        self.send_raw(channel, FLAGS_PLAIN, payload).await;
    }

    async fn send_enc(&mut self, channel: u8, flags: u8, payload: &[u8]) {
        self.ssl.encrypt(payload, &mut self.mem).unwrap();
        let ct = self.mem.drain_outgoing();
        self.send_raw(channel, flags, &ct).await;
    }

    async fn recv(&mut self) -> Frame {
        loop {
            if self.buf.len() >= 4 {
                let flags = self.buf[1];
                let len = u16::from_be_bytes([self.buf[2], self.buf[3]]) as usize;
                let header = if flags & 3 == 1 { 8 } else { 4 };
                if self.buf.len() >= header + len {
                    let channel = self.buf[0];
                    let mut payload = self.buf[header..header + len].to_vec();
                    self.buf.drain(..header + len);
                    if flags & ENCRYPTED != 0 {
                        payload = self.ssl.decrypt(&payload, &mut self.mem).unwrap();
                    }
                    return Frame {
                        channel,
                        flags,
                        payload,
                    };
                }
            }
            let mut chunk = [0u8; 4096];
            let n = self.rd.read(&mut chunk).await.unwrap();
            assert!(n > 0, "Pi closed the pipe");
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Version exchange and TLS handshake, as the HU (the TLS client).
    async fn connect(&mut self) {
        self.send_plain(0, &msg(1, &[0, 1, 0, 7])).await;
        let v = self.recv().await;
        assert_eq!(v.id(), 2, "VERSION_RESPONSE");
        assert_eq!(v.body(), &[0, 1, 0, 7, 0, 0], "same version, status 0");

        loop {
            let still = self.ssl.process(&mut self.mem).unwrap();
            let out = self.mem.drain_outgoing();
            if !out.is_empty() {
                self.send_plain(0, &msg(3, &out)).await;
            }
            if !still && !self.ssl.is_handshaking() {
                break;
            }
            let f = self.recv().await;
            assert_eq!(f.id(), 3, "ENCAPSULATED_SSL");
            self.mem.feed_incoming(f.body());
        }
        // plain AUTH_COMPLETE, status 0
        let mut auth = AuthResponse::new();
        auth.set_status(0);
        self.send_plain(0, &msg(4, &auth.write_to_bytes().unwrap()))
            .await;
    }

    /// Expects the discovery request, answers it, and returns the channel opens the Pi sends.
    async fn discover(&mut self, sdr: ServiceDiscoveryResponse) -> Vec<u8> {
        let req = self.recv().await;
        assert_eq!((req.channel, req.id()), (0, 5), "SERVICE_DISCOVERY_REQUEST");
        let parsed = ServiceDiscoveryRequest::parse_from_bytes(req.body()).unwrap();
        assert_eq!(parsed.device_name(), "Kodi");
        self.send_enc(0, FLAGS_DATA, &msg(6, &sdr.write_to_bytes().unwrap()))
            .await;
        let mut opened = Vec::new();
        for _ in 0..3 {
            let open = self.recv().await;
            assert_eq!(open.flags, FLAGS_CONTROL, "channel open frame flags");
            assert_eq!(open.id(), 7, "CHANNEL_OPEN_REQUEST");
            let r = ChannelOpenRequest::parse_from_bytes(open.body()).unwrap();
            assert_eq!(
                r.service_id() as u8,
                open.channel,
                "channel number = service id"
            );
            opened.push(open.channel);
            let mut resp = ChannelOpenResponse::new();
            resp.set_status(MessageStatus::STATUS_SUCCESS);
            self.send_enc(
                open.channel,
                FLAGS_CONTROL,
                &msg(8, &resp.write_to_bytes().unwrap()),
            )
            .await;
        }
        opened
    }
}

fn service(id: i32) -> Service {
    let mut s = Service::new();
    s.set_id(id);
    s
}

fn sample_sdr(with_video: bool) -> ServiceDiscoveryResponse {
    let mut sdr = ServiceDiscoveryResponse::new();

    let mut sensor = service(1);
    let mut ss = SensorSourceService::new();
    for t in [
        SensorType::SENSOR_SPEED,
        SensorType::SENSOR_DRIVING_STATUS_DATA,
    ] {
        let mut s = sensor_source_service::Sensor::new();
        s.set_sensor_type(t);
        ss.sensors.push(s);
    }
    sensor.sensor_source_service = Some(ss).into();
    sdr.services.push(sensor);

    let mut input = service(2);
    let mut is = InputSourceService::new();
    is.keycodes_supported = vec![4, 19, 20, 21, 22, 23, 66, 3, 5];
    let mut ts = input_source_service::TouchScreen::new();
    ts.set_width(800);
    ts.set_height(480);
    is.touchscreen.push(ts);
    input.input_source_service = Some(is).into();
    sdr.services.push(input);

    if with_video {
        let mut video = service(3);
        let mut ms = MediaSinkService::new();
        ms.set_display_type(DisplayType::DISPLAY_TYPE_MAIN);
        let mut vc = VideoConfiguration::new();
        vc.set_codec_resolution(VideoCodecResolutionType::VIDEO_800x480);
        vc.set_frame_rate(VideoFrameRateType::VIDEO_FPS_30);
        ms.video_configs.push(vc);
        video.media_sink_service = Some(ms).into();
        sdr.services.push(video);
    }

    // Offered but never opened: an audio sink and something else.
    let mut audio = service(4);
    let mut ms = MediaSinkService::new();
    ms.set_audio_type(AudioStreamType::AUDIO_STREAM_MEDIA);
    let mut ac = AudioConfiguration::new();
    ac.set_sampling_rate(48000);
    ac.set_number_of_bits(16);
    ac.set_number_of_channels(2);
    ms.audio_configs.push(ac);
    audio.media_sink_service = Some(ms).into();
    sdr.services.push(audio);

    let mut other = service(5);
    other.generic_notification_service = Some(GenericNotificationService::new()).into();
    sdr.services.push(other);
    sdr
}

fn fast_params(keys: &KeyDir) -> SessionParams {
    let mut p = SessionParams::new(keys.path(), TlsBackend::Rustls, false);
    p.ping_interval = Duration::from_millis(150);
    p.auth_wait = Duration::from_millis(500);
    p.step_timeout = Duration::from_secs(10);
    p.bye_wait = Duration::from_millis(500);
    p
}

/// Splits one end of the pipe into the two `IoDevice` halves the session takes.
fn pi_side(stream: DuplexStream) -> (impl IoDevice, impl IoDevice) {
    let (r, w) = split(stream);
    (StreamReader(r), StreamWriter(w))
}

/// Runs the fake HU script against a session; both under a 30 s guard.
async fn run<F, Fut>(
    keys: &KeyDir,
    params: SessionParams,
    cancel: CancellationToken,
    script: F,
) -> anyhow::Result<aa_proxy_rs::source_mode::SessionSummary>
where
    F: FnOnce(FakeHu) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let (pi, hu) = duplex(256 * 1024);
    let (r, w) = pi_side(pi);
    let hu = FakeHu::new(hu, keys);
    let (res, _) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(run_session(r, w, params, cancel), script(hu))
    })
    .await
    .expect("test timed out");
    res
}

/// Full happy path, then the HU says goodbye.
#[tokio::test]
async fn handshake_discovery_channels_sensors_input_pings_and_hu_byebye() {
    let keys = KeyDir::new();
    let summary = run(
        &keys,
        fast_params(&keys),
        CancellationToken::new(),
        |mut hu| async move {
            hu.connect().await;
            let opened = hu.discover(sample_sdr(true)).await;
            // Video (3) is opened first, then input (2), then sensors (1). Audio (4) and the
            // notification service (5) are never opened.
            assert_eq!(opened, vec![3, 2, 1]);

            // Key binding request on the input channel, narrowed to keys the HU offers and Kodi uses.
            // Sensor requests on the sensor channel.
            let mut keys_requested = None;
            let mut sensors_requested = Vec::new();
            let mut hu_ping_answered = false;
            let mut pi_pings = 0;
            while keys_requested.is_none() || sensors_requested.len() < 2 {
                let f = hu.recv().await;
                match (f.channel, f.id()) {
                    (2, 0x8002) => {
                        let r = KeyBindingRequest::parse_from_bytes(f.body()).unwrap();
                        keys_requested = Some(r.keycodes.clone());
                        let mut resp = KeyBindingResponse::new();
                        resp.set_status(0);
                        hu.send_enc(2, FLAGS_DATA, &msg(0x8003, &resp.write_to_bytes().unwrap()))
                            .await;
                    }
                    (1, 0x8001) => {
                        let r = SensorRequest::parse_from_bytes(f.body()).unwrap();
                        sensors_requested.push(r.type_());
                        let mut resp = SensorResponse::new();
                        resp.set_status(MessageStatus::STATUS_SUCCESS);
                        hu.send_enc(1, FLAGS_DATA, &msg(0x8002, &resp.write_to_bytes().unwrap()))
                            .await;
                    }
                    (0, 0x0b) => pi_pings += 1,
                    other => panic!("unexpected frame {other:?}"),
                }
            }
            assert_eq!(keys_requested.unwrap(), vec![4, 19, 20, 21, 22, 23, 66]);
            assert_eq!(
                sensors_requested,
                vec![
                    SensorType::SENSOR_SPEED,
                    SensorType::SENSOR_DRIVING_STATUS_DATA
                ]
            );

            // Sensor data: stationary, then 30 km/h. Then a touch.
            for speed_e3 in [0, 8333] {
                let mut batch = SensorBatch::new();
                let mut sd = SpeedData::new();
                sd.set_speed_e3(speed_e3);
                batch.speed_data.push(sd);
                hu.send_enc(
                    1,
                    FLAGS_DATA,
                    &msg(0x8003, &batch.write_to_bytes().unwrap()),
                )
                .await;
            }
            let mut report = InputReport::new();
            report.set_timestamp(1);
            let mut touch = TouchEvent::new();
            let mut p = touch_event::Pointer::new();
            p.set_x(400);
            p.set_y(240);
            p.set_pointer_id(0);
            touch.pointer_data.push(p);
            touch.set_action(PointerAction::ACTION_DOWN);
            report.touch_event = Some(touch).into();
            hu.send_enc(
                2,
                FLAGS_DATA,
                &msg(0x8001, &report.write_to_bytes().unwrap()),
            )
            .await;

            // The HU pings the Pi; the Pi must echo the timestamp. The Pi pings the HU; we answer.
            let mut ping = PingRequest::new();
            ping.set_timestamp(12345);
            hu.send_enc(0, FLAGS_DATA, &msg(0x0b, &ping.write_to_bytes().unwrap()))
                .await;
            while !hu_ping_answered || pi_pings < 2 {
                let f = hu.recv().await;
                match (f.channel, f.id()) {
                    (0, 0x0c) => {
                        let r = PingResponse::parse_from_bytes(f.body()).unwrap();
                        assert_eq!(r.timestamp(), 12345);
                        hu_ping_answered = true;
                    }
                    (0, 0x0b) => {
                        pi_pings += 1;
                        let req = PingRequest::parse_from_bytes(f.body()).unwrap();
                        let mut resp = PingResponse::new();
                        resp.set_timestamp(req.timestamp());
                        hu.send_enc(0, FLAGS_DATA, &msg(0x0c, &resp.write_to_bytes().unwrap()))
                            .await;
                    }
                    other => panic!("unexpected frame {other:?}"),
                }
            }

            // HU ends the session.
            let mut bye = ByeByeRequest::new();
            bye.set_reason(ByeByeReason::USER_SELECTION);
            hu.send_enc(0, FLAGS_DATA, &msg(0x0f, &bye.write_to_bytes().unwrap()))
                .await;
            loop {
                let f = hu.recv().await;
                if (f.channel, f.id()) == (0, 0x10) {
                    break;
                }
            }
        },
    )
    .await
    .expect("session should end cleanly");

    assert_eq!(summary.end, EndReason::HuByeBye);
    assert!(summary.auth_complete_seen);
    assert_eq!(summary.opened, vec![3, 2, 1]);
    assert_eq!(summary.speed_samples, 2);
    assert_eq!(summary.last_speed_e3, Some(8333));
    assert_eq!(summary.touch_events, 1);
    assert_eq!(summary.key_binding_ok, Some(true));
    assert!(summary.hu_pings_answered >= 1);
    assert!(summary.pings_answered >= 1);
    assert_eq!(
        summary.caps.audio_sinks.len(),
        1,
        "audio sink parsed but not opened"
    );
    assert_eq!(summary.caps.other.len(), 1);
}

/// The Pi sends BYEBYE when cancelled and ends when the HU answers.
#[tokio::test]
async fn cancel_sends_byebye_and_ends_cleanly() {
    let keys = KeyDir::new();
    let cancel = CancellationToken::new();
    let cancel_in_script = cancel.clone();
    let summary = run(&keys, fast_params(&keys), cancel, |mut hu| async move {
        hu.connect().await;
        hu.discover(sample_sdr(true)).await;
        // Drain requests until the session is up, then cancel.
        let mut requests = 0;
        while requests < 3 {
            let f = hu.recv().await;
            if matches!((f.channel, f.id()), (2, 0x8002) | (1, 0x8001)) {
                requests += 1;
            }
        }
        cancel_in_script.cancel();
        loop {
            let f = hu.recv().await;
            if (f.channel, f.id()) == (0, 0x0f) {
                break;
            }
        }
        hu.send_enc(0, FLAGS_DATA, &msg(0x10, &[])).await;
    })
    .await
    .expect("session should end cleanly");
    assert_eq!(summary.end, EndReason::Cancelled);
}

/// A HU that never answers pings ends the session after three missed replies.
#[tokio::test]
async fn silent_hu_ends_the_session_after_three_missed_pings() {
    let keys = KeyDir::new();
    let mut params = fast_params(&keys);
    params.ping_interval = Duration::from_millis(100);
    let summary = run(
        &keys,
        params,
        CancellationToken::new(),
        |mut hu| async move {
            hu.connect().await;
            hu.discover(sample_sdr(true)).await;
            // Never answer. Keep reading so the pipe does not fill.
            let mut pings = 0;
            while pings < 3 {
                let f = hu.recv().await;
                if (f.channel, f.id()) == (0, 0x0b) {
                    pings += 1;
                }
            }
            // Keep the pipe open until the Pi notices on its fourth tick.
            tokio::time::sleep(Duration::from_millis(800)).await;
        },
    )
    .await
    .expect("ping timeout is a clean end");
    assert_eq!(summary.end, EndReason::PingTimeout);
    assert_eq!(summary.pings_sent, 3);
}

/// A HU without a main video sink fails the session with a clear message.
#[tokio::test]
async fn hu_without_video_sink_fails_with_clear_error() {
    let keys = KeyDir::new();
    let err = run(
        &keys,
        fast_params(&keys),
        CancellationToken::new(),
        |mut hu| async move {
            hu.connect().await;
            let req = hu.recv().await;
            assert_eq!(req.id(), 5);
            hu.send_enc(
                0,
                FLAGS_DATA,
                &msg(6, &sample_sdr(false).write_to_bytes().unwrap()),
            )
            .await;
        },
    )
    .await
    .expect_err("no video sink must fail the session");
    assert!(
        err.to_string().contains("no usable video sink"),
        "error was: {err:#}"
    );
}

/// Without AUTH_COMPLETE the Pi still goes on to service discovery after `auth_wait`.
#[tokio::test]
async fn missing_auth_complete_does_not_stall_the_session() {
    let keys = KeyDir::new();
    let summary = run(
        &keys,
        fast_params(&keys),
        CancellationToken::new(),
        |mut hu| async move {
            // Version + handshake, but no AUTH_COMPLETE.
            hu.send_plain(0, &msg(1, &[0, 1, 0, 7])).await;
            assert_eq!(hu.recv().await.id(), 2);
            loop {
                let still = hu.ssl.process(&mut hu.mem).unwrap();
                let out = hu.mem.drain_outgoing();
                if !out.is_empty() {
                    hu.send_plain(0, &msg(3, &out)).await;
                }
                if !still && !hu.ssl.is_handshaking() {
                    break;
                }
                let f = hu.recv().await;
                hu.mem.feed_incoming(f.body());
            }
            hu.discover(sample_sdr(true)).await;
            let mut bye = ByeByeRequest::new();
            bye.set_reason(ByeByeReason::USER_SELECTION);
            hu.send_enc(0, FLAGS_DATA, &msg(0x0f, &bye.write_to_bytes().unwrap()))
                .await;
            loop {
                let f = hu.recv().await;
                if (f.channel, f.id()) == (0, 0x10) {
                    break;
                }
            }
        },
    )
    .await
    .expect("session should end cleanly");
    assert!(!summary.auth_complete_seen);
    assert_eq!(summary.end, EndReason::HuByeBye);
}
