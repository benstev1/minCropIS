# minIS Design

## Goal

Stabilize a video using the **smallest crop possible**. The user sets a jitter threshold.
Motion within the threshold counts as shake: minIS removes it and crops by the largest shake
actually observed. Motion beyond the threshold counts as intentional camera movement: minIS
leaves it in.

## Workflow

```
input video ─▶ 1. decode ─▶ 2. estimate motion ─▶ 3. split jitter / intent ─▶ 4. compute crop ─▶ 5. render
                (ffmpeg)     (dx, dy per frame)     (per axis, threshold)       (max jitter x, y)    (shift + crop)
```

### 1. Input

- v1 interface is a CLI: `minis input.mp4 -o output.mp4 --threshold 2 [--intent-frames 5]`.
- Frames are decoded by piping raw grayscale frames out of an `ffmpeg` subprocess. Analysis
  only needs luma, and the render pass decodes the frames again in full color.
- `--threshold` is a **percentage of the frame** (default `2`), and the same percentage applies
  to both axes. Internally it becomes pixels per axis: `T_x = p·W`, `T_y = p·H`. For example,
  2% of 1920×1080 gives `T_x = 38.4 px` and `T_y = 21.6 px`.

### 2. Frame-by-frame motion estimation

- For each consecutive pair of frames (t-1, t), estimate the global translation `d_t = (dx_t, dy_t)`.
- Method: **phase correlation** on a Hann-windowed, downscaled luma frame (pure Rust via
  `rustfft`), with sub-pixel peak refinement. It is fast and reliable for global translation,
  and it has no feature-tracking dependencies.
- The confidence comes from the peak-to-sidelobe ratio. If confidence is low (scene cut, heavy
  blur), the frame gets `d_t = 0` and is flagged, so it can't poison the path.
- v1 handles translation only. Rotation and zoom are listed under Future.

### 3. Separate jitter from intentional movement (per axis, independently)

**Key point: jitter has to be measured as displacement, not as frame-to-frame delta.** Suppose
the threshold is 2 px and the camera moves by deltas of `+1, +1, +1, -1, -1, -1`. Every delta is
under the threshold, but the frame drifts 3 px away. A crop sized from the deltas alone would be
too small. So we work on the cumulative camera path:

- Raw path: `P_t = Σ d_i` (per axis).
- Intended path `I_t`: where the camera "means" to point.
- Jitter: `J_t = P_t − I_t`. This is the correction that gets applied to frame t.

The intended path follows a **deadband ("rubber band") rule** with threshold `T`. On top of
that, motion only counts as intentional once it has stayed past `T` for at least `N` frames
(`--intent-frames`, default 5). Analysis runs offline, so this check **looks ahead** in the
clip instead of waiting. A confirmed pan therefore starts on its very first frame, with no delay.

```
e = P_t − I_{t-1}
if |e| <= T:
    I_t = I_{t-1} + k_recenter · e          # jitter; drift slowly back to center
elif |P − I_{t-1}| > T for frames t..t+N-1:
    I_t = P_t − sign(e) · T                 # intentional: sustained ≥ N frames → intent is dragged along
else:
    I_t = P_t − sign(e) · T                 # short spike (bump): still jitter, correction clamped at ±T
```

- Within the threshold, the output holds still and the shake is cancelled.
- When motion stays past the threshold for N or more frames, it is intentional. The excess goes
  straight through to the output, so pans and tilts keep the camera's real speed. No smoothing
  lag is added.
- When motion goes past the threshold for fewer than N frames, it is a **bump** and is labelled
  jitter. It is corrected as far as the margin allows (±T). Only the part beyond T shows
  through, briefly. Its label stays jitter, so it doesn't start a pan or reset the intended path.
- `k_recenter` (small, e.g. 0.02/frame) eases the correction back toward 0 while the shot is
  still. Without it, a pan would leave the correction parked at ±T and the margin would be used up.
- By construction `|J_t| <= T` always, so no frame ever needs more than T px of margin.

Each frame is also labelled `jitter`, `bump` or `intentional` per axis, for reporting and debugging.

### 4. Compute the crop

- `M_x = max_t |J_x,t|` and `M_y = max_t |J_y,t|`, taken over the whole clip.
- This is a fixed crop for the whole video: `M_x` px off the left and right, `M_y` off the top and bottom.
  - Output size: `(W − 2·M_x) × (H − 2·M_y)`. The margin is rounded up to whole pixels so the
    output dimensions are even, as the encoder requires.
  - **The output keeps this cropped size.** There is no upscaling back to the original
    resolution, so no detail is lost to resampling.
- Since `M ≤ T`, the crop is never larger than the threshold. It is only as large as the worst
  shake that actually happened, which is the "min crop".
- Stabilization runs in two passes: analysis (steps 2–4) completes before any rendering. That is
  required because the crop depends on the whole clip.

### 5. Render

- Decode the frames again in full color. Shift each frame by `−J_t` with bilinear sampling for
  sub-pixel accuracy, then crop the centered `(W − 2M_x) × (H − 2M_y)` window.
- Pipe the result into `ffmpeg` for encoding and copy the audio stream unchanged.
- Print a report: threshold (% and px per axis), `M_x`, `M_y`, crop % per axis, output size,
  and counts of jitter, bump and intentional frames.
- Optional `--report path.json` writes per-frame `d`, `P`, `I`, `J` and labels, for plotting.

## Module layout (Rust)

| Module      | Responsibility                                        |
|-------------|-------------------------------------------------------|
| `main.rs`   | CLI (clap), orchestration, report                     |
| `video.rs`  | ffmpeg probe / decode / encode pipes                  |
| `motion.rs` | phase-correlation translation estimate + confidence   |
| `path.rs`   | cumulative path, deadband filter, jitter labelling    |
| `crop.rs`   | max jitter per axis → crop rectangle                  |
| `render.rs` | sub-pixel shift + crop                                |

## Testing

- **Synthetic clips:** shift a textured still image by a known path (shake within T, shake plus
  a pan above T, a sawtooth drift like the delta example). Assert the following:
  - the estimated `d_t` is within 0.1 px of the truth,
  - `M_x` and `M_y` equal the injected shake amplitude, not T,
  - pan segments get labelled intentional and come out at their true speed.
  - a spike past T shorter than N frames gets labelled bump, and the crop stays ≤ T.
- **Unit tests:** deadband filter edge cases (exactly at T, sign flips, recentering), runs of
  exactly N−1 and N frames, conversion from % to px, and crop rounding.
- **Manual:** real handheld clips, comparing the input and output side by side.

## Decisions

1. **Output resolution:** the output keeps the cropped size, with no upscaling.
2. **Intent detection:** motion counts as intentional only after staying past T for ≥ N
   frames (`--intent-frames`, default 5). Shorter spikes are bumps, clamped at ±T.
3. **Threshold:** a single percentage of the frame, applied to both axes (`T_x = p·W`, `T_y = p·H`).

## Future

- Rotation and zoom (similarity transform): this needs feature tracking plus RANSAC.
- Rolling-shutter correction.
- A GUI or web front end for the "upload video" step.
