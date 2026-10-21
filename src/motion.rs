//! Camera motion between frames via tiled phase correlation.

use rustfft::num_complex::Complex;
use rustfft::{Fft, FftPlanner};
use std::sync::Arc;

/// Peak-to-sidelobe ratio below which a match is considered unreliable
/// (scene cut, heavy blur). Noise-only peaks sit around 4-5.
pub const MIN_CONFIDENCE: f64 = 10.0;

/// Regularization of the cross-power normalization, relative to its mean magnitude.
const REGULARIZATION: f32 = 0.1;

#[derive(Debug, Clone, Copy, Default)]
pub struct Shift {
    /// Content displacement from the previous frame to this one, in pixels.
    pub dx: f64,
    pub dy: f64,
    /// Peak-to-sidelobe ratio of the correlation peak.
    pub confidence: f64,
}

impl Shift {
    pub fn is_reliable(&self) -> bool {
        self.confidence >= MIN_CONFIDENCE
    }
}

/// Tiles whose shifts lie within this distance (analysis px) of each other agree.
const CONSENSUS_RADIUS: f64 = 1.5;

/// Minimum number of agreeing tiles for a frame's motion to be trusted.
const MIN_SUPPORT: usize = 3;

/// Consecutive frames without a trusted match after which tracking restarts from
/// the current frame (e.g. after a scene cut), accepting a jump in the path.
const MAX_FAILURES: usize = 3;

/// Camera position of a frame relative to the first frame, in analysis pixels.
#[derive(Debug, Clone, Copy, Default)]
pub struct Position {
    pub x: f64,
    pub y: f64,
    /// Mean peak-to-sidelobe ratio of the agreeing tiles.
    pub confidence: f64,
    /// Number of tiles that agreed on the motion.
    pub support: usize,
    /// False when the motion could not be trusted; the position is then filled in
    /// later by `outliers::repair`.
    pub reliable: bool,
}

/// Motion agreed on by a majority of tiles.
#[derive(Debug, Clone, Copy)]
struct Consensus {
    dx: f64,
    dy: f64,
    confidence: f64,
    support: usize,
}

/// Spectra of every tile of a frame, with the frame's position.
struct Reference {
    spectra: Vec<Vec<Complex<f32>>>,
    x: f64,
    y: f64,
}

/// Tracks the camera position of each frame.
///
/// Each frame is split into a grid of tiles that are matched independently, and
/// the motion most tiles agree on wins. Regions that move on their own (water, a
/// moving subject) disagree with the camera motion and are outvoted.
///
/// Frames are matched against a keyframe rather than the previous frame, so
/// per-match errors don't accumulate into drift while the camera stays around one
/// spot. The keyframe is replaced (via the last trusted frame) when the frame
/// moves too far from it. Untrusted frames never become a reference.
pub struct Tracker {
    correlator: PhaseCorrelator,
    width: usize,
    tile_width: usize,
    tile_height: usize,
    /// Top-left corner of each tile.
    tiles: Vec<(usize, usize)>,
    /// Offset from the keyframe (analysis px) beyond which it is replaced.
    max_offset: f64,
    key: Option<Reference>,
    /// Last trusted frame.
    prev: Option<Reference>,
    failures: usize,
    tile_buf: Vec<u8>,
}

impl Tracker {
    pub fn new(width: usize, height: usize) -> Self {
        // About 4x3 tiles at the 640 px analysis size, keeping tiles >= ~110 px.
        let cols = (width / 150).clamp(2, 4);
        let rows = (height / 110).clamp(2, 3);
        let (tile_width, tile_height) = (width / cols, height / rows);
        let tiles = (0..rows)
            .flat_map(|r| (0..cols).map(move |c| (c * tile_width, r * tile_height)))
            .collect();
        Self {
            correlator: PhaseCorrelator::new(tile_width, tile_height),
            width,
            tile_width,
            tile_height,
            tiles,
            max_offset: 0.15 * tile_width.min(tile_height) as f64,
            key: None,
            prev: None,
            failures: 0,
            tile_buf: Vec::new(),
        }
    }

