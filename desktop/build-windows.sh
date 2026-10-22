#!/usr/bin/env bash
# Builds the Windows desktop app installer from WSL, using the Rust toolchain
# installed on the Windows side (https://rustup.rs) and the Visual Studio C++
# build tools. Run from anywhere in the repo:
#
#   desktop/build-windows.sh
#
# Sources are copied to %USERPROFILE%\minis-build and built there: building
# across the WSL filesystem boundary is slow and unreliable.
set -euo pipefail

repo=$(git rev-parse --show-toplevel)
win_profile=$(cmd.exe /c 'echo %USERPROFILE%' 2>/dev/null | tr -d '\r')
cargo_bin=$(wslpath "$win_profile")/.cargo/bin
build=$(wslpath "$win_profile")/minis-build

# 1. Windows Rust toolchain.
if [[ ! -e "$cargo_bin/cargo.exe" ]]; then
    echo "Rust isn't installed on Windows. Install it from https://rustup.rs (run rustup-init.exe" >&2
    echo "in Windows, accept the defaults), then run this script again." >&2
    exit 1
fi
# `win DIR CMD...` runs CMD in Windows cmd, in DIR (a WSL path on the Windows
# drive), with the Windows cargo on PATH: the PATH WSL inherited may predate the
# Rust install.
mkdir -p "$build"
printf '@echo off\r\nset "PATH=%%USERPROFILE%%\\.cargo\\bin;%%PATH%%"\r\n%%*\r\n' >"$build/run.cmd"
win() {
    local dir=$1
    shift
    (cd "$dir" && cmd.exe /c "$(wslpath -w "$build/run.cmd")" "$@")
}

# 2. Tauri's build tool (one-time, takes a few minutes to compile).
if [[ ! -e "$cargo_bin/cargo-tauri.exe" ]]; then
    echo "==> Installing the Tauri CLI on Windows (one-time)"
    win "$build" cargo install tauri-cli@2 --locked
fi

# 3. Windows ffmpeg to bundle with the app.
ffmpeg_dir="$repo/desktop/ffmpeg"
if [[ ! -e "$ffmpeg_dir/ffmpeg.exe" || ! -e "$ffmpeg_dir/ffprobe.exe" ]]; then
    echo "==> Downloading a Windows ffmpeg build (gyan.dev release essentials)"
    tmp=$(mktemp -d)
    curl -fL --progress-bar -o "$tmp/ffmpeg.zip" \
        https://www.gyan.dev/ffmpeg/builds/ffmpeg-release-essentials.zip
    mkdir -p "$ffmpeg_dir" "$tmp/unpacked"
    python3 -c 'import sys, zipfile; zipfile.ZipFile(sys.argv[1]).extractall(sys.argv[2])' \
        "$tmp/ffmpeg.zip" "$tmp/unpacked"
    for tool in ffmpeg.exe ffprobe.exe; do
        find "$tmp/unpacked" -path "*/bin/$tool" -exec cp {} "$ffmpeg_dir/" \;
    done
    find "$tmp/unpacked" -maxdepth 2 -name LICENSE -exec cp {} "$ffmpeg_dir/LICENSE.txt" \;
    rm -rf "$tmp"
fi

# 4. Copy the sources (tracked and new, not ignored) plus the bundled ffmpeg.
echo "==> Copying sources to $build"
(cd "$repo" && git ls-files -z --cached --others --exclude-standard) |
    rsync -a --from0 --files-from=- "$repo/" "$build/"
rsync -a "$ffmpeg_dir/" "$build/desktop/ffmpeg/"

# 5. Build the installer.
echo "==> Building (the first build takes several minutes)"
win "$build/desktop" cargo tauri build

installer=$(ls -t "$build"/target/release/bundle/nsis/*-setup.exe | head -1)
echo
echo "Installer: $(wslpath -w "$installer")"
