//! Runs the `minis` binary on synthetic clips with a known camera path.

use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

// The size real footage is analysed at, so tiling matches real use.
const W: usize = 640;
const H: usize = 360;
const FRAMES: usize = 60;

/// Rows at the top of the frame showing "water" in `make_clip_with_water`.
const WATER_ROWS: usize = 150;

/// Smooth, non-periodic texture of Gaussian blobs over a sinusoid. Blobs are
/// positioned analytically, so sub-pixel shifts are exact rather than interpolated.
struct Texture {
    /// (cx, cy, radius, amplitude)
    blobs: Vec<(f64, f64, f64, f64)>,
}

impl Texture {
    fn new(seed: u32, count: usize, radius: (f64, f64)) -> Self {
        let mut seed = seed;
        let mut rand = || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 8) as f64 / (1u32 << 24) as f64
        };
        // Cover the frame plus room for the largest camera offsets in the tests.
        let blobs = (0..count)
            .map(|_| {
                let cx = rand() * (W as f64 + 400.0) - 200.0;
                let cy = rand() * (H as f64 + 200.0) - 100.0;
                let r = radius.0 + rand() * (radius.1 - radius.0);
                (cx, cy, r, rand() * 2.0 - 1.0)
            })
            .collect();
        Self { blobs }
    }

    fn scene() -> &'static Texture {
        static SCENE: OnceLock<Texture> = OnceLock::new();
        SCENE.get_or_init(|| Texture::new(0x2545_f491, 300, (3.0, 15.0)))
    }

    fn water() -> &'static Texture {
        static WATER: OnceLock<Texture> = OnceLock::new();
        WATER.get_or_init(|| Texture::new(0x9e37_79b9, 900, (2.0, 6.0)))
    }

    /// Renders rows `rows` of the texture offset by (ox, oy) into `out` (W wide).
    fn render(&self, out: &mut [f64], rows: std::ops::Range<usize>, ox: f64, oy: f64) {
        for y in rows.clone() {
            for x in 0..W {
                let (fx, fy) = (x as f64 - ox, y as f64 - oy);
                out[y * W + x] = 0.2 * (fx * 0.05 + fy * 0.03).sin();
            }
        }
        // Splat each blob over its own neighbourhood only.
        for &(cx, cy, r, a) in &self.blobs {
            let (cx, cy, reach) = (cx + ox, cy + oy, 4.0 * r);
            let x0 = (cx - reach).max(0.0) as usize;
            let x1 = ((cx + reach).max(0.0) as usize).min(W);
            let y0 = ((cy - reach).max(0.0) as usize).max(rows.start);
            let y1 = ((cy + reach).max(0.0) as usize).min(rows.end);
            for y in y0..y1 {
                for x in x0..x1 {
                    let d2 = (x as f64 - cx).powi(2) + (y as f64 - cy).powi(2);
                    out[y * W + x] += a * (-d2 / (2.0 * r * r)).exp();
                }
            }
        }
    }
}

/// Encodes a lossless clip whose content is offset by `camera(t)` in frame t.
fn make_clip(name: &str, camera: impl Fn(usize) -> (f64, f64)) -> PathBuf {
    encode(name, |t, field| {
        Texture::scene().render(field, 0..H, camera(t).0, camera(t).1)
    })
}

/// Like `make_clip`, but the top `WATER_ROWS` rows show a texture that moves on
/// its own (`water(t)`, on top of the camera motion), like waves or a crowd.
fn make_clip_with_water(
    name: &str,
    camera: impl Fn(usize) -> (f64, f64),
    water: impl Fn(usize) -> (f64, f64),
) -> PathBuf {
    encode(name, |t, field| {
        let ((cx, cy), (wx, wy)) = (camera(t), water(t));
        Texture::water().render(field, 0..WATER_ROWS, cx + wx, cy + wy);
        Texture::scene().render(field, WATER_ROWS..H, cx, cy);
    })
}

fn encode(name: &str, render: impl Fn(usize, &mut [f64])) -> PathBuf {
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
    let mut field = vec![0.0; W * H];
    let mut frame = Vec::with_capacity(W * H * 3);
    for t in 0..FRAMES {
        render(t, &mut field);
        frame.clear();
        for &v in &field {
            let g = (128.0 + 90.0 * v).clamp(0.0, 255.0);
            frame.extend([g as u8, (255.0 - g) as u8, (g * 0.5 + 60.0) as u8]);
        }
        stdin.write_all(&frame).unwrap();
    }
    drop(stdin);
    assert!(ffmpeg.wait().unwrap().success());
    file
}

