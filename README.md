# minIS

Video stabilization with the **minimum crop**. Motion within a threshold (a percentage of the
frame) counts as jitter and is removed. The crop is sized to the largest jitter that actually
occurred, not to the threshold. Motion that stays past the threshold counts as intentional
(pans, tilts) and is kept. See [design.md](design.md) for how it works.

## Requirements

- Rust (stable)
- `ffmpeg` and `ffprobe` on `PATH`

## Usage

```bash
cargo build --release
target/release/minis input.mp4 -o output.mp4 --threshold 2
```

| Option | Default | Meaning |
|---|---|---|
| `-t, --threshold` | `2` | Jitter threshold, % of the frame (same % for x and y) |
| `--intent-frames` | `5` | Frames motion must stay past the threshold to count as intentional |
| `--recenter` | `0.02` | Per-frame rate the correction eases back to center while within the threshold |
| `--report` | | Write per-frame motion, paths, jitter and labels as JSON |

The output keeps the cropped size. Audio is copied as-is.

## Web app

```bash
cargo run --release --bin minis-web
```

Then open <http://localhost:8765>. Drop in a video, optionally adjust the settings, and
follow the upload, analysis and rendering progress. When it's done you can preview the
result and download it. Jobs run one at a time, and the page picks a job back up if you
reload it. Uploads and results are kept in the system temp folder (`/tmp/minis-web`), or
in `--data-dir`. Use `--addr` to listen on another address or port.

## Desktop app (Windows)

The same app in its own window, with ffmpeg bundled: a Tauri wrapper in [desktop/](desktop/)
that runs the web app's server inside the app and shows its page.

To build the installer from WSL, first install Rust on Windows from <https://rustup.rs>
(the Visual Studio C++ build tools are also needed). Then run:

```bash
desktop/build-windows.sh
```

The script copies the sources to `%USERPROFILE%\minis-build` and builds there. The first
time, it also installs the Tauri CLI on Windows and downloads a Windows ffmpeg build from
gyan.dev. It prints the path of the installer. Running the installer adds the app to the
Start menu and the desktop.

The icon is drawn by `desktop/icons/make_icons.py`.

## Tests

```bash
cargo test --release
```

The end-to-end tests encode synthetic clips with known shake, pans and jolts, then check the
detected jitter, labels, crop and output video.
