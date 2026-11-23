# Hacking on MavPlayer

This document explains how to build MavPlayer, how to produce each release
package, and how the code is organised so you can change it.

- [Getting the source](#getting-the-source)
- [Development build](#development-build)
- [Release packages](#release-packages)
- [How the code is organised](#how-the-code-is-organised)
- [Common changes](#common-changes)
- [Debugging and testing](#debugging-and-testing)

## Getting the source

```bash
git clone --recurse-submodules https://github.com/Drogobo/mavplayer.git
```

The [Papirus icon theme](https://github.com/PapirusDevelopmentTeam/papirus-icon-theme)
is a git submodule (`third_party/papirus-icon-theme`, shallow). `build.rs` compiles
the ~55 icons the UI uses into the binary, so neither the repository nor the user
needs Papirus installed. If you cloned without `--recurse-submodules`:

```bash
git submodule update --init --depth 1
```

You can also point the build at any Papirus checkout with
`MAVPLAYER_PAPIRUS_DIR=/path/to/papirus-icon-theme`.

## Development build

You need:

- **Rust** 1.90 or newer (`rustup`, stable toolchain).
- **FFmpeg 9** libraries and headers: libavcodec, libavformat, libavfilter,
  libswscale, libswresample, libavutil. The bindings are generated with bindgen,
  so **clang/libclang** is needed too.
- The libraries GPUI needs on Linux.

| System | Install |
| --- | --- |
| Arch / CachyOS / Manjaro | `sudo pacman -S rust clang pkgconf ffmpeg libdvdnav libpipewire alsa-lib libxkbcommon-x11 libxcb wayland vulkan-icd-loader fontconfig` |
| Fedora (RPM Fusion for FFmpeg 9) | `sudo dnf install rust cargo clang pkgconf ffmpeg-devel libdvdnav-devel pipewire-devel alsa-lib-devel libxkbcommon-x11-devel libxcb-devel wayland-devel vulkan-loader-devel fontconfig-devel` |
| Debian / Ubuntu | `sudo apt install build-essential clang pkg-config libpipewire-0.3-dev libasound2-dev libxkbcommon-x11-dev libxcb1-dev libwayland-dev libvulkan-dev libfontconfig1-dev`, then build FFmpeg 9 with `scripts/build-ffmpeg.sh` (distribution FFmpeg is usually older) |
| macOS | `xcode-select --install`, `brew install ffmpeg libdvdnav pkg-config` |
| Windows | Rust (MSVC), [LLVM](https://github.com/llvm/llvm-project/releases) (set `LIBCLANG_PATH`), and an FFmpeg 9 "shared" build such as BtbN's `ffmpeg-n9.0-latest-win64-gpl-shared-9.0.zip` with `FFMPEG_DIR` pointing at it; put its `bin` folder on `PATH` to run |

Then:

```bash
cargo run -- path/to/video.mkv           # debug build (opt-level 1, deps at 2)
cargo run --release -- path/to/video.mkv # what users get
cargo test
cargo clippy -- -D warnings              # CI enforces this
cargo fmt                                # CI checks formatting
```

If FFmpeg is installed somewhere `pkg-config` can't see, set
`PKG_CONFIG_PATH=/prefix/lib/pkgconfig` or `FFMPEG_DIR=/prefix`.

## Release packages

All packages are built by [`.github/workflows/build.yml`](.github/workflows/build.yml)
when a tag such as `v0.2.0` is pushed, which also publishes a GitHub release
with everything attached. Pull requests build too, and so does **Actions ▸
Build ▸ Run workflow**; pushes to `main` don't. To release: bump `version` in
`Cargo.toml`, `packaging/arch/PKGBUILD` and the `<releases>` list in
`packaging/linux/io.github.drogobo.MavPlayer.metainfo.xml`, commit, tag, and
push both.

You can build each package locally.

### Statically linked FFmpeg (AppImage, deb, rpm, macOS)

Distribution FFmpeg versions vary a lot, so the portable packages link a
decode-only FFmpeg 9 statically. This also makes them smaller and keeps startup
fast. [`scripts/build-ffmpeg.sh`](scripts/build-ffmpeg.sh) downloads and builds
it together with dav1d (AV1), zimg (HDR tone mapping), libdvdread/libdvdnav
(DVD) and the NVIDIA decode headers. On Linux it also builds glslang and
libplacebo and enables FFmpeg's Vulkan support and `libplacebo` filter, which run
the GPU upscalers:

```bash
# Debian/Ubuntu build dependencies (libplacebo needs meson >= 1.3; on older
# releases use `pip install meson`):
sudo apt install nasm meson ninja-build cmake git python3 autoconf automake libtool curl \
    libssl-dev zlib1g-dev libva-dev
scripts/build-ffmpeg.sh                       # → ffmpeg-build/prefix
export PKG_CONFIG_PATH=$PWD/ffmpeg-build/prefix/lib/pkgconfig
cargo build --release --features static-ffmpeg
```

The `static-ffmpeg` feature turns on `ffmpeg-sys-next/static`, which links
everything `pkg-config --static` lists. OpenSSL, zlib and libva stay dynamic on
Linux, because every distribution ships them.

libplacebo and glslang (without its SPIR-V optimizer) are linked statically too,
but the Vulkan loader is never linked: FFmpeg `dlopen`s the system's
`libvulkan.so.1` when the first upscaler starts and passes libplacebo its
`vkGetInstanceProcAddr` (libplacebo is built with `-Dvk-proc-addr=disabled`).
Without a Vulkan driver the upscalers simply fail to start and MavPlayer falls
back to bilinear. FFmpeg's newer Vulkan headers come from libplacebo's
`3rdparty/Vulkan-Headers` submodule and are copied into the prefix. macOS builds
skip libplacebo, since it would need MoltenVK bundled in the app, so they offer
only Bilinear, Bicubic and Nearest.

### AppImage, .deb and .rpm

```bash
cargo install cargo-deb cargo-generate-rpm
export PKG_CONFIG_PATH=$PWD/ffmpeg-build/prefix/lib/pkgconfig
scripts/package-linux.sh            # all three, into dist/
scripts/package-linux.sh appimage   # or just one: deb | rpm | appimage
```

- **deb** and **rpm** come from `[package.metadata.deb]` and
  `[package.metadata.generate-rpm]` in `Cargo.toml`. Library dependencies are
  detected from the binary.
- The **AppImage** is assembled by
  [linuxdeploy](https://github.com/linuxdeploy/linuxdeploy), which is downloaded
  if it isn't installed. linuxdeploy bundles the shared libraries that aren't
  guaranteed on every system. The entry point is `packaging/appimage/AppRun`.
  Build on the oldest distribution you want to support (CI uses Ubuntu 22.04),
  because the AppImage needs a glibc at least as new as the build machine's.
- `MAVPLAYER_DYNAMIC_FFMPEG=1 scripts/package-linux.sh` links the system FFmpeg
  instead (for distribution-specific packages).
- **Old PipeWire (Ubuntu 22.04's 0.3.48):** the `libspa` crate needs newer SPA
  headers. `scripts/spa-headers.sh` puts PipeWire 1.2.7's into `spa-headers/`;
  put `$PWD/spa-headers/lib/pkgconfig` first in `PKG_CONFIG_PATH`. SPA is
  header-only and its audio structs haven't changed, so the binary still runs
  with the system's older PipeWire.

**AppImage desktop integration** lives in the app itself, in
[`src/integration.rs`](src/integration.rs). When the app runs from an AppImage
(the runtime sets `$APPIMAGE`), it offers on first launch to write
`~/.local/share/applications/io.github.drogobo.MavPlayer.desktop` and icons that
point at the AppImage file. The same is available from the Media menu,
Preferences ▸ Interface, and `--integrate` / `--remove-integration`.

**Window icon on Wayland**: compositors find a window's icon through the
`.desktop` file named after its app ID (`io.github.drogobo.MavPlayer`). Packages
install one. For unpackaged runs (`cargo run`, a non-integrated AppImage),
`integration::ensure_window_icon()` writes a hidden (`NoDisplay=true`), icon-only
entry with no MIME types to `~/.local/share/applications` at startup.

### Arch Linux (.pkg.tar.zst)

[`packaging/arch/PKGBUILD`](packaging/arch/PKGBUILD) builds the checkout it lives
in, against Arch's FFmpeg:

```bash
cd packaging/arch
makepkg -s                       # → mavplayer-<ver>-1-x86_64.pkg.tar.zst
_from_git=1 makepkg -s           # build the tagged release from GitHub instead
```

### Flatpak

[`packaging/flatpak/io.github.drogobo.MavPlayer.yml`](packaging/flatpak/io.github.drogobo.MavPlayer.yml)
builds libdvdread, libdvdnav, zimg, libplacebo and FFmpeg 9 as modules on the
freedesktop 25.08 runtime, which already provides dav1d, VA-API, and the Vulkan
loader and headers, shaderc and lcms2 that libplacebo uses. Cargo dependencies must be
vendored for the offline build:

```bash
curl -O https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/master/cargo/flatpak-cargo-generator.py
pip install aiohttp toml tomlkit
python3 flatpak-cargo-generator.py Cargo.lock -o packaging/flatpak/cargo-sources.json
flatpak install --user flathub org.freedesktop.Sdk//25.08 \
    org.freedesktop.Sdk.Extension.rust-stable//25.08 org.freedesktop.Sdk.Extension.llvm20//25.08
cd packaging/flatpak
flatpak-builder --user --install --force-clean build io.github.drogobo.MavPlayer.yml
flatpak run io.github.drogobo.MavPlayer
# single-file bundle:
flatpak build-bundle ~/.local/share/flatpak/repo MavPlayer.flatpak io.github.drogobo.MavPlayer
```

Regenerate `cargo-sources.json` whenever `Cargo.lock` changes.

### Windows (.zip and installer)

From PowerShell, with Rust, LLVM and (for the installer)
[Inno Setup 6](https://jrsoftware.org/isinfo.php) installed:

```powershell
$env:LIBCLANG_PATH = "C:\Program Files\LLVM\bin"
scripts\package-windows.ps1     # downloads FFmpeg unless -FfmpegDir / $env:FFMPEG_DIR is given
```

This produces a portable `dist\MavPlayer-<ver>-windows-x64.zip` (the exe plus
FFmpeg DLLs) and `dist\MavPlayer-<ver>-windows-x64-setup.exe`. The installer
script is [`packaging/windows/mavplayer.iss`](packaging/windows/mavplayer.iss);
it adds Start-menu and optional desktop shortcuts and "Open with" entries. The
exe gets its icon and version info from `build.rs` (winresource). Release builds
use the Windows GUI subsystem, so no console window opens.

### macOS (.dmg)

```bash
brew install nasm meson ninja pkg-config autoconf automake libtool
scripts/build-ffmpeg.sh
export PKG_CONFIG_PATH=$PWD/ffmpeg-build/prefix/lib/pkgconfig
scripts/package-macos.sh        # → dist/MavPlayer-<ver>-macos-<arch>.dmg
```

The script builds `MavPlayer.app` from `packaging/macos/Info.plist` and
`MavPlayer.icns`. It checks that only system libraries are linked dynamically,
signs the app (ad-hoc, or with `CODESIGN_IDENTITY="Developer ID Application: …"`)
and wraps it in a DMG. Notarization (`xcrun notarytool`) is needed for
distribution without Gatekeeper warnings.

### Icons

`assets/app-icon.svg` is the application icon. After editing it, run
`scripts/gen-icons.sh` (needs `rsvg-convert` and Python Pillow) to regenerate
`packaging/icons/*.png`, `packaging/windows/mavplayer.ico` and
`packaging/macos/MavPlayer.icns`. Copy the SVG to
`packaging/linux/io.github.drogobo.MavPlayer.svg` too.

## How the code is organised

```
src/
  main.rs            CLI parsing, logger, GPUI app and window setup
  settings.rs        Settings (settings.json) and resume positions (resume.json)
  playlist.rs        Playlist model, file-type lists, M3U/PLS, argument parsing
  icons.rs           Icon enum; desktop icon-theme lookup; bundled Papirus fallback
  integration.rs     Linux .desktop integration (AppImage, Wayland window icon)
  font_prefetch.rs   Linux: warms GPUI's system-font scan in parallel (cold-start speed)
  engine/            Playback engine; no UI code
  ui/                GPUI interface
build.rs             Embeds Papirus icons; Windows exe resources
```

### The engine (`src/engine`)

The engine follows the design of FFmpeg's `ffplay`, with threads connected by
queues. One playback session (`SessionCtx` in `engine/mod.rs`) runs these
threads:

```
                ┌──────────── video packets ──► video.rs  decode → libavfilter → FrameQueue
demux.rs  ──────┼──────────── audio packets ──► audio.rs  decode → libavfilter → AudioBuffer
 (read, seek)   └──────────── subtitle pkts ──► subs.rs   decode → SubStore
                                                    │
present.rs: picks the frame due now (relative to the master clock), converts it
            to BGRA with swscale at display size, hands it to the UI.
audio.rs:   output callback pulls samples and drives the master clock: native
            PipeWire on Linux (audio/pw_out.rs), cpal elsewhere and as the
            ALSA fallback.
```

- **`types.rs`**: public data types (`MediaSource`, `MediaInfo`, `Track`,
  `PlaybackState`, `EngineConfig`, effect settings, events).
- **`av.rs`**: thin RAII wrappers over the FFmpeg C API (`Packet`, `Frame`,
  `CodecCtx`, `FormatCtx`, …) and helpers. All `unsafe` FFI is kept close to
  these wrappers.
- **`sync.rs`**: `PacketQueue` and `FrameQueue`, and `Clock`. Every queue item
  carries a *serial*; a seek increments it, so stale data is recognised and
  dropped everywhere without locks between stages.
- **`demux.rs`**: opens inputs (files, URLs with reconnect options, DVDs through
  FFmpeg's `dvdvideo` demuxer), picks tracks by language preference, and runs the
  read loop: seeks, track switches, pause, back-pressure, EOF.
- **`decoder.rs`**: opens codecs, including hardware decoding through a
  `get_format` callback with software fallback, and runs the send/receive loop.
  Hardware devices (CUDA, VAAPI, …) are shared by every decoder, since opening
  one takes 100–500 ms. One is opened in the background as soon as a file turns
  out to have a video stream (`demux::open_input`, while the streams are
  probed), and closed again once no decoder has used it for 30 seconds: a CUDA
  device costs about 100 MB. A decoder that has reached the end of the file
  closes its codec after 20 seconds without a seek (`Decoder::park`) and opens
  it again on the next one. On Linux, CUDA is tried before
  VAAPI when VAAPI would only reach an NVIDIA GPU through the NVDEC wrapper
  driver (it opens twice as slowly and decodes the same way).
- **`video.rs` / `audio.rs` / `filters.rs`**: build libavfilter chains from the
  current settings. These do deinterlacing, tone mapping, crop, rotation, colour
  adjustment, the equalizer, normalization, tempo, and format conversion. The
  decoder threads rebuild the graph when a setting changes.
- **`upscale.rs`**: GPU upscaling. When the picture is enlarged and a GPU
  upscaler is chosen, `build_chain` ends with FFmpeg's `libplacebo` filter at
  display size. The bundled mpv shaders (`third_party/shaders`, compiled in with
  `include_str!`) are concatenated into one hook file and passed as
  `custom_shader_bin`. One Vulkan device is shared by every graph (creating it
  is slow on some drivers; `prewarm` starts it when a video opens, and it is
  closed after 30 seconds unused), and compiled shaders are
  cached under the user cache directory. Window resizes are debounced before the
  graph is rebuilt, and a graph that fails to build turns upscaling off for
  those settings with a warning instead of retrying every frame.
- **`present.rs`**: frame timing (`compute_target_delay`), frame dropping,
  stepping, colour conversion, screenshots.
- **`subs.rs`**: subtitle decoding (text, ASS, bitmap), external file loading
  with charset detection, sidecar discovery.
- **`disc.rs` / `probe.rs`**: disc detection (DVD folders, ISOs, drives, audio
  CDs and `.cue` images; `read_disc` tells them apart for the Open Disc dialog)
  and DVD title probing, plus quick metadata probes for playlist entries.
- **`nav.rs` / `spu.rs`**: DVDs with their menus (see below).
- **`cdda.rs`**: audio CDs (see below).

**DVD menus.** `dvd://` with no title (`MediaSource::Dvd { title: 0 }`) is
played through libdvdnav when the build has it, instead of FFmpeg's `dvdvideo`
demuxer, which can only play titles. libdvdnav runs the disc's navigation
program (first play, menus, button commands, what follows a title) and returns
2048-byte MPEG-PS blocks, which `nav::NavIo` feeds to FFmpeg's `mpeg` demuxer
through a custom AVIOContext (`av::custom_io_context`).

- Playback is split into *segments*: one per menu domain, VTS or title.
  libdvdnav's events (still frame, wait, VTS or title change, hop, stop) end
  the read with EOF, and `nav::play_segment` handles them: a still or a wait
  drains the decoders and holds the last picture; a jump flushes them. Each
  segment gets its own `FormatCtx` whose streams are described by the disc's IFO
  attributes, so the UI's track lists show the disc's languages, and decoders
  restart for each segment.
- Timestamps are rewritten into one continuous timeline. Each VOBU (from its
  NAV packet's PCI) records the offset for its byte range, and packets look up
  theirs by `pos`. The offset is rebased from `dvdnav_get_current_time` at the
  start of a segment, after a jump and after a seek.
- The disc chooses audio and subtitle streams (logical stream numbers, mapped
  to physical ones). Choosing a track in the UI tells the VM too.
- `spu.rs` decodes DVD subpictures itself, because FFmpeg's `dvdsub` decoder
  can't draw menu button highlights. `Overlay` keeps the CLUT, the current
  subpicture and the highlight (rectangle and palette from libdvdnav) and
  redraws the subtitle bitmap when the selected button changes.
- The UI sends input through `Player::dvd_command` (`DvdCommand`: arrows,
  activate, hover and click in picture coordinates, menu calls, titles,
  angles), and reads `PlaybackState::dvd` to know whether buttons are showing.
  While they are, the player's key context is `Player DvdMenu`, whose arrow and
  Enter bindings take precedence.
- Resume positions aren't saved for DVDs played this way.

`build.rs` sets `cfg(has_dvdnav)` when libdvdnav can be linked: with
`static-ffmpeg`, when libavformat's static link line already includes
`-ldvdnav`; otherwise when `pkg-config dvdnav` finds it.
`MAVPLAYER_NO_DVDNAV=1` builds without it. Windows builds never have it, so
run clippy that way too. Without it, DVDs play title by title
through `dvdvideo` and the menu options are hidden (`dvd_menus_available`).

**Audio CDs.** `MediaSource::Cd { device, track }` (track 0 is the whole disc,
which the UI expands into one playlist entry per track). `cdda.rs` reads the
TOC (and CD-Text through a raw READ TOC/PMA/ATIP command) and the raw 44.1 kHz
16-bit stereo sectors, and `demux.rs` opens them with FFmpeg's `s16le` demuxer
through a seekable custom IO, so seeking, gapless playback and the audio
filters work as for files. Sectors are read in chunks; a chunk that fails is
retried sector by sector, and unreadable sectors play as silence. Drives are read with
the Linux `CDROM*` ioctls or Windows `IOCTL_CDROM_*` (untested). macOS mounts
audio CDs as AIFF files, which are played as files. `.cue` sheets with
`BINARY` or `MOTOROLA` `.bin` images are read the same way. The URL form is
`cdda://[TRACK][/DEVICE]`.

**Gapless transitions.** The UI queues what follows the current item with
`Player::set_next` (the next playlist entry, or the same file when looping).
About 10 s before the end, once the current file is fully read, the demuxer
preloads it as a second, *standby* session: it opens, decodes and buffers,
but shows and plays nothing, and its state and events are held back. The
audio output (`audio::OutputLink`) is not tied to one session: the standby
session queues its audio on it, and on the sample where the current file runs
dry the output callback carries straight on with the next one and promotes
it (`SessionCtx::promote`), which publishes its state and sends
`PlayerEvent::Advanced`. Without audio, or when the next file needs a
different channel layout, the demuxer promotes it at end of file instead.

The UI talks to the engine only through `engine::Player`: `load`, `seek`,
`set_paused`, `set_volume`, `select_track`, `set_config`, `take_frame`,
`state`, `drain_events`, and so on. The engine wakes the UI through a callback
whenever a new frame or event is ready.

**Audio is the master clock** when there is audio. The device is opened on the
audio thread, so video never waits for it to show the first frame. Playback
itself starts once the device is ready, so audio and video begin together. If
no device is available, it doesn't open within 3 s, or it stops consuming
samples, `audio.rs` switches to a silent sink that consumes audio in real time.
Playback then keeps going at the right speed.

### The interface (`src/ui`)

All UI state lives in one GPUI view, `PlayerView`. Its methods are split across
files:

| File | Contents |
| --- | --- |
| `player.rs` | `PlayerView` struct, engine plumbing (wake loop, 200 ms tick), opening media, playlist logic, action handlers (`register_actions`) |
| `render.rs` | `impl Render`: video canvas, subtitles, OSD, seek bar, control bar, playlist panel, titlebar for client-side decorations |
| `menus.rs` | Contents of the menu bar menus, context menu and control-bar popups |
| `menu.rs` | The in-window popup menu widget (`MenuItem`, submenus) |
| `dialogs.rs` | Preferences, Effects & Filters, Media Info, Open URL/Disc, Jump to Time, About, Shortcuts, Integration |
| `actions.rs` | GPUI actions and the macOS native menu |
| `keymap.rs` | Every rebindable command with its default keys (`COMMANDS`), the user's changes (`Settings::key_bindings`), applying them to GPUI, shortcut labels for menus |
| `shell.rs` | What outlives the window: opening it, media from later launches and the OS, the tray icon, and waiting in the tray when the window closes |
| `tray.rs` | The system tray icon and its menu (`ksni` on Linux, `tray-icon` on Windows and macOS) |
| `theme.rs` | Light/dark palettes |
| `widgets.rs` | Small helpers: `icon()`, tooltips, the draggable `scrollbar()`, time formatting/parsing |
| `text_input.rs` | Single-line text field |

Video frames reach GPUI as `RenderImage`s. Old textures are released a few frames
later with `window.drop_image`, because GPUI frees atlas textures immediately.

**Keeping running in the tray.** On Linux and Windows, GPUI ends its event loop
as soon as the last window closes, and an app can't be started again in the
same process. So with "Keep running in the system tray" on, the closing
process starts `mavplayer --background --handover` and quits. That process
loads everything a window needs (libraries, fonts, the GPU context, the
hardware decoder), takes over the single-instance socket and the tray icon,
but opens no window: GPUI only ends the loop when a window closes. The
hand-over (`shell::finish_handover`):

1. The new process shows its icon first thing. On Linux the icon's D-Bus name
   is fixed (`io.github.drogobo.MavPlayer.Tray`) and requested with
   `ReplaceExisting`, so it passes straight from the old process to the new
   one. Plasma's watcher only drops an item once its name has no owner, so
   the icon never blinks. This needs a small patch to ksni (`dbus_name`, in
   `third_party/ksni`). The new process then reports "ready" on its stdout pipe.
2. The old process deletes the instance socket's file, closes the new
   process's stdin and exits.
3. On that cue the new process binds the socket at once (no polling for the
   old one to exit) and brings up GPUI. It opens no hardware decoder or other
   GPU device until a video is opened: waiting in the tray takes about 70 MB.

Any later launch, with files or not, is handed to it (`background_only`
requests), and it opens a window in a few tens of milliseconds. On macOS the
app stays running without windows instead. Without a working tray (GNOME
without an AppIndicator extension, say) closing the window quits as usual.

**Playing in the background.** "Play in the background" can't use the
hand-over: the player lives in the window. Instead, closing the window while
a file is open (`on_window_should_close`) hides it (`PlayerView::hide_to_tray`):
the window stays open, so GPUI's loop, the engine, media keys and gapless
playback carry on, and showing it is instant. GPUI has no way to hide a
window on Linux, so it is patched (`third_party/gpui`, `Window::set_window_visible`):
on Wayland, attaching no buffer unmaps the toplevel, and showing it maps it
again like a new window; X11 unmaps, Windows and macOS hide. The tray menu
follows the player through `shell::set_tray_state` (from the 200 ms tick),
and its clicks reach `PlayerView::on_tray`. Picture conversion stops while
hidden (`set_video_hidden`). With nothing open, closing the window works as
before (hand-over or quit).

**Memory.** Measured on Linux with an NVIDIA GPU:

- GPUI's renderer, blade-graphics, is patched (`third_party/blade-graphics`)
  never to enable Vulkan ray tracing, which NVIDIA's driver otherwise sets up
  for 50 MB in every process.
  Big textures and upload buffers get exact-size memory, and its upload belt
  (`third_party/blade-util`) frees idle frame-sized staging chunks instead of
  keeping one per window size played at.
- `mem.rs` installs a global allocator that keeps a few freed blocks of 1 MB
  or more for reuse at the same size (converted video frames), and fixes
  glibc's mmap threshold at 1 MB. Without that, glibc's per-thread arenas held
  30 to 70 MB of freed frames. Its thread returns idle memory and closes idle
  GPU devices every second.
- With CUDA, pictures are resized and converted to BGRA on the GPU
  (`scale_cuda`, needs FFmpeg 9.1 for BGRA) and downloaded once, at display
  size, into pinned memory. Other frames downloaded from a hardware decoder
  come from a buffer pool (`DownloadPool` in `decoder.rs`).
- "Auto" decoder threads means at most 4 (8 for software decoding above
  1080p), not FFmpeg's one per CPU. Each frame thread holds its own pictures.
- Files and discs are read 3 seconds ahead (`LOCAL_READ_AHEAD_SECS`); the
  network cache setting applies to streams only.
- The UI keeps only the IDs of textures it retires (`retire_frame`), not their
  pixels.

## Common changes

**Add a keyboard shortcut / action**
1. Add the action name to `actions!(…)` in `ui/actions.rs`.
2. Add it to `COMMANDS` in `ui/keymap.rs` with a label, a group and its default
   keys (`"secondary-x"` means Ctrl on Linux/Windows and ⌘ on macOS). This is
   what binds it, lists it under Preferences → Keyboard Shortcuts and lets users
   change it. A unit test rejects default keys two commands share.
3. Handle it in `PlayerView::register_actions` in `ui/player.rs`.
4. Optionally add it to a menu in `ui/menus.rs` with `act("Label", MyAction)`,
   which shows its current shortcut automatically.

**Add a setting**
1. Add a field to `Settings` in `settings.rs` and give it a default in
   `impl Default`. Fields are `#[serde(default)]`, so old settings files keep
   working.
2. If the engine needs it, map it in `Settings::engine_config()` and add it to
   `EngineConfig`.
3. Add a control in `ui/dialogs.rs` → `settings_dialog` using the `toggle_row`,
   `choice`, `stepper` or `input_box` helpers. These save and apply the change
   for you.

**Add a video or audio filter**
Extend `build_chain` in `engine/video.rs` or `fx_chain` in `engine/audio.rs` with
the libavfilter string, and add the setting to `VideoAdjust` / `AudioFx` in
`engine/types.rs`. The decoder threads compare a settings generation counter and
rebuild the graph automatically. Use `av::filter_available("name")` for filters
an FFmpeg build might lack.

**Add an upscaling shader**
Put the unmodified mpv `.glsl`/`.hook` file under `third_party/shaders/<project>/`
(the license must be GPL-3.0-compatible: MIT, BSD, LGPL, GPL-3.0). Record its source
and commit in `third_party/shaders/README.md`, and add its license text to
`third_party/shaders/LICENSES.txt`. Then add an `Upscaler` variant (label and
`ALL` in `engine/types.rs`) and map it to the file in `ShaderSet::source` in
`engine/upscale.rs`. Test it with `ffmpeg -vf libplacebo=w=3840:h=2160:custom_shader_path=FILE`
first: libplacebo rejects some mpv-only features.

**Add an icon**
Add a variant to `Icon` in `icons.rs` with its freedesktop name, and add the
Papirus `*-symbolic.svg` name to `ICONS` in `build.rs`. On Linux the user's
icon theme is tried first, and the bundled Papirus icon is the fallback.

**Support another file type**
Add the extension to `VIDEO_EXTS` / `AUDIO_EXTS` in `playlist.rs`, its MIME type
to `MIME_TYPES` in `integration.rs` *and* the static `.desktop` file (keep the two
lists identical), and to the Windows and macOS file associations if relevant.

## Debugging and testing

- `mavplayer -v` (or `MAVPLAYER_DEBUG=1`) prints engine diagnostics and FFmpeg
  warnings to stderr, including the time to the first video frame.
- **View ▸ Media Information** (<kbd>I</kbd>) shows the decoder in use, hardware
  decoding status, pixel format, dropped frames and buffer level.
- `MAVPLAYER_CONFIG_DIR=/tmp/mp` runs with a throwaway settings folder.
- `MAVPLAYER_TEST_AUDIO_DELAY_MS=5000` (debug builds only) delays opening the
  audio device, to test slow or hanging sound servers. Video shows its first
  frame immediately; playback starts when audio is ready, or without sound
  after 3 s.
- `MAVPLAYER_ICON_THEME=Adwaita` overrides the detected Linux icon theme.
- To try the UI without disturbing your desktop on KDE Plasma, run it in a
  nested, invisible KWin:
  ```bash
  kwin_wayland --virtual --width 1280 --height 800 --socket mavtest &
  WAYLAND_DISPLAY=mavtest cargo run -- video.mkv
  WAYLAND_DISPLAY=mavtest spectacle -b -n -f -o shot.png   # screenshot
  ```
- Generate test media of any format with the `ffmpeg` CLI, for example
  `ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=30 -f lavfi -i sine -t 20 -c:v libx264 -c:a aac test.mp4`.
