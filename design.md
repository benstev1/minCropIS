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

- For every frame t, estimate its content position `P_t = (x, y)` relative to frame 0.
- Method: **phase correlation** on Hann-windowed luma frames, downscaled so the longest side
  is ≤ 640 px (pure Rust via `rustfft`). It is fast and reliable for global translation, and
  it has no feature-tracking dependencies.
  - **Regularized normalization:** the cross-power spectrum is normalized by `|R| + ε`
    instead of `|R|`. Frequencies that hold mostly noise then can't swamp the peak, which
    matters on smooth content.
  - **Gaussian low-pass** on the cross-power spectrum. This gives a Gaussian-shaped peak,
    which a log-parabola fit locates to sub-pixel accuracy (< 0.05 px on synthetic shifts).
- **Tile consensus:** each analysis frame is split into a grid of tiles (about 4×3 at 640 px),
  and each tile is matched on its own. The shift that most tiles agree on (within 1.5 px)
  wins, averaged over the agreeing tiles. At least 3 tiles must agree. Regions that move on
  their own, like water or a moving subject, disagree with the camera and get outvoted.
- **Keyframe tracking:** each frame is matched against a keyframe, not the previous frame.
  Chaining frame-to-frame estimates turns their ~0.1 px errors into a random-walk drift
  (~0.7 px over 60 frames in testing). Shake oscillates around one spot, so one keyframe
  stays valid for long stretches. The keyframe is replaced (matched via the last trusted
  frame) when the frame moves more than 15% of a tile away from it. Untrusted frames never
  become a reference. After 3 failed frames in a row (e.g. a scene cut), tracking restarts
  from the current frame.
- **Outlier repair** (`outliers.rs`), run on the whole path after tracking:
  - *Weak matches:* frames whose confidence is below 30% of the clip's median are rejected.
    This adapts to how much texture the footage has, which a fixed cutoff can't do.
  - *Impossible motion:* a frame more than 1% of the frame size away from the median of
    its ±2-frame neighbourhood is rejected. A camera can't jump there and back that fast.
    Steady pans and multi-frame jolts pass this check.
  - Rejected frames are filled in by interpolating between the trusted frames around them,
    not by assuming no motion.
- v1 handles translation only. Rotation and zoom are listed under Future.

### 3. Separate jitter from intentional movement (per axis, independently)

**Key point: jitter has to be measured as displacement, not as frame-to-frame delta.** Suppose
the threshold is 2 px and the camera moves by deltas of `+1, +1, +1, -1, -1, -1`. Every delta is
under the threshold, but the frame drifts 3 px away. A crop sized from the deltas alone would be
too small. So we work on the camera path from step 2:

- Raw path: `P_t` (per axis).
- Intended path `I_t`: where the camera "means" to point.
- Jitter: `J_t = P_t − I_t`. This is the correction that gets applied to frame t.

The intended path follows a **deadband rule** with threshold `T`. On top of that, motion only
counts as intentional once it has stayed past `T` for at least `N` frames (`--intent-frames`,
default 5). Analysis runs offline, so this check **looks ahead** in the clip instead of waiting.

```
d = P_t − P_{t-1};  e = P_t − I_{t-1}
if panning and d moves in the pan direction:     # pan continues
    J_t = J_{t-1} · (1 − r_pan);  I_t = P_t − J_t
elif |e| <= T:                                   # jitter
    I_t = I_{t-1} + k_recenter · e;  J_t = P_t − I_t
elif |P − I_{t-1}| > T on the same side for frames t..t+N-1:   # pan confirmed
    relabel the ramp-up (preceding jitter frames already moving this way) as pan
    J carries over from before the ramp, easing by (1 − r_pan) per frame;  I_t = P_t − J_t
else:                                            # bump
    I_t = I_{t-1};  J_t = clamp(e, −T, T)
```

- **Jitter:** within the threshold, the output holds still and the shake is cancelled.
- **Intentional (pan):** motion that stays past the threshold for ≥ N frames passes through at
  the camera's real speed, with no smoothing lag. A pan lasts as long as the camera keeps
  moving in the same direction.
  - *Ramp-up:* a pan is only confirmed once it is past T. The frames leading into it, back
    to where the camera started moving that way, would otherwise read as jitter, and the
    correction would build up to T. That would cost the full T of crop for any pan slower
    than T px/frame. Since analysis is offline, those frames are relabelled as part of the pan.
  - *No jump:* when a pan starts, the existing correction carries over unchanged. It then
    eases to 0 (`r_pan = 0.1`/frame) while the camera moves, where the change is invisible.
    So a clean pan costs **no crop**. (An earlier draft dragged the intended path at exactly T
    behind the camera. That pinned the correction at ±T, which made any clip with a pan take
    the maximum crop.)
- **Bump:** motion past the threshold for fewer than N frames. It is corrected as far as the
  margin allows (±T), and only the part beyond T shows through, briefly. A bump leaves the
  intended path where it was, so it doesn't start a pan or reset the intended path.
- `k_recenter` (`--recenter`, default 0.02/frame) eases the correction back toward 0 while
  within the threshold. If the camera settles at a new spot, that spot gradually becomes the
  intended position.
- By construction `|J_t| <= T` always, so no frame ever needs more than T px of margin.
- After a pan, the camera comes to rest wherever the shake left it. The margin that follows can
  reach the shake's peak-to-peak amplitude, which is still well below T.

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
| `motion.rs` | phase correlation, tile consensus, keyframe tracker   |
| `outliers.rs` | reject weak / impossible measurements, interpolate  |
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
