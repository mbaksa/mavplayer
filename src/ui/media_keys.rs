//! Media keys and the desktop's now-playing controls: MPRIS on Linux (the
//! desktop grabs Play/Pause/Next/Previous and forwards them over D-Bus),
//! the system media transport controls on Windows, Now Playing on macOS.

use super::player::PlayerView;
use crate::engine::{MediaSource, Status};
use gpui::{Context, Window};
use souvlaki::{MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition, PlatformConfig};
use std::time::{Duration, Instant};

pub type Event = MediaControlEvent;

/// What was last published, so updates go out only when something changed.
#[derive(Default)]
struct Published {
    meta: Option<(String, Option<String>, Option<String>, u64)>,
    status: Option<&'static str>,
    pos: f64,
    at: Option<Instant>,
    speed: f64,
    volume: Option<f64>,
}

pub struct MediaKeys {
    controls: MediaControls,
    published: Published,
}

impl MediaKeys {
    /// Registers with the desktop. `primary` is the window other launches
    /// talk to; any further windows get their own MPRIS name.
    pub fn new(window: &Window, primary: bool, on_event: impl Fn(Event) + Send + 'static) -> Option<Self> {
        #[cfg(all(unix, not(target_os = "macos")))]
        {
            // souvlaki's D-Bus thread panics without a session bus.
            let bus = std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some()
                || dirs::runtime_dir().is_some_and(|d| d.join("bus").exists());
            if !bus {
                log::debug!("no D-Bus session bus; media keys unavailable");
                return None;
            }
        }
        let dbus_name =
            if primary { "mavplayer".to_string() } else { format!("mavplayer.instance{}", std::process::id()) };
        let config = PlatformConfig { display_name: "MavPlayer", dbus_name: &dbus_name, hwnd: native_handle(window) };
        let mut controls = match MediaControls::new(config) {
            Ok(c) => c,
            Err(e) => {
                log::warn!("media keys unavailable: {e:?}");
                return None;
            }
        };
        if let Err(e) = controls.attach(on_event) {
            log::warn!("media keys unavailable: {e:?}");
            return None;
        }
        log::debug!("media keys: registered as {dbus_name}");
        Some(Self { controls, published: Published::default() })
    }

    /// Publishes the current item, play state and position. Called from the
    /// UI tick; sends what changed, and the position about once a second
    /// while playing (clients extrapolate it at 1×).
    pub fn update(&mut self, view: &PlayerView) {
        let st = view.player.state();
        let p = &mut self.published;

        let meta = st.media.as_ref().map(|m| {
            let title = m.title.clone().unwrap_or_else(|| m.source_name.clone());
            let dur_ms = if m.duration.is_finite() && m.duration > 0.0 { (m.duration * 1000.0) as u64 } else { 0 };
            (title, m.artist.clone(), m.album.clone(), dur_ms)
        });
        let meta = meta.or_else(|| st.source.as_ref().map(|s: &MediaSource| (s.display_name(), None, None, 0)));
        if meta != p.meta {
            let (title, artist, album, dur_ms) = meta.clone().unwrap_or_default();
            let _ = self.controls.set_metadata(MediaMetadata {
                title: meta.as_ref().map(|_| title.as_str()),
                artist: artist.as_deref(),
                album: album.as_deref(),
                cover_url: None,
                duration: (dur_ms > 0).then(|| Duration::from_millis(dur_ms)),
            });
            p.meta = meta;
            p.status = None; // re-send the state with the new item
        }

        let status = match st.status {
            Status::Playing | Status::Loading if !view.player.paused() => "Playing",
            Status::Playing | Status::Paused | Status::Loading => "Paused",
            _ => "Stopped",
        };
        let pos = view.player.position().max(0.0);
        let speed = view.player.speed();
        // Where a client extrapolating from the last update thinks we are.
        let expected = match (p.status, p.at) {
            (Some("Playing"), Some(at)) => p.pos + at.elapsed().as_secs_f64(),
            _ => p.pos,
        };
        let drifted = (pos - expected).abs() > 1.0;
        // souvlaki serves Position as of the last update, so refresh it
        // while playing for clients that re-read it.
        let stale = status == "Playing" && p.at.is_none_or(|at| at.elapsed() >= Duration::from_secs(1));
        if p.status != Some(status) || drifted || stale || (speed - p.speed).abs() > 1e-3 {
            let progress = Some(MediaPosition(Duration::from_secs_f64(pos)));
            let playback = match status {
                "Playing" => MediaPlayback::Playing { progress },
                "Paused" => MediaPlayback::Paused { progress },
                _ => MediaPlayback::Stopped,
            };
            let _ = self.controls.set_playback(playback);
            p.status = Some(status);
            p.pos = pos;
            p.at = Some(Instant::now());
            p.speed = speed;
        }

        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let vol = if view.player.muted() { 0.0 } else { view.player.volume() };
            if p.volume.is_none_or(|v| (v - vol).abs() > 1e-3) {
                let _ = self.controls.set_volume(vol.min(1.0));
                p.volume = Some(vol);
            }
        }
    }
}

#[cfg(windows)]
fn native_handle(window: &Window) -> Option<*mut std::ffi::c_void> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    // Spelled out: gpui's own `Window::window_handle` returns its window id.
    match HasWindowHandle::window_handle(window).ok()?.as_raw() {
        RawWindowHandle::Win32(h) => Some(h.hwnd.get() as *mut std::ffi::c_void),
        _ => None,
    }
}

#[cfg(not(windows))]
fn native_handle(_: &Window) -> Option<*mut std::ffi::c_void> {
    None
}

impl PlayerView {
    /// A media key or the desktop's player controls were used.
    pub fn on_media_event(&mut self, ev: Event, window: &mut Window, cx: &mut Context<Self>) {
        log::debug!("media control: {ev:?}");
        let secs = |d: Duration| d.as_secs_f64();
        match ev {
            Event::Play => {
                if self.player.is_active() {
                    if self.player.paused() || self.player.with_state(|s| s.status == Status::Ended) {
                        self.player.toggle_pause();
                    }
                } else if !self.playlist.items.is_empty() {
                    let i = self.playlist.current.unwrap_or(0);
                    self.play_index(i, window, cx);
                }
            }
            Event::Pause => {
                if self.player.is_active() && !self.player.paused() {
                    self.player.set_paused(true);
                }
            }
            Event::Toggle => {
                if self.player.is_active() {
                    self.player.toggle_pause();
                } else {
                    return self.on_media_event(Event::Play, window, cx);
                }
            }
            Event::Next => self.next_item(window, cx),
            Event::Previous => self.prev_item(window, cx),
            Event::Stop => self.stop(window, cx),
            Event::Seek(dir) => {
                let step = self.settings.seek_step;
                self.seek_relative(if dir == souvlaki::SeekDirection::Forward { step } else { -step }, false, cx);
            }
            Event::SeekBy(dir, d) => {
                let s = secs(d);
                self.seek_relative(if dir == souvlaki::SeekDirection::Forward { s } else { -s }, true, cx);
            }
            Event::SetPosition(MediaPosition(d)) => self.player.seek(secs(d), false, true),
            Event::SetVolume(v) => self.set_volume(v, cx),
            Event::OpenUri(u) => self.open_sources(vec![crate::playlist::source_for_arg(&u)], window, cx),
            Event::Raise => self.show_window(window, cx),
            Event::Quit => super::shell::quit(cx),
            _ => {}
        }
        cx.notify();
    }
}
