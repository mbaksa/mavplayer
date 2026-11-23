# MavPlayer

A fast, native media player for video, music, network streams and DVDs, for
Linux, Windows and macOS.

![MavPlayer playing a video with the playlist open](docs/screenshot.png)

MavPlayer has its own playback engine built directly on the FFmpeg libraries
(no VLC or mpv underneath) and a GPU-rendered interface built with
[GPUI](https://www.gpui.rs). It opens in a fraction of a second, plays nearly
anything, and keeps the settings you would expect from a desktop media player
one click away.

## Features

**Playback**
- Plays almost every audio and video format FFmpeg understands: MKV, MP4, MOV,
  AVI, WebM, MPEG-TS/M2TS, VOB, FLV, WMV, OGG, FLAC, MP3, AAC, Opus, WAV, AC-3,
  DTS and many more; H.264, HEVC, AV1, VP9, MPEG-2/4 and other codecs.
- Network streams: HTTP(S), HLS, RTSP, RTMP, SRT, FTP.
- **DVDs** from a drive, a `VIDEO_TS` folder or an ISO image, with their
  **menus**: buttons work with the arrow keys and Enter or with the mouse, and
  the disc picks titles, audio and subtitles and decides what plays next, as on
  a DVD player. Or skip the menus and play the main feature or any title
  directly, with chapter and angle selection. (Menus need libdvdnav, which the
  Windows build doesn't have; it plays titles directly.)
- **Audio CDs** from a drive or a `.cue`/`.bin` image, with track names from
  CD-Text or the cue sheet, and gapless playback from one track to the next.
- Hardware decoding (VA-API, NVDEC, D3D11VA/DXVA2, VideoToolbox), on by default,
  with automatic fallback to software.
- HDR10/HLG to SDR tone mapping, automatic deinterlacing, exact frame seeking,
  frame stepping, A-B loop, playback speed 0.25×–4× with pitch preserved.
- Resumes files where you left off.
- Media keys and the desktop's player controls (MPRIS on Linux, the media
  overlay on Windows, Now Playing on macOS).
- Opening files while MavPlayer is open can start a new window, or add to the
  open window's playlist (and play), or replace it: your choice in settings.
- Optionally keeps running in the system tray when you close the window
  (Preferences ▸ Playback, off by default), so the next file you open plays
  almost instantly. Click the tray icon to open the window; quit from its menu.
- Optionally plays in the background (Preferences ▸ Playback, off by default):
  closing the window while a file is open hides it in the tray and playback
  goes on. The tray icon's menu has Play/Pause, Previous and Next; clicking
  the icon shows or hides the window. Media ▸ Hide to Tray (Ctrl+W) does the
  same.

**Video**: aspect ratio override, crop, zoom, rotate and flip, brightness /
contrast / saturation / gamma / hue, sharpen, denoise, grayscale, invert.

**Upscaling** (Video ▸ Upscaling, or Settings ▸ Video): besides plain bilinear,
bicubic and nearest neighbour (for pixel art), GPU upscalers run through FFmpeg's
libplacebo filter (Vulkan) when FFmpeg is built with it, as in every Linux
and Windows package (not yet on macOS):
- **Anime4K** with its six presets (A, B, C, A+A, B+B, C+A) in HQ and Fast
  versions.
- **FSRCNNX** (16 and 8 filters), **ArtCNN** C4F16 (plain or denoise +
  sharpen), **RAVU-Lite**, **AMD FSR**, **NVIDIA Image Scaling** and EWA
  Lanczos.
- **KrigBilateral** chroma upscaling and **AMD CAS** sharpening on top of any
  of them.
- Your own mpv-style `.glsl`/`.hook` shader file (NNEDI3, SSimSuperRes, …).

**Audio**: native PipeWire output on Linux; output device and channel layout selection (stereo through 7.1),
10-band equalizer with presets, volume normalization, night mode (dynamic range
compression), stereo widening, channel swap, karaoke, audio delay, volume up to
150 %.

**Subtitles**: embedded text, ASS/SSA, PGS, VobSub and DVB subtitles; external
SRT, ASS, SSA, VTT, SUB and more, loaded automatically when they sit next to the
video; preferred languages (falling back to your system language), encoding
detection, delay, size, position and style. Showing subtitles picks the best
track by itself, and whether they are on is remembered for later videos and
launches: only you turn them on or off.

**Interface**
- Light and dark themes that follow the system; menus, context menu, keyboard
  shortcuts for everything (searchable and rebindable), drag and drop,
  mouse-wheel volume or seek.
- Playlists (M3U/PLS load and save), shuffle, repeat, recent files. The
  playlist works in fullscreen too, starting hidden there unless you choose
  otherwise.
- Gapless playback: songs follow each other and loops repeat with no gap, down
  to the sample.
- Uses your desktop's icon theme on Linux, falling back to built-in
  [Papirus](https://github.com/PapirusDevelopmentTeam/papirus-icon-theme) icons.
- Native window decorations everywhere they exist.

## Install

Download a package from the
[releases page](https://github.com/Drogobo/mavplayer/releases):

| Platform | Package |
| --- | --- |
| Any Linux distribution | `MavPlayer-<version>-x86_64.AppImage` or `MavPlayer.flatpak` |
| Debian, Ubuntu, Mint | `mavplayer_<version>_amd64.deb` |
| Fedora, openSUSE, RHEL | `mavplayer-<version>.x86_64.rpm` |
| Arch, Manjaro, CachyOS | `mavplayer-<version>-1-x86_64.pkg.tar.zst` |
| Windows 10/11 | `MavPlayer-<version>-windows-x64-setup.exe`, or the portable `.zip` |
| macOS 11+ | `MavPlayer-<version>-macos-arm64.dmg` (Apple Silicon) or `-x86_64.dmg` (Intel) |

```bash
sudo apt install ./mavplayer_*_amd64.deb             # Debian / Ubuntu
sudo dnf install ./mavplayer-*.x86_64.rpm            # Fedora
sudo pacman -U mavplayer-*.pkg.tar.zst               # Arch
flatpak install --user MavPlayer.flatpak             # Flatpak
```

### AppImage

Make it executable and run it:

```bash
chmod +x MavPlayer-*-x86_64.AppImage
./MavPlayer-*-x86_64.AppImage
```

On first launch MavPlayer offers to **integrate with your desktop**: it adds a
launcher to your application menu and registers itself under "Open With" for
media files, pointing at the AppImage where it is. You can do the same later from
**Media ▸ Integrate into Desktop…**, or from a terminal:

```bash
./MavPlayer-*-x86_64.AppImage --integrate            # add to the application menu
./MavPlayer-*-x86_64.AppImage --remove-integration   # remove it again
```

### Encrypted DVDs

Most commercial DVDs are encrypted (CSS). Install `libdvdcss` (for example
`libdvdcss2` on Debian/Ubuntu, `libdvdcss` on Arch and Fedora RPM Fusion) and
MavPlayer will use it automatically. Check that doing so is legal where you live.

## Usage

```
mavplayer [OPTIONS] [FILE | FOLDER | URL | dvd://[TITLE][/DEVICE] | cdda://[TRACK][/DEVICE]] ...

  -f, --fullscreen          Start in fullscreen
  -n, --new-window          Open a new window even if MavPlayer is already open
      --background          Start in the system tray without a window, so the
                            first file opened plays almost instantly
      --integrate           Add this AppImage to the desktop's application menu
      --remove-integration  Remove that menu entry again
  -v, --verbose             Print diagnostics to stderr
  -h, --help                Show help
  -V, --version             Show the version
```

Examples: `mavplayer movie.mkv`, `mavplayer ~/Music/Album/`,
`mavplayer https://example.com/live.m3u8`, `mavplayer dvd://` (the disc in the
default drive), `mavplayer MOVIE.iso` or `mavplayer /path/to/VIDEO_TS` (a DVD
image or folder), `mavplayer dvd://2/dev/sr1` (title 2 of the disc in `/dev/sr1`),
`mavplayer cdda://` (the audio CD in the default drive), `mavplayer cdda://3`
(from its third track), `mavplayer album.cue` (a CD image).

`dvd://` with no title starts at the disc's menu. **Media ▸ Open Disc…**
(<kbd>Ctrl</kbd>+<kbd>D</kbd>) offers both: **Play from Disc Menu** or **Play
Main Feature**.

### Keyboard shortcuts

On macOS, <kbd>Ctrl</kbd> below means <kbd>⌘</kbd>. **Preferences ▸
Keyboard Shortcuts** (<kbd>F1</kbd>) lists them all, where you can search them
and add, remove or change keys. These are the defaults.

| Keys | Action |
| --- | --- |
| <kbd>Space</kbd> / <kbd>K</kbd> | Play / pause |
| <kbd>←</kbd> <kbd>→</kbd> | Seek 5 s (Shift: 1 s exact, Ctrl: 60 s) |
| <kbd>↑</kbd> <kbd>↓</kbd> / <kbd>M</kbd> | Volume / mute |
| <kbd>F</kbd> or double-click | Fullscreen |
| <kbd>PgUp</kbd> <kbd>PgDn</kbd> | Previous / next chapter |
| <kbd>.</kbd> <kbd>,</kbd> | Next / previous frame |
| <kbd>[</kbd> <kbd>]</kbd> <kbd>Backspace</kbd> | Slower / faster / normal speed |
| <kbd>N</kbd> / <kbd>Shift</kbd>+<kbd>N</kbd> | Next / previous playlist item |
| <kbd>B</kbd> / <kbd>Shift</kbd>+<kbd>B</kbd> | Next / previous audio track |
| <kbd>V</kbd> | Show / hide subtitles (picks a track if none is chosen) |
| <kbd>C</kbd> / <kbd>Shift</kbd>+<kbd>C</kbd> | Next / previous subtitle language |
| <kbd>Z</kbd> <kbd>X</kbd> | Subtitle delay −/+ 100 ms |
| <kbd>S</kbd> / <kbd>I</kbd> / <kbd>A</kbd> | Screenshot / media info / A-B loop |
| <kbd>1</kbd>…<kbd>9</kbd>, <kbd>0</kbd> | Jump to 10 %…90 %, start |
| <kbd>Ctrl</kbd>+<kbd>O</kbd> / <kbd>U</kbd> / <kbd>D</kbd> | Open file / URL / disc |
| <kbd>Shift</kbd>+<kbd>M</kbd> / <kbd>Shift</kbd>+<kbd>T</kbd> | DVD menu / DVD title menu |
| Arrow keys, <kbd>Enter</kbd> | Move between and press DVD menu buttons (while a menu shows) |
| <kbd>Ctrl</kbd>+<kbd>L</kbd> / <kbd>E</kbd> / <kbd>,</kbd> | Playlist / effects / preferences |

### Where settings live

| Platform | Folder |
| --- | --- |
| Linux | `~/.config/mavplayer/` (Flatpak: `~/.var/app/io.github.drogobo.MavPlayer/config/mavplayer/`) |
| Windows | `%APPDATA%\mavplayer\` |
| macOS | `~/Library/Application Support/mavplayer/` |

`settings.json` holds your preferences and `resume.json` the resume positions.
Set `MAVPLAYER_CONFIG_DIR` to use a different folder.

## Building from source

See [HACKING.md](HACKING.md). In short, with Rust and FFmpeg 9 development files
installed:

```bash
git clone --recurse-submodules https://github.com/Drogobo/mavplayer.git
cd mavplayer
cargo run --release -- some-video.mkv
```

## License

MavPlayer is free software: you can redistribute it and/or modify it under the
terms of the [GNU General Public License](LICENSE) as published by the Free
Software Foundation, either version 3 of the License, or (at your option) any
later version.

Release builds also contain these components, under GPL-3.0-compatible licenses:

- **FFmpeg** (GPL, configured with `--enable-gpl --enable-version3`), built
  with libdvdread and libdvdnav (GPL), dav1d (BSD) and zimg (WTFPL). On Linux
  also with libplacebo (LGPL-2.1-or-later) and the glslang shader compiler
  (BSD-3-Clause, BSD-2-Clause, MIT, Apache-2.0, and GPL-3.0 with the Bison
  exception for its generated parser); the Flatpak's libplacebo
  uses the runtime's shaderc (Apache-2.0). The Windows packages ship BtbN's
  GPL FFmpeg build, which includes libplacebo too.
- **Papirus icons** (GPL-3.0), compiled into the binary as fallback icons.
- **Upscaling shaders** (MIT and LGPL-3.0-or-later): Anime4K, ArtCNN, FSRCNNX,
  RAVU, AMD FSR and CAS, NVIDIA Image Scaling and KrigBilateral, compiled into
  the binary. See [`third_party/shaders`](third_party/shaders/README.md) for
  sources and [`LICENSES.txt`](third_party/shaders/LICENSES.txt) for the
  license texts, which packages install as `SHADER-LICENSES.txt`.
- **GPUI** (Apache-2.0) and Rust crates under MIT/Apache-2.0.

`scripts/build-ffmpeg.sh` records the exact versions; their source code is
available from each project.
