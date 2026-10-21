//! Rejects untrustworthy motion measurements and fills the gaps they leave.
//! Runs on the whole tracked path, between tracking and jitter separation.

use crate::motion::Position;

/// Matches weaker than this fraction of the clip's median confidence are rejected.
/// Judging against the clip itself adapts to how much texture the footage has.
const RELATIVE_CONFIDENCE: f64 = 0.3;

/// A frame whose position is further than this fraction of the frame size from
/// the median of its neighbourhood is a measurement error: the camera cannot
/// jump there and back within a couple of frames.
const SPIKE_FRACTION: f64 = 0.01;

/// Frames either side of a frame in the spike check's neighbourhood.
const SPIKE_RADIUS: usize = 2;

/// Marks weak and physically impossible measurements unreliable, then replaces
/// every unreliable position by interpolating between the trusted frames around it.
/// `width` and `height` are the frame size the positions are measured in.
pub fn repair(positions: &mut [Position], width: usize, height: usize) {
    reject_weak(positions);
    interpolate(positions);
    reject_spikes(positions, width as f64, height as f64);
    interpolate(positions);
}

fn reject_weak(positions: &mut [Position]) {
    // Frame 0 is the reference and has no match of its own.
    let mut confidences: Vec<f64> = positions
        .iter()
        .skip(1)
        .filter(|p| p.reliable)
        .map(|p| p.confidence)
        .collect();
    if confidences.is_empty() {
        return;
    }
    let floor = RELATIVE_CONFIDENCE * median(&mut confidences);
    for p in positions.iter_mut().skip(1) {
        if p.confidence < floor {
            p.reliable = false;
        }
    }
}

fn reject_spikes(positions: &mut [Position], width: f64, height: f64) {
    // Only frames with a full neighbourhood: a truncated window at the clip edges
    // would read the end of a steady pan as a spike.
    let n = positions.len();
    let spikes: Vec<usize> = (SPIKE_RADIUS..n.saturating_sub(SPIKE_RADIUS))
        .filter(|&t| {
            let window = &positions[t - SPIKE_RADIUS..=t + SPIKE_RADIUS];
            let mut xs: Vec<f64> = window.iter().map(|p| p.x).collect();
            let mut ys: Vec<f64> = window.iter().map(|p| p.y).collect();
            positions[t].reliable
                && ((positions[t].x - median(&mut xs)).abs() > SPIKE_FRACTION * width
                    || (positions[t].y - median(&mut ys)).abs() > SPIKE_FRACTION * height)
        })
        .collect();
    for t in spikes {
        positions[t].reliable = false;
    }
}

/// Linear interpolation across runs of unreliable frames; a run at the end of
/// the clip holds the last trusted position.
fn interpolate(positions: &mut [Position]) {
    let mut last: Option<usize> = None;
    for t in 0..positions.len() {
        if !positions[t].reliable {
            continue;
        }
        let gap_start = last.map_or(0, |l| l + 1);
        if let Some(l) = last {
            let (a, b) = (positions[l], positions[t]);
            for (k, p) in positions[gap_start..t].iter_mut().enumerate() {
                let f = (k + 1) as f64 / (t - l) as f64;
                p.x = a.x + f * (b.x - a.x);
                p.y = a.y + f * (b.y - a.y);
            }
        }
        last = Some(t);
    }
    if let Some(l) = last {
        let (x, y) = (positions[l].x, positions[l].y);
        for p in &mut positions[l + 1..] {
            (p.x, p.y) = (x, y);
        }
    }
}

fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len().is_multiple_of(2) {
        (values[mid - 1] + values[mid]) / 2.0
    } else {
        values[mid]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(xs: &[f64]) -> Vec<Position> {
        xs.iter()
            .map(|&x| Position {
                x,
                confidence: 100.0,
                reliable: true,
                ..Default::default()
            })
            .collect()
    }

    fn xs(positions: &[Position]) -> Vec<f64> {
        positions.iter().map(|p| p.x).collect()
    }

    #[test]
    fn weak_match_is_rejected_relative_to_clip() {
        let mut p = path(&[0.0, 1.0, 9.0, 3.0, 4.0]);
        p[2].confidence = 20.0; // Above any fixed floor, but weak for this clip.
        repair(&mut p, 10_000, 10_000);
        assert!(!p[2].reliable);
        assert_eq!(xs(&p), vec![0.0, 1.0, 2.0, 3.0, 4.0]);
    }

    #[test]
    fn single_frame_spike_is_rejected() {
        let mut p = path(&[0.0, 0.5, 1.0, 30.0, 1.5, 2.0, 2.5]);
        repair(&mut p, 640, 360);
        assert!(!p[3].reliable);
        assert!((p[3].x - 1.25).abs() < 1e-9);
        assert!(p.iter().enumerate().all(|(i, q)| i == 3 || q.reliable));
    }

    #[test]
    fn zigzag_is_rejected() {
        // Alternating ±50 px every frame, like the beach clip's bad stretch.
        let mut p = path(&[0.0, 0.0, 0.0, 15.0, -10.0, 12.0, -15.0, 0.0, 0.0, 0.0]);
        repair(&mut p, 640, 360);
        assert!(p[3..7].iter().all(|q| !q.reliable));
        assert!(p[3..7].iter().all(|q| q.x.abs() < 1e-9));
    }

    #[test]
    fn steady_pan_and_step_are_kept() {
        let ramp: Vec<f64> = (0..20).map(|t| 8.0 * t as f64).collect();
        let mut p = path(&ramp);
        repair(&mut p, 640, 360);
        assert!(p.iter().all(|q| q.reliable));

        // A 3-frame jolt is real movement, not a spike.
        let mut p = path(&[0.0, 0.0, 0.0, 20.0, 20.0, 20.0, 0.0, 0.0, 0.0]);
        repair(&mut p, 640, 360);
        assert!(p.iter().all(|q| q.reliable));
    }

    #[test]
    fn trailing_gap_holds_last_trusted_position() {
        let mut p = path(&[0.0, 1.0, 2.0, 3.0]);
        p[3].reliable = false;
        p[3].x = 50.0;
        repair(&mut p, 640, 360);
        assert_eq!(p[3].x, 2.0);
    }
}
