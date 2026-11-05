# RAV4 Pi Media Dongle — Build Spec

Nov 5, 2026 · Akhif

## Goal and scope

One Pi Zero 2 W in the RAV4's Android Auto USB port does two jobs. While driving it is a wireless Android Auto dongle for the phone. When parked it becomes a Kodi media player on the car screen. This is a personal build for one car, not a product.

- **In v1:** Google Maps, music and calls through the phone's Android Auto; movies from the SD card; media streamed or cast from the phone; YouTube.
- **Stretch goal:** Netflix and Shahid through Kodi and Widevine, possibly on a Pi 4.
- **Out of scope:** rooting the head unit, video while moving, iPhone/CarPlay, extra USB devices on the Pi.
- **Accepted grey area:** using certificates extracted from Google software so the Pi can talk to the Toyota head unit.

## Architecture

The same Pi relays the phone's Android Auto while driving and becomes the Android Auto source when parked. The car sees one USB device either way.

```
DRIVE MODE (default, automatic)
  Phone (Android Auto app) <-- Bluetooth + Wi-Fi --> Pi Zero 2 W (aa-proxy-rs, relays traffic) <-- USB --> RAV4 head unit

        | tap Disconnect in Android Auto            ^ reconnect Android Auto
        v                                           |

PI MODE (parked)
  Phone (proxy app, DLNA/SMB server) <-- Wi-Fi: internet + media --> Pi Zero 2 W (Kodi -> H.264 encoder, Android Auto source) <-- USB: video + audio out / touch + speed in --> RAV4 head unit
```

In Pi mode the car sends touch and speed back to the Pi, which feed Kodi and the motion lock. The highlighted box is the new work.

## Hardware

The build is one Pi Zero 2 W, a card and a cable. Nothing else plugs into the Pi, because its only data port goes to the car.

| Item | Purpose | Note |
| --- | --- | --- |
| Raspberry Pi Zero 2 W | Dongle and media player | 512 MB RAM; hardware H.264 encoder |
| microSD, high-endurance, 64 GB or more | OS and offline movies | The only local storage available |
| Short micro-USB to USB-A data cable | Car link and power | Into the Pi port marked USB, not PWR |
| Vented case with heatsink | Heat | Parked cabins in Riyadh run very hot |

## Software stack

Everything ships as one Buildroot image built from aa-proxy-rs, so drive mode boots as fast as the stock dongle.

| Layer | Choice | Role |
| --- | --- | --- |
| Build system | aa-proxy Buildroot | One SD image, fast boot, read-only root |
| Android Auto link | Fork of aa-proxy-rs (Rust) | Relay in drive mode; Android Auto source in Pi mode |
| Video out | V4L2 hardware H.264 encoder | Encodes Kodi's screen at the head unit's resolution |
| Media UI | Kodi with a touch skin | Local files, DLNA/SMB browsing, YouTube add-on |
| Casting | Kodi UPnP renderer | Receives media pushed from the phone |
| Touch | uinput | Car touch events into Kodi |
| Audio | Android Auto media channel | Kodi audio to the car speakers |

The core new work is "Pi as source" mode. The fork must open its own session with the head unit, send Kodi's encoded screen, and answer the touch, audio and sensor channels. aa-proxy-rs already has the parts to start from: man-in-the-middle sessions with both certificate sets, a decode-render-encode video pipeline for its overlay, and injected-display support.

The official aa-proxy images leave out the V4L2 codec drivers, so the image has to be built with them.

## Networking and mode switching

The Pi hosts one Wi-Fi network in both modes. Whether the phone's Android Auto session is connected decides which mode the Pi is in.

1. Car starts, the Pi boots, the phone joins over Bluetooth and then the Pi's Wi-Fi. Drive mode starts with no action.
2. Tap Disconnect in Android Auto on the phone. The Pi sees a user disconnect, blocks auto-reconnect and starts its own session with Kodi.
3. The phone stays on the Pi's Wi-Fi. A proxy app on the phone (Every Proxy type) shares mobile data, and Kodi's traffic goes through it.
4. Reconnect Android Auto on the phone. Pi mode stops and drive mode resumes.

One-time phone setup:

- [ ] Set the Pi's Wi-Fi network to stay connected without internet
- [ ] Install the proxy app, exempt it from battery optimisation and set it to autostart
- [ ] Install a DLNA/SMB server app for phone media
- [ ] Pair the phone with the RAV4's own Bluetooth for calls

Open question: how the head unit reacts when the Pi switches from relay to source mid-drive (USB re-enumeration).

## Features

v1 delivers every goal except DRM streaming, which follows as a stretch goal.

| Feature | How | Release |
| --- | --- | --- |
| Maps, music, calls while driving | Phone's own Android Auto in drive mode | v1 (unchanged) |
| Movies on the SD card | Kodi library | v1 |
| Phone's music and movies | Phone's DLNA/SMB server, browsed in Kodi | v1 |
| Casting from the phone | Kodi UPnP renderer | v1 |
| YouTube | Kodi YouTube add-on | v1 |
| Netflix, Shahid | Kodi with Widevine | Stretch; may need a Pi 4 |

