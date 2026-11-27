#!/usr/bin/env bash
# Build native tools from pinned upstream sources, not from PATH on customers' machines.
set -euo pipefail
root=$(cd "$(dirname "$0")/.." && pwd)
out="$root/src-tauri/resources/chat-video"; work=$(mktemp -d); trap 'rm -rf "$work"' EXIT
src() { git init -q "$1"; git -C "$1" fetch -q --depth 1 "$2" "$3"; git -C "$1" checkout -q FETCH_HEAD; test "$(git -C "$1" rev-parse HEAD)" = "$3"; }
cd "$work"; mkdir -p "$out" "$work/prefix"
src zimg https://github.com/sekrit-twc/zimg.git e5b0de6bebbcbc66732ed5afaafef6b2c7dfef87
(cd zimg; git submodule update --init --depth 1; ./autogen.sh; ./configure --prefix="$work/prefix" --disable-shared --enable-static; make -j4 install)
src ffmpeg https://github.com/FFmpeg/FFmpeg.git 894da5ca7d742e4429ffb2af534fcda0103ef593
cd ffmpeg; args=(); suffix=''; cxxlib=-lc++
case "$(uname -s)" in Darwin) args+=(--enable-videotoolbox --enable-encoder=h264_videotoolbox);; MINGW*|MSYS*) args+=(--enable-mediafoundation --enable-encoder=h264_mf --extra-ldflags=-static);suffix=.exe;cxxlib=-lstdc++;; *) exit 2;; esac
PKG_CONFIG_PATH="$work/prefix/lib/pkgconfig" ./configure --disable-autodetect --disable-everything --disable-debug --disable-doc --disable-ffplay --disable-network --disable-x86asm --enable-small --enable-ffmpeg --enable-ffprobe --enable-libzimg --pkg-config-flags=--static --extra-libs="$cxxlib" --enable-protocol=file --enable-demuxer=mov,matroska,avi,flv,mpegts,asf --enable-muxer=mp4,mov --enable-decoder=h264,hevc,vp8,vp9,av1,mpeg4,aac,mp3,opus,alac,pcm_s16le,pcm_s24le,pcm_f32le,rawvideo --enable-parser=h264,hevc,vp8,vp9,av1,aac,mpeg4video,mpegaudio,opus --enable-encoder=aac --enable-filter=scale,fps,format,zscale,tonemap,aresample,aformat,anull,null,transpose,hflip,vflip --enable-swscale --enable-swresample "${args[@]}"
make -j4; cp "ffmpeg$suffix" "ffprobe$suffix" LICENSE.md COPYING.LGPLv2.1 "$out/"; cp ../zimg/COPYING "$out/ZIMG-LICENSE"; "$out/ffmpeg$suffix" -version
