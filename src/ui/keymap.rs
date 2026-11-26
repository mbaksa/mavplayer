//! Keyboard shortcuts: every command that can have one, its default keys,
//! and the user's changes on top (`Settings::key_bindings`).
//!
//! `apply` (re)builds GPUI's keymap from this table. Preferences → Keyboard
//! Shortcuts edits the overrides, and menus look their shortcut labels up here.

use super::actions::*;
use gpui::{Action, App, KeyBinding, Keystroke};
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};

pub struct Command {
    /// Stable name used in the settings file (the action's name).
    pub id: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    pub keys: &'static [&'static str],
    /// Bound in every key context (not only while the player has focus).
    global: bool,
    bind: fn(&str, Option<&str>) -> KeyBinding,
}

fn bind_as<A: Action + Default>(keys: &str, ctx: Option<&str>) -> KeyBinding {
    KeyBinding::new(keys, A::default(), ctx)
}

macro_rules! commands {
    ($( $group:literal => [ $( $act:ident $(@$global:ident)?, $label:literal, [$($k:literal),*] ; )* ] )*) => {
        /// Every rebindable command, in the order Preferences lists them.
        pub const COMMANDS: &[Command] = &[ $( $( Command {
            id: stringify!($act),
            label: $label,
            group: $group,
            keys: &[$($k),*],
            global: commands!(@g $($global)?),
            bind: bind_as::<$act>,
        }, )* )* ];
    };
    (@g global) => { true };
    (@g) => { false };
}

