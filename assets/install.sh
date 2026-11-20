#!/bin/sh
# Installs Clip Recorder for the current user (no root needed).
#   ./install.sh            install or update
#   ./install.sh uninstall  remove it (clips and settings are kept)
set -eu

here=$(cd "$(dirname "$0")" && pwd)
bin="${XDG_BIN_HOME:-$HOME/.local/bin}"
apps="${XDG_DATA_HOME:-$HOME/.local/share}/applications"
autostart="${XDG_CONFIG_HOME:-$HOME/.config}/autostart"

if [ "${1:-}" = "uninstall" ]; then
    "$bin/clip-recorder" quit 2>/dev/null || true
    rm -f "$bin/clip-recorder" "$apps/clip-recorder.desktop" "$autostart/clip-recorder.desktop"
    echo "Removed. Clips in ~/Videos/Clips and ~/.config/clip-recorder are untouched."
    exit 0
fi

"$bin/clip-recorder" quit 2>/dev/null || true
install -Dm755 "$here/clip-recorder" "$bin/clip-recorder"
for dir in "$apps" "$autostart"; do
    mkdir -p "$dir"
    sed "s|^Exec=.*|Exec=$bin/clip-recorder|" "$here/clip-recorder.desktop" > "$dir/clip-recorder.desktop"
done

missing=""
if ! command -v gst-inspect-1.0 >/dev/null 2>&1; then
    missing="GStreamer (gstreamer, gst-plugins-base, gst-plugins-good, gst-plugins-bad, the PipeWire plugin)"
else
    for el in pipewiresrc mp4mux h264parse; do
        gst-inspect-1.0 "$el" >/dev/null 2>&1 || missing="$missing $el"
    done
fi
if [ -n "$missing" ]; then
    echo "warning: missing GStreamer pieces: $missing"
    echo "  Arch:   sudo pacman -S gstreamer gst-plugins-base gst-plugins-good gst-plugins-bad gst-plugin-pipewire gst-libav"
    echo "  Fedora: sudo dnf install gstreamer1-plugins-base gstreamer1-plugins-good gstreamer1-plugins-bad-free pipewire-gstreamer"
    echo "  Ubuntu: sudo apt install gstreamer1.0-plugins-base gstreamer1.0-plugins-good gstreamer1.0-plugins-bad gstreamer1.0-pipewire gstreamer1.0-libav"
fi

echo "Installed to $bin/clip-recorder. It starts with your session; start it now from the app menu or run: clip-recorder"
case ":$PATH:" in
    *":$bin:"*) ;;
    *) echo "note: $bin is not on your PATH" ;;
esac