    pub fn track(&mut self, gray: &[u8]) -> Position {
        let spectra = self.tile_spectra(gray);
        let (Some(key), Some(prev)) = (&self.key, &self.prev) else {
            self.key = Some(Reference {
                spectra: spectra.clone(),
                x: 0.0,
                y: 0.0,
            });
            self.prev = Some(Reference {
                spectra,
                x: 0.0,
                y: 0.0,
            });
            return Position {
                support: self.tiles.len(),
                reliable: true,
                ..Default::default()
            };
        };

        let correlator = &mut self.correlator;
        let max_offset = self.max_offset;
        let matched = consensus(correlator, &key.spectra, &spectra)
            .filter(|c| c.dx.hypot(c.dy) <= max_offset)
            .map(|c| (c, key.x, key.y, false))
            .or_else(|| {
                consensus(correlator, &prev.spectra, &spectra).map(|c| (c, prev.x, prev.y, true))
            });

        let Some((c, base_x, base_y, rekey)) = matched else {
            let (x, y) = (prev.x, prev.y);
            self.failures += 1;
            if self.failures >= MAX_FAILURES {
                self.failures = 0;
                self.key = Some(Reference {
                    spectra: spectra.clone(),
                    x,
                    y,
                });
                self.prev = Some(Reference { spectra, x, y });
            }
            return Position {
                x,
                y,
                ..Default::default()
            };
        };

        self.failures = 0;
        let (x, y) = (base_x + c.dx, base_y + c.dy);
        if rekey {
            self.key = Some(Reference {
                spectra: spectra.clone(),
                x,
                y,
            });
        }
        self.prev = Some(Reference { spectra, x, y });
        Position {
            x,
            y,
            confidence: c.confidence,
            support: c.support,
            reliable: true,
        }
    }

    fn tile_spectra(&mut self, gray: &[u8]) -> Vec<Vec<Complex<f32>>> {
        let (tw, th) = (self.tile_width, self.tile_height);
        let mut spectra = Vec::with_capacity(self.tiles.len());
        for &(x0, y0) in &self.tiles {
            self.tile_buf.clear();
            for y in y0..y0 + th {
                let row = y * self.width + x0;
                self.tile_buf.extend_from_slice(&gray[row..row + tw]);
            }
            spectra.push(self.correlator.spectrum(&self.tile_buf));
        }
        spectra
    }
}

/// The shift supported by the most tiles (ties broken by total confidence),
/// averaged over the agreeing tiles weighted by confidence.
fn consensus(
    correlator: &mut PhaseCorrelator,
    reference: &[Vec<Complex<f32>>],
    current: &[Vec<Complex<f32>>],
) -> Option<Consensus> {
    let shifts: Vec<Shift> = reference
        .iter()
        .zip(current)
        .map(|(r, c)| correlator.shift(r, c))
        .filter(Shift::is_reliable)
        .collect();
    let agreeing = |a: &Shift| -> Vec<&Shift> {
        shifts
            .iter()
            .filter(|b| (a.dx - b.dx).hypot(a.dy - b.dy) <= CONSENSUS_RADIUS)
            .collect()
    };
    let inliers = shifts
        .iter()
        .map(agreeing)
        .max_by(|a, b| {
            let total = |v: &Vec<&Shift>| v.iter().map(|s| s.confidence).sum::<f64>();
            a.len().cmp(&b.len()).then(total(a).total_cmp(&total(b)))
        })
        .filter(|inliers| inliers.len() >= MIN_SUPPORT)?;

    let weight: f64 = inliers.iter().map(|s| s.confidence).sum();
    Some(Consensus {
        dx: inliers.iter().map(|s| s.dx * s.confidence).sum::<f64>() / weight,
        dy: inliers.iter().map(|s| s.dy * s.confidence).sum::<f64>() / weight,
        confidence: weight / inliers.len() as f64,
        support: inliers.len(),
    })
}

/// Reusable phase correlator for grayscale frames of a fixed size.
pub struct PhaseCorrelator {
    width: usize,
    height: usize,
    window: Vec<f32>,
    lowpass: Vec<f32>,
    row_fft: Arc<dyn Fft<f32>>,
    col_fft: Arc<dyn Fft<f32>>,
    row_ifft: Arc<dyn Fft<f32>>,
    col_ifft: Arc<dyn Fft<f32>>,
    column: Vec<Complex<f32>>,
}

