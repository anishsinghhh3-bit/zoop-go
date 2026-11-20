#!/usr/bin/env bash
set -euo pipefail

root=$(pwd -W)
tools=$(pwd)
export PATH="$tools/w64devkit/bin:$tools/nasm/nasm-3.02:$PATH"

pushd x264-31e19f92f00c7003fa115047ce50978bc98c3a0d
./configure --host=x86_64-w64-mingw32 --prefix="$root/x264-build" \
    --enable-static --disable-cli --disable-opencl --bit-depth=8 --chroma-format=420
make -j"${ZOOP_FFMPEG_JOBS:-2}"
make install
popd

mkdir ffmpeg-fast
pushd ffmpeg-fast
export PKG_CONFIG_PATH="$root/x264-build/lib/pkgconfig"
sh "$root/ffmpeg-9.0.2/configure" --target-os=mingw32 --arch=x86_64 --enable-gpl --enable-libx264 --pkg-config-flags=--static \
    --disable-autodetect --disable-everything --disable-network --disable-doc \
    --disable-avdevice --disable-ffplay --disable-ffprobe --disable-debug \
    --enable-protocol=file,pipe --enable-demuxer=mov,rawvideo \
    --enable-muxer=mp4,gif,rawvideo --enable-decoder=h264,rawvideo \
    --enable-encoder=libx264,gif,rawvideo \
    --enable-filter=zoompan,fps,scale,setsar,pad,overlay,format,palettegen,paletteuse
make -j"${ZOOP_FFMPEG_JOBS:-2}" V=1 ffmpeg.exe
popd
