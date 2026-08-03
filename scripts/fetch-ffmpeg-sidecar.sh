#!/usr/bin/env bash
set -euo pipefail

readonly FFMPEG_VERSION="n7.1.5-12-g1fdbca85aa-20260731"
readonly FFMPEG_URL="https://github.com/BtbN/FFmpeg-Builds/releases/download/autobuild-2026-07-31-14-10/ffmpeg-n7.1.5-12-g1fdbca85aa-linux64-lgpl-7.1.tar.xz"
readonly FFMPEG_SHA256="58057a52db17bd2fefa87f271956f04aa2277d55efc13f288594cc2c65c59479"

script_dir="$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
agent_dir="$(dirname -- "$script_dir")"
binaries_dir="$agent_dir/src-tauri/binaries"
resources_dir="$agent_dir/src-tauri/resources"
stage_dir="$(mktemp -d "${RUNNER_TEMP:-/tmp}/aftercalls-ffmpeg.XXXXXX")"
archive="$stage_dir/ffmpeg.tar.xz"
unpack_dir="$stage_dir/unpacked"
ffmpeg_destination="$binaries_dir/ffmpeg-aftercalls-x86_64-unknown-linux-gnu"
ffprobe_destination="$binaries_dir/ffprobe-aftercalls-x86_64-unknown-linux-gnu"

if [[ -f "$agent_dir/THIRD_PARTY_NOTICES.md" ]]; then
  notices_source="$agent_dir/THIRD_PARTY_NOTICES.md"
elif [[ -f "$agent_dir/../THIRD_PARTY_NOTICES.md" ]]; then
  notices_source="$agent_dir/../THIRD_PARTY_NOTICES.md"
else
  echo "THIRD_PARTY_NOTICES.md is missing from the checkout" >&2
  exit 1
fi

trap 'rm -rf -- "$stage_dir"' EXIT

mkdir -p "$binaries_dir" "$resources_dir" "$unpack_dir"
curl --fail --show-error --location --retry 3 \
  --output "$archive" \
  "$FFMPEG_URL"
printf '%s  %s\n' "$FFMPEG_SHA256" "$archive" \
  | sha256sum --check --strict -
tar --extract --xz --file "$archive" \
  --directory "$unpack_dir" \
  --strip-components=1
install -m 0755 \
  "$unpack_dir/bin/ffmpeg" \
  "$ffmpeg_destination"
install -m 0755 \
  "$unpack_dir/bin/ffprobe" \
  "$ffprobe_destination"
install -m 0644 \
  "$unpack_dir/LICENSE.txt" \
  "$resources_dir/FFmpeg-LICENSE.txt"
install -m 0644 \
  "$notices_source" \
  "$resources_dir/THIRD_PARTY_NOTICES.md"

version_output="$("$ffmpeg_destination" -hide_banner -version 2>&1)"
printf '%s\n' "$version_output"
if [[ "$version_output" != *"ffmpeg version $FFMPEG_VERSION"* ]]; then
  echo "unexpected ffmpeg build identity" >&2
  exit 1
fi
if grep -Eq -- '--enable-(gpl|nonfree)' <<<"$version_output"; then
  echo "refusing a GPL or non-redistributable ffmpeg build" >&2
  exit 1
fi
if [[ "$version_output" != *"--enable-libopus"* ]]; then
  echo "ffmpeg build is missing the required libopus encoder" >&2
  exit 1
fi
probe_version_output="$("$ffprobe_destination" -hide_banner -version 2>&1)"
printf '%s\n' "$probe_version_output"
if [[ "$probe_version_output" != *"ffprobe version $FFMPEG_VERSION"* ]]; then
  echo "unexpected ffprobe build identity" >&2
  exit 1
fi
if ! grep -Fq 'GNU LESSER GENERAL PUBLIC LICENSE' \
  "$resources_dir/FFmpeg-LICENSE.txt"; then
  echo "ffmpeg archive did not contain the expected LGPL license" >&2
  exit 1
fi

sha256sum \
  "$ffmpeg_destination" \
  "$ffprobe_destination" \
  "$resources_dir/FFmpeg-LICENSE.txt" \
  "$resources_dir/THIRD_PARTY_NOTICES.md"
