//! The HTTP API. Handlers are thin: reads come from the job store or the
//! controller snapshot, and every job transition goes through the controller.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::watch;

use crate::bounds::Bounds;
use crate::controller::{
    AxisValues, ControlError, ControllerHandle, JogRequest, MachineAction, MachineSnapshot,
    ResumeOptions, WorkChange,
};
use crate::pairing::{Access, Pairing, PairingView, RedeemError};
use crate::settings::{PauseOptions, Settings};
use crate::store::{JobRecord, JobStore};

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<JobStore>,
    pub controller: ControllerHandle,
    pub pairing: Arc<Pairing>,
    /// Turns true when the server is shutting down, to end open event streams.
    pub shutdown: watch::Receiver<bool>,
    /// True for the plain-HTTP listener on this machine, false for the HTTPS
    /// listener that remote browsers use. Only the first is trusted without a
    /// token.
    pub local: bool,
}

/// The name of the cookie that holds a paired browser's token.
const SESSION_COOKIE: &str = "gcode_sender_session";

/// A paired browser keeps its token for as long as it is not revoked.
const SESSION_MAX_AGE: u64 = 10 * 365 * 24 * 60 * 60;

pub fn router(state: AppState) -> Router {
    // Pages and the calls that let a browser pair. Anyone who can reach the
    // sender can load these; none of them changes the machine.
    let public = Router::new()
        .merge(crate::web::routes())
        .route("/health", get(health))
        .route("/session", get(session))
        .route("/pairing", get(pairing_view).post(pair));

    // Everything else needs the machine's own pages or a paired browser.
    let api = Router::new()
        .route("/events", get(events))
        .route("/machine", get(machine))
        .route("/machine/unlock", post(unlock))
        .route("/machine/home", post(home))
        .route("/machine/reset", post(reset))
        .route("/machine/work-zero", post(work_zero))
        .route("/machine/position-check", post(position_check))
        .route("/machine/work-offset", put(work_offset))
        .route("/machine/work-system", put(work_system))
        .route("/machine/jog", post(jog))
        .route("/machine/jog/cancel", post(jog_cancel))
        .route("/settings", get(get_settings).put(put_settings))
        .route(
            "/machine/bounds",
            get(get_bounds).put(put_bounds).delete(delete_bounds),
        )
        .route("/jobs", get(list_jobs).post(create_job))
        .route("/jobs/{id}", get(get_job).delete(delete_job))
        .route("/jobs/{id}/program", get(get_program))
        .route("/jobs/{id}/start", post(start_job))
        .route("/jobs/{id}/pause", post(pause_job))
        .route("/jobs/{id}/resume", post(resume_job))
        .route("/jobs/{id}/stop", post(stop_job))
        .route("/pairing/code", post(new_pairing_code))
        .route("/pairing/devices/{id}", delete(revoke_device))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            require_access,
        ));

    public
        .merge(api)
        .layer(DefaultBodyLimit::max(256 * 1024 * 1024))
        .layer(middleware::from_fn(same_origin))
        .with_state(state)
}

/// Refuses requests that a page from another site made in the visitor's
/// browser. A browser marks such a request with a `Sec-Fetch-Site` other
/// than `same-origin` or `none`, and it sends an `Origin` that is not this
/// sender's own address. Requests without either header come from programs
/// such as `curl`, and they are judged by the token check instead.
async fn same_origin(mut req: Request, next: Next) -> Response {
    // Browsers speaking HTTP/2 send the host as `:authority` rather than a
    // `Host` header. The rest of the checks read `Host`, so take it from there.
    if !req.headers().contains_key(header::HOST)
        && let Some(authority) = req.uri().authority()
        && let Ok(host) = HeaderValue::from_str(authority.as_str())
    {
        req.headers_mut().insert(header::HOST, host);
    }
    if origin_is_ours(req.headers()) {
        next.run(req).await
    } else {
        ApiError(
            StatusCode::FORBIDDEN,
            "requests must come from this sender's own pages".into(),
        )
        .into_response()
    }
}

fn origin_is_ours(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    if let Some(site) = headers.get("sec-fetch-site")
        && !matches!(site.to_str(), Ok("same-origin" | "none"))
    {
        return false;
    }
    match headers.get(header::ORIGIN) {
        None => true,
        Some(origin) => origin
            .to_str()
            .ok()
            .and_then(|o| o.split_once("://"))
            .is_some_and(|(scheme, authority)| {
                matches!(scheme, "http" | "https") && authority.eq_ignore_ascii_case(host)
            }),
    }
}

/// Serves requests from this machine's own pages without a token, and other
/// requests only with a token from a paired browser.
async fn require_access(State(app): State<AppState>, mut req: Request, next: Next) -> Response {
    match access_of(app.local, req.headers(), &app.pairing) {
        Some(access) => {
            req.extensions_mut().insert(access);
            next.run(req).await
        }
        None => (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error": "pair this browser with the code shown on the sender",
                "pairing": true,
            })),
        )
            .into_response(),
    }
}

