# clip-recorder

Ultra-lightweight game clip recorder for Linux. It keeps the last N seconds of your screen and audio in RAM as already-encoded H.264, and writes them to an MP4 when you press **Alt+S**. Nothing touches the disk until you save, and saving never re-encodes.

- Screen capture through the xdg-desktop-portal ScreenCast API and PipeWire, so it works on Wayland (GNOME, KDE, Hyprland, Sway, ...) and on X11 with a portal backend.
- Hardware encoding through VA-API (Intel/AMD) or NVENC (NVIDIA), with frames kept on the GPU. Falls back to x264 with a warning if no hardware encoder works.
- Per-app audio tracks: PipeWire's `pw-dump` reports which applications play audio, and each category is captured to its own MP4 track — **Game** (only the game, never the desktop mix), **Voice** (Discord, TeamSpeak, Mumble and friends) and **Other** (browser, music, everything else). Defaults: Game, Voice and your microphone on, Other off. Toggle each in the library's popover and mix them independently in the built-in player. To record the whole desktop mix as a single track instead, use `--audio pipewire` (or `--audio pulse`).
- Alt+S registered through the GlobalShortcuts portal (KDE, GNOME 48+, Hyprland); on other compositors it reads `/dev/input` instead, which works in fullscreen games and still lets the game see the key.
- Tray icon (Save clip, Open library, Quit), a clips library window with Quality and Duration in a header popover built with Slint, and a toast after each save. The toast is a layer-shell overlay (KDE, Hyprland, Sway, ...): it sits above fullscreen games, never takes focus and ignores clicks; on compositors without layer-shell (GNOME) it becomes a desktop notification. The windows are separate short-lived processes, so the recorder itself stays small. Thumbnails need an H.264 decoder (gst-libav or a VA-API decoder).

## Install

### Prebuilt binary