commands! {
    "Playback" => [
        TogglePause, "Play / pause", ["space", "k", "p"];
        Stop, "Stop", ["secondary-."];
        PlayNext, "Next in playlist", ["n", ">"];
        PlayPrevious, "Previous in playlist", ["shift-n", "<"];
        SpeedUp, "Faster", ["]"];
        SpeedDown, "Slower", ["["];
        SpeedReset, "Normal speed", ["backspace"];
        FrameStep, "Next frame", ["."];
        FrameBackStep, "Previous frame", [","];
        ToggleAbLoop, "A-B loop (set A, set B, clear)", ["a"];
        ToggleLoopFile, "Loop current file", ["shift-l"];
        CycleRepeat, "Cycle repeat mode", ["r"];
        ToggleShuffle, "Shuffle", ["shift-s"];
    ]
    "Seeking" => [
        SeekForward, "Jump forward (5 s, configurable)", ["right", "l"];
        SeekBackward, "Jump backward (5 s, configurable)", ["left", "j"];
        SeekForwardSmall, "Jump forward 1 s (exact)", ["shift-right"];
        SeekBackwardSmall, "Jump backward 1 s (exact)", ["shift-left"];
        SeekForwardLarge, "Jump forward (60 s, configurable)", ["secondary-right"];
        SeekBackwardLarge, "Jump backward (60 s, configurable)", ["secondary-left"];
        NextChapter, "Next chapter", ["pageup"];
        PreviousChapter, "Previous chapter", ["pagedown"];
        JumpToTime, "Jump to time…", ["secondary-t", "secondary-g"];
        Seek0, "Jump to start", ["0", "home"];
        Seek1, "Jump to 10 %", ["1"];
        Seek2, "Jump to 20 %", ["2"];
        Seek3, "Jump to 30 %", ["3"];
        Seek4, "Jump to 40 %", ["4"];
        Seek5, "Jump to 50 %", ["5"];
        Seek6, "Jump to 60 %", ["6"];
        Seek7, "Jump to 70 %", ["7"];
        Seek8, "Jump to 80 %", ["8"];
        Seek9, "Jump to 90 %", ["9"];
    ]
    "Audio" => [
        VolumeUp, "Volume up", ["up"];
        VolumeDown, "Volume down", ["down"];
        ToggleMute, "Mute", ["m"];
        CycleAudio, "Next audio track", ["b", "#"];
        CycleAudioBack, "Previous audio track", ["shift-b"];
        AudioDelayIncrease, "Audio delay +100 ms", ["secondary-=", "secondary-+"];
        AudioDelayDecrease, "Audio delay −100 ms", ["secondary--"];
        ShowEffects, "Equalizer & effects", ["secondary-e"];
    ]
    "Subtitles" => [
        ToggleSubtitles, "Show / hide subtitles", ["v"];
        CycleSubtitle, "Next subtitle language", ["c"];
        CycleSubtitleBack, "Previous subtitle language", ["shift-c"];
        SubDelayIncrease, "Subtitle delay +100 ms", ["x"];
        SubDelayDecrease, "Subtitle delay −100 ms", ["z"];
        SubScaleUp, "Larger subtitles", ["shift-g"];
        SubScaleDown, "Smaller subtitles", ["shift-f"];
        OpenSubtitle, "Load subtitle file…", ["secondary-shift-o"];
    ]
    "Video" => [
        ToggleFullscreen, "Fullscreen", ["f", "f11", "alt-enter"];
        ExitFullscreen, "Leave fullscreen", ["escape"];
        Screenshot, "Screenshot", ["s"];
        CycleAspect, "Cycle aspect ratio", ["shift-a"];
        CycleDeinterlace, "Cycle deinterlacing", ["d"];
        ToggleHwdec, "Toggle hardware decoding", ["secondary-h"];
        RotateClockwise, "Rotate 90°", ["secondary-r"];
        ZoomIn, "Zoom in", ["alt-=", "alt-+"];
        ZoomOut, "Zoom out", ["alt--"];
        ZoomReset, "Reset zoom", ["alt-backspace"];
        FitWindow, "Resize window to video", ["secondary-0"];
    ]
    "Media" => [
        OpenFile, "Open file…", ["secondary-o"];
        OpenFolder, "Open folder…", ["secondary-f"];
        OpenUrl, "Open URL…", ["secondary-u"];
        OpenDisc, "Open disc…", ["secondary-d"];
        DiscMenu, "Disc menu (DVD)", ["shift-m"];
        DiscTitleMenu, "Disc title menu (DVD)", ["shift-t"];
        SavePlaylist, "Save playlist…", ["secondary-s"];
        ShowMediaInfo, "Media information", ["i", "secondary-i"];
    ]
    "Window" => [
        TogglePlaylist, "Show / hide playlist", ["secondary-l"];
        ToggleMenubar, "Show / hide menu bar", ["secondary-m"];
        ShowPreferences, "Preferences", ["secondary-,", "secondary-p"];
        ShowShortcuts, "Keyboard shortcuts", ["f1", "?"];
        HideToTray, "Hide to the tray (keeps playing)", ["secondary-w"];
        Quit @global, "Quit", ["secondary-q"];
    ]
}

thread_local! {
    /// The keys each command has right now (set by `apply`).
    static CURRENT: RefCell<HashMap<&'static str, Vec<String>>> = RefCell::new(HashMap::new());
}

pub fn command(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|c| c.id == id)
}

/// A command's keys with the user's changes applied.
pub fn keys_for(c: &Command, overrides: &BTreeMap<String, Vec<String>>) -> Vec<String> {
    overrides.get(c.id).cloned().unwrap_or_else(|| c.keys.iter().map(|k| k.to_string()).collect())
}

/// Sets a command's keys, dropping the override when they are the defaults.
pub fn set_keys(c: &Command, keys: Vec<String>, overrides: &mut BTreeMap<String, Vec<String>>) {
    if keys.iter().map(String::as_str).eq(c.keys.iter().copied()) {
        overrides.remove(c.id);
    } else {
        overrides.insert(c.id.to_string(), keys);
    }
}

/// Parses every keystroke of a binding ("ctrl-k" or a sequence "g g").
fn valid(keys: &str) -> bool {
    !keys.trim().is_empty() && keys.split_whitespace().all(|k| Keystroke::parse(k).is_ok())
}

/// Same keys, however they were written ("secondary-s" and "ctrl-s" on Linux).
pub fn same_keys(a: &str, b: &str) -> bool {
    let norm = |s: &str| -> String {
        s.split_whitespace()
            .map(|k| Keystroke::parse(k).map(|k| k.unparse()).unwrap_or_else(|_| k.to_string()))
            .collect::<Vec<_>>()
            .join(" ")
    };
    norm(a) == norm(b)
}

