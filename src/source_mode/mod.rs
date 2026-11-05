//! Source mode ("Pi mode"): the Pi is the Android Auto mobile device and the head unit is the sink.
//!
//! Design: `docs/source-mode-design.md`. This module is new code; the only hooks into upstream
//! files are small (`config.rs` keys, `main.rs` branch, a few `pub(crate)` in `mitm.rs`).
//!
//! T05 scope: the session core with no media (`session`, `caps`) and the `always` bring-up mode.

pub mod caps;
pub mod session;

use crate::config::{AppConfig, SharedConfig};
use crate::io_backend::IoDevice as IoDeviceTrait;
use crate::ssl::TlsBackend;
use log::{error, info, warn};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

pub use session::{run_session, EndReason, SessionParams, SessionSummary};

/// Stop retrying after this many quick failures in `QUICK_FAILURE_WINDOW`.
const MAX_QUICK_FAILURES: usize = 3;
const QUICK_FAILURE_WINDOW: Duration = Duration::from_secs(60);

macro_rules! string_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? } default $default:ident, expected $expected:literal) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
        pub enum $name { $(#[serde(rename = $text)] $variant),+ }

        impl Default for $name {
            fn default() -> Self { Self::$default }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(match self { $(Self::$variant => $text),+ })
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                let s = String::deserialize(d)?;
                match s.trim().to_ascii_lowercase().as_str() {
                    $($text => Ok(Self::$variant),)+
                    other => Err(serde::de::Error::custom(format!(
                        "invalid {}: '{other}', expected {}", stringify!($name), $expected
                    ))),
                }
            }
        }
    };
}

string_enum! {
    /// `source_mode` config key.
    SourceMode {
        Off => "off",
        OnDisconnect => "on_disconnect",
        Always => "always",
    } default Off, expected "'off', 'on_disconnect' or 'always'"
}

string_enum! {
    /// `source_encoder` config key (used from the video task on).
    SourceEncoder {
        Auto => "auto",
        V4l2 => "v4l2",
        Software => "software",
    } default Auto, expected "'auto', 'v4l2' or 'software'"
}

string_enum! {
    /// `source_video` config key (used from the video task on).
    SourceVideo {
        TestPattern => "test_pattern",
        Kodi => "kodi",
    } default TestPattern, expected "'test_pattern' or 'kodi'"
}

pub fn default_source_fps() -> u8 {
    30
}

pub fn default_source_audio_device() -> String {
    "hw:Loopback,1,0".to_string()
}

/// Startup guards (design note, section 7). Returns the mode source mode may actually run in.
/// Every refusal logs an error and leaves source mode off, so drive mode is never affected.
pub fn effective_mode(cfg: &AppConfig) -> SourceMode {
    if cfg.source_mode == SourceMode::Off {
        return SourceMode::Off;
    }
    if cfg.video_in_motion {
        error!("source: source_mode needs video_in_motion = false (source mode never lifts the motion lock); source mode stays off");
        return SourceMode::Off;
    }
    if !cfg.mitm || cfg.runtime_mitm_failed {
        error!("source: source_mode needs the TLS stack (mitm = true); source mode stays off");
        return SourceMode::Off;
    }
    if cfg.source_mode == SourceMode::OnDisconnect {
        warn!("source: source_mode = on_disconnect arrives with the mode-switching task; source mode stays off");
        return SourceMode::Off;
    }
    SourceMode::Always
}

// ---- I/O adapters ---------------------------------------------------------------------------

/// Read half of a tokio stream as an [`IoDeviceTrait`].
pub struct StreamReader<T>(pub T);
/// Write half of a tokio stream as an [`IoDeviceTrait`].
pub struct StreamWriter<T>(pub T);

fn unsupported(what: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Unsupported, what.to_string())
}

impl<T: tokio::io::AsyncRead + Unpin> IoDeviceTrait for StreamReader<T> {
    async fn write_data(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(unsupported("cannot write to a read half"))
    }