Download `clip-recorder-x86_64-linux.tar.gz` from the [latest release](https://github.com/ImperialBebek/clip-recorder/releases/latest), then:

```sh
tar -xzf clip-recorder-x86_64-linux.tar.gz
./clip-recorder/install.sh        # ~/.local/bin, app menu entry, autostart
clip-recorder
```

The binary links the system GStreamer, so the runtime plugins below are still needed (not the `-dev` packages); `install.sh` warns when one is missing. `./install.sh uninstall` removes it again and keeps your clips and settings.

Or run the **AppImage** instead — same binary, single file:

```sh
chmod +x clip-recorder-x86_64.AppImage
./clip-recorder-x86_64.AppImage
```

It still needs the same system GStreamer plugins as the tarball. It updates itself in place (`clip-recorder update` replaces the file), but does not add an app-menu or autostart entry — use `install.sh` above if you want it to start with your session.

### Updates

The recorder checks for a new release once a day. When there is one, the tray menu shows **Update to vX.Y.Z**; or run `clip-recorder update` (`--check` only reports). It downloads the release, verifies its checksum, replaces the binary and restarts the recorder. While the repository is private, the check needs a GitHub token: `GITHUB_TOKEN`, `GH_TOKEN`, or a logged-in `gh`. Turn it off with `check_updates = false`.

To publish a release, bump `version` in `Cargo.toml`, merge, and push a matching tag (`git tag v0.2.0 && git push origin v0.2.0`); the Release workflow builds and uploads the tarball and AppImage.

### Dependencies

Arch / CachyOS / EndeavourOS:

```sh
sudo pacman -S --needed rust gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-plugin-pipewire xdg-desktop-portal fontconfig libxkbcommon
# Intel/AMD hardware encoding is in gst-plugins-bad (va). NVIDIA: gst-plugins-bad (nvcodec) + nvidia-utils.
# Optional software fallback: gst-plugins-ugly. Optional AAC audio: gst-libav.
```

Fedora:

```sh
sudo dnf install rust cargo gstreamer1-devel gstreamer1-plugins-base-devel gstreamer1-plugins-good gstreamer1-plugins-bad-free gstreamer1-plugins-bad-free-extras pipewire-gstreamer fontconfig-devel libxkbcommon-devel
# AAC + x264 come from RPM Fusion (gstreamer1-plugins-ugly, gstreamer1-libav).
```

Ubuntu / Debian:

```sh
sudo apt install cargo libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-pipewire gstreamer1.0-vaapi gstreamer1.0-plugins-ugly gstreamer1.0-libav \
  libfontconfig1-dev libxkbcommon-dev
```

You also need a portal backend for your desktop (`xdg-desktop-portal-gnome`, `-kde`, `-hyprland`, `-wlr`, ...), which most desktops ship already.

### Build

```sh
cargo build --release
install -Dm755 target/release/clip-recorder ~/.local/bin/clip-recorder
install -Dm644 assets/clip-recorder.desktop ~/.local/share/applications/clip-recorder.desktop
# start it with your session:
sed 's|^Exec=.*|Exec=clip-recorder --background|' assets/clip-recorder.desktop > ~/.config/autostart/clip-recorder.desktop
```

### First start

The first time it starts, the library opens with a short welcome card: how it spots your game, what Alt+S does and that it keeps running from the tray. After you dismiss it, starting the app by hand (app menu) opens the library, or just raises it if the recorder is already running. Autostart at login (`--background`) goes straight to the tray without a window.

### Hotkey

On KDE, GNOME 48+ and Hyprland, Alt+S is registered through the GlobalShortcuts portal and shows up in the desktop's own shortcut settings; nothing else to do.

Elsewhere (Sway, older GNOME, ...) the recorder reads keyboards from `/dev/input`, which needs the `input` group:

```sh
sudo usermod -aG input $USER   # then log out and back in
```

Or skip both: run with `--no-hotkey` and bind `clip-recorder save` to a key in your desktop (Hyprland: `bind = ALT, S, exec, clip-recorder save`; Sway: `bindsym Mod1+s exec clip-recorder save`).

## Use

```sh
clip-recorder
```

On KDE Plasma there is nothing to pick at all: the recorder asks KWin for the game's own window (found by its Steam, Lutris, Heroic or Wine marker), records that window while the game runs, and goes idle when it closes, so every clip is just the game, with a fresh buffer per game. Set `capture = screen` (or pass `--reselect`) to record a whole screen instead. Elsewhere (GNOME, Hyprland, Sway, ...) the portal is used, as follows. The first run shows the portal's screen picker, after a toast that says what it is for. Pick the monitor your games run on (windows aren't offered, since they'd vanish when a game restarts). The choice is remembered, so every later start, including autostart at login, records straight away. If a game is already running on a first run, the picker waits until it has closed instead of appearing on top of it. If you cancel, or the desktop forgets the choice, the tray icon turns into a warning and its menu offers **Choose screen…**; the library's state pill and its settings popover (**Change screen…**) do the same, as does `clip-recorder reselect` (`--reselect` at startup). Press **Alt+S** and the last 60 seconds land in `~/Videos/Clips/<Game>_<date>_<time>.mp4` with a toast in the corner. When a game starts and the buffer is really recording, a toast names it and reminds you to press Alt+S; if it isn't recording (no screen chosen, encoder failed), the toast says so instead (turn it off with `toast = false`); there is nothing to pick first. The game name comes from Steam (`SteamAppId`, Proton included), Lutris or Heroic; otherwise clips are named `clip_...`.

Double-click the tray icon to open the library, or to focus it if it's already open; its menu has Save clip, Open library and Quit (quitting the recorder closes the library window too). Without a tray (GNOME without the AppIndicator extension), run `clip-recorder library`.

To delete clips, click the check button in the library header, or just hold **Ctrl** and click a clip. Then click clips to pick them, **Shift**+click for a range, or drag a box over them. **Ctrl+A** selects all, **Delete** asks to delete the selection (**Delete** or **Enter** again confirms), **Esc** backs out. Deleting removes the files for good.

Clicking a clip opens the built-in player: video on top, and below it one timeline lane per track — the video filmstrip, then a waveform per audio track (game audio, mic, and any future tracks), each with its own mute and volume (0–200%, so a quiet mic can be boosted). Click or drag on a lane to seek. The transport row has previous/next clip, a volume slider, open-in-external-player, open-folder, delete (asks first) and fullscreen. Back returns to the library.

| Key | Action |
| --- | --- |
| Space, K | play/pause |
| Left, Right | −5 s / +5 s |
| Up, Down | volume ±10% |
| M | mute |
| F | fullscreen |
| PageUp, PageDown | previous / next clip |
| Delete, Enter | delete the clip (asks first) |
| Esc | leave fullscreen, cancel delete, or back to the library |

```
clip-recorder [run] [OPTIONS]   start recording (tray icon, Alt+S to save)
clip-recorder save              save a clip from the running recorder
clip-recorder pause|resume      pause or resume the buffer (pausing frees its RAM)
clip-recorder reselect          choose the screen to record again
clip-recorder status            print the recorder's state
clip-recorder library           open the clips window
clip-recorder quit              stop the running recorder

-d, --duration <SECS>     seconds kept in the buffer          [default: 60]
-f, --fps <N>             frame rate cap                      [default: 60]
-b, --bitrate <KBPS>      video bitrate, overrides the quality preset
-e, --encoder <NAME>      auto, va, vaapi, nvenc, x264        [default: auto]
-a, --audio <MODE>        auto (per-app tracks), pipewire|pulse (whole desktop, one track), none
                          [default: auto]
-m, --mic <SOURCE>        microphone, its own audio track     [default: auto]
    --mic-device <NAME>   microphone to record, from the library's list [default: system default]
-o, --output <DIR>        where clips go                      [default: ~/Videos/Clips]
-k, --hotkey <COMBO>      global save hotkey                  [default: alt+s]
    --no-hotkey           only save via `clip-recorder save` or SIGUSR1
    --headless            no tray or toasts
    --background          stay in the tray; without it the library opens (autostart uses it)
    --reselect            show the screen picker again
    --test-source         record a test pattern and tone instead of the screen
```

`pkill -USR1 clip-recorder` also saves a clip.

### Settings

The library's popover sets Quality (Light/High/Max), Duration (30/60/120 s), the App audio tracks (Game / Voice chat / Other apps) and the Microphone (Off / System default / a specific device); encoder and folder are picked automatically. Both live in `~/.config/clip-recorder/config`, which the running recorder applies at once: buffer length on the next keyframe, encoder, quality, frame rate and audio by restarting the encoder (the buffer starts over). Command-line options override the file for that run. The file is plain `key = value`:

```
duration = 60
fps = 60
quality = high            # light 12 Mb/s, high 25 Mb/s, max 50 Mb/s
encoder = auto
audio = auto            # auto: per-app tracks; pipewire/pulse: whole desktop, one track; none
track_game = true       # Game track: only the game's audio
track_voice = true      # Voice track: Discord, TeamSpeak, Mumble, Zoom, …
track_other = false     # Other track: browser, music, everything else
mic = auto              # your microphone, saved as its own audio track
mic_device =            # empty: system default; else a source name from the library's Microphone list or `pactl list short sources`
capture = auto            # auto: the game's window on KDE, else a portal screen (read at startup); screen: always a screen
output = ~/Videos/Clips
hotkey = alt+s            # read at startup
pause_without_game = false
toast = true
sound = true              # chime when a clip is saved
check_updates = true      # daily release check
```

RAM use is roughly `bitrate × duration`: 25 Mbit/s for 60 s is about 190 MB. "Pause when no game is running" (portal capture only; the KDE window capture always idles between games) stops the encoder and frees the buffer while no Steam, Lutris, Heroic, Wine or gamescope game is running.

### Fonts

The UI uses Geist for headings and body text and Geist Mono for numbers, keys and paths. Both are OFL and bundled into the binary from `assets/fonts`, so nothing needs to be installed.

### Run as a systemd service

Instead of autostart, `~/.config/systemd/user/clip-recorder.service`:

```ini
[Unit]
Description=clip-recorder
PartOf=graphical-session.target
After=graphical-session.target

[Service]
ExecStart=%h/.local/bin/clip-recorder --background
Restart=on-failure

[Install]
WantedBy=graphical-session.target
```

```sh
systemctl --user enable --now clip-recorder
```

## How it works

```
portal ScreenCast ─► pipewiresrc ─► videorate (fps cap) ─► GPU convert ─► H.264 encoder ─► ring buffer (RAM)
pw-dump (which apps play audio) ─► pipewiresrc per app ─► audiomixer per category ─► AAC/Opus encoder ─► ring buffer (RAM)
pipewiresrc (microphone) ─► AAC/Opus encoder ─────────────────────────────────────────────► ring buffer (RAM)
Alt+S ─► snapshot ring ─► mp4mux ─► ~/Videos/Clips/*.mp4   (no re-encode, capture keeps running)
```

Tray and library talk to the recorder over `$XDG_RUNTIME_DIR/clip-recorder.sock` (one command per connection: `save`, `status`, `pause`, `resume`, `reload`, `reselect`, `quit`, ...). Everything OS-specific (portal capture, audio sources, hotkey, tray, socket) lives in `src/platform/linux/`; the ring buffer, encoder selection, muxing and UI are portable, so a Windows port adds `src/platform/windows/` (WGC capture, WASAPI loopback, Media Foundation/NVENC/AMF/QSV encoders, `RegisterHotKey`, a named pipe) without touching the rest.

The encoder emits a keyframe every second, and the buffer drops whole GOPs from the front, so a saved clip always starts on a keyframe and is at most one second longer than `--duration`. Encoders are tried in the order va, vaapi, nvenc, x264; the first that produces a frame wins, and `--encoder` forces one.

## Troubleshooting

- `ScreenCast portal not available`: install the portal backend for your desktop and make sure `xdg-desktop-portal` is running in your session.
- `encoder va: ...` then falls back to x264: check `gst-inspect-1.0 vah264enc` (Intel/AMD) or `gst-inspect-1.0 nvh264enc` (NVIDIA). Run with `GST_DEBUG=3` for details.
- No audio: per-app capture needs PipeWire's `pw-dump` tool; without it the recorder falls back to the desktop mix. `--audio pulse` records the default output through pipewire-pulse as one track.
- No microphone track: `clip-recorder status | grep mic` shows what was picked up; try `--mic pulse`, or `--mic none` to silence the "microphone unavailable" warning.
- Alt+S does nothing: `clip-recorder status | grep hotkey_backend` says which path is in use. `none` means neither the portal nor `/dev/input` was available; see [Hotkey](#hotkey).
- No tray icon on GNOME: install the AppIndicator extension, or use `clip-recorder library`.