## Safety

In Pi mode, video pauses whenever the car reports a speed above 0 km/h; audio keeps playing. The Pi reads speed from the head unit's Android Auto sensor channel, which aa-proxy-rs already collects. The fork never enables aa-proxy-rs's "video in motion" tweak. Drive mode is untouched, so the phone keeps Android Auto's normal driving restrictions.

## Milestones

The riskiest step, the Pi acting as the Android Auto source, comes at M2, before any Kodi work. Each milestone ends with a test in the car. M2 to M5 can be built at a desk first, because aa-proxy-rs works with Google's Desktop Head Unit on a PC.

1. **M0, dongle baseline.** Flash stock aa-proxy-rs on the Zero 2 W.
   - Exit: wireless Android Auto starts in the RAV4 on every ignition.
2. **M1, own image.** Build the Buildroot image with V4L2 codecs, Kodi and the fork.
   - Exit: M0 still works from your image; boot time measured.
3. **M2, Pi as source.** The Pi opens its own session and streams a test pattern.
   - Exit: pattern shows on the RAV4 screen and touches are logged. If the head unit rejects the certificates here, Pi mode stops and drive mode stays.
4. **M3, Kodi on the car screen.** Encode Kodi's output, send touch through uinput, audio through Android Auto.
   - Exit: an SD-card movie plays with sound and touch control.
5. **M4, switching and internet.** Disconnect/reconnect switching and the phone proxy.
   - Exit: switch both ways 10 times without a reboot; calls still reach the car.
6. **M5, phone media and YouTube.** DLNA/SMB browsing, casting, YouTube add-on.
   - Exit: each source plays for 30 minutes.
7. **M6, motion lock and hardening.** Speed-based pause, read-only root, heat test.
   - Exit: video pauses when moving; the Pi survives a summer afternoon parked.
8. **Stretch, Netflix and Shahid.** Widevine in Kodi.
   - Exit: playback works, or the decision to move to a Pi 4.

## Risks

Certificate rejection is the one risk that can kill Pi mode outright; M2 tests it first.

| Risk | Impact | Mitigation |
| --- | --- | --- |
| Toyota head unit rejects the extracted certificates | No Pi mode | Test at M2; drive mode is unaffected |
| 512 MB RAM with Kodi and encoding | Stutter or crashes | Light Kodi skin, 480p/720p output; Pi 4 as fallback |
| Phone drops the no-internet Wi-Fi | Pi mode loses internet | "Stay connected" setting; Bluetooth tethering as backup |
| Car USB port can't supply enough power under load | Brownouts, SD corruption | Read-only root; measure current at M3 |
| Cabin heat | Throttling or shutdowns | Vented case and heatsink; heat test at M6 |
| Calls while the Pi holds the session | Calls may not reach the car | Test at M4; switch back to drive mode for calls |
| Android Auto updates on the phone | Link breaks | Follow aa-proxy-rs releases; delay phone updates |
| Google deprecating 2.4 GHz wireless Android Auto | Drive mode degrades long-term | Watch aa-proxy-rs notes; wired phone mode as fallback |

## Decision log

| Decision | Choice |
| --- | --- |
| Scope | Personal build; grey-area certificates accepted |
| Hardware | Pi Zero 2 W, limits accepted |
| OS base | Linux on Buildroot |
| Architecture | Hybrid: drive mode and Pi mode |
| Android Auto link | Fork of aa-proxy-rs |
| Media UI | Kodi |
| Phone media | Wi-Fi streaming, casting, SD card |
| Pi mode internet | Pi hosts Wi-Fi; phone shares data through a proxy app |
| Mode trigger | Disconnect in Android Auto starts Pi mode; reconnect ends it |
| Calls | Phone paired to the RAV4's Bluetooth |
| Safety | Video pauses above 0 km/h |
| Netflix, Shahid | Stretch goal after v1 |
| Builder | Akhif, with Claude Code |

## Sources

- [aa-proxy-rs](https://github.com/aa-proxy/aa-proxy-rs): supported boards, man-in-the-middle mode, overlay encoder, Desktop Head Unit testing
- [aa-proxy supported hardware](https://aa-proxy.github.io/docs/supported-hardware)
- [AACS](https://github.com/tomasz-grobelny/AACS): earlier Linux project that streams video to a head unit
- [Flywheel setup guide](https://github.com/Benjamin-Wiegand/Flywheel/wiki/Setup-Guide): certificate notes for OEM head units
- [Widevine on Raspberry Pi](https://pimylifeup.com/raspberry-pi-widevine/)
- [Every Proxy](https://www.apkmirror.com/apk/gorilla-software-llp/every-proxy/every-proxy-15-7-0-release/)
