//! The HTTP API. Handlers are thin: reads come from the job store or the
//! controller snapshot, and every job transition goes through the controller.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;

use crate::bounds::Bounds;
use crate::controller::{
    AxisValues, ControlError, ControllerHandle, JogRequest, MachineAction, MachineSnapshot,
    ResumeOptions, WorkChange,
};
use crate::settings::{PauseOptions, Settings};
use crate::store::{JobRecord, JobStore};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<JobStore>,
    pub controller: ControllerHandle,
    /// Turns true when the server is shutting down, to end open event streams.
    pub shutdown: watch::Receiver<bool>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/events", get(events))
        .route("/machine", get(machine))
        .route("/machine/unlock", post(unlock))
        .route("/machine/home", post(home))
        .route("/machine/reset", post(reset))
        .route("/machine/work-zero", post(work_zero))
        .route("/machine/position-check", post(position_check))
        .route("/machine/work-offset", put(work_offset))
        .route("/machine/jog", post(jog))
        .route("/machine/jog/cancel", post(jog_cancel))
        .route("/settings", get(get_settings).put(put_settings))
        .route(
            "/machine/bounds",
            get(get_bounds).put(put_bounds).delete(delete_bounds),
        )
        .route("/jobs", get(list_jobs).post(create_job))
        .route("/jobs/{id}", get(get_job).delete(delete_job))
        .route("/jobs/{id}/start", post(start_job))
        .route("/jobs/{id}/pause", post(pause_job))
        .route("/jobs/{id}/resume", post(resume_job))
        .route("/jobs/{id}/stop", post(stop_job))
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
        .with_state(state)
}

pub struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}"))
    }
}

impl From<ControlError> for ApiError {
    fn from(e: ControlError) -> Self {
        match e {
            ControlError::NotFound => not_found(),
            ControlError::Conflict(m) => Self(StatusCode::CONFLICT, m),
            ControlError::Unavailable(m) => Self(StatusCode::SERVICE_UNAVAILABLE, m),
            ControlError::Invalid(m) => Self(StatusCode::UNPROCESSABLE_ENTITY, m),
            ControlError::Internal(e) => e.into(),
        }
    }
}

fn not_found() -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "job not found".into())
}

type ApiResult<T> = Result<T, ApiError>;

/// Reads an optional JSON body. Errors are reported in the same JSON shape
/// as every other error, naming the field that was wrong.
fn parse_json<T: serde::de::DeserializeOwned>(body: &[u8]) -> ApiResult<Option<T>> {
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    serde_json::from_slice(body).map(Some).map_err(|e| {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            format!("invalid body: {e}"),
        )
    })
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

async fn machine(State(app): State<AppState>) -> impl IntoResponse {
    Json(app.controller.snapshot())
}

/// The active job's record is fresher in the controller than on disk.
fn freshest(app: &AppState, record: JobRecord) -> JobRecord {
    match app.controller.snapshot().active_job {
        Some(active) if active.id == record.id => active,
        _ => record,
    }
}

async fn list_jobs(State(app): State<AppState>) -> ApiResult<Json<Vec<JobRecord>>> {
    let jobs = app.store.list()?;
    Ok(Json(jobs.into_iter().map(|j| freshest(&app, j)).collect()))
}

#[derive(Deserialize)]
struct CreateParams {
    name: Option<String>,
}

/// The request body is the G-code program itself.
async fn create_job(
    State(app): State<AppState>,
    Query(params): Query<CreateParams>,
    body: String,
) -> ApiResult<(StatusCode, Json<JobRecord>)> {
    if crate::program::prepare(&body).is_empty() {
        return Err(ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "the program has no lines to send".into(),
        ));
    }
    let store = app.store.clone();
    let record = tokio::task::spawn_blocking(move || store.create(params.name, &body))
        .await
        .map_err(|e| anyhow::anyhow!(e))??;
    Ok((StatusCode::CREATED, Json(record)))
}

async fn get_job(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<Json<JobRecord>> {
    let record = app.store.get(&id)?.ok_or_else(not_found)?;
    Ok(Json(freshest(&app, record)))
}

async fn delete_job(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    app.controller.delete(id).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn start_job(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<JobRecord>)> {
    Ok((StatusCode::ACCEPTED, Json(app.controller.start(id).await?)))
}

async fn pause_job(
    State(app): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<JobRecord>)> {
    let options = parse_json::<PauseOptions>(&body)?.unwrap_or_default();
    Ok((
        StatusCode::ACCEPTED,
        Json(app.controller.pause(id, options).await?),
    ))
}

async fn resume_job(
    State(app): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<JobRecord>)> {
    let options = parse_json::<ResumeOptions>(&body)?.unwrap_or_default();
    Ok((
        StatusCode::ACCEPTED,
        Json(app.controller.resume(id, options).await?),
    ))
}

async fn stop_job(
    State(app): State<AppState>,
    Path(id): Path<String>,
) -> ApiResult<(StatusCode, Json<JobRecord>)> {
    Ok((StatusCode::ACCEPTED, Json(app.controller.stop(id).await?)))
}

async fn machine_action(
    app: AppState,
    action: MachineAction,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    app.controller.machine(action).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "status": "accepted" }))))
}

async fn unlock(State(app): State<AppState>) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    machine_action(app, MachineAction::Unlock).await
}

async fn home(State(app): State<AppState>) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    machine_action(app, MachineAction::Home).await
}

async fn reset(State(app): State<AppState>) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    machine_action(app, MachineAction::Reset).await
}

async fn work_change(
    app: AppState,
    change: WorkChange,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let values = parse_json::<AxisValues>(&body)?.ok_or_else(|| {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "a JSON body is required".into(),
        )
    })?;
    app.controller.work(change, values).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "status": "accepted" }))))
}

