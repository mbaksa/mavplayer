//! The system tray icon shown while "Keep running in the system tray" or
//! "Play in the background" is on. Clicking it opens the window; its menu
//! opens the window or quits, and with "Play in the background" also has
//! Play/Pause, Previous and Next and hides the window to the tray.
//!
//! Linux uses StatusNotifierItem over D-Bus (`ksni`), which KDE, Cinnamon,
//! Xfce, LXQt, Budgie, Sway bars and GNOME's AppIndicator extension show.
//! Windows (notification area) and macOS (menu bar) use `tray-icon`.

use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayEvent {
    /// The icon itself was clicked: show the window, or hide it to the tray
    /// when it is in front and playing in the background is on.
    Activate,
    /// Open the window (or bring it to the front).
    Show,
    /// Hide the window to the tray; playback goes on.
    Hide,
    PlayPause,
    Previous,
    Next,
    /// Quit MavPlayer completely.
    Quit,
}

/// What the tray's menu and tooltip show.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TrayState {
    /// Playback controls and "Hide to Tray" ("Play in the background").
    pub controls: bool,
    pub playing: bool,
    /// The playlist isn't empty: there is something to play or skip.
    pub has_media: bool,
    /// The current item's title.
    pub title: Option<String>,
    /// The window is hidden in the tray.
    pub hidden: bool,
}

impl TrayState {
    fn tooltip(&self) -> String {
        match (&self.title, self.controls) {
            (Some(t), true) => format!("{t} ({})", if self.playing { "playing" } else { "paused" }),
            _ => TOOLTIP.into(),
        }
    }

    /// The title as a (disabled) menu entry, kept short.
    fn title_label(&self) -> Option<String> {
        let t = self.title.as_ref().filter(|_| self.controls)?;
        const MAX: usize = 48;
        Some(if t.chars().count() > MAX {
            format!("{}…", t.chars().take(MAX - 1).collect::<String>())
        } else {
            t.clone()
        })
    }

    fn play_label(&self) -> &'static str {
        if self.playing { "Pause" } else { "Play" }
    }
}

type Handler = Arc<dyn Fn(TrayEvent) + Send + Sync>;

/// Removes the icon when dropped.
pub struct Tray {
    inner: imp::Tray,
}

impl Tray {
    /// Shows the tray icon. Fails when the desktop has no system tray (GNOME
    /// without an extension, say), so closing the window must quit instead.
    /// `on_event` runs on whatever thread the platform delivers clicks on.
    pub fn new(on_event: impl Fn(TrayEvent) + Send + Sync + 'static) -> anyhow::Result<Self> {
        imp::Tray::new(Arc::new(on_event)).map(|inner| Self { inner })
    }

    /// Updates the menu and tooltip. Doesn't wait for the desktop to catch up.
    pub fn set_state(&self, state: TrayState) {
        self.inner.set_state(state);
    }
}

const TOOLTIP: &str = "MavPlayer";
const SHOW_LABEL: &str = "Open MavPlayer";
const HIDE_LABEL: &str = "Hide to Tray";
const PREV_LABEL: &str = "Previous";
const NEXT_LABEL: &str = "Next";
const QUIT_LABEL: &str = "Quit MavPlayer";

/// The app icon as RGBA pixels at about `size` pixels.
fn icon_rgba(size: u32) -> anyhow::Result<(Vec<u8>, u32, u32)> {
    let png: &[u8] = match size {
        0..=16 => include_bytes!("../../packaging/icons/mavplayer-16.png"),
        17..=22 => include_bytes!("../../packaging/icons/mavplayer-22.png"),
        23..=24 => include_bytes!("../../packaging/icons/mavplayer-24.png"),
        25..=32 => include_bytes!("../../packaging/icons/mavplayer-32.png"),
        33..=48 => include_bytes!("../../packaging/icons/mavplayer-48.png"),
        _ => include_bytes!("../../packaging/icons/mavplayer-64.png"),
    };
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)?.into_rgba8();
    let (w, h) = img.dimensions();
    Ok((img.into_raw(), w, h))
}

#[cfg(target_os = "linux")]
mod imp {
    use super::{Handler, TrayEvent, TrayState};
    use ksni::blocking::TrayMethods;

    struct Item {
        on_event: Handler,
        icons: Vec<ksni::Icon>,
        state: TrayState,
    }

    impl ksni::Tray for Item {
        fn id(&self) -> String {
            "mavplayer".into()
        }