fn stabilize(input: &Path, threshold: &str, extra: &[&str]) -> Value {
    let output = input.with_extension("out.mp4");
    let report = input.with_extension("json");
    let status = Command::new(env!("CARGO_BIN_EXE_minis"))
        .arg(input)
        .arg("-o")
        .arg(&output)
        .arg("--report")
        .arg(&report)
        .args(["--threshold", threshold, "--recenter", "0"])
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
    let report = stabilize(&clip, "5", &[]);

    // T is 5% (32 x 18 px); the crop follows the actual shake, not T.
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
    let residual = stabilize(&output, "5", &[]);
    let rx = residual["max_jitter_x"].as_f64().unwrap();
    let ry = residual["max_jitter_y"].as_f64().unwrap();
    assert!(rx < 0.35 && ry < 0.35, "residual jitter {rx}, {ry}");
}

#[test]
fn pan_beyond_threshold_is_kept_and_costs_no_extra_crop() {
    // Shake throughout, plus a 6 px/frame pan in x over frames 20-39 (120 px total).
    let pan = |t: usize| 6.0 * (t.clamp(20, 40) - 20) as f64;
    let clip = make_clip("pan", |t| (pan(t) + shake(t).0, shake(t).1));
    let report = stabilize(&clip, "5", &[]);

    let x = labels(&report, "x");
    assert!(x[21..40].iter().all(|l| l == "intentional"), "{x:?}");
    assert!(x[50..].iter().all(|l| l == "jitter"), "{x:?}");
    assert!(labels(&report, "y").iter().all(|l| l == "jitter"));

    // The pan is passed through rather than cropped for. The pan ends mid-shake, so the
    // new rest position includes that shake offset: margin is bounded by the shake's
    // peak-to-peak (~5.2 px), far below T = 32.
    let mx = report["max_jitter_x"].as_f64().unwrap();
    assert!(mx < 6.0, "max jitter x {mx}");
}

#[test]
fn short_spike_is_a_bump_and_crop_stays_within_threshold() {
    // A 3-frame jolt of 30 px (past T = 19.2) with intent requiring 5 frames.
    let clip = make_clip("bump", |t| {
        let jolt = if (30..33).contains(&t) { 30.0 } else { 0.0 };
        (shake(t).0 + jolt, shake(t).1)
    });
    let report = stabilize(&clip, "3", &["--intent-frames", "5"]);

    let x = labels(&report, "x");
    assert!(x[30..33].iter().all(|l| l == "bump"), "{x:?}");
    assert!(!x.iter().any(|l| l == "intentional"), "{x:?}");
    let mx = report["max_jitter_x"].as_f64().unwrap();
    let tx = report["threshold_x"].as_f64().unwrap();
    assert!(mx <= tx + 1e-9, "max jitter {mx} > threshold {tx}");
}

#[test]
fn independently_moving_region_does_not_add_shake() {
    // The top 150 rows (5 of 12 analysis tiles, fully or partly) show "water" that
    // drifts and churns on its own by up to ~15 px per frame, as in the beach clip.
    let water = |t: usize| {
        let t = t as f64;
        (3.0 * t + 12.0 * (2.9 * t).sin(), 10.0 * (3.7 * t).cos())
    };
    let clip = make_clip_with_water("water", shake, water);
    let report = stabilize(&clip, "5", &[]);

    // The camera shake is measured from the static part of the scene.
    let true_max_x = (0..FRAMES).map(|t| shake(t).0.abs()).fold(0.0, f64::max);
    let true_max_y = (0..FRAMES).map(|t| shake(t).1.abs()).fold(0.0, f64::max);
    let (mx, my) = (
        report["max_jitter_x"].as_f64().unwrap(),
        report["max_jitter_y"].as_f64().unwrap(),
    );
    assert!(
        (mx - true_max_x).abs() < 0.5,
        "max jitter x {mx} vs {true_max_x}"
    );
    assert!(
        (my - true_max_y).abs() < 0.5,
        "max jitter y {my} vs {true_max_y}"
    );
    assert!(labels(&report, "x").iter().all(|l| l == "jitter"));

    // And the stabilized output's static part holds still.
    let residual = stabilize(&clip.with_extension("out.mp4"), "5", &[]);
    let rx = residual["max_jitter_x"].as_f64().unwrap();
    let ry = residual["max_jitter_y"].as_f64().unwrap();
    assert!(rx < 0.5 && ry < 0.5, "residual jitter {rx}, {ry}");
}