#[derive(Deserialize)]
struct PositionAnswer {
    moved: bool,
}

/// Answers a question about whether the machine moved since the tool's
/// position was last recorded.
async fn position_check(
    State(app): State<AppState>,
    Json(answer): Json<PositionAnswer>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    app.controller.position_check(answer.moved).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "status": "accepted" }))))
}

async fn work_zero(
    State(app): State<AppState>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    work_change(app, WorkChange::Zero, body).await
}

async fn work_offset(
    State(app): State<AppState>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    work_change(app, WorkChange::Offset, body).await
}

async fn jog(
    State(app): State<AppState>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let request = parse_json::<JogRequest>(&body)?.ok_or_else(|| {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "a JSON body is required".into(),
        )
    })?;
    app.controller.jog(request).await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "status": "accepted" }))))
}

async fn jog_cancel(
    State(app): State<AppState>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    machine_action(app, MachineAction::JogCancel).await
}

async fn get_settings(State(app): State<AppState>) -> ApiResult<Json<Settings>> {
    Ok(Json(app.store.settings()?))
}

async fn put_settings(State(app): State<AppState>, body: Bytes) -> ApiResult<Json<Settings>> {
    let settings = parse_json::<Settings>(&body)?.ok_or_else(|| {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "a JSON body is required".into(),
        )
    })?;
    settings
        .validate()
        .map_err(|m| ApiError(StatusCode::UNPROCESSABLE_ENTITY, m))?;
    app.store.set_settings(&settings)?;
    Ok(Json(settings))
}

async fn get_bounds(State(app): State<AppState>) -> ApiResult<Json<Bounds>> {
    Ok(Json(app.store.bounds()?.unwrap_or_default()))
}

async fn put_bounds(State(app): State<AppState>, body: Bytes) -> ApiResult<Json<Bounds>> {
    let bounds = parse_json::<Bounds>(&body)?.ok_or_else(|| {
        ApiError(
            StatusCode::UNPROCESSABLE_ENTITY,
            "a JSON body is required".into(),
        )
    })?;
    bounds
        .validate()
        .map_err(|m| ApiError(StatusCode::UNPROCESSABLE_ENTITY, m))?;
    app.store.set_bounds(Some(&bounds))?;
    Ok(Json(bounds))
}

async fn delete_bounds(State(app): State<AppState>) -> ApiResult<StatusCode> {
    app.store.set_bounds(None)?;
    Ok(StatusCode::NO_CONTENT)
}

/// The longest a change waits before it is sent. Changes that arrive within
/// this window are folded into one event carrying the latest state.
const EVENT_INTERVAL: Duration = Duration::from_millis(100);

struct EventFeed {
    snapshot: watch::Receiver<MachineSnapshot>,
    shutdown: watch::Receiver<bool>,
    store: Arc<JobStore>,
    started: bool,
    pending: VecDeque<Event>,
    last_machine: Option<String>,
    last_job: Option<String>,
    /// The job last seen active, whose final record is sent when it ends.
    last_active: Option<String>,
}

impl EventFeed {
    fn push(&mut self, name: &str, data: String) {
        self.pending
            .push_back(Event::default().event(name).data(data));
    }

    /// Queues what changed since the last snapshot this client was sent. A
    /// job's record goes first, so the machine event that follows already
    /// shows its effect.
    fn update(&mut self, snapshot: &MachineSnapshot) {
        match &snapshot.active_job {
            Some(job) => {
                let data = serde_json::to_string(job).unwrap_or_default();
                if self.last_job.as_ref() != Some(&data) {
                    self.push("job", data.clone());
                    self.last_job = Some(data);
                }
                self.last_active = Some(job.id.clone());
            }
            None => {
                // The job ended; its last state is on disk.
                if let Some(id) = self.last_active.take()
                    && let Ok(Some(job)) = self.store.get(&id)
                {
                    let data = serde_json::to_string(&job).unwrap_or_default();
                    self.push("job", data.clone());
                    self.last_job = Some(data);
                }
            }
        }
        let machine = serde_json::to_string(snapshot).unwrap_or_default();
        if self.last_machine.as_ref() != Some(&machine) {
            self.push("machine", machine.clone());
            self.last_machine = Some(machine);
        }
    }

    async fn next(mut self) -> Option<(Result<Event, Infallible>, Self)> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some((Ok(event), self));
            }
            if self.started {
                tokio::select! {
                    changed = self.snapshot.changed() => changed.ok()?,
                    _ = self.shutdown.changed() => return None,
                }
                tokio::time::sleep(EVENT_INTERVAL).await;
            }
            self.started = true;
            let snapshot = self.snapshot.borrow_and_update().clone();
            self.update(&snapshot);
        }
    }
}

/// A server-sent event stream of everything `GET /machine` and
/// `GET /jobs/{id}` report. It starts with the current state, so a client
/// that reconnects needs nothing else to catch up.
///
/// `machine` events carry the machine snapshot. `job` events carry a job
/// record whenever the active job changes, and once more with its final
/// state when it ends.
async fn events(
    State(app): State<AppState>,
) -> Sse<impl futures::Stream<Item = Result<Event, Infallible>>> {
    let feed = EventFeed {
        snapshot: app.controller.subscribe(),
        shutdown: app.shutdown,
        store: app.store,
        started: false,
        pending: VecDeque::new(),
        last_machine: None,
        last_job: None,
        last_active: None,
    };
    Sse::new(futures::stream::unfold(feed, EventFeed::next)).keep_alive(KeepAlive::default())
}