/// Who a request comes from. A request is this machine's own page when it
/// arrived on the plain-HTTP listener, which only listens on loopback, and
/// its `Host` names a loopback address. That `Host` check stops a page of
/// another site from reaching this listener by DNS rebinding. Every other
/// request needs a token from a paired browser, including one from this
/// machine that came in over the HTTPS listener.
fn access_of(local: bool, headers: &HeaderMap, pairing: &Pairing) -> Option<Access> {
    let host_is_loopback = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .is_some_and(is_loopback_authority);
    if local && host_is_loopback {
        return Some(Access::Local);
    }
    let token = token_of(headers)?;
    pairing.is_paired(token).then_some(Access::Paired)
}

/// The token a browser or program presents: a `Bearer` authorization, or the
/// session cookie that pairing sets.
fn token_of(headers: &HeaderMap) -> Option<&str> {
    if let Some(auth) = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
    {
        return auth.strip_prefix("Bearer ").map(str::trim);
    }
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())?
        .split(';')
        .filter_map(|pair| pair.trim().strip_prefix(SESSION_COOKIE)?.strip_prefix('='))
        .next()
}

/// Whether an authority (`host`, or `host:port`, with IPv6 in brackets) names
/// this machine: `localhost` or a loopback address.
fn is_loopback_authority(authority: &str) -> bool {
    let host = match authority.strip_prefix('[') {
        Some(rest) => match rest.split_once(']') {
            Some((ip, _)) => ip,
            None => return false,
        },
        None => authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host),
    };
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
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

/// Who is asking, so the page knows whether to ask for a pairing code.
async fn session(State(app): State<AppState>, headers: HeaderMap) -> Json<serde_json::Value> {
    let access = match access_of(app.local, &headers, &app.pairing) {
        Some(Access::Local) => "local",
        Some(Access::Paired) => "paired",
        None => "none",
    };
    Json(json!({ "access": access }))
}

/// Pairing is managed from the machine's own screen, so a paired browser
/// cannot see the code or revoke another browser.
fn require_local(app: &AppState, headers: &HeaderMap) -> ApiResult<()> {
    match access_of(app.local, headers, &app.pairing) {
        Some(Access::Local) => Ok(()),
        _ => Err(ApiError(
            StatusCode::FORBIDDEN,
            "pairing is managed from the machine the sender runs on".into(),
        )),
    }
}

/// The code to show on the machine, and the browsers paired so far.
async fn pairing_view(
    State(app): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<PairingView>> {
    require_local(&app, &headers)?;
    Ok(Json(app.pairing.view()))
}

#[derive(Deserialize)]
struct PairRequest {
    code: String,
}

/// Pairs a browser: a correct code gives it a token, kept in a cookie and
/// also returned for programs that send `Authorization: Bearer`.
async fn pair(
    State(app): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<PairRequest>,
) -> ApiResult<Response> {
    let user_agent = headers
        .get(header::USER_AGENT)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let token = app
        .pairing
        .redeem(&request.code, user_agent)
        .map_err(|e| match e {
            RedeemError::NoCode => ApiError(
                StatusCode::FORBIDDEN,
                "there is no code showing on the sender; read the current one from its screen"
                    .into(),
            ),
            RedeemError::Wrong { left: 0 } => ApiError(
                StatusCode::FORBIDDEN,
                "that code is wrong, and no tries are left; read the new code from the sender's screen"
                    .into(),
            ),
            RedeemError::Wrong { left } => ApiError(
                StatusCode::FORBIDDEN,
                format!("that code is wrong; {left} more tries"),
            ),
            RedeemError::Storage(e) => e.into(),
        })?;
    // Only the HTTPS listener sends the cookie over TLS, so only there is it
    // marked Secure.
    let secure = if app.local { "" } else { "; Secure" };
    let cookie = format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={SESSION_MAX_AGE}{secure}"
    );
    let mut res = (
        StatusCode::CREATED,
        Json(json!({ "access": "paired", "token": token })),
    )
        .into_response();
    res.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).expect("the cookie is ASCII"),
    );
    Ok(res)
}

/// Makes a new code, replacing the one on screen.
async fn new_pairing_code(
    State(app): State<AppState>,
    headers: HeaderMap,
) -> ApiResult<Json<PairingView>> {
    require_local(&app, &headers)?;
    app.pairing.new_code();
    Ok(Json(app.pairing.view()))
}

/// Forgets a paired browser, whose token stops working at once.
async fn revoke_device(
    State(app): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> ApiResult<StatusCode> {
    require_local(&app, &headers)?;
    if app.pairing.revoke(&id)? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError(
            StatusCode::NOT_FOUND,
            "no paired browser has that id".into(),
        ))
    }
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

/// The program as uploaded, for clients that want to show it.
async fn get_program(State(app): State<AppState>, Path(id): Path<String>) -> ApiResult<Response> {
    let store = app.store.clone();
    let source = tokio::task::spawn_blocking(move || store.source(&id))
        .await
        .map_err(|e| anyhow::anyhow!(e))??
        .ok_or_else(not_found)?;
    Ok((
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        source,
    )
        .into_response())
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

async fn work_system(
    State(app): State<AppState>,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    work_change(app, WorkChange::Select, body).await
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
