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

## Tests

```bash
cargo test --release
```

The end-to-end tests encode synthetic clips with known shake, pans and jolts, then check the
detected jitter, labels, crop and output video.
