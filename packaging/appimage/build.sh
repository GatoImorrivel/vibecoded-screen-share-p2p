#!/usr/bin/env bash
# Builds screenshare-x86_64.AppImage with linuxdeploy.
# Requires the same build dependencies as the CI Linux job, including an LGPL
# shared FFmpeg in $FFMPEG_DIR (the CI uses BtbN's linux64-lgpl-shared build),
# whose libavcodec/libavutil are bundled.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
work="$root/target/appimage"
id="io.github.screenshareapp.ScreenShare"

cd "$root"
if [ -n "${FFMPEG_DIR:-}" ]; then
    export LD_LIBRARY_PATH="$FFMPEG_DIR/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
    export RUSTFLAGS="${RUSTFLAGS:-} -L native=$FFMPEG_DIR/lib"
fi
cargo build --release --locked -p screenshareapp
bin="$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')/release/screenshareapp"

mkdir -p "$work"
cd "$work"
if [ ! -x linuxdeploy-x86_64.AppImage ]; then
    curl -fsSLO https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage
    chmod +x linuxdeploy-x86_64.AppImage
fi

rm -rf AppDir
# Graphics, audio and PipeWire libraries come from the host: they must match
# the running drivers and PipeWire daemon. The same goes for libva and the
# NVIDIA CUDA/NVENC libraries, which FFmpeg loads at runtime.
APPIMAGE_EXTRACT_AND_RUN=1 OUTPUT=screenshare-x86_64.AppImage ./linuxdeploy-x86_64.AppImage \
    --appdir AppDir \
    --executable "$bin" \
    --desktop-file "$root/packaging/linux/$id.desktop" \
    --icon-file "$root/packaging/linux/$id.svg" \
    --exclude-library 'libpipewire-0.3.so*' \
    --exclude-library 'libasound.so*' \
    --exclude-library 'libwayland-*' \
    --exclude-library 'libxkbcommon*' \
    --exclude-library 'libvulkan.so*' \
    --exclude-library 'libEGL*' \
    --exclude-library 'libGL*' \
    --exclude-library 'libgbm.so*' \
    --exclude-library 'libdrm.so*' \
    --exclude-library 'libva*' \
    --exclude-library 'libcuda.so*' \
    --exclude-library 'libnvidia-*' \
    --output appimage

echo "Built $work/screenshare-x86_64.AppImage"