/// Rebuilds the app's keymap from the table and `overrides`.
pub fn apply(cx: &mut App, overrides: &BTreeMap<String, Vec<String>>) {
    let mut bindings = Vec::new();
    let mut current = HashMap::new();
    for c in COMMANDS {
        let mut keys = keys_for(c, overrides);
        keys.retain(|k| {
            let ok = valid(k);
            if !ok {
                log::warn!("ignoring unreadable shortcut {k:?} for {}", c.id);
            }
            ok
        });
        for k in &keys {
            bindings.push((c.bind)(k, if c.global { None } else { Some(CTX) }));
        }
        current.insert(c.id, keys);
    }
    bindings.push(KeyBinding::new("escape", CloseOverlay, Some("Overlay")));
    // Fixed navigation keys, scoped to the focused playlist rather than playback.
    let playlist = Some(PLAYLIST_CTX);
    bindings.extend([
        KeyBinding::new("tab", PlaylistFocus, Some("PlaylistVisible")),
        KeyBinding::new("shift-tab", PlaylistFocus, Some("PlaylistVisible")),
        KeyBinding::new("tab", PlaylistFocus, Some("EditingText")),
        KeyBinding::new("shift-tab", PlaylistFocusPlayer, Some("EditingText")),
        KeyBinding::new("tab", PlaylistFocusPlayer, playlist),
        KeyBinding::new("shift-tab", PlaylistFocusPlayer, playlist),
        KeyBinding::new("escape", PlaylistFocusPlayer, playlist),
        KeyBinding::new("up", PlaylistUp, playlist),
        KeyBinding::new("down", PlaylistDown, playlist),
        KeyBinding::new("shift-up", PlaylistExtendUp, playlist),
        KeyBinding::new("shift-down", PlaylistExtendDown, playlist),
        KeyBinding::new("home", PlaylistHome, playlist),
        KeyBinding::new("end", PlaylistEnd, playlist),
        KeyBinding::new("pageup", PlaylistPageUp, playlist),
        KeyBinding::new("pagedown", PlaylistPageDown, playlist),
        KeyBinding::new("delete", PlaylistDelete, playlist),
        KeyBinding::new("enter", PlaylistPlay, playlist),
        KeyBinding::new("secondary-a", PlaylistSelectAll, playlist),
        KeyBinding::new("f3", PlaylistFilter, playlist),
    ]);
    // A DVD menu's buttons take the arrow keys and Enter (after the rest,
    // so they win over seeking and volume while it shows).
    bindings.push(KeyBinding::new("up", DiscUp, Some(DVD_CTX)));
    bindings.push(KeyBinding::new("down", DiscDown, Some(DVD_CTX)));
    bindings.push(KeyBinding::new("left", DiscLeft, Some(DVD_CTX)));
    bindings.push(KeyBinding::new("right", DiscRight, Some(DVD_CTX)));
    bindings.push(KeyBinding::new("enter", DiscActivate, Some(DVD_CTX)));
    cx.clear_key_bindings();
    cx.bind_keys(bindings);
    super::text_input::bind_keys(cx);
    CURRENT.with(|m| *m.borrow_mut() = current);
}

/// The first shortcut of the command for `action`, for menus and tooltips.
pub fn shortcut_for(action: &dyn Action) -> Option<String> {
    let id = action.name().rsplit("::").next()?;
    CURRENT.with(|m| m.borrow().get(id).and_then(|k| k.first()).map(|k| shortcut(k)))
}

/// "Label (Ctrl+L)", or just the label when the command has no keys.
pub fn hint(label: &str, action: &dyn Action) -> String {
    match shortcut_for(action) {
        Some(k) => format!("{label} ({k})"),
        None => label.to_string(),
    }
}

