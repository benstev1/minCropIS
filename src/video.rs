//! Video I/O through `ffprobe` / `ffmpeg` subprocesses using raw frame pipes.

use anyhow::{Context, Result, bail};
use std::io::{BufReader, BufWriter, ErrorKind, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::OnceLock;
use std::thread::JoinHandle;

static TOOL_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Use `ffmpeg` / `ffprobe` from `dir` instead of looking them up on `PATH`,
/// e.g. the copies bundled with the desktop app. Only the first call has effect.
pub fn set_tool_dir(dir: PathBuf) {
    let _ = TOOL_DIR.set(dir);
}

/// ffmpeg's option for passing every decoded frame through exactly once:
/// `-fps_mode` since ffmpeg 5.1, `-vsync` before that (removed in later versions).
/// Detected once from the ffmpeg in use.
fn passthrough_frames() -> [&'static str; 2] {
    static OPTION: OnceLock<&'static str> = OnceLock::new();
    let option = OPTION.get_or_init(|| {
        let help = tool("ffmpeg").args(["-hide_banner", "-h", "full"]).output();
        match help {
            Ok(out) if String::from_utf8_lossy(&out.stdout).contains("-fps_mode") => "-fps_mode",
            _ => "-vsync",
        }
    });
    [option, "passthrough"]
}

/// Collects a child's stderr on a background thread, so a failure can report
/// ffmpeg's own message (a desktop app has no console to show it on).
fn collect_stderr(child: &mut Child) -> Option<JoinHandle<String>> {
    let mut stderr = child.stderr.take()?;
    Some(std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = stderr.read_to_end(&mut bytes);
        String::from_utf8_lossy(&bytes).trim().to_string()
    }))
}

/// Waits for an ffmpeg child and turns a failure into an error with its message.
fn wait_for(child: &mut Child, stderr: Option<JoinHandle<String>>, what: &str) -> Result<()> {
    let status = child.wait()?;
    let message = stderr.and_then(|h| h.join().ok()).unwrap_or_default();
    if !status.success() {
        if message.is_empty() {
            bail!("ffmpeg {what} failed ({status})");
        }
        bail!("ffmpeg {what} failed: {message}");
    }
    Ok(())
}

/// Kills and reaps an ffmpeg child that is being abandoned (e.g. a cancelled job),
/// so it stops writing and releases its files.
fn stop(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

/// A command for an ffmpeg tool. On Windows it runs without opening a console window.
fn tool(name: &str) -> Command {
    let program = match TOOL_DIR.get() {
        Some(dir) => dir
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX))
            .into_os_string(),
        None => name.into(),
    };
    #[allow(unused_mut)]
    let mut command = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

#[derive(Debug, Clone)]
pub struct VideoInfo {
    pub width: usize,
    pub height: usize,
    /// Average frame rate as ffmpeg reports it, e.g. `30000/1001`. Output is
    /// written at this constant rate, so it lasts as long as the input even when
    /// the input has a variable frame rate (common for phone footage).
    pub frame_rate: String,
    /// Frame count from the container, or estimated from duration × frame rate.
    /// Only used for progress reporting.
    pub frame_count: Option<usize>,
}

