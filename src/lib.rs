//! minIS: video stabilization with the minimum crop that removes jitter within a threshold.
//! See design.md for the full design.

pub mod crop;
pub mod motion;
pub mod outliers;
pub mod path;
pub mod render;
pub mod video;
#[cfg(feature = "web")]
pub mod web;

use anyhow::{Result, bail};
use serde::Serialize;
use std::path::Path;

use crop::CropRect;
use motion::Tracker;
use path::{AxisSample, Label, Params};
use video::{Decoder, Encoder, PixelFormat, VideoInfo};

/// Longest side of the downscaled frames used for motion estimation.
const ANALYSIS_MAX_DIM: usize = 640;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Settings {
    /// Jitter threshold as a percentage of the frame, applied to both axes.
    pub threshold_percent: f64,
    pub intent_frames: usize,
    pub recenter: f64,
}

impl Settings {
    pub fn validate(&self) -> Result<()> {
        if !(self.threshold_percent > 0.0 && self.threshold_percent < 50.0) {
            bail!("threshold must be a percentage between 0 and 50");
        }
        if self.intent_frames == 0 {
            bail!("intent frames must be at least 1");
        }
        if !(0.0..=1.0).contains(&self.recenter) {
            bail!("recenter must be between 0 and 1");
        }
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct FrameReport {
    pub index: usize,
    /// Measured content motion from the previous frame, in full-resolution pixels.
    pub dx: f64,
    pub dy: f64,
    /// Mean peak-to-sidelobe ratio of the agreeing tiles (0 for the first frame).
    pub confidence: f64,
    /// Number of tiles that agreed on this frame's motion.
    pub support: usize,
    /// False when the measurement was rejected (no tile consensus, weak for this clip,
    /// or a physically impossible spike) and the position was interpolated.
    pub reliable: bool,
    pub x: AxisSample,
    pub y: AxisSample,
}

#[derive(Debug, Default, Serialize)]
pub struct LabelCounts {
    pub jitter: usize,
    pub bump: usize,
    pub intentional: usize,
}

#[derive(Debug, Serialize)]
pub struct Analysis {
    pub width: usize,
    pub height: usize,
    #[serde(skip)]
    pub frame_rate: String,
    pub threshold_percent: f64,
    pub threshold_x: f64,
    pub threshold_y: f64,
    pub intent_frames: usize,
    pub max_jitter_x: f64,
    pub max_jitter_y: f64,
    pub crop: CropRect,
    pub counts_x: LabelCounts,
    pub counts_y: LabelCounts,
    pub unreliable_frames: usize,
    pub frames: Vec<FrameReport>,
}

/// Measured camera path of a clip. It doesn't depend on `Settings`, so it can
/// be reused to re-plan the same clip with different settings.
#[derive(Debug, Clone)]
pub struct Motion {
    pub width: usize,
    pub height: usize,
    pub frame_rate: String,
    /// Camera position per frame, in full-resolution pixels.
    pub raw_x: Vec<f64>,
    pub raw_y: Vec<f64>,
    positions: Vec<motion::Position>,
}

/// Pass 1: estimate motion, split jitter from intentional movement, and size the crop.
/// `progress` is called with the number of frames analyzed so far.
pub fn analyze(input: &Path, settings: Settings, progress: impl FnMut(usize)) -> Result<Analysis> {
    Ok(plan(&measure(input, progress)?, settings))
}

/// The slow, settings-independent part of pass 1: track the camera path.
/// `progress` is called with the number of frames measured so far.
pub fn measure(input: &Path, mut progress: impl FnMut(usize)) -> Result<Motion> {
    let VideoInfo {
        width,
        height,
        frame_rate,
        ..
    } = video::probe(input)?;
    let scale = (width.max(height) as f64 / ANALYSIS_MAX_DIM as f64).max(1.0);
    let aw = ((width as f64 / scale).round() as usize).max(16);
    let ah = ((height as f64 / scale).round() as usize).max(16);
    let (sx, sy) = (width as f64 / aw as f64, height as f64 / ah as f64);

    let mut tracker = Tracker::new(aw, ah);
    let mut decoder = Decoder::open(input, PixelFormat::Gray, aw, ah)?;
    let mut gray = Vec::new();
    let mut positions = Vec::new();
    while decoder.read_frame(&mut gray)? {
        positions.push(tracker.track(&gray));
        progress(positions.len());
    }
    decoder.finish()?;
    if positions.is_empty() {
        bail!("{} contains no frames", input.display());
    }
    outliers::repair(&mut positions, aw, ah);
    Ok(Motion {
        width,
        height,
        frame_rate,
        raw_x: positions.iter().map(|p| p.x * sx).collect(),
        raw_y: positions.iter().map(|p| p.y * sy).collect(),
        positions,
    })
}

/// The fast part of pass 1: split the measured path into jitter and intentional
/// movement under `settings`, and size the crop.
pub fn plan(motion: &Motion, settings: Settings) -> Analysis {
    let Motion {
        width,
        height,
        ref frame_rate,
        ref raw_x,
        ref raw_y,
        ref positions,
    } = *motion;
    let threshold_x = settings.threshold_percent / 100.0 * width as f64;
    let threshold_y = settings.threshold_percent / 100.0 * height as f64;
    let params = |threshold| Params {
        threshold,
        intent_frames: settings.intent_frames,
        recenter: settings.recenter,
    };
    let xs = path::separate(raw_x, params(threshold_x));
    let ys = path::separate(raw_y, params(threshold_y));

    let max_abs =
        |samples: &[AxisSample]| samples.iter().map(|s| s.jitter.abs()).fold(0.0, f64::max);
    let (max_jitter_x, max_jitter_y) = (max_abs(&xs), max_abs(&ys));

    let frames: Vec<FrameReport> = xs
        .into_iter()
        .zip(ys)
        .enumerate()
        .map(|(index, (x, y))| {
            let (prev_x, prev_y) = index
                .checked_sub(1)
                .map_or((x.raw, y.raw), |i| (raw_x[i], raw_y[i]));
            FrameReport {
                index,
                dx: x.raw - prev_x,
                dy: y.raw - prev_y,
                confidence: positions[index].confidence,
                support: positions[index].support,
                reliable: positions[index].reliable,
                x,
                y,
            }
        })
        .collect();

    Analysis {
        width,
        height,
        frame_rate: frame_rate.clone(),
        threshold_percent: settings.threshold_percent,
        threshold_x,
        threshold_y,
        intent_frames: settings.intent_frames,
        max_jitter_x,
        max_jitter_y,
        crop: crop::compute(width, height, max_jitter_x, max_jitter_y),
        counts_x: count_labels(frames.iter().map(|f| f.x.label)),
        counts_y: count_labels(frames.iter().map(|f| f.y.label)),
        unreliable_frames: frames.iter().filter(|f| !f.reliable).count(),
        frames,
    }
}

fn count_labels(labels: impl Iterator<Item = Label>) -> LabelCounts {
    let mut counts = LabelCounts::default();
    for label in labels {
        match label {
            Label::Jitter => counts.jitter += 1,
            Label::Bump => counts.bump += 1,
            Label::Intentional => counts.intentional += 1,
        }
    }
    counts
}

/// Pass 2: shift each full-color frame by its jitter, crop, and encode.
/// `progress` is called with the number of frames rendered so far.
pub fn render(
    input: &Path,
    output: &Path,
    analysis: &Analysis,
    mut progress: impl FnMut(usize),
) -> Result<()> {
    let (w, h, crop) = (analysis.width, analysis.height, analysis.crop);
    let mut decoder = Decoder::open(input, PixelFormat::Rgb24, w, h)?;
    let mut encoder =
        Encoder::create(output, crop.width, crop.height, &analysis.frame_rate, input)?;
    let (mut rgb, mut out) = (Vec::new(), Vec::new());
    let mut frames = analysis.frames.iter();
    let mut done = 0;
    while decoder.read_frame(&mut rgb)? {
        let Some(frame) = frames.next() else { break };
        render::shift_and_crop(&rgb, w, h, crop, frame.x.jitter, frame.y.jitter, &mut out);
        encoder.write_frame(&out)?;
        done += 1;
        progress(done);
    }
    encoder.finish()?;
    decoder.finish()
}