/// Shortcut text for people (platform-appropriate modifier names).
pub fn shortcut(keys: &str) -> String {
    let mac = cfg!(target_os = "macos");
    keys.split_whitespace()
        .map(|chord| {
            // The key is after the last '-', unless the key itself is '-'.
            let (mods, key) = if chord == "-" {
                ("", "-")
            } else if let Some(m) = chord.strip_suffix("--") {
                (m, "-")
            } else {
                chord.rsplit_once('-').unwrap_or(("", chord))
            };
            let mut parts: Vec<String> = mods
                .split('-')
                .filter(|m| !m.is_empty())
                .map(|m| {
                    match m {
                        "secondary" => {
                            if mac {
                                "⌘"
                            } else {
                                "Ctrl"
                            }
                        }
                        "ctrl" | "control" => {
                            if mac {
                                "⌃"
                            } else {
                                "Ctrl"
                            }
                        }
                        "cmd" | "platform" => "⌘",
                        "super" => "Super",
                        "win" => "Win",
                        "shift" => {
                            if mac {
                                "⇧"
                            } else {
                                "Shift"
                            }
                        }
                        "alt" => {
                            if mac {
                                "⌥"
                            } else {
                                "Alt"
                            }
                        }
                        "fn" => "Fn",
                        m => m,
                    }
                    .to_string()
                })
                .collect();
            parts.push(key_name(key));
            parts.join(if mac { "" } else { "+" })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn key_name(k: &str) -> String {
    match k {
        "pageup" => "Page Up".into(),
        "pagedown" => "Page Down".into(),
        "backspace" => "Backspace".into(),
        "escape" => "Esc".into(),
        "enter" => "Enter".into(),
        "space" => "Space".into(),
        "up" => "↑".into(),
        "down" => "↓".into(),
        "left" => "←".into(),
        "right" => "→".into(),
        k if k.chars().count() == 1 => k.to_uppercase(),
        k => {
            let mut c = k.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        }
    }
}

/// Keystrokes that only press a modifier, which can't be a shortcut alone.
pub fn is_modifier_only(k: &Keystroke) -> bool {
    matches!(k.key.as_str(), "shift" | "control" | "ctrl" | "alt" | "platform" | "cmd" | "super" | "fn" | "function")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sound() {
        let mut ids = std::collections::HashSet::new();
        for c in COMMANDS {
            assert!(ids.insert(c.id), "{} listed twice", c.id);
            for k in c.keys {
                assert!(valid(k), "{}: {k}", c.id);
            }
        }
        // No two commands share a default key.
        let all: Vec<(&str, &str)> = COMMANDS.iter().flat_map(|c| c.keys.iter().map(move |k| (c.id, *k))).collect();
        for (i, (a, ka)) in all.iter().enumerate() {
            for (b, kb) in &all[i + 1..] {
                assert!(!same_keys(ka, kb), "{a} and {b} both use {ka}");
            }
        }
    }

    #[test]
    fn overrides_round_trip() {
        let c = command("TogglePause").unwrap();
        let mut o = BTreeMap::new();
        set_keys(c, vec!["ctrl-k".into()], &mut o);
        assert_eq!(keys_for(c, &o), vec!["ctrl-k".to_string()]);
        set_keys(c, c.keys.iter().map(|k| k.to_string()).collect(), &mut o);
        assert!(o.is_empty());
        set_keys(c, vec![], &mut o);
        assert!(keys_for(c, &o).is_empty());
    }

    #[test]
    fn readable_shortcuts() {
        if cfg!(target_os = "macos") {
            return;
        }
        assert_eq!(shortcut("secondary-shift-o"), "Ctrl+Shift+O");
        assert_eq!(shortcut("secondary--"), "Ctrl+-");
        assert_eq!(shortcut("alt-backspace"), "Alt+Backspace");
        assert_eq!(shortcut("-"), "-");
        assert_eq!(shortcut("pageup"), "Page Up");
        assert_eq!(shortcut("ctrl-shift-/"), "Ctrl+Shift+/");
        assert!(same_keys("secondary-s", "ctrl-s"));
        assert!(!same_keys("s", "shift-s"));
    }
}
