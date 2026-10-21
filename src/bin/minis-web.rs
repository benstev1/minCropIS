//! Local web front end: upload a video, follow its progress, then preview and
//! download the stabilized result. Jobs run one at a time in the background.

use anyhow::Context;
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use clap::Parser;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tower::ServiceExt;
use tower_http::services::ServeFile;

const INDEX: &str = include_str!("../../web/index.html");

/// Serve the minIS web app.
#[derive(Parser)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8765")]
    addr: String,

    /// Directory for uploads and results (default: a `minis-web` folder in the system temp dir).
    #[arg(long)]
    data_dir: Option<PathBuf>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
enum Status {
    Queued,
    Analyzing { progress: f64 },
    Rendering { progress: f64 },
    Done { summary: Summary },
    Failed { error: String },
}

#[derive(Clone, Serialize)]
struct Summary {
    frames: usize,
    width: usize,
    height: usize,
    output_width: usize,
    output_height: usize,
    crop_percent_x: f64,
    crop_percent_y: f64,
    max_jitter_x: f64,
    max_jitter_y: f64,
    threshold_x: f64,
    threshold_y: f64,
    intentional_frames_x: usize,
    intentional_frames_y: usize,
    unreliable_frames: usize,
}

impl Summary {
    fn new(a: &minis::Analysis) -> Self {
        let pct = |kept: usize, size: usize| 100.0 * (size - kept) as f64 / size as f64;
        Self {
            frames: a.frames.len(),
            width: a.width,
            height: a.height,
            output_width: a.crop.width,
            output_height: a.crop.height,
            crop_percent_x: pct(a.crop.width, a.width),
            crop_percent_y: pct(a.crop.height, a.height),
            max_jitter_x: a.max_jitter_x,
            max_jitter_y: a.max_jitter_y,
            threshold_x: a.threshold_x,
            threshold_y: a.threshold_y,
            intentional_frames_x: a.counts_x.intentional,
            intentional_frames_y: a.counts_y.intentional,
            unreliable_frames: a.unreliable_frames,
        }
    }
}

struct Job {
    status: Status,
    /// Name offered when downloading the result.
    download_name: String,
    output: PathBuf,
}

struct App {
    jobs: Mutex<HashMap<String, Job>>,
    data_dir: PathBuf,
    /// Stabilizing is CPU- and ffmpeg-heavy, so jobs run one at a time.
    worker: Semaphore,
    next_id: AtomicU64,
}

type Shared = Arc<App>;

impl App {
    fn set_status(&self, id: &str, status: Status) {
        if let Some(job) = self.jobs.lock().unwrap().get_mut(id) {
            job.status = status;
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let data_dir = args
        .data_dir
        .unwrap_or_else(|| std::env::temp_dir().join("minis-web"));
    tokio::fs::create_dir_all(&data_dir)
        .await
        .with_context(|| format!("creating {}", data_dir.display()))?;

    let app = Arc::new(App {
        jobs: Mutex::new(HashMap::new()),
        data_dir: data_dir.clone(),
        worker: Semaphore::new(1),
        next_id: AtomicU64::new(1),
    });
    let router = Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/api/jobs", post(create_job))
        .route("/api/jobs/{id}", get(job_status))
        .route("/api/jobs/{id}/video", get(job_video))
        .layer(DefaultBodyLimit::disable())
        .with_state(app);

    let listener = tokio::net::TcpListener::bind(&args.addr)
        .await
        .with_context(|| format!("binding {}", args.addr))?;
    println!("minIS web app on http://{}", listener.local_addr()?);
    println!("files are kept in {}", data_dir.display());
    axum::serve(listener, router).await?;
    Ok(())
}

fn bad_request(message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": message.into() })),
    )
        .into_response()
}

fn server_error(error: impl std::fmt::Display) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
        .into_response()
}

