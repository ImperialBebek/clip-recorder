# clip-recorder

Ultra-lightweight game clip recorder for Linux. It keeps the last N seconds of your screen (and desktop audio) in RAM as already-encoded H.264, and writes them to an MP4 when you press **Alt+S**. Nothing touches the disk until you save, and saving never re-encodes.

- Screen capture through the xdg-desktop-portal ScreenCast API and PipeWire, so it works on Wayland (GNOME, KDE, Hyprland, Sway, ...) and on X11 with a portal backend.
- Hardware encoding through VA-API (Intel/AMD) or NVENC (NVIDIA), with frames kept on the GPU. Falls back to x264 if no hardware encoder works.
- Desktop audio (what you hear) through PipeWire, AAC or Opus.
- Global hotkey read straight from `/dev/input`, so it works in fullscreen games on any compositor and the game still receives the key.

## Install

### Dependencies

Arch / CachyOS / EndeavourOS:

```sh
sudo pacman -S --needed rust gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-plugin-pipewire xdg-desktop-portal
# Intel/AMD hardware encoding is in gst-plugins-bad (va). NVIDIA: gst-plugins-bad (nvcodec) + nvidia-utils.
# Optional software fallback: gst-plugins-ugly. Optional AAC audio: gst-libav.
```

Fedora:

```sh
sudo dnf install rust cargo gstreamer1-devel gstreamer1-plugins-base-devel gstreamer1-plugins-good gstreamer1-plugins-bad-free gstreamer1-plugins-bad-free-extras pipewire-gstreamer
# AAC + x264 come from RPM Fusion (gstreamer1-plugins-ugly, gstreamer1-libav).
```

Ubuntu / Debian:

```sh
sudo apt install cargo libgstreamer1.0-dev libgstreamer-plugins-base1.0-dev gstreamer1.0-plugins-good \
  gstreamer1.0-plugins-bad gstreamer1.0-pipewire gstreamer1.0-vaapi gstreamer1.0-plugins-ugly gstreamer1.0-libav
```

You also need a portal backend for your desktop (`xdg-desktop-portal-gnome`, `-kde`, `-hyprland`, `-wlr`, ...), which most desktops ship already.

### Build

```sh
cargo build --release
install -Dm755 target/release/clip-recorder ~/.local/bin/clip-recorder
```

### Hotkey permission

The built-in Alt+S hotkey reads keyboards from `/dev/input`, which needs the `input` group:

```sh
sudo usermod -aG input $USER   # then log out and back in
```

If you would rather not, run with `--no-hotkey` and bind `clip-recorder save` to Alt+S in your desktop's shortcut settings (Hyprland: `bind = ALT, S, exec, clip-recorder save`).

## Use

```sh
clip-recorder
```

The first run shows the portal's screen picker; the choice is remembered, so later runs start straight away (`--reselect` shows it again). Press **Alt+S** and the last 30 seconds land in `~/Videos/Clips/clip_<date>_<time>.mp4`, with a desktop notification.

```
clip-recorder [run] [OPTIONS]   start recording into the rolling buffer
clip-recorder save              save a clip from the running recorder

-d, --duration <SECS>     seconds kept in the buffer          [default: 30]
-f, --fps <N>             frame rate cap                      [default: 60]
-b, --bitrate <KBPS>      video bitrate in kbit/s             [default: 20000]
-e, --encoder <NAME>      auto, va, vaapi, nvenc, x264        [default: auto]
-a, --audio <SOURCE>      auto, pipewire, pulse, none         [default: auto]
-o, --output <DIR>        where clips go                      [default: ~/Videos/Clips]
-k, --hotkey <COMBO>      global save hotkey                  [default: alt+s]
    --no-hotkey           only save via `clip-recorder save` or SIGUSR1
    --no-notify           no desktop notification on save
    --reselect            show the screen picker again
    --test-source         record a test pattern instead of the screen
```

`pkill -USR1 clip-recorder` also saves a clip.

RAM use is roughly `bitrate × duration`: 20 Mbit/s for 30 s is about 75 MB.

### Run in the background

`~/.config/systemd/user/clip-recorder.service`:

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

The encoder emits a keyframe every second, and the buffer drops whole GOPs from the front, so a saved clip always starts on a keyframe and is at most one second longer than `--duration`. Encoders are tried in the order va, vaapi, nvenc, x264; the first that produces a frame wins, and `--encoder` forces one.

## Troubleshooting

- `ScreenCast portal not available`: install the portal backend for your desktop and make sure `xdg-desktop-portal` is running in your session.
- `encoder va: ...` then falls back to x264: check `gst-inspect-1.0 vah264enc` (Intel/AMD) or `gst-inspect-1.0 nvh264enc` (NVIDIA). Run with `GST_DEBUG=3` for details.
- No audio: try `--audio pulse`, which records the default output through pipewire-pulse.
