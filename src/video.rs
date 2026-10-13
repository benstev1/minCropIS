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
}

pub fn probe(path: &Path) -> Result<VideoInfo> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0"])
        .args(["-show_entries", "stream=width,height,r_frame_rate"])
        .args(["-of", "csv=p=0"])
        .arg(path)
        .output()
        .context("failed to run ffprobe (is ffmpeg installed?)")?;
    if !out.status.success() {
        bail!(
            "ffprobe failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let fields: Vec<&str> = text.trim().split(',').collect();
    if fields.len() < 3 {
        bail!("no video stream found in {}", path.display());
    }
    Ok(VideoInfo {
        width: fields[0].parse().context("bad width from ffprobe")?,
        height: fields[1].parse().context("bad height from ffprobe")?,
        frame_rate: fields[2].to_string(),
    })
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