impl PhaseCorrelator {
    pub fn new(width: usize, height: usize) -> Self {
        let mut planner = FftPlanner::new();
        let hann = |n: usize| -> Vec<f32> {
            (0..n)
                .map(|i| 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n as f32).cos())
                .collect()
        };
        let (wx, wy) = (hann(width), hann(height));
        let window = (0..height)
            .flat_map(|y| {
                let wy = wy[y];
                wx.iter().map(move |&w| w * wy)
            })
            .collect();
        // Gaussian low-pass on the cross-power spectrum. Plain phase correlation weights
        // all frequencies equally, so sensor and quantization noise at high frequencies
        // distorts the peak on smooth content. With this weighting the peak becomes a
        // Gaussian of std PEAK_SIGMA px, which `subpixel` fits exactly.
        const PEAK_SIGMA: f32 = 1.0;
        let freq = |i: usize, n: usize| {
            let k = if i > n / 2 {
                i as f32 - n as f32
            } else {
                i as f32
            };
            k / n as f32
        };
        let sigma_f = 1.0 / (2.0 * std::f32::consts::PI * PEAK_SIGMA);
        let lowpass = (0..height)
            .flat_map(|y| {
                let fy = freq(y, height);
                (0..width).map(move |x| {
                    let fx = freq(x, width);
                    (-(fx * fx + fy * fy) / (2.0 * sigma_f * sigma_f)).exp()
                })
            })
            .collect();
        Self {
            width,
            height,
            window,
            lowpass,
            row_fft: planner.plan_fft_forward(width),
            col_fft: planner.plan_fft_forward(height),
            row_ifft: planner.plan_fft_inverse(width),
            col_ifft: planner.plan_fft_inverse(height),
            column: vec![Complex::default(); height],
        }
    }

    /// Windowed forward spectrum of an 8-bit grayscale frame.
    pub fn spectrum(&mut self, gray: &[u8]) -> Vec<Complex<f32>> {
        assert_eq!(gray.len(), self.width * self.height);
        let mean = gray.iter().map(|&p| p as f32).sum::<f32>() / gray.len() as f32;
        let mut data: Vec<Complex<f32>> = gray
            .iter()
            .zip(&self.window)
            .map(|(&p, &w)| Complex::new((p as f32 - mean) * w, 0.0))
            .collect();
        let (row, col) = (self.row_fft.clone(), self.col_fft.clone());
        self.fft2d(&mut data, &*row, &*col);
        data
    }

    /// Shift of `curr`'s content relative to `prev`'s.
    pub fn shift(&mut self, prev: &[Complex<f32>], curr: &[Complex<f32>]) -> Shift {
        let mut cross: Vec<Complex<f32>> =
            curr.iter().zip(prev).map(|(c, p)| c * p.conj()).collect();
        // Regularized phase correlation: normalizing by |R| + eps keeps the sharp peak of
        // phase correlation for frequencies with real signal, while frequencies that hold
        // little more than noise contribute in proportion to their energy instead of fully.
        let eps = REGULARIZATION * cross.iter().map(|r| r.norm()).sum::<f32>() / cross.len() as f32;
        for (r, &weight) in cross.iter_mut().zip(&self.lowpass) {
            *r *= weight / (r.norm() + eps + f32::MIN_POSITIVE);
        }
        let (row, col) = (self.row_ifft.clone(), self.col_ifft.clone());
        self.fft2d(&mut cross, &*row, &*col);
        let surface: Vec<f64> = cross.iter().map(|c| c.re as f64).collect();
        self.locate_peak(&surface)
    }

    fn fft2d(&mut self, data: &mut [Complex<f32>], row: &dyn Fft<f32>, col: &dyn Fft<f32>) {
        let (w, h) = (self.width, self.height);
        row.process(data);
        for x in 0..w {
            for y in 0..h {
                self.column[y] = data[y * w + x];
            }
            col.process(&mut self.column);
            for y in 0..h {
                data[y * w + x] = self.column[y];
            }
        }
    }

    fn locate_peak(&self, surface: &[f64]) -> Shift {
        let (w, h) = (self.width, self.height);
        let (peak_idx, &peak) = surface
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.total_cmp(b.1))
            .unwrap();
        let (px, py) = (peak_idx % w, peak_idx / w);
        let at = |x: isize, y: isize| {
            let x = x.rem_euclid(w as isize) as usize;
            let y = y.rem_euclid(h as isize) as usize;
            surface[y * w + x]
        };
        let (pxi, pyi) = (px as isize, py as isize);
        let fx = subpixel(at(pxi - 1, pyi), peak, at(pxi + 1, pyi));
        let fy = subpixel(at(pxi, pyi - 1), peak, at(pxi, pyi + 1));

        // Indices past the midpoint wrap around to negative shifts.
        let wrap = |p: usize, n: usize| {
            if p > n / 2 {
                p as f64 - n as f64
            } else {
                p as f64
            }
        };

        Shift {
            dx: wrap(px, w) + fx,
            dy: wrap(py, h) + fy,
            confidence: peak_to_sidelobe(surface, peak, px, py, w, h),
        }
    }
}

