# clip-recorder

Ultra-lightweight game clip recorder for Linux. It keeps the last N seconds of your screen (and desktop audio) in RAM as already-encoded H.264, and writes them to an MP4 when you press **Alt+S**. Nothing touches the disk until you save, and saving never re-encodes.

- Screen capture through the xdg-desktop-portal ScreenCast API and PipeWire, so it works on Wayland (GNOME, KDE, Hyprland, Sway, ...) and on X11 with a portal backend.
- Hardware encoding through VA-API (Intel/AMD) or NVENC (NVIDIA), with frames kept on the GPU. Falls back to x264 with a warning if no hardware encoder works.
- Desktop audio (what you hear) through PipeWire, or PulseAudio's monitor.
- Alt+S registered through the GlobalShortcuts portal (KDE, GNOME 48+, Hyprland); on other compositors it reads `/dev/input` instead, which works in fullscreen games and still lets the game see the key.
- Tray icon (Save clip, Open library, Quit), a clips library window with Quality and Duration in a header popover, and a toast after each save, built with Slint. The windows are separate short-lived processes, so the recorder itself stays small. Thumbnails need an H.264 decoder (gst-libav or a VA-API decoder).

## Install

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
install -Dm644 assets/clip-recorder.desktop ~/.config/autostart/clip-recorder.desktop
```

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

The first run shows the portal's screen picker. The choice is remembered, so later runs start straight away (`--reselect` shows it again). Press **Alt+S** and the last 60 seconds land in `~/Videos/Clips/<Game>_<date>_<time>.mp4` with a toast in the corner. The game name comes from Steam (`SteamAppId`, Proton included), Lutris or Heroic; otherwise clips are named `clip_...`.

Left-click the tray icon to open the library; its menu has Save clip, Open library and Quit. Without a tray (GNOME without the AppIndicator extension), run `clip-recorder library`.

```
clip-recorder [run] [OPTIONS]   start recording (tray icon, Alt+S to save)
clip-recorder save              save a clip from the running recorder
clip-recorder pause|resume      pause or resume the buffer (pausing frees its RAM)
clip-recorder status            print the recorder's state
clip-recorder library           open the clips window
clip-recorder quit              stop the running recorder

-d, --duration <SECS>     seconds kept in the buffer          [default: 60]
-f, --fps <N>             frame rate cap                      [default: 60]
-b, --bitrate <KBPS>      video bitrate, overrides the quality preset
-e, --encoder <NAME>      auto, va, vaapi, nvenc, x264        [default: auto]
-a, --audio <SOURCE>      auto, pipewire, pulse, none         [default: auto]
-o, --output <DIR>        where clips go                      [default: ~/Videos/Clips]
-k, --hotkey <COMBO>      global save hotkey                  [default: alt+s]
    --no-hotkey           only save via `clip-recorder save` or SIGUSR1
    --headless            no tray or toasts
    --reselect            show the screen picker again
    --test-source         record a test pattern and tone instead of the screen
```

`pkill -USR1 clip-recorder` also saves a clip.

### Settings

The library's popover sets Quality (Light/High/Max) and Duration (30/60/120 s); encoder and folder are picked automatically. Both live in `~/.config/clip-recorder/config`, which the running recorder applies at once: buffer length on the next keyframe, encoder, quality, frame rate and audio by restarting the encoder (the buffer starts over). Command-line options override the file for that run. The file is plain `key = value`:

```
duration = 60
fps = 60
quality = high            # light 12 Mb/s, high 25 Mb/s, max 50 Mb/s
encoder = auto
audio = auto
output = ~/Videos/Clips
hotkey = alt+s            # read at startup
pause_without_game = false
toast = true
sound = false
```

RAM use is roughly `bitrate × duration`: 25 Mbit/s for 60 s is about 190 MB. "Pause when no game is running" stops the encoder and frees the buffer while no Steam, Lutris, Heroic, Wine or gamescope game is running.

### Fonts

The UI uses Bricolage Grotesque for headings, Geist for body text and Geist Mono for numbers, keys and paths. All three are OFL and bundled into the binary from `assets/fonts`, so nothing needs to be installed.

### Run as a systemd service

Instead of autostart, `~/.config/systemd/user/clip-recorder.service`:

```ini
[Unit]
Description=clip-recorder
PartOf=graphical-session.target
After=graphical-session.target

[Service]
ExecStart=%h/.local/bin/clip-recorder
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
pipewiresrc (default sink monitor) ─► AAC/Opus encoder ───────────────────────────────────► ring buffer (RAM)
Alt+S ─► snapshot ring ─► mp4mux ─► ~/Videos/Clips/*.mp4   (no re-encode, capture keeps running)
```

Tray, library and toast talk to the recorder over `$XDG_RUNTIME_DIR/clip-recorder.sock` (one command per connection: `save`, `status`, `pause`, `resume`, `reload`, `reselect`, `quit`, ...). Everything OS-specific (portal capture, audio sources, hotkey, tray, socket) lives in `src/platform/linux/`; the ring buffer, encoder selection, muxing and UI are portable, so a Windows port adds `src/platform/windows/` (WGC capture, WASAPI loopback, Media Foundation/NVENC/AMF/QSV encoders, `RegisterHotKey`, a named pipe) without touching the rest.

The encoder emits a keyframe every second, and the buffer drops whole GOPs from the front, so a saved clip always starts on a keyframe and is at most one second longer than `--duration`. Encoders are tried in the order va, vaapi, nvenc, x264; the first that produces a frame wins, and `--encoder` forces one.

## Troubleshooting

- `ScreenCast portal not available`: install the portal backend for your desktop and make sure `xdg-desktop-portal` is running in your session.
- `encoder va: ...` then falls back to x264: check `gst-inspect-1.0 vah264enc` (Intel/AMD) or `gst-inspect-1.0 nvh264enc` (NVIDIA). Run with `GST_DEBUG=3` for details.
- No audio: try `--audio pulse`, which records the default output through pipewire-pulse.
- Alt+S does nothing: `clip-recorder status | grep hotkey_backend` says which path is in use. `none` means neither the portal nor `/dev/input` was available; see [Hotkey](#hotkey).
- No tray icon on GNOME: install the AppIndicator extension, or use `clip-recorder library`.
