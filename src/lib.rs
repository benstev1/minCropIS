//! minIS: video stabilization with the minimum crop that removes jitter within a threshold.
//! See design.md for the full design.

pub mod crop;
pub mod motion;
pub mod path;
pub mod render;
pub mod video;

use anyhow::{Result, bail};
use serde::Serialize;
use std::path::Path;

use crop::CropRect;
use motion::Tracker;
use path::{AxisSample, Label, Params};
use video::{Decoder, Encoder, PixelFormat, VideoInfo};

/// Longest side of the downscaled frames used for motion estimation.
const ANALYSIS_MAX_DIM: usize = 640;

#[derive(Debug, Clone, Copy)]
pub struct Settings {
    /// Jitter threshold as a percentage of the frame, applied to both axes.
    pub threshold_percent: f64,
    pub intent_frames: usize,
    pub recenter: f64,
}

#[derive(Debug, Serialize)]
pub struct FrameReport {
    pub index: usize,
    /// Measured content motion from the previous frame, in full-resolution pixels.
    pub dx: f64,
    pub dy: f64,
    /// Peak-to-sidelobe ratio of this frame's match (0 for the first frame).
    pub confidence: f64,
    /// False when the motion estimate was discarded (scene cut, blur) and treated as 0.
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

/// Pass 1: estimate motion, split jitter from intentional movement, and size the crop.
pub fn analyze(input: &Path, settings: Settings) -> Result<Analysis> {
    let VideoInfo {
        width,
        height,
        frame_rate,
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
    }
    decoder.finish()?;
    if positions.is_empty() {
        bail!("{} contains no frames", input.display());
    }
    let raw_x: Vec<f64> = positions.iter().map(|p| p.x * sx).collect();
    let raw_y: Vec<f64> = positions.iter().map(|p| p.y * sy).collect();

    let threshold_x = settings.threshold_percent / 100.0 * width as f64;
    let threshold_y = settings.threshold_percent / 100.0 * height as f64;
    let params = |threshold| Params {
        threshold,
        intent_frames: settings.intent_frames,
        recenter: settings.recenter,
    };
    let xs = path::separate(&raw_x, params(threshold_x));
    let ys = path::separate(&raw_y, params(threshold_y));

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
                reliable: positions[index].reliable,
                x,
                y,
            }
        })
        .collect();

    Ok(Analysis {
        width,
        height,
        frame_rate,
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
    })
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
pub fn render(input: &Path, output: &Path, analysis: &Analysis) -> Result<()> {
    let (w, h, crop) = (analysis.width, analysis.height, analysis.crop);
    let mut decoder = Decoder::open(input, PixelFormat::Rgb24, w, h)?;
    let mut encoder =
        Encoder::create(output, crop.width, crop.height, &analysis.frame_rate, input)?;
    let (mut rgb, mut out) = (Vec::new(), Vec::new());
    let mut frames = analysis.frames.iter();
    while decoder.read_frame(&mut rgb)? {
        let Some(frame) = frames.next() else { break };
        render::shift_and_crop(&rgb, w, h, crop, frame.x.jitter, frame.y.jitter, &mut out);
        encoder.write_frame(&out)?;
    }
    encoder.finish()?;
    decoder.finish()
}
