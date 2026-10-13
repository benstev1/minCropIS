//! Splits a camera path into jitter (to be corrected) and intentional movement
//! (to be kept), independently per axis. See design.md §3.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Label {
    /// Within the threshold: fully corrected.
    Jitter,
    /// Past the threshold for fewer than `intent_frames`: corrected up to ±T.
    Bump,
    /// Past the threshold for at least `intent_frames`: passed through.
    Intentional,
}

#[derive(Debug, Clone, Copy)]
pub struct Params {
    /// Threshold in pixels for this axis.
    pub threshold: f64,
    /// Frames motion must stay past the threshold to count as intentional.
    pub intent_frames: usize,
    /// Fraction of the remaining offset the intended path recovers per frame while within the threshold.
    pub recenter: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct AxisSample {
    /// Raw cumulative camera path.
    pub raw: f64,
    /// Intended camera path.
    pub intended: f64,
    /// Correction applied to the frame; always within ±threshold.
    pub jitter: f64,
    pub label: Label,
}

/// Cumulative path from per-frame deltas; `deltas[i]` is the motion into frame `i + 1`.
pub fn cumulative(deltas: &[f64]) -> Vec<f64> {
    std::iter::once(0.0)
        .chain(deltas.iter().scan(0.0, |p, d| {
            *p += d;
            Some(*p)
        }))
        .collect()
}

/// Per-frame fraction of the correction released while a pan is in progress.
/// Easing it out during camera movement is invisible and frees the margin a
/// pan would otherwise hold for its whole length.
const PAN_RELEASE: f64 = 0.1;

pub fn separate(raw: &[f64], params: Params) -> Vec<AxisSample> {
    let Params {
        threshold: t,
        intent_frames: n,
        recenter: k,
    } = params;
    let mut out: Vec<AxisSample> = Vec::with_capacity(raw.len());
    let mut intended = raw.first().copied().unwrap_or(0.0);
    let mut jitter = 0.0;
    // Direction (+1 / -1) of the pan in progress, or 0 when not panning.
    let mut pan = 0.0;

    for (i, &p) in raw.iter().enumerate() {
        let delta = if i > 0 { p - raw[i - 1] } else { 0.0 };
        let e = p - intended;
        let label = if delta * pan > 0.0 {
            // A confirmed pan continues while the camera keeps moving the same way.
            jitter *= 1.0 - PAN_RELEASE;
            Label::Intentional
        } else if e.abs() <= t {
            pan = 0.0;
            intended += k * e;
            jitter = p - intended;
            Label::Jitter
        } else if sustained(raw, i, intended, t, n) {
            pan = e.signum();
            // The pan's ramp-up was within the threshold, so it was labelled jitter. Walk
            // back over the frames already moving this way and relabel them as part of the pan.
            let mut start = i;
            while start >= 2
                && out[start - 1].label == Label::Jitter
                && (raw[start - 1] - raw[start - 2]) * pan > 0.0
            {
                start -= 1;
            }
            // The correction carries over from before the ramp, so the output doesn't jump.
            jitter = if start > 0 {
                out[start - 1].jitter
            } else {
                0.0
            };
            for (j, sample) in out.iter_mut().enumerate().skip(start) {
                jitter *= 1.0 - PAN_RELEASE;
                sample.intended = raw[j] - jitter;
                sample.jitter = jitter;
                sample.label = Label::Intentional;
            }
            jitter *= 1.0 - PAN_RELEASE;
            Label::Intentional
        } else {
            pan = 0.0;
            jitter = e.clamp(-t, t);
            Label::Bump
        };
        if label == Label::Intentional {
            intended = p - jitter;
        }
        out.push(AxisSample {
            raw: p,
            intended,
            jitter,
            label,
        });
    }
    out
}

/// Whether the path stays past `threshold` from `intended`, on the same side,
/// for `n` frames starting at `start`. A run that reaches the end of the clip
/// counts, since the movement may continue past the cut.
fn sustained(raw: &[f64], start: usize, intended: f64, threshold: f64, n: usize) -> bool {
    let side = (raw[start] - intended).signum();
    raw[start..]
        .iter()
        .take(n)
        .all(|&p| (p - intended) * side > threshold)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(threshold: f64, intent_frames: usize) -> Params {
        Params {
            threshold,
            intent_frames,
            recenter: 0.0,
        }
    }

    fn labels(samples: &[AxisSample]) -> Vec<Label> {
        samples.iter().map(|s| s.label).collect()
    }

    fn max_jitter(samples: &[AxisSample]) -> f64 {
        samples.iter().map(|s| s.jitter.abs()).fold(0.0, f64::max)
    }

    #[test]
    fn cumulative_starts_at_zero() {
        assert_eq!(cumulative(&[1.0, 2.0, -0.5]), vec![0.0, 1.0, 3.0, 2.5]);
    }

    #[test]
    fn small_deltas_accumulate_into_displacement() {
        // Every delta is 1 px, but the frame drifts 3 px: jitter must measure displacement.
        let raw = cumulative(&[1.0, 1.0, 1.0, -1.0, -1.0, -1.0]);
        let s = separate(&raw, params(5.0, 3));
        assert!(labels(&s).iter().all(|&l| l == Label::Jitter));
        assert_eq!(max_jitter(&s), 3.0);
    }

    #[test]
    fn exactly_at_threshold_is_jitter() {
        let s = separate(&[0.0, 2.0, -2.0, 0.0], params(2.0, 3));
        assert!(labels(&s).iter().all(|&l| l == Label::Jitter));
        assert_eq!(max_jitter(&s), 2.0);
    }

    #[test]
    fn sustained_motion_is_intentional_and_passes_through() {
        // Hold, then pan at 3 px/frame, then hold.
        let mut deltas = vec![0.0; 5];
        deltas.extend([3.0; 10]);
        deltas.extend([0.0; 5]);
        let raw = cumulative(&deltas);
        let s = separate(&raw, params(2.0, 3));
        assert!(s[6..=15].iter().all(|s| s.label == Label::Intentional));
        assert_eq!(s[16].label, Label::Jitter);
        // Output moves at the camera's true speed during the pan (intended path tracks it).
        for i in 5..15 {
            assert!((s[i + 1].intended - s[i].intended - 3.0).abs() < 1e-9);
        }
        // A clean pan costs no crop.
        assert_eq!(max_jitter(&s), 0.0);
    }

    #[test]
    fn run_of_n_minus_one_is_bump_run_of_n_is_intentional() {
        let t = 2.0;
        let n = 4;
        let spike = |len: usize| {
            let mut raw = vec![0.0; 3];
            raw.extend(std::iter::repeat_n(5.0, len));
            raw.extend([0.0; 3]);
            raw
        };
        let short = separate(&spike(n - 1), params(t, n));
        assert!(short[3..3 + n - 1].iter().all(|s| s.label == Label::Bump));
        // A bump doesn't move the intended path.
        assert!(short.iter().all(|s| s.intended == 0.0));
        assert_eq!(max_jitter(&short), t);

        let long = separate(&spike(n), params(t, n));
        assert_eq!(long[3].label, Label::Intentional);
        assert_eq!(long[3].intended, 5.0);
        // Once the intent catches up, holding the new position is within the threshold.
        assert_eq!(long[4].label, Label::Jitter);
    }

    #[test]
    fn sign_flip_breaks_a_pan() {
        // A one-frame jolt backwards straight after a pan is re-checked, not continued.
        let raw = vec![0.0, 5.0, 10.0, 15.0, 5.0, 15.0, 15.0];
        let s = separate(&raw, params(2.0, 2));
        assert_eq!(s[3].label, Label::Intentional);
        assert_eq!(s[4].label, Label::Bump);
        assert_eq!(s[5].label, Label::Jitter);
    }

    #[test]
    fn run_reaching_end_of_clip_is_intentional() {
        let s = separate(&[0.0, 0.0, 6.0, 7.0], params(2.0, 5));
        assert_eq!(s[2].label, Label::Intentional);
    }

    #[test]
    fn recentering_eases_correction_back() {
        // The camera settles 1.5 px off; recentering treats the new position as intended over time.
        let mut raw = vec![0.0];
        raw.extend(std::iter::repeat_n(1.5, 200));
        let s = separate(
            &raw,
            Params {
                threshold: 2.0,
                intent_frames: 2,
                recenter: 0.05,
            },
        );
        assert!(s[1].jitter > 1.4);
        assert!(s.last().unwrap().jitter.abs() < 0.01);
    }

    #[test]
    fn pan_releases_existing_correction_without_jumping() {
        // 1.5 px of jitter (against the pan direction) is being corrected when a pan at 5 px/frame starts.
        let mut deltas = vec![-1.5];
        deltas.extend([5.0; 40]);
        let s = separate(&cumulative(&deltas), params(2.0, 3));
        assert_eq!(s[2].label, Label::Intentional);
        // No jump at the start of the pan, then the correction eases out.
        assert!((s[2].jitter - s[1].jitter).abs() <= 0.2);
        assert!(s.last().unwrap().jitter.abs() < 0.05);
        assert_eq!(max_jitter(&s), 1.5);
    }

    #[test]
    fn pan_slower_than_threshold_includes_its_ramp_up() {
        // 0.5 px/frame only passes T = 2 at frame 5. Once confirmed, the ramp-up
        // (frames 1-4) is relabelled as part of the pan, so it costs no crop.
        let mut deltas = vec![0.0; 3];
        deltas.extend([0.5; 30]);
        let s = separate(&cumulative(&deltas), params(2.0, 3));
        assert_eq!(s[3].label, Label::Jitter);
        assert!(s[4..].iter().all(|s| s.label == Label::Intentional));
        assert_eq!(max_jitter(&s), 0.0);
    }

    #[test]
    fn ramp_up_stops_at_direction_change() {
        // Shake (-1 then +1) before a slow pan: only the frames moving with the pan are relabelled.
        let mut deltas = vec![-1.0, 1.0];
        deltas.extend([0.5; 30]);
        let s = separate(&cumulative(&deltas), params(2.0, 3));
        assert_eq!(s[1].label, Label::Jitter);
        assert_eq!(s[2].label, Label::Intentional);
        assert_eq!(max_jitter(&s), 1.0);
    }
}