        fn title(&self) -> String {
            super::TOOLTIP.into()
        }

        // The app's own pictures rather than a themed icon name: a portable
        // build or AppImage has no icon installed in the theme.
        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            self.icons.clone()
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            let description = if self.state.controls { self.state.tooltip() } else { String::new() };
            ksni::ToolTip { title: super::TOOLTIP.into(), description, ..Default::default() }
        }

        fn activate(&mut self, _x: i32, _y: i32) {
            (self.on_event)(TrayEvent::Activate);
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            use ksni::menu::StandardItem;
            fn item(label: &str, icon: &str, enabled: bool, ev: TrayEvent) -> ksni::MenuItem<Item> {
                StandardItem {
                    label: label.into(),
                    icon_name: icon.into(),
                    enabled,
                    activate: Box::new(move |t: &mut Item| (t.on_event)(ev)),
                    ..Default::default()
                }
                .into()
            }
            let st = &self.state;
            let mut v = Vec::new();
            if st.controls {
                if let Some(title) = st.title_label() {
                    // An underscore marks the access key in a dbusmenu label.
                    let label = title.replace('_', "__");
                    v.push(StandardItem { label, enabled: false, ..Default::default() }.into());
                }
                let icon = if st.playing { "media-playback-pause" } else { "media-playback-start" };
                v.push(item(st.play_label(), icon, st.has_media, TrayEvent::PlayPause));
                v.push(item(super::PREV_LABEL, "media-skip-backward", st.has_media, TrayEvent::Previous));
                v.push(item(super::NEXT_LABEL, "media-skip-forward", st.has_media, TrayEvent::Next));
                v.push(ksni::MenuItem::Separator);
            }
            if st.controls && !st.hidden {
                v.push(item(super::HIDE_LABEL, "window-minimize", true, TrayEvent::Hide));
            } else {
                v.push(item(super::SHOW_LABEL, crate::integration::APP_ID, true, TrayEvent::Show));
            }
            v.push(ksni::MenuItem::Separator);
            v.push(item(super::QUIT_LABEL, "application-exit", true, TrayEvent::Quit));
            v
        }