    async fn read_data(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        use tokio::io::AsyncReadExt;
        self.0.read(buf).await
    }
}

impl<T: tokio::io::AsyncWrite + Unpin> IoDeviceTrait for StreamWriter<T> {
    async fn write_data(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        use tokio::io::AsyncWriteExt;
        // `write_all` so a partial TCP write cannot split a frame.
        self.0.write_all(buf).await?;
        Ok(buf.len())
    }

    async fn read_data(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
        Err(unsupported("cannot read from a write half"))
    }
}

// ---- always mode ----------------------------------------------------------------------------

fn params_from_config(cfg: &AppConfig) -> SessionParams {
    SessionParams::new(
        crate::mitm::KEYS_PATH,
        TlsBackend::from_config(&cfg.tls_backend),
        cfg.source_gps,
    )
}

/// Runs `source_mode = "always"` until `shutdown` is set: one source session per HU connection.
///
/// With `dhu = true` the HU is the Desktop Head Unit on TCP port 5277 (works with either I/O
/// backend). Otherwise the HU is the car on the USB gadget (builds without `io-uring`).
/// Bluetooth, Wi-Fi and the phone are skipped entirely.
pub async fn run_always(
    config: SharedConfig,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cfg = config.read().await.clone();
    info!(
        "source: source_mode = always (dhu = {}, source_video = {}, source_encoder = {}); phone, Bluetooth and Wi-Fi are skipped",
        cfg.dhu, cfg.source_video, cfg.source_encoder
    );
    let cancel = CancellationToken::new();
    let cancel_on_shutdown = cancel.clone();
    let shutdown_watch = shutdown.clone();
    tokio::spawn(async move {
        while !shutdown_watch.load(std::sync::atomic::Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        cancel_on_shutdown.cancel();
    });

    if cfg.dhu {
        run_always_dhu(&cfg, &cancel).await
    } else {
        run_always_usb(&cfg, &cancel).await
    }
}

/// Counts quick failures; returns `true` once the retry budget is spent.
fn record_failure(failures: &mut Vec<Instant>) -> bool {
    let now = Instant::now();
    failures.retain(|t| now.duration_since(*t) < QUICK_FAILURE_WINDOW);
    failures.push(now);
    failures.len() >= MAX_QUICK_FAILURES
}

async fn run_always_dhu(
    cfg: &AppConfig,
    cancel: &CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener =
        tokio::net::TcpListener::bind(("0.0.0.0", crate::config::TCP_DHU_PORT as u16)).await?;
    let mut failures = Vec::new();
    loop {
        info!(
            "source: waiting for the Desktop Head Unit on TCP port {}",
            crate::config::TCP_DHU_PORT
        );
        let stream = tokio::select! {
            r = listener.accept() => r?.0,
            _ = cancel.cancelled() => return Ok(()),
        };
        stream.set_nodelay(true)?;
        info!("source: DHU connected");
        let (r, w) = stream.into_split();
        let res = run_session(
            StreamReader(r),
            StreamWriter(w),
            params_from_config(cfg),
            cancel.clone(),
        )
        .await;
        match res {
            Ok(s) => info!("source: session over ({:?})", s.end),
            Err(e) => {
                error!("source: session failed: {e:#}");
                if record_failure(&mut failures) {
                    error!("source: {MAX_QUICK_FAILURES} failures in {QUICK_FAILURE_WINDOW:?}; giving up");
                    return Err(e.into());
                }
            }
        }
        if cancel.is_cancelled() {
            return Ok(());
        }
    }
}

#[cfg(not(feature = "io-uring"))]
async fn run_always_usb(
    cfg: &AppConfig,
    cancel: &CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use crate::io_tokio::IoDevice;
    use crate::usb_functionfs::FfsGadget;
    use std::sync::Arc;
    use tokio::sync::Mutex;

    let mut usb = FfsGadget::new(cfg.legacy, cfg.udc.clone(), cfg.usb_stick.clone());
    let mut failures = Vec::new();
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        // `init` removes every gadget and builds a fresh one, as the drive-mode loop does.
        if let Err(e) = usb.init().await {
            error!("source: USB init error: {e}");
        }
        info!("source: waiting for the head unit's USB accessory handshake");
        let session = tokio::select! {
            s = usb.wait_for_accessory_session(cfg.usb_gadget_require_accessory_start) => s,
            _ = cancel.cancelled() => return Ok(()),
        };
        let Some(session) = session else {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        };
        let reader = IoDevice::FfsReader(Arc::new(Mutex::new(session.reader)));
        let writer = IoDevice::FfsWriter(Arc::new(Mutex::new(session.writer)));
        let res = run_session(reader, writer, params_from_config(cfg), cancel.clone()).await;
        match res {
            Ok(s) => info!("source: session over ({:?})", s.end),
            Err(e) => {
                error!("source: session failed: {e:#}");
                if record_failure(&mut failures) {
                    error!("source: {MAX_QUICK_FAILURES} failures in {QUICK_FAILURE_WINDOW:?}; giving up");
                    return Err(e.into());
                }
            }
        }
        usb.rearm_for_next_session(Duration::from_millis(cfg.usb_gadget_rearm_cooldown_ms))
            .await;
    }
}

