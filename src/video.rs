//! Video I/O through `ffprobe` / `ffmpeg` subprocesses using raw frame pipes.

use anyhow::{Context, Result, bail};
use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub width: usize,
    pub height: usize,
    /// Frame rate as ffmpeg reports it, e.g. `30000/1001`.
    pub frame_rate: String,
    /// Frame count from the container, or estimated from duration × frame rate.
    /// Only used for progress reporting.
    pub frame_count: Option<usize>,
}

pub fn probe(path: &Path) -> Result<VideoInfo> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_streams"])
        .args(["-of", "json"])
        .arg(path)
        .output()
        .context("failed to run ffprobe (is ffmpeg installed?)")?;
    if !out.status.success() {
        bail!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("bad JSON from ffprobe")?;
    let Some(stream) = json["streams"].get(0) else {
        bail!("no video stream found in {}", path.display());
    };
    let dimension = |key: &str| -> Result<usize> {
        Ok(stream[key]
            .as_u64()
            .with_context(|| format!("bad {key} from ffprobe"))? as usize)
    };
    let (width, height) = (dimension("width")?, dimension("height")?);
    // ffmpeg applies rotation metadata when decoding, so frames arrive upright:
    // a quarter turn swaps the dimensions we decode at.
    let (width, height) = if rotation(stream).rem_euclid(180) == 90 {
        (height, width)
    } else {
        (width, height)
    };
    let frame_rate = stream["r_frame_rate"]
        .as_str()
        .context("bad frame rate from ffprobe")?
        .to_string();
    let number = |key: &str| stream[key].as_str().and_then(|v| v.parse::<f64>().ok());
    let fps = frame_rate
        .split_once('/')
        .and_then(|(n, d)| Some(n.parse::<f64>().ok()? / d.parse::<f64>().ok()?));
    let frame_count = number("nb_frames")
        .or_else(|| Some(number("duration")? * fps?))
        .map(|n| n.round() as usize)
        .filter(|&n| n > 0);
    Ok(VideoInfo {
        width,
        height,
        frame_rate,
        frame_count,
    })
}

/// Display rotation in degrees, from the legacy `rotate` tag or the display matrix
/// side data used by newer ffmpeg versions.
fn rotation(stream: &serde_json::Value) -> i64 {
    let tag = stream["tags"]["rotate"]
        .as_str()
        .and_then(|r| r.parse::<i64>().ok());
    let side_data = stream["side_data_list"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|d| d["rotation"].as_i64());
    tag.or(side_data).unwrap_or(0)
}

#[derive(Clone, Copy)]
pub enum PixelFormat {
    Gray,
    Rgb24,
}

impl PixelFormat {
    fn ffmpeg_name(self) -> &'static str {
        match self {
            PixelFormat::Gray => "gray",
            PixelFormat::Rgb24 => "rgb24",
        }
    }

    pub fn channels(self) -> usize {
        match self {
            PixelFormat::Gray => 1,
            PixelFormat::Rgb24 => 3,
        }
    }
}

/// Streams decoded frames out of ffmpeg, optionally scaled to `width` x `height`.
pub struct Decoder {
    child: Child,
    stdout: BufReader<ChildStdout>,
    frame_bytes: usize,
}

impl Decoder {
    pub fn open(path: &Path, format: PixelFormat, width: usize, height: usize) -> Result<Self> {
        let mut child = Command::new("ffmpeg")
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(path)
            .args(["-map", "0:v:0", "-vf"])
            .arg(format!("scale={width}:{height}:flags=area"))
            .args(["-f", "rawvideo", "-pix_fmt", format.ffmpeg_name(), "-"])
            .stdout(Stdio::piped())
            .spawn()
            .context("failed to start ffmpeg decoder")?;
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Ok(Self {
            child,
            stdout,
            frame_bytes: width * height * format.channels(),
        })
    }

    /// Reads the next frame into `buf`; returns false at end of stream.
    pub fn read_frame(&mut self, buf: &mut Vec<u8>) -> Result<bool> {
        buf.resize(self.frame_bytes, 0);
        match self.stdout.read_exact(buf) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub fn finish(mut self) -> Result<()> {
        drop(self.stdout);
        let status = self.child.wait()?;
        if !status.success() {
            bail!("ffmpeg decoder exited with {status}");
        }
        Ok(())
    }
}

/// Encodes raw RGB frames to H.264, copying audio from `audio_source` when present.
pub struct Encoder {
    child: Child,
    stdin: BufWriter<ChildStdin>,
}

impl Encoder {
    pub fn create(
        path: &Path,
        width: usize,
        height: usize,
        frame_rate: &str,
        audio_source: &Path,
    ) -> Result<Self> {
        let mut child = Command::new("ffmpeg")
            .args(["-v", "error", "-y", "-f", "rawvideo", "-pix_fmt", "rgb24"])
            .args([
                "-s",
                &format!("{width}x{height}"),
                "-r",
                frame_rate,
                "-i",
                "-",
            ])
            .arg("-i")
            .arg(audio_source)
            .args(["-map", "0:v:0", "-map", "1:a?", "-c:a", "copy"])
            .args([
                "-c:v",
                "libx264",
                "-crf",
                "18",
                "-pix_fmt",
                "yuv420p",
                "-shortest",
                // Put the index at the front so players can start before the whole file arrives.
                "-movflags",
                "+faststart",
            ])
            .arg(path)
            .stdin(Stdio::piped())
            .spawn()
            .context("failed to start ffmpeg encoder")?;
        let stdin = BufWriter::new(child.stdin.take().unwrap());
        Ok(Self { child, stdin })
    }

    pub fn write_frame(&mut self, rgb: &[u8]) -> Result<()> {
        self.stdin
            .write_all(rgb)
            .context("ffmpeg encoder closed its input")
    }

    pub fn finish(self) -> Result<()> {
        let Self { mut child, stdin } = self;
        stdin.into_inner().map_err(|e| e.into_error())?;
        let status = child.wait()?;
        if !status.success() {
            bail!("ffmpeg encoder exited with {status}");
        }
        Ok(())
    }
}

/// Writes a browser-playable copy of `input`: H.264 with the index first, upright,
/// no audio. Used to show the original next to the result, since uploads may be
/// in formats browsers can't play (e.g. HEVC .MOV).
pub fn browser_preview(input: &Path, output: &Path) -> Result<()> {
    let out = Command::new("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(input)
        .args([
            "-map",
            "0:v:0",
            "-an",
            "-vf",
            "scale=trunc(iw/2)*2:trunc(ih/2)*2",
        ])
        .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20"])
        .args(["-pix_fmt", "yuv420p", "-movflags", "+faststart"])
        .arg(output)
        .output()
        .context("failed to start ffmpeg")?;
    if !out.status.success() {
        bail!(
            "ffmpeg preview failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}