/// Accepts a multipart form with optional `threshold` and `intent_frames` fields
/// followed by the `file` field, and starts a job.
async fn create_job(State(app): State<Shared>, mut form: Multipart) -> Response {
    let id = format!(
        "{:x}{:04x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis()),
        app.next_id.fetch_add(1, Ordering::Relaxed)
    );
    let dir = app.data_dir.join(&id);
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return server_error(e);
    }

    let mut settings = minis::Settings {
        threshold_percent: 2.0,
        intent_frames: 5,
        recenter: 0.02,
    };
    let mut upload = None;
    loop {
        let mut field = match form.next_field().await {
            Ok(Some(field)) => field,
            Ok(None) => break,
            Err(e) => return bad_request(format!("upload failed: {e}")),
        };
        match field.name() {
            Some("threshold") | Some("intent_frames") => {
                let name = field.name().unwrap_or_default().to_string();
                let text = field.text().await.unwrap_or_default();
                let parsed = match name.as_str() {
                    "threshold" => text
                        .trim()
                        .parse()
                        .map(|v| settings.threshold_percent = v)
                        .is_ok(),
                    _ => text
                        .trim()
                        .parse()
                        .map(|v| settings.intent_frames = v)
                        .is_ok(),
                };
                if !parsed {
                    return bad_request(format!("invalid {name}: {text:?}"));
                }
            }
            Some("file") => {
                let original = field.file_name().unwrap_or("video.mp4").to_string();
                let path = dir.join(format!("input.{}", safe_extension(&original)));
                let mut file = match tokio::fs::File::create(&path).await {
                    Ok(file) => file,
                    Err(e) => return server_error(e),
                };
                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => {
                            if let Err(e) = file.write_all(&chunk).await {
                                return server_error(e);
                            }
                        }
                        Ok(None) => break,
                        Err(e) => return bad_request(format!("upload failed: {e}")),
                    }
                }
                if let Err(e) = file.flush().await {
                    return server_error(e);
                }
                upload = Some((path, original));
            }
            _ => {}
        }
    }
    let Some((input, original)) = upload else {
        return bad_request("no video file in the upload");
    };
    if let Err(e) = settings.validate() {
        return bad_request(e.to_string());
    }

    let output = dir.join("stabilized.mp4");
    app.jobs.lock().unwrap().insert(
        id.clone(),
        Job {
            status: Status::Queued,
            download_name: download_name(&original),
            output: output.clone(),
        },
    );

    let (app_bg, id_bg) = (app.clone(), id.clone());
    tokio::spawn(async move {
        let _permit = app_bg.worker.acquire().await;
        let (app_job, id_job) = (app_bg.clone(), id_bg.clone());
        let result = tokio::task::spawn_blocking(move || {
            run_job(&app_job, &id_job, &input, &output, settings)
        })
        .await;
        let status = match result {
            Ok(Ok(summary)) => Status::Done { summary },
            Ok(Err(e)) => Status::Failed {
                error: format!("{e:#}"),
            },
            Err(e) => Status::Failed {
                error: format!("processing crashed: {e}"),
            },
        };
        app_bg.set_status(&id_bg, status);
    });

    Json(serde_json::json!({ "id": id })).into_response()
}

fn run_job(
    app: &App,
    id: &str,
    input: &std::path::Path,
    output: &std::path::Path,
    settings: minis::Settings,
) -> anyhow::Result<Summary> {
    let total = minis::video::probe(input)
        .map_err(|_| anyhow::anyhow!("That file isn't a video ffmpeg can read."))?
        .frame_count;
    app.set_status(id, Status::Analyzing { progress: 0.0 });
    let analysis = minis::analyze(input, settings, |done| {
        // The frame count is an estimate, so hold below 100% until analysis ends.
        let progress = total.map_or(0.0, |t| (done as f64 / t as f64).min(0.99));
        app.set_status(id, Status::Analyzing { progress });
    })?;

    let frames = analysis.frames.len().max(1);
    app.set_status(id, Status::Rendering { progress: 0.0 });
    minis::render(input, output, &analysis, |done| {
        let progress = done as f64 / frames as f64;
        app.set_status(id, Status::Rendering { progress });
    })?;
    Ok(Summary::new(&analysis))
}

async fn job_status(State(app): State<Shared>, Path(id): Path<String>) -> Response {
    match app.jobs.lock().unwrap().get(&id) {
        Some(job) => Json(job.status.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, "no such job").into_response(),
    }
}

/// The stabilized video, inline so the page can preview it; the page's download
/// link names the file. Served with HTTP range support so the player can stream and seek.
async fn job_video(
    State(app): State<Shared>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let (path, name) = match app.jobs.lock().unwrap().get(&id) {
        Some(Job {
            status: Status::Done { .. },
            output,
            download_name,
        }) => (output.clone(), download_name.clone()),
        Some(_) => return (StatusCode::CONFLICT, "not finished yet").into_response(),
        None => return (StatusCode::NOT_FOUND, "no such job").into_response(),
    };
    let Ok(mut response) = ServeFile::new(path).oneshot(request).await;
    if let Ok(value) = HeaderValue::from_str(&format!("inline; filename=\"{name}\"")) {
        response
            .headers_mut()
            .insert(header::CONTENT_DISPOSITION, value);
    }
    response.map(Body::new)
}

/// Extension of the uploaded file name, limited to a few alphanumerics so it is
/// safe in a path; ffmpeg detects the actual format from the content.
fn safe_extension(name: &str) -> String {
    let ext: String = name
        .rsplit_once('.')
        .map(|(_, e)| e)
        .unwrap_or("")
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    if ext.is_empty() {
        "video".into()
    } else {
        ext.to_lowercase()
    }
}

/// `<original stem>_stabilized.mp4`, with characters that are unsafe in a header removed.
fn download_name(original: &str) -> String {
    let stem = original.rsplit_once('.').map_or(original, |(s, _)| s);
    let stem: String = stem
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | ' ' | '.'))
        .take(80)
        .collect();
    let stem = stem.trim();
    format!(
        "{}_stabilized.mp4",
        if stem.is_empty() { "video" } else { stem }
    )
}