        // The panel went away (restarted, or its tray widget removed). Keep
        // the item: it reappears when the panel comes back, and launching
        // MavPlayer again still reaches this process meanwhile.
        fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
            log::info!("system tray went away: {reason:?}");
            true
        }
    }

    /// The item's D-Bus name. It stays the same from one process to the next:
    /// the process taking over in the tray requests it with
    /// `ReplaceExisting`, the name passes straight to it, and Plasma (whose
    /// watcher drops an item only once its name has no owner) keeps the icon
    /// as it is. A host that tracks items by unique name (the GNOME
    /// extension) shows the new item next to the old one until that exits.
    /// A separate config folder gets its own name, as it gets its own socket.
    fn bus_name() -> String {
        let base = format!("{}.Tray", crate::integration::APP_ID);
        match std::env::var_os("MAVPLAYER_CONFIG_DIR") {
            Some(d) => {
                use std::hash::{Hash, Hasher};
                let mut h = std::collections::hash_map::DefaultHasher::new();
                d.hash(&mut h);
                format!("{base}.c{:016x}", h.finish())
            }
            None => base,
        }
    }

    /// ARGB32 in network byte order, as StatusNotifierItem wants.
    fn argb_icon(size: u32) -> Option<ksni::Icon> {
        let (mut px, w, h) = super::icon_rgba(size).ok()?;
        for p in px.as_chunks_mut::<4>().0 {
            p.rotate_right(1); // RGBA → ARGB
        }
        Some(ksni::Icon { width: w as i32, height: h as i32, data: px })
    }

    pub struct Tray {
        handle: ksni::blocking::Handle<Item>,
        /// To the thread that applies new states: `update` waits until the
        /// change has gone out over D-Bus.
        updates: std::sync::mpsc::Sender<TrayState>,
    }

    impl Tray {
        pub fn new(on_event: Handler) -> anyhow::Result<Self> {
            let icons = [16, 22, 24, 32, 48, 64].into_iter().filter_map(argb_icon).collect();
            // A Flatpak sandbox may not own the item's usual bus name (it
            // can own `bus_name`, which is under the app ID).
            let sandboxed = std::env::var_os("FLATPAK_ID").is_some() || std::path::Path::new("/.flatpak-info").exists();
            let handle = Item { on_event, icons, state: TrayState::default() }
                .disable_dbus_name(sandboxed)
                .dbus_name(Some(bus_name()))
                .spawn()
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            let (updates, rx) = std::sync::mpsc::channel::<TrayState>();
            let h = handle.clone();
            std::thread::Builder::new().name("tray".into()).spawn(move || {
                while let Ok(mut st) = rx.recv() {
                    // Only the latest of a burst matters.
                    while let Ok(newer) = rx.try_recv() {
                        st = newer;
                    }
                    if h.update(|item| item.state = st).is_none() {
                        break;
                    }
                }
            })?;
            Ok(Self { handle, updates })
        }

        pub fn set_state(&self, state: TrayState) {
            let _ = self.updates.send(state);
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            self.handle.shutdown().wait();
        }
    }
}

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use super::{Handler, TrayEvent, TrayState};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};

    pub struct Tray {
        icon: TrayIcon,
    }

    const EVENTS: [(&str, TrayEvent); 6] = [
        ("show", TrayEvent::Show),
        ("hide", TrayEvent::Hide),
        ("playpause", TrayEvent::PlayPause),
        ("previous", TrayEvent::Previous),
        ("next", TrayEvent::Next),
        ("quit", TrayEvent::Quit),
    ];

    /// The menu for `st`; its items' IDs are `EVENTS`' names.
    fn build_menu(st: &TrayState) -> anyhow::Result<Menu> {
        let menu = Menu::new();
        let item = |id: &str, label: &str, enabled: bool| MenuItem::with_id(id, label, enabled, None);
        if st.controls {
            if let Some(title) = st.title_label() {
                // `&` marks the access key on Windows.
                let label = if cfg!(windows) { title.replace('&', "&&") } else { title };
                menu.append(&MenuItem::new(label, false, None))?;
            }
            menu.append(&item("playpause", st.play_label(), st.has_media))?;
            menu.append(&item("previous", super::PREV_LABEL, st.has_media))?;
            menu.append(&item("next", super::NEXT_LABEL, st.has_media))?;
            menu.append(&PredefinedMenuItem::separator())?;
        }
        if st.controls && !st.hidden {
            menu.append(&item("hide", super::HIDE_LABEL, true))?;
        } else {
            menu.append(&item("show", super::SHOW_LABEL, true))?;
        }
        menu.append(&PredefinedMenuItem::separator())?;
        menu.append(&item("quit", super::QUIT_LABEL, true))?;
        Ok(menu)
    }

    impl Tray {
        /// Must run on the main thread (it owns the platform's event loop).
        pub fn new(on_event: Handler) -> anyhow::Result<Self> {
            let menu = build_menu(&TrayState::default())?;
            let on_menu = on_event.clone();
            MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
                if let Some((_, ev)) = EVENTS.iter().find(|(id, _)| e.id.0 == *id) {
                    on_menu(*ev);
                }
            }));
            TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| {
                if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = e {
                    on_event(TrayEvent::Activate);
                }
            }));
            // Retina menu bars draw the icon at 2x.
            let (rgba, w, h) = super::icon_rgba(if cfg!(target_os = "macos") { 64 } else { 32 })?;
            let icon = TrayIconBuilder::new()
                .with_tooltip(super::TOOLTIP)
                .with_icon(tray_icon::Icon::from_rgba(rgba, w, h)?)
                .with_menu(Box::new(menu))
                // Left click opens the window; the menu is on the right button.
                .with_menu_on_left_click(false)
                .build()?;
            Ok(Self { icon })
        }

        /// Main thread only, like `new`.
        pub fn set_state(&self, state: TrayState) {
            match build_menu(&state) {
                Ok(menu) => self.icon.set_menu(Some(Box::new(menu))),
                Err(e) => log::warn!("tray menu: {e:#}"),
            }
            let _ = self.icon.set_tooltip(Some(state.tooltip()));
        }
    }

    impl Drop for Tray {
        fn drop(&mut self) {
            MenuEvent::set_event_handler(None::<fn(MenuEvent)>);
            TrayIconEvent::set_event_handler(None::<fn(TrayIconEvent)>);
        }
    }
}

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
mod imp {
    use super::{Handler, TrayState};

    pub struct Tray;

    impl Tray {
        pub fn new(_: Handler) -> anyhow::Result<Self> {
            anyhow::bail!("no system tray support on this platform")
        }

        pub fn set_state(&self, _: TrayState) {}
    }
}
