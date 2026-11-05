//! What the head unit offers, parsed from its `ServiceDiscoveryResponse`.
//!
//! In source mode the head unit (HU) advertises its services and the Pi picks which ones to open
//! (design note, section 0, item 1). `HuCaps` is the Pi's view of that list.

use crate::mitm::protos::{
    AudioStreamType, DisplayType, MediaCodecType, SensorType, Service, ServiceDiscoveryResponse,
    VideoCodecResolutionType, VideoFrameRateType,
};

/// One video configuration offered by a video sink.
#[derive(Debug, Clone, PartialEq)]
pub struct VideoConfig {
    pub resolution: VideoCodecResolutionType,
    pub frame_rate: VideoFrameRateType,
    pub width_margin: u32,
    pub height_margin: u32,
    pub density: u32,
    pub h264: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct VideoSink {
    pub service_id: u8,
    pub display_type: DisplayType,
    pub configs: Vec<VideoConfig>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioFormat {
    pub sampling_rate: u32,
    pub bits: u32,
    pub channels: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AudioSink {
    pub service_id: u8,
    pub stream: AudioStreamType,
    pub formats: Vec<AudioFormat>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InputSource {
    pub service_id: u8,
    pub keycodes: Vec<i32>,
    /// `(width, height)` of `touchscreen[0]`, in the HU's touch coordinate space.
    pub touchscreen: Option<(i32, i32)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SensorSource {
    pub service_id: u8,
    pub sensors: Vec<SensorType>,
}

/// The HU's service list in the shape the source session needs.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HuCaps {
    pub video_sinks: Vec<VideoSink>,
    pub audio_sinks: Vec<AudioSink>,
    pub input: Option<InputSource>,
    pub sensors: Option<SensorSource>,
    /// Every other service: `(service id, kind)`. Logged only.
    pub other: Vec<(u8, &'static str)>,
}

fn other_kind(svc: &Service) -> &'static str {
    if svc.media_source_service.is_some() {
        "media_source (microphone)"
    } else if svc.bluetooth_service.is_some() {
        "bluetooth"
    } else if svc.radio_service.is_some() {
        "radio"
    } else if svc.navigation_status_service.is_some() {
        "navigation_status"
    } else if svc.media_playback_service.is_some() {
        "media_playback_status"
    } else if svc.phone_status_service.is_some() {
        "phone_status"
    } else if svc.media_browser_service.is_some() {
        "media_browser"
    } else if svc.vendor_extension_service.is_some() {
        "vendor_extension"
    } else if svc.generic_notification_service.is_some() {
        "generic_notification"
    } else if svc.wifi_projection_service.is_some() {
        "wifi_projection"
    } else {
        "unknown"
    }
}

impl HuCaps {
    /// Builds the capability view. Service ids above 255 cannot be channel numbers and are skipped.
    pub fn from_sdr(sdr: &ServiceDiscoveryResponse) -> Self {
        let mut caps = HuCaps::default();
        for svc in &sdr.services {
            let Ok(id) = u8::try_from(svc.id()) else {
                continue;
            };
            if let Some(sink) = svc.media_sink_service.as_ref() {
                if !sink.video_configs.is_empty() {
                    caps.video_sinks.push(VideoSink {
                        service_id: id,
                        display_type: sink.display_type(),
                        configs: sink
                            .video_configs
                            .iter()
                            .map(|v| VideoConfig {
                                resolution: v.codec_resolution(),
                                frame_rate: v.frame_rate(),
                                width_margin: v.width_margin(),
                                height_margin: v.height_margin(),
                                density: v.density(),
                                // A missing codec field means H.264 on every HU seen so far.
                                h264: !v.has_video_codec_type()
                                    || v.video_codec_type()
                                        == MediaCodecType::MEDIA_CODEC_VIDEO_H264_BP,
                            })
                            .collect(),
                    });
                } else {
                    caps.audio_sinks.push(AudioSink {
                        service_id: id,
                        stream: sink.audio_type(),
                        formats: sink
                            .audio_configs
                            .iter()
                            .map(|a| AudioFormat {
                                sampling_rate: a.sampling_rate(),
                                bits: a.number_of_bits(),
                                channels: a.number_of_channels(),
                            })
                            .collect(),
                    });
                }
            } else if let Some(input) = svc.input_source_service.as_ref() {
                caps.input = Some(InputSource {
                    service_id: id,
                    keycodes: input.keycodes_supported.clone(),
                    touchscreen: input.touchscreen.first().map(|t| (t.width(), t.height())),
                });
            } else if let Some(sensors) = svc.sensor_source_service.as_ref() {
                caps.sensors = Some(SensorSource {
                    service_id: id,
                    sensors: sensors.sensors.iter().map(|s| s.sensor_type()).collect(),
                });
            } else {
                caps.other.push((id, other_kind(svc)));
            }
        }
        caps
    }

    /// The main-display video sink with at least one H.264 configuration.
    /// Auxiliary and cluster displays are ignored (design note, section 2).
    pub fn main_video(&self) -> Option<&VideoSink> {
        self.video_sinks.iter().find(|s| {
            s.display_type == DisplayType::DISPLAY_TYPE_MAIN && s.configs.iter().any(|c| c.h264)
        })
    }

    pub fn has_sensor(&self, sensor: SensorType) -> bool {
        self.sensors
            .as_ref()
            .map(|s| s.sensors.contains(&sensor))
            .unwrap_or(false)
    }

    /// Human-readable lines for the HU's channel list, logged once per session.
    pub fn describe(&self) -> Vec<String> {
        let mut out = Vec::new();
        for v in &self.video_sinks {
            let cfgs: Vec<String> = v
                .configs
                .iter()
                .map(|c| {
                    format!(
                        "{:?}/{:?} margins {}x{} density {} h264={}",
                        c.resolution,
                        c.frame_rate,
                        c.width_margin,
                        c.height_margin,
                        c.density,
                        c.h264
                    )
                })
                .collect();
            out.push(format!(
                "service {:#04x}: video sink {:?}: {}",
                v.service_id,
                v.display_type,
                cfgs.join("; ")
            ));
        }
        for a in &self.audio_sinks {
            let fmts: Vec<String> = a
                .formats
                .iter()
                .map(|f| format!("{} Hz/{} bit/{} ch", f.sampling_rate, f.bits, f.channels))
                .collect();
            out.push(format!(
                "service {:#04x}: audio sink {:?}: {}",
                a.service_id,
                a.stream,
                fmts.join("; ")
            ));
        }
        if let Some(i) = &self.input {
            out.push(format!(
                "service {:#04x}: input source: {} keycodes, touchscreen {:?}",
                i.service_id,
                i.keycodes.len(),
                i.touchscreen
            ));
        }
        if let Some(s) = &self.sensors {
            out.push(format!(
                "service {:#04x}: sensor source: {:?}",
                s.service_id, s.sensors
            ));
        }
        for (id, kind) in &self.other {
            out.push(format!("service {:#04x}: {} (not opened)", id, kind));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mitm::protos::{MediaSinkService, SensorSourceService, Service, VideoConfiguration};

    fn svc(id: i32) -> Service {
        let mut s = Service::new();
        s.set_id(id);
        s
    }

    #[test]
    fn parses_video_input_sensor_and_other() {
        let mut sdr = ServiceDiscoveryResponse::new();

        let mut video = svc(3);
        let mut sink = MediaSinkService::new();
        sink.set_display_type(DisplayType::DISPLAY_TYPE_MAIN);
        let mut vc = VideoConfiguration::new();
        vc.set_codec_resolution(VideoCodecResolutionType::VIDEO_1280x720);
        vc.set_frame_rate(VideoFrameRateType::VIDEO_FPS_30);
        sink.video_configs.push(vc);
        video.media_sink_service = Some(sink).into();
        sdr.services.push(video);

        let mut sensor = svc(1);
        let mut ss = SensorSourceService::new();
        let mut s1 = crate::mitm::protos::sensor_source_service::Sensor::new();
        s1.set_sensor_type(SensorType::SENSOR_SPEED);
        ss.sensors.push(s1);
        sensor.sensor_source_service = Some(ss).into();
        sdr.services.push(sensor);

        let mut other = svc(9);
        other.generic_notification_service = Some(Default::default()).into();
        sdr.services.push(other);

        let caps = HuCaps::from_sdr(&sdr);
        assert_eq!(caps.main_video().map(|v| v.service_id), Some(3));
        assert!(caps.has_sensor(SensorType::SENSOR_SPEED));
        assert!(!caps.has_sensor(SensorType::SENSOR_GEAR));
        assert_eq!(caps.other, vec![(9, "generic_notification")]);
        assert_eq!(caps.describe().len(), 3);
    }

    #[test]
    fn cluster_display_is_not_the_main_video() {
        let mut sdr = ServiceDiscoveryResponse::new();
        let mut video = svc(4);
        let mut sink = MediaSinkService::new();
        sink.set_display_type(DisplayType::DISPLAY_TYPE_CLUSTER);
        sink.video_configs.push(VideoConfiguration::new());
        video.media_sink_service = Some(sink).into();
        sdr.services.push(video);
        assert!(HuCaps::from_sdr(&sdr).main_video().is_none());
    }
}