#[cfg(feature = "io-uring")]
async fn run_always_usb(
    _cfg: &AppConfig,
    _cancel: &CancellationToken,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    Err(
        "source_mode = always on the real USB gadget needs a build without the io-uring feature \
         (the same limit as drive mode); set dhu = true for desk testing"
            .into(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(mode: SourceMode) -> AppConfig {
        AppConfig {
            source_mode: mode,
            mitm: true,
            video_in_motion: false,
            ..AppConfig::default()
        }
    }

    #[test]
    fn off_stays_off() {
        assert_eq!(effective_mode(&cfg(SourceMode::Off)), SourceMode::Off);
    }

    #[test]
    fn always_is_allowed_with_clean_config() {
        assert_eq!(effective_mode(&cfg(SourceMode::Always)), SourceMode::Always);
    }

    #[test]
    fn video_in_motion_blocks_source_mode() {
        let mut c = cfg(SourceMode::Always);
        c.video_in_motion = true;
        assert_eq!(effective_mode(&c), SourceMode::Off);
    }

    #[test]
    fn missing_tls_blocks_source_mode() {
        let mut c = cfg(SourceMode::Always);
        c.mitm = false;
        assert_eq!(effective_mode(&c), SourceMode::Off);
        let mut c = cfg(SourceMode::Always);
        c.runtime_mitm_failed = true;
        assert_eq!(effective_mode(&c), SourceMode::Off);
    }

    #[test]
    fn on_disconnect_is_not_active_yet() {
        assert_eq!(
            effective_mode(&cfg(SourceMode::OnDisconnect)),
            SourceMode::Off
        );
    }

    #[test]
    fn defaults_are_stock() {
        let c = AppConfig::default();
        assert_eq!(c.source_mode, SourceMode::Off);
        assert_eq!(c.source_fps, 30);
        assert_eq!(c.source_encoder, SourceEncoder::Auto);
        assert_eq!(c.source_video, SourceVideo::TestPattern);
        assert_eq!(c.source_audio_device, "hw:Loopback,1,0");
        assert!(!c.source_touch_uinput);
        assert!(!c.source_gps);
    }

    #[test]
    fn parses_from_strings() {
        #[derive(Deserialize)]
        struct T {
            m: SourceMode,
        }
        let t: T = serde_json::from_str(r#"{"m":"Always"}"#).unwrap();
        assert_eq!(t.m, SourceMode::Always);
        assert!(serde_json::from_str::<T>(r#"{"m":"sometimes"}"#).is_err());
    }
}
