//! Runs the `minis` binary on synthetic clips with a known camera path.

use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

const W: usize = 320;
const H: usize = 240;
const FRAMES: usize = 60;

/// Smooth, non-periodic texture sampled analytically, so sub-pixel shifts are exact.
fn texture(x: f64, y: f64) -> f64 {
    static BLOBS: OnceLock<Vec<(f64, f64, f64, f64)>> = OnceLock::new();
    let blobs = BLOBS.get_or_init(|| {
        let mut seed = 0x2545_f491u32;
        let mut rand = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f64 / (1u32 << 24) as f64
        };
        (0..60)
            .map(|_| {
                let (cx, cy) = (rand() * 400.0 - 40.0, rand() * 320.0 - 40.0);
                let r = 3.0 + rand() * 12.0;
                (cx, cy, 2.0 * r * r, rand() * 2.0 - 1.0)
            })
            .collect()
    });
    let mut v = 0.2 * (x * 0.05 + y * 0.03).sin();
    for &(cx, cy, two_r2, a) in blobs {
        v += a * (-((x - cx).powi(2) + (y - cy).powi(2)) / two_r2).exp();
    }
    v
}

/// Encodes a lossless clip whose content is offset by `path(t)` in frame t.
fn make_clip(name: &str, path: impl Fn(usize) -> (f64, f64)) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"));
    let file = dir.join(format!("{name}.mkv"));
    let mut ffmpeg = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgb24"])
        .args([
            "-s",
            &format!("{W}x{H}"),
            "-r",
            "30",
            "-i",
            "-",
            "-c:v",
            "ffv1",
        ])
        .arg(&file)
        .stdin(Stdio::piped())
        .spawn()
        .expect("ffmpeg must be installed");
    let mut stdin = ffmpeg.stdin.take().unwrap();
    for t in 0..FRAMES {
        let (ox, oy) = path(t);
        let mut frame = Vec::with_capacity(W * H * 3);
        for y in 0..H {
            for x in 0..W {
                let v = texture(x as f64 - ox, y as f64 - oy);
                let g = (128.0 + 90.0 * v).clamp(0.0, 255.0);
                frame.extend([g as u8, (255.0 - g) as u8, (g * 0.5 + 60.0) as u8]);
            }
        }
        stdin.write_all(&frame).unwrap();
    }
    drop(stdin);
    assert!(ffmpeg.wait().unwrap().success());
    file
}

fn stabilize(input: &Path, extra: &[&str]) -> Value {
    let output = input.with_extension("out.mp4");
    let report = input.with_extension("json");
    let status = Command::new(env!("CARGO_BIN_EXE_minis"))
        .arg(input)
        .arg("-o")
        .arg(&output)
        .arg("--report")
        .arg(&report)
        .args(["--threshold", "5", "--recenter", "0"])
        .args(extra)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    serde_json::from_slice(&std::fs::read(report).unwrap()).unwrap()
}

fn labels(report: &Value, axis: &str) -> Vec<String> {
    report["frames"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f[axis]["label"].as_str().unwrap().to_string())
        .collect()
}

fn output_size(video: &Path) -> (u64, u64, u64) {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-count_frames"])
        .args(["-show_entries", "stream=width,height,nb_read_frames"])
        .args(["-of", "csv=p=0"])
        .arg(video)
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let v: Vec<u64> = text.trim().split(',').map(|s| s.parse().unwrap()).collect();
    (v[0], v[1], v[2])
}

fn shake(t: usize) -> (f64, f64) {
    let t = t as f64;
    (
        2.0 * (0.9 * t).sin() + 0.6 * (2.3 * t).sin(),
        1.5 * (1.7 * t).sin(),
    )
}

#[test]
fn shake_within_threshold_is_removed_with_minimal_crop() {
    let clip = make_clip("shake", shake);
    let report = stabilize(&clip, &[]);

    // T is 5% (16 x 12 px); the crop follows the actual shake, not T.
    let true_max_x = (0..FRAMES).map(|t| shake(t).0.abs()).fold(0.0, f64::max);
    let true_max_y = (0..FRAMES).map(|t| shake(t).1.abs()).fold(0.0, f64::max);
    let (mx, my) = (
        report["max_jitter_x"].as_f64().unwrap(),
        report["max_jitter_y"].as_f64().unwrap(),
    );
    assert!(
        (mx - true_max_x).abs() < 0.25,
        "max jitter x {mx} vs {true_max_x}"
    );
    assert!(
        (my - true_max_y).abs() < 0.25,
        "max jitter y {my} vs {true_max_y}"
    );
    assert!(labels(&report, "x").iter().all(|l| l == "jitter"));
    assert!(labels(&report, "y").iter().all(|l| l == "jitter"));

    // The output keeps the cropped size and every frame.
    let crop = &report["crop"];
    let output = clip.with_extension("out.mp4");
    assert_eq!(
        output_size(&output),
        (
            crop["width"].as_u64().unwrap(),
            crop["height"].as_u64().unwrap(),
            FRAMES as u64
        )
    );

    // Re-analyzing the stabilized output finds almost no remaining motion.
    let residual = stabilize(&output, &[]);
    let rx = residual["max_jitter_x"].as_f64().unwrap();
    let ry = residual["max_jitter_y"].as_f64().unwrap();
    assert!(rx < 0.35 && ry < 0.35, "residual jitter {rx}, {ry}");
}

#[test]
fn pan_beyond_threshold_is_kept_and_costs_no_extra_crop() {
    // Shake throughout, plus a 6 px/frame pan in x over frames 20-39 (120 px total).
    let pan = |t: usize| 6.0 * (t.clamp(20, 40) - 20) as f64;
    let clip = make_clip("pan", |t| (pan(t) + shake(t).0, shake(t).1));
    let report = stabilize(&clip, &[]);

    let x = labels(&report, "x");
    assert!(x[21..40].iter().all(|l| l == "intentional"), "{x:?}");
    assert!(x[50..].iter().all(|l| l == "jitter"), "{x:?}");
    assert!(labels(&report, "y").iter().all(|l| l == "jitter"));

    // The pan is passed through rather than cropped for. The pan ends mid-shake, so the
    // new rest position includes that shake offset: margin is bounded by the shake's
    // peak-to-peak (~5.2 px), far below T = 16.
    let mx = report["max_jitter_x"].as_f64().unwrap();
    assert!(mx < 6.0, "max jitter x {mx}");
}

#[test]
fn short_spike_is_a_bump_and_crop_stays_within_threshold() {
    // A 3-frame jolt of 25 px (past T = 16) with intent requiring 5 frames.
    let clip = make_clip("bump", |t| {
        let jolt = if (30..33).contains(&t) { 25.0 } else { 0.0 };
        (shake(t).0 + jolt, shake(t).1)
    });
    let report = stabilize(&clip, &["--intent-frames", "5"]);

    let x = labels(&report, "x");
    assert!(x[30..33].iter().all(|l| l == "bump"), "{x:?}");
    assert!(!x.iter().any(|l| l == "intentional"), "{x:?}");
    let mx = report["max_jitter_x"].as_f64().unwrap();
    let tx = report["threshold_x"].as_f64().unwrap();
    assert!(mx <= tx + 1e-9, "max jitter {mx} > threshold {tx}");
}