pub fn probe(path: &Path) -> Result<VideoInfo> {
    let out = tool("ffprobe")
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
    let rate = |key: &str| {
        let text = stream[key].as_str()?;
        let (n, d) = text.split_once('/')?;
        let fps = n.parse::<f64>().ok()? / d.parse::<f64>().ok()?;
        (fps.is_finite() && fps > 0.0).then(|| (text.to_string(), fps))
    };
    // avg_frame_rate is 0/0 when unknown; r_frame_rate is the nominal rate.
    let (frame_rate, fps) = rate("avg_frame_rate")
        .or_else(|| rate("r_frame_rate"))
        .context("bad frame rate from ffprobe")?;
    let number = |key: &str| stream[key].as_str().and_then(|v| v.parse::<f64>().ok());
    let frame_count = number("nb_frames")
        .or_else(|| Some(number("duration")? * fps))
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
/// Dropping it before `finish` (e.g. on cancellation) stops ffmpeg.
pub struct Decoder {
    child: Child,
    stderr: Option<JoinHandle<String>>,
    stdout: Option<BufReader<ChildStdout>>,
    frame_bytes: usize,
    finished: bool,
}

impl Decoder {
    pub fn open(path: &Path, format: PixelFormat, width: usize, height: usize) -> Result<Self> {
        let mut child = tool("ffmpeg")
            .args(["-v", "error", "-nostdin", "-i"])
            .arg(path)
            // Every decoded frame exactly once: no duplicates or drops for
            // variable-frame-rate input, so passes and previews line up.
            .args(["-map", "0:v:0"])
            .args(passthrough_frames())
            .arg("-vf")
            .arg(format!("scale={width}:{height}:flags=area"))
            .args(["-f", "rawvideo", "-pix_fmt", format.ffmpeg_name(), "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to start ffmpeg decoder")?;
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let stderr = collect_stderr(&mut child);
        Ok(Self {
            child,
            stderr,
            stdout: Some(stdout),
            frame_bytes: width * height * format.channels(),
            finished: false,
        })
    }

    /// Reads the next frame into `buf`; returns false at end of stream.
    pub fn read_frame(&mut self, buf: &mut Vec<u8>) -> Result<bool> {
        buf.resize(self.frame_bytes, 0);
        let stdout = self.stdout.as_mut().expect("decoder already finished");
        match stdout.read_exact(buf) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == ErrorKind::UnexpectedEof => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    pub fn finish(mut self) -> Result<()> {
        self.stdout = None;
        self.finished = true;
        wait_for(&mut self.child, self.stderr.take(), "decoder")
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        if !self.finished {
            self.stdout = None;
            stop(&mut self.child);
        }
    }
}

/// Encodes raw RGB frames to H.264, copying audio from `audio_source` when present.
/// Dropping it before `finish` (e.g. on cancellation) stops ffmpeg.
pub struct Encoder {
    child: Child,
    stderr: Option<JoinHandle<String>>,
    stdin: Option<BufWriter<ChildStdin>>,
    finished: bool,
}

impl Encoder {
    pub fn create(
        path: &Path,
        width: usize,
        height: usize,
        frame_rate: &str,
        audio_source: &Path,
    ) -> Result<Self> {
        let mut child = tool("ffmpeg")
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
                // Put the index at the front so players can start before the whole file arrives.
                "-movflags",
                "+faststart",
            ])
            .arg(path)
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("failed to start ffmpeg encoder")?;
        let stdin = BufWriter::new(child.stdin.take().unwrap());
        let stderr = collect_stderr(&mut child);
        Ok(Self {
            child,
            stderr,
            stdin: Some(stdin),
            finished: false,
        })
    }

    pub fn write_frame(&mut self, rgb: &[u8]) -> Result<()> {
        self.stdin
            .as_mut()
            .expect("encoder already finished")
            .write_all(rgb)
            .context("ffmpeg encoder closed its input")
    }

    pub fn finish(mut self) -> Result<()> {
        self.finished = true;
        let stdin = self.stdin.take().expect("encoder already finished");
        if let Err(e) = stdin.into_inner() {
            stop(&mut self.child);
            return Err(e.into_error().into());
        }
        wait_for(&mut self.child, self.stderr.take(), "encoder")
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if !self.finished {
            self.stdin = None;
            stop(&mut self.child);
        }
    }
}

/// Writes a browser-playable copy of `input`: H.264 with the index first, upright,
/// no audio. Used to show the original next to the result, since uploads may be
/// in formats browsers can't play (e.g. HEVC .MOV).
///
/// `should_stop` is polled while it runs; when it returns true, ffmpeg is
/// stopped and an error returned.
pub fn browser_preview(input: &Path, output: &Path, should_stop: impl Fn() -> bool) -> Result<()> {
    let mut child = tool("ffmpeg")
        .args(["-v", "error", "-nostdin", "-y", "-i"])
        .arg(input)
        .args([
            "-map",
            "0:v:0",
            "-an",
            "-vf",
            "scale=trunc(iw/2)*2:trunc(ih/2)*2",
        ])
        .args(passthrough_frames())
        .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "20"])
        .args(["-pix_fmt", "yuv420p", "-movflags", "+faststart"])
        .arg(output)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("failed to start ffmpeg")?;
    let stderr = collect_stderr(&mut child);
    loop {
        if child.try_wait()?.is_some() {
            return wait_for(&mut child, stderr, "preview");
        }
        if should_stop() {
            stop(&mut child);
            bail!("cancelled");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
