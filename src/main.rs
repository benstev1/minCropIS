use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;

/// Stabilize a video with the minimum crop that removes jitter within a threshold.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Input video.
    input: PathBuf,

    /// Output video (H.264, cropped size, audio copied).
    #[arg(short, long)]
    output: PathBuf,

    /// Jitter threshold as a percentage of the frame, applied to both axes.
    #[arg(short, long, default_value_t = 2.0)]
    threshold: f64,

    /// Frames motion must stay past the threshold to count as intentional.
    #[arg(long, default_value_t = 5)]
    intent_frames: usize,

    /// Per-frame rate at which the correction eases back to center while within the threshold.
    #[arg(long, default_value_t = 0.02)]
    recenter: f64,

    /// Write per-frame analysis (motion, paths, jitter, labels) as JSON.
    #[arg(long)]
    report: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let settings = minis::Settings {
        threshold_percent: args.threshold,
        intent_frames: args.intent_frames,
        recenter: args.recenter,
    };
    settings.validate()?;

    eprintln!("analyzing {} ...", args.input.display());
    let analysis = minis::analyze(&args.input, settings, |_| Ok(()))?;

    if let Some(report) = &args.report {
        let file = std::fs::File::create(report)
            .with_context(|| format!("creating {}", report.display()))?;
        serde_json::to_writer_pretty(std::io::BufWriter::new(file), &analysis)?;
    }

    eprintln!("rendering {} ...", args.output.display());
    minis::render(&args.input, &args.output, &analysis, |_| Ok(()))?;

    let a = &analysis;
    let pct = |kept: usize, size: usize| 100.0 * (size - kept) as f64 / size as f64;
    println!("frames:        {}", a.frames.len());
    println!(
        "threshold:     {}% ({:.1} px x, {:.1} px y), intent after {} frames",
        a.threshold_percent, a.threshold_x, a.threshold_y, a.intent_frames
    );
    println!(
        "max jitter:    {:.2} px x, {:.2} px y",
        a.max_jitter_x, a.max_jitter_y
    );
    println!(
        "crop:          {:.1}% x, {:.1}% y  ->  {}x{} (from {}x{})",
        pct(a.crop.width, a.width),
        pct(a.crop.height, a.height),
        a.crop.width,
        a.crop.height,
        a.width,
        a.height
    );
    for (axis, c) in [("x", &a.counts_x), ("y", &a.counts_y)] {
        println!(
            "frames ({axis}):    {} jitter, {} bump, {} intentional",
            c.jitter, c.bump, c.intentional
        );
    }
    if a.unreliable_frames > 0 {
        println!(
            "unreliable:    {} frames (motion interpolated)",
            a.unreliable_frames
        );
    }
    Ok(())
}
