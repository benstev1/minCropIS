//! Web front end: upload a video, follow its progress, compare the result side
//! by side with the original, rerun with other settings, and download. Served by
//! the `minis-web` binary and inside the desktop app. Jobs run one at a time.
//!
//! Files only exist while they're in use: an upload's folder (the original, its
//! preview copy and the latest result) is deleted when the page asks for it
//! (`DELETE /api/jobs/{id}`) or the server shuts down, and a rerun's result
//! replaces the previous one.

use anyhow::Context;
use axum::{
    Json, Router,
    body::Body,
    extract::{DefaultBodyLimit, Multipart, Path, Request, State},
    http::{HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;
use tower::ServiceExt;
use tower_http::services::ServeFile;

const INDEX: &str = include_str!("../web/index.html");

#[derive(Clone, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
enum Status {
    Queued,
    Analyzing {
        progress: f64,
    },
    Rendering {
        progress: f64,
    },
    Done {
        summary: Summary,
        settings: crate::Settings,
        /// Whether `/original` can serve the original for side-by-side viewing.
        original: bool,
    },
    Failed {
        error: String,
    },
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
    fn new(a: &crate::Analysis) -> Self {
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

/// An uploaded video. Several jobs (the first run and any reruns with other
/// settings) can share one upload.
struct Upload {
    /// Folder holding everything belonging to this upload.
    dir: PathBuf,
    input: PathBuf,
    original_name: String,
    /// Browser-playable copy of the original, made alongside the first job.
    original_preview: PathBuf,
    /// Camera motion measured by the first job. It doesn't depend on the settings,
    /// so reruns skip straight to rendering.
    motion: Mutex<Option<Arc<crate::Motion>>>,
    /// Set when the upload is deleted: running work stops, and whichever job
    /// finishes last removes the folder.
    removed: AtomicBool,
}

impl Upload {
    fn is_removed(&self) -> bool {
        self.removed.load(Ordering::SeqCst)
    }

    /// Deletes the upload's folder. Safe to call more than once.
    async fn delete_files(&self) {
        let _ = tokio::fs::remove_dir_all(&self.dir).await;
    }
}

struct Job {
    status: Status,
    upload: Arc<Upload>,
    output: PathBuf,
    /// For a rerun, the job whose result this one replaces once it succeeds.
    replaces: Option<String>,
}

/// Deletes a folder when dropped, unless disarmed: cleans up after an upload
/// that fails or is abandoned partway.
struct DirGuard(Option<PathBuf>);

impl Drop for DirGuard {
    fn drop(&mut self) {
        if let Some(dir) = self.0.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
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
    fn new_id(&self) -> String {
        format!(
            "{:x}{:04x}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis()),
            self.next_id.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn set_status(&self, id: &str, status: Status) {
        if let Some(job) = self.jobs.lock().unwrap().get_mut(id) {
            job.status = status;
        }
    }
}

/// Serves the web app on `listener`, keeping uploads and results under `data_dir`,
/// until `shutdown` completes. Then it stops any running job and deletes every
/// upload it created before returning.
pub async fn serve(
    listener: tokio::net::TcpListener,
    data_dir: PathBuf,
    shutdown: impl Future<Output = ()>,
) -> anyhow::Result<()> {
    tokio::fs::create_dir_all(&data_dir)
        .await
        .with_context(|| format!("creating {}", data_dir.display()))?;
    let app = Arc::new(App {
        jobs: Mutex::new(HashMap::new()),
        data_dir,
        worker: Semaphore::new(1),
        next_id: AtomicU64::new(1),
    });
    let router = Router::new()
        .route("/", get(|| async { Html(INDEX) }))
        .route("/api/jobs", post(create_job))
        .route("/api/jobs/{id}", get(job_status).delete(delete_job))
        .route("/api/jobs/{id}/rerun", post(rerun_job))
        .route("/api/jobs/{id}/video", get(job_video))
        .route("/api/jobs/{id}/original", get(job_original))
        .layer(DefaultBodyLimit::disable())
        .with_state(app.clone());
    // Stop serving outright on shutdown: open video streams would hold up a
    // graceful shutdown.
    let served = tokio::select! {
        result = axum::serve(listener, router).into_future() => result,
        _ = shutdown => Ok(()),
    };
    clean_up(&app).await;
    Ok(served?)
}

/// Deletes every upload: stops the running job (if any) and waits for it, then
/// removes the remaining folders.
async fn clean_up(app: &App) {
    let uploads: Vec<Arc<Upload>> = app
        .jobs
        .lock()
        .unwrap()
        .drain()
        .map(|(_, job)| job.upload)
        .collect();
    for upload in &uploads {
        upload.removed.store(true, Ordering::SeqCst);
    }
    // Jobs hold the permit while running and while deleting their files, and
    // the semaphore is fair, so once we get it every earlier job is done.
    let _permit = app.worker.acquire().await;
    for upload in &uploads {
        upload.delete_files().await;
    }
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

fn default_settings() -> crate::Settings {
    crate::Settings {
        threshold_percent: 2.0,
        intent_frames: 5,
        recenter: 0.02,
    }
}

/// Accepts a multipart form with optional `threshold` and `intent_frames` fields
/// followed by the `file` field, and starts a job.
async fn create_job(State(app): State<Shared>, mut form: Multipart) -> Response {
    let dir = app.data_dir.join(app.new_id());
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return server_error(e);
    }
    // Removes the folder on any early return, or if the client goes away mid-upload.
    let mut guard = DirGuard(Some(dir.clone()));

    let mut settings = default_settings();
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

    guard.0 = None;
    let upload = Arc::new(Upload {
        input,
        original_preview: dir.join("original.mp4"),
        original_name: original,
        motion: Mutex::new(None),
        removed: AtomicBool::new(false),
        dir,
    });
    let id = start_job(&app, upload, settings, None);
    Json(serde_json::json!({ "id": id })).into_response()
}

#[derive(Deserialize)]
struct Rerun {
    threshold: f64,
    intent_frames: usize,
}

/// Starts a new job on the same upload as job `id`, with different settings.
async fn rerun_job(
    State(app): State<Shared>,
    Path(id): Path<String>,
    Json(rerun): Json<Rerun>,
) -> Response {
    let Some(upload) = app.jobs.lock().unwrap().get(&id).map(|j| j.upload.clone()) else {
        return (StatusCode::NOT_FOUND, "no such job").into_response();
    };
    let settings = crate::Settings {
        threshold_percent: rerun.threshold,
        intent_frames: rerun.intent_frames,
        ..default_settings()
    };
    if let Err(e) = settings.validate() {
        return bad_request(e.to_string());
    }
    let id = start_job(&app, upload, settings, Some(id));
    Json(serde_json::json!({ "id": id })).into_response()
}

/// Deletes job `id`'s upload: the original, its preview and every result made
/// from it. Running work on it is stopped first.
async fn delete_job(State(app): State<Shared>, Path(id): Path<String>) -> Response {
    let upload = {
        let mut jobs = app.jobs.lock().unwrap();
        let Some(upload) = jobs.get(&id).map(|j| j.upload.clone()) else {
            return (StatusCode::NOT_FOUND, "no such job").into_response();
        };
        jobs.retain(|_, job| !Arc::ptr_eq(&job.upload, &upload));
        upload
    };
    upload.removed.store(true, Ordering::SeqCst);
    // If a job is still using the files, they can't all be deleted yet (Windows
    // won't delete open files); that job deletes the rest when it stops.
    upload.delete_files().await;
    StatusCode::NO_CONTENT.into_response()
}

/// Queues a job and returns its id. Its output sits next to the upload.
fn start_job(
    app: &Shared,
    upload: Arc<Upload>,
    settings: crate::Settings,
    replaces: Option<String>,
) -> String {
    let id = app.new_id();
    let output = upload.dir.join(format!("{id}.mp4"));
    app.jobs.lock().unwrap().insert(
        id.clone(),
        Job {
            status: Status::Queued,
            upload: upload.clone(),
            output: output.clone(),
            replaces,
        },
    );

    let (app, job_id) = (app.clone(), id.clone());
    tokio::spawn(async move {
        let _permit = app.worker.acquire().await;
        if upload.is_removed() {
            upload.delete_files().await;
            return;
        }
        let (app_job, id_job, upload_job) = (app.clone(), job_id.clone(), upload.clone());
        let result = tokio::task::spawn_blocking(move || {
            run_job(&app_job, &id_job, &upload_job, &output, settings)
        })
        .await;
        if upload.is_removed() {
            // Deleted while running: the job has stopped, so its files can go now.
            upload.delete_files().await;
            return;
        }
        let status = match result {
            Ok(Ok(summary)) => Status::Done {
                summary,
                settings,
                original: upload.original_preview.exists(),
            },
            Ok(Err(e)) => Status::Failed {
                error: format!("{e:#}"),
            },
            Err(e) => Status::Failed {
                error: format!("processing crashed: {e}"),
            },
        };
        let succeeded = matches!(status, Status::Done { .. });
        let replaced = {
            let mut jobs = app.jobs.lock().unwrap();
            let replaces = jobs.get_mut(&job_id).and_then(|job| {
                job.status = status;
                job.replaces.take()
            });
            // Only the latest result is kept: drop the one this rerun replaces.
            replaces
                .filter(|_| succeeded)
                .and_then(|old| jobs.remove(&old))
        };
        if let Some(old) = replaced {
            let _ = tokio::fs::remove_file(&old.output).await;
        }
    });
    id
}

fn run_job(
    app: &App,
    id: &str,
    upload: &Upload,
    output: &std::path::Path,
    settings: crate::Settings,
) -> anyhow::Result<Summary> {
    let input = &upload.input;
    let keep_going = || {
        if upload.is_removed() {
            anyhow::bail!("cancelled");
        }
        Ok(())
    };
    let total = crate::video::probe(input)
        .map_err(|_| anyhow::anyhow!("That file isn't a video ffmpeg can read."))?
        .frame_count;

    // Make the side-by-side copy of the original in parallel with stabilizing.
    // Write to a temporary name so a half-written file is never served.
    let preview = PreviewTask::start(upload);

    let cached = upload.motion.lock().unwrap().clone();
    let motion = match cached {
        Some(motion) => motion,
        None => {
            app.set_status(id, Status::Analyzing { progress: 0.0 });
            let motion = Arc::new(crate::measure(input, |done| {
                // The frame count is an estimate, so hold below 100% until measuring ends.
                let progress = total.map_or(0.0, |t| (done as f64 / t as f64).min(0.99));
                app.set_status(id, Status::Analyzing { progress });
                keep_going()
            })?);
            *upload.motion.lock().unwrap() = Some(motion.clone());
            motion
        }
    };
    let analysis = crate::plan(&motion, settings);

    let frames = analysis.frames.len().max(1);
    app.set_status(id, Status::Rendering { progress: 0.0 });
    crate::render(input, output, &analysis, |done| {
        let progress = done as f64 / frames as f64;
        app.set_status(id, Status::Rendering { progress });
        keep_going()
    })?;

    // A missing side-by-side view shouldn't fail the job; the page shows the result alone.
    if let Some(Err(e)) = preview.and_then(PreviewTask::wait) {
        eprintln!("job {id}: original preview failed: {e:#}");
    }
    Ok(Summary::new(&analysis))
}

/// The side-by-side copy of the original being made on a background thread. If
/// the job stops early (an error, or the upload was deleted), dropping it stops
/// ffmpeg and waits, so nothing is left writing into the upload's folder.
struct PreviewTask {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<anyhow::Result<()>>>,
}

impl PreviewTask {
    /// Starts making the preview, unless an earlier job already made it.
    fn start(upload: &Upload) -> Option<Self> {
        if upload.original_preview.exists() {
            return None;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (input, path, flag) = (
            upload.input.clone(),
            upload.original_preview.clone(),
            stop.clone(),
        );
        let handle = std::thread::spawn(move || {
            let partial = path.with_file_name("original.partial.mp4");
            crate::video::browser_preview(&input, &partial, || flag.load(Ordering::SeqCst))?;
            std::fs::rename(&partial, &path)?;
            anyhow::Ok(())
        });
        Some(Self {
            stop,
            handle: Some(handle),
        })
    }

    /// Waits for the preview to finish; `None` if its thread panicked.
    fn wait(mut self) -> Option<anyhow::Result<()>> {
        self.handle.take()?.join().ok()
    }
}

impl Drop for PreviewTask {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            self.stop.store(true, Ordering::SeqCst);
            let _ = handle.join();
        }
    }
}

async fn job_status(State(app): State<Shared>, Path(id): Path<String>) -> Response {
    match app.jobs.lock().unwrap().get(&id) {
        Some(job) => Json(job.status.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, "no such job").into_response(),
    }
}

/// The stabilized video, inline so the page can preview it; the page's download
/// link names the file.
async fn job_video(
    State(app): State<Shared>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let file = match app.jobs.lock().unwrap().get(&id) {
        Some(
            job @ Job {
                status: Status::Done { .. },
                ..
            },
        ) => (job.output.clone(), download_name(&job.upload.original_name)),
        Some(_) => return (StatusCode::CONFLICT, "not finished yet").into_response(),
        None => return (StatusCode::NOT_FOUND, "no such job").into_response(),
    };
    serve_video(file.0, &file.1, request).await
}

/// The browser-playable copy of the job's original upload.
async fn job_original(
    State(app): State<Shared>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    let path = match app.jobs.lock().unwrap().get(&id) {
        Some(job) if job.upload.original_preview.exists() => job.upload.original_preview.clone(),
        Some(_) => return (StatusCode::NOT_FOUND, "no original preview").into_response(),
        None => return (StatusCode::NOT_FOUND, "no such job").into_response(),
    };
    serve_video(path, "original.mp4", request).await
}

/// Serves an MP4 with HTTP range support, so players can stream and seek.
async fn serve_video(path: PathBuf, name: &str, request: Request) -> Response {
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