/// Sub-pixel offset of a Gaussian-shaped peak from its two neighbours: a parabola
/// through the log values. Falls back to a plain parabola if a neighbour is not positive.
fn subpixel(left: f64, center: f64, right: f64) -> f64 {
    let fit = |l: f64, c: f64, r: f64| {
        let denom = l - 2.0 * c + r;
        if denom < 0.0 {
            (0.5 * (l - r) / denom).clamp(-0.5, 0.5)
        } else {
            0.0
        }
    };
    if left > 0.0 && center > 0.0 && right > 0.0 {
        fit(left.ln(), center.ln(), right.ln())
    } else {
        fit(left, center, right)
    }
}

/// (peak - mean) / std of the surface, excluding a small region around the peak.
fn peak_to_sidelobe(surface: &[f64], peak: f64, px: usize, py: usize, w: usize, h: usize) -> f64 {
    const EXCLUDE: usize = 5;
    let near = |a: usize, b: usize, n: usize| {
        let d = a.abs_diff(b);
        d.min(n - d) <= EXCLUDE
    };
    let (mut sum, mut sum_sq, mut count) = (0.0, 0.0, 0.0);
    for (i, &v) in surface.iter().enumerate() {
        if near(i % w, px, w) && near(i / w, py, h) {
            continue;
        }
        sum += v;
        sum_sq += v * v;
        count += 1.0;
    }
    if count < 2.0 {
        return 0.0;
    }
    let mean = sum / count;
    let std = (sum_sq / count - mean * mean).max(0.0).sqrt();
    if std > 0.0 { (peak - mean) / std } else { 0.0 }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Smooth, non-periodic texture that can be sampled at any sub-pixel position,
    /// so shifted frames are exact rather than interpolated.
    pub fn texture(x: f64, y: f64) -> f64 {
        let blobs = [
            (40.0, 30.0, 9.0, 1.0),
            (120.0, 80.0, 14.0, -0.8),
            (200.0, 50.0, 6.0, 0.9),
            (70.0, 150.0, 11.0, 0.7),
            (230.0, 170.0, 17.0, -0.6),
            (160.0, 130.0, 5.0, 1.0),
            (20.0, 200.0, 8.0, -0.9),
            (280.0, 20.0, 10.0, 0.8),
        ];
        let mut v = 0.3 * (x * 0.071 + y * 0.023).sin() + 0.25 * (x * 0.017 - y * 0.093).cos();
        for (cx, cy, r, a) in blobs {
            let d2 = (x - cx).powi(2) + (y - cy).powi(2);
            v += a * (-d2 / (2.0 * r * r)).exp();
        }
        v
    }

    pub fn render_gray(w: usize, h: usize, ox: f64, oy: f64) -> Vec<u8> {
        let mut out = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                let v = texture(x as f64 - ox, y as f64 - oy);
                out.push((128.0 + 70.0 * v).clamp(0.0, 255.0) as u8);
            }
        }
        out
    }

    fn measure(dx: f64, dy: f64) -> Shift {
        let (w, h) = (320, 240);
        let mut pc = PhaseCorrelator::new(w, h);
        let a = pc.spectrum(&render_gray(w, h, 3.0, -2.0));
        let b = pc.spectrum(&render_gray(w, h, 3.0 + dx, -2.0 + dy));
        pc.shift(&a, &b)
    }

    #[test]
    fn recovers_integer_shift() {
        let s = measure(5.0, -3.0);
        assert!(
            (s.dx - 5.0).abs() < 0.1 && (s.dy + 3.0).abs() < 0.1,
            "{s:?}"
        );
        assert!(s.is_reliable());
    }

    #[test]
    fn recovers_subpixel_shift() {
        for (dx, dy) in [(0.3, -0.6), (2.5, 1.25), (-4.7, 0.8), (0.0, 0.0)] {
            let s = measure(dx, dy);
            assert!((s.dx - dx).abs() < 0.1, "dx {dx}: {s:?}");
            assert!((s.dy - dy).abs() < 0.1, "dy {dy}: {s:?}");
        }
    }

    #[test]
    fn unrelated_frames_are_unreliable() {
        let (w, h) = (320, 240);
        let mut pc = PhaseCorrelator::new(w, h);
        let mut seed = 12345u32;
        let mut noise = || {
            (0..w * h)
                .map(|_| {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    (seed >> 24) as u8
                })
                .collect::<Vec<u8>>()
        };
        let a = pc.spectrum(&noise());
        let b = pc.spectrum(&noise());
        assert!(!pc.shift(&a, &b).is_reliable());
    }
}
