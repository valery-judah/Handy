//! Transcription server: lets other devices on the network send audio to this
//! Handy and get the text back, so they use this machine's model and GPU.
//!
//! It follows OpenAI's speech-to-text API, so clients written for it work:
//!
//! - `POST /v1/audio/transcriptions`: a multipart form with a `file` field
//!   (any format Handy can open: WAV, MP3, M4A, FLAC, OGG/Opus, ...) and an
//!   optional `response_format` (`json`, the default, `verbose_json` or
//!   `text`). The other OpenAI fields (`model`, `language`, `prompt`,
//!   `temperature`) are accepted and ignored: Handy's own settings apply.
//! - `GET /v1/models`: the selected model, for clients that check it.
//! - `GET /health`: needs no token; answers while the server is up.
//!
//! Every other request needs `Authorization: Bearer <token>`. Requests are
//! transcribed one at a time, and dictation on this machine goes first.

use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Multipart, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use log::{error, info, warn};
use serde::{Deserialize, Serialize};
use serde_json::json;
use specta::Type;
use tauri::{AppHandle, Manager};
use tokio::net::TcpSocket;
use tokio::sync::oneshot;

use crate::audio_toolkit::decode_audio_file;
use crate::managers::transcription::{panic_payload_message, ChunkControl, TranscriptionManager};
use crate::settings::{self, ServerToken};

/// Largest upload accepted. An hour of 16 kHz 16-bit WAV is ~115 MB.
const MAX_UPLOAD_BYTES: usize = 512 * 1024 * 1024;

/// How often a request waiting on dictation checks whether it has finished.
const DICTATION_POLL: Duration = Duration::from_millis(200);

/// Owns the running server, if any. Managed as Tauri state.
#[derive(Default)]
pub struct TranscriptionServer {
    running: Mutex<Option<Running>>,
}

struct Running {
    port: u16,
    shutdown: oneshot::Sender<()>,
}

impl TranscriptionServer {
    fn lock(&self) -> MutexGuard<'_, Option<Running>> {
        self.running.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn stop(&self) {
        if let Some(running) = self.lock().take() {
            info!("Stopping the transcription server on port {}", running.port);
            let _ = running.shutdown.send(());
        }
    }

    fn port(&self) -> Option<u16> {
        self.lock().as_ref().map(|r| r.port)
    }
}

#[derive(Clone)]
struct ServerState {
    app: AppHandle,
    /// One transcription at a time: the engine runs one anyway, and this
    /// keeps a second request from holding the model while it waits.
    busy: Arc<tokio::sync::Mutex<()>>,
}

/// Starts, stops or moves the server to match the settings. Returns why it
/// couldn't start, such as the port being in use.
pub async fn sync(app: &AppHandle) -> Result<(), String> {
    let Some(server) = app.try_state::<TranscriptionServer>() else {
        return Ok(());
    };
    let settings = settings::get_settings(app);
    if !settings.transcription_server_enabled {
        server.stop();
        return Ok(());
    }
    let port = settings.transcription_server_port;
    if server.port() == Some(port) {
        return Ok(());
    }
    server.stop();

    let listener = bind(port)?;
    let (shutdown, stopped) = oneshot::channel();
    *server.lock() = Some(Running { port, shutdown });

    let state = ServerState {
        app: app.clone(),
        busy: Arc::new(tokio::sync::Mutex::new(())),
    };
    let router = Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(models))
        .route("/v1/audio/transcriptions", post(transcriptions))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_BYTES))
        .with_state(state);

    info!("Transcription server listening on port {}", port);
    tauri::async_runtime::spawn(async move {
        let served = axum::serve(listener, router)
            .with_graceful_shutdown(async {
                let _ = stopped.await;
            })
            .await;
        if let Err(e) = served {
            error!("Transcription server stopped: {}", e);
        }
    });
    Ok(())
}

/// Listens on every interface, so other devices on the network can connect.
fn bind(port: u16) -> Result<tokio::net::TcpListener, String> {
    let addr = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    let socket = TcpSocket::new_v4().map_err(|e| e.to_string())?;
    // A restart can rebind the port while the old connections close.
    socket.set_reuseaddr(true).map_err(|e| e.to_string())?;
    socket.bind(addr).map_err(|e| match e.kind() {
        std::io::ErrorKind::AddrInUse => {
            format!("Port {port} is already in use by another program")
        }
        _ => format!("Can't listen on port {port}: {e}"),
    })?;
    socket
        .listen(128)
        .map_err(|e| format!("Can't listen on port {port}: {e}"))
}

/// Starts the server at launch if the user turned it on.
pub fn start_at_launch(app: &AppHandle) {
    app.manage(TranscriptionServer::default());
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        if let Err(e) = sync(&app).await {
            error!("Transcription server didn't start: {}", e);
        }
    });
}

/// A new random token, as 32 hex characters.
fn new_token() -> Result<ServerToken, String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| format!("Can't create a token: {e}"))?;
    Ok(ServerToken(
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>(),
    ))
}

/// This machine's address on the local network: the one it would send from
/// to reach the internet. Connecting a UDP socket sends nothing.
fn local_ip() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match socket.local_addr().ok()?.ip() {
        std::net::IpAddr::V4(ip) if !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[derive(Serialize, Deserialize, Debug, Clone, Type)]
pub struct TranscriptionServerStatus {
    pub running: bool,
    /// The base URL clients use, such as `http://192.168.1.20:8765/v1`.
    pub url: Option<String>,
}

#[tauri::command]
#[specta::specta]
pub fn get_transcription_server_status(app: AppHandle) -> TranscriptionServerStatus {
    let port = app
        .try_state::<TranscriptionServer>()
        .and_then(|server| server.port());
    TranscriptionServerStatus {
        running: port.is_some(),
        url: port.map(|port| {
            let host = local_ip()
                .map(|ip| ip.to_string())
                .unwrap_or_else(|| "localhost".to_string());
            format!("http://{host}:{port}/v1")
        }),
    }
}

#[tauri::command]
#[specta::specta]
pub async fn change_transcription_server_enabled_setting(
    app: AppHandle,
    enabled: bool,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.transcription_server_enabled = enabled;
    if enabled && settings.transcription_server_token.0.is_empty() {
        settings.transcription_server_token = new_token()?;
    }
    settings::write_settings(&app, settings);

    if let Err(e) = sync(&app).await {
        let mut settings = settings::get_settings(&app);
        settings.transcription_server_enabled = false;
        settings::write_settings(&app, settings);
        return Err(e);
    }
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub async fn change_transcription_server_port_setting(
    app: AppHandle,
    port: u16,
) -> Result<(), String> {
    if port < 1024 {
        return Err("Pick a port from 1024 to 65535".to_string());
    }
    let mut settings = settings::get_settings(&app);
    let previous = settings.transcription_server_port;
    settings.transcription_server_port = port;
    settings::write_settings(&app, settings);

    if let Err(e) = sync(&app).await {
        // Go back to the port that worked.
        let mut settings = settings::get_settings(&app);
        settings.transcription_server_port = previous;
        settings::write_settings(&app, settings);
        if let Err(again) = sync(&app).await {
            warn!("Transcription server didn't restart on port {previous}: {again}");
        }
        return Err(e);
    }
    Ok(())
}

/// Replaces the token. Clients using the old one stop working at once.
#[tauri::command]
#[specta::specta]
pub fn regenerate_transcription_server_token(app: AppHandle) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.transcription_server_token = new_token()?;
    settings::write_settings(&app, settings);
    Ok(())
}

// ---------------------------------------------------------------------------
// HTTP handlers
// ---------------------------------------------------------------------------

/// An error in the shape OpenAI clients expect.
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }

    fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let kind = match self.status {
            StatusCode::UNAUTHORIZED => "authentication_error",
            s if s.is_client_error() => "invalid_request_error",
            _ => "server_error",
        };
        let body = json!({ "error": { "message": self.message, "type": kind } });
        (self.status, Json(body)).into_response()
    }
}

fn check_token(app: &AppHandle, headers: &HeaderMap) -> Result<(), ApiError> {
    let expected = settings::get_settings(app).transcription_server_token.0;
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .unwrap_or("");
    if expected.is_empty() || !same_secret(given.as_bytes(), expected.as_bytes()) {
        return Err(ApiError::new(
            StatusCode::UNAUTHORIZED,
            "Missing or wrong token. Send `Authorization: Bearer <token>` with the token from Handy's settings.",
        ));
    }
    Ok(())
}

/// Compares in time that doesn't depend on where the two differ.
fn same_secret(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({ "status": "ok" }))
}

async fn models(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<serde_json::Value>, ApiError> {
    check_token(&state.app, &headers)?;
    let model = settings::get_settings(&state.app).selected_model;
    Ok(Json(json!({
        "object": "list",
        "data": [{ "id": model, "object": "model", "owned_by": "handy" }],
    })))
}

#[derive(Clone, Copy)]
enum ResponseFormat {
    Json,
    VerboseJson,
    Text,
}

async fn transcriptions(
    State(state): State<ServerState>,
    headers: HeaderMap,
    mut form: Multipart,
) -> Result<Response, ApiError> {
    check_token(&state.app, &headers)?;

    let mut upload: Option<(String, Vec<u8>)> = None;
    let mut format = ResponseFormat::Json;
    while let Some(field) = form
        .next_field()
        .await
        .map_err(|e| ApiError::bad_request(format!("Can't read the form: {e}")))?
    {
        match field.name() {
            Some("file") => {
                let name = field.file_name().unwrap_or("audio").to_string();
                let data = field
                    .bytes()
                    .await
                    .map_err(|e| ApiError::bad_request(format!("Can't read the file: {e}")))?;
                upload = Some((name, data.to_vec()));
            }
            Some("response_format") => {
                let value = field
                    .text()
                    .await
                    .map_err(|e| ApiError::bad_request(format!("Can't read the form: {e}")))?;
                format = match value.trim() {
                    "" | "json" => ResponseFormat::Json,
                    "verbose_json" => ResponseFormat::VerboseJson,
                    "text" => ResponseFormat::Text,
                    other => {
                        return Err(ApiError::bad_request(format!(
                            "response_format `{other}` isn't supported. Use json, verbose_json or text."
                        )))
                    }
                };
            }
            // `model`, `language`, `prompt` and the rest: Handy's settings apply.
            _ => {}
        }
    }
    let (name, data) = upload.ok_or_else(|| {
        ApiError::bad_request("The form has no `file` field with the audio to transcribe")
    })?;

    let _turn = state.busy.lock().await;
    let app = state.app.clone();
    let result = tauri::async_runtime::spawn_blocking(move || transcribe_upload(&app, &name, data))
        .await
        .map_err(|e| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))??;

    Ok(match format {
        ResponseFormat::Json => Json(json!({ "text": result.text })).into_response(),
        ResponseFormat::VerboseJson => Json(json!({
            "task": "transcribe",
            "language": result.language,
            "duration": result.duration_secs,
            "text": result.text,
        }))
        .into_response(),
        ResponseFormat::Text => (
            [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
            result.text,
        )
            .into_response(),
    })
}

struct Transcribed {
    text: String,
    language: Option<String>,
    duration_secs: f64,
}

/// Decodes the upload and transcribes it with the selected model, the same
/// way Handy transcribes a file. Blocks until done.
fn transcribe_upload(app: &AppHandle, name: &str, data: Vec<u8>) -> Result<Transcribed, ApiError> {
    if settings::get_settings(app).selected_model.is_empty() {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "No model is selected in Handy",
        ));
    }
    let samples = decode_upload(name, data)?;
    if samples.is_empty() {
        return Err(ApiError::bad_request("The file has no audio"));
    }
    let duration_secs = samples.len() as f64 / 16_000.0;
    info!(
        "Transcription server: transcribing {:.1}s of audio",
        duration_secs
    );

    let tm = Arc::clone(&app.state::<Arc<TranscriptionManager>>());
    let hold = tm.hold_for_server_request();
    let control = WaitForDictation { app };
    let result = catch_unwind(AssertUnwindSafe(|| {
        tm.transcribe_in_chunks(samples, &control)
    }));
    drop(hold);
    // Same as the end of a file job: the "unload immediately" setting
    // applies once nothing else holds the model.
    if !crate::tray::dictation_in_progress(app) {
        tm.maybe_unload_immediately("transcription server request");
    }

    match result {
        Ok(Ok(Some(t))) => Ok(Transcribed {
            text: t.text,
            language: t.language.and_then(|decision| decision.language),
            duration_secs,
        }),
        Ok(Ok(None)) => Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "The transcription was stopped",
        )),
        Ok(Err(e)) => {
            warn!("Transcription server request failed: {}", e);
            Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                e.to_string(),
            ))
        }
        Err(payload) => Err(ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "Transcription crashed: {}",
                panic_payload_message(payload.as_ref())
            ),
        )),
    }
}

/// Writes the upload to a temporary file, since the decoder reads files, and
/// decodes it to 16 kHz mono.
fn decode_upload(name: &str, data: Vec<u8>) -> Result<Vec<f32>, ApiError> {
    static NEXT: AtomicU64 = AtomicU64::new(0);

    let dir = std::env::temp_dir().join("handy-transcription-server");
    std::fs::create_dir_all(&dir).map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Can't save the upload: {e}"),
        )
    })?;
    // The extension helps the decoder guess the format; it also probes the
    // content, so a missing one is fine.
    let extension = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
        .unwrap_or("bin");
    let path: PathBuf = dir.join(format!(
        "upload-{}-{}.{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed),
        extension
    ));
    std::fs::write(&path, &data).map_err(|e| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Can't save the upload: {e}"),
        )
    })?;
    drop(data);

    // A malformed file may panic the decoder.
    let decoded = catch_unwind(AssertUnwindSafe(|| decode_audio_file(&path)));
    if let Err(e) = std::fs::remove_file(&path) {
        warn!("Can't remove {}: {}", path.display(), e);
    }
    match decoded {
        Ok(Ok(samples)) => Ok(samples),
        Ok(Err(e)) => Err(ApiError::bad_request(format!("Can't read the audio: {e}"))),
        Err(payload) => Err(ApiError::bad_request(format!(
            "Can't read the audio: {}",
            panic_payload_message(payload.as_ref())
        ))),
    }
}

/// Dictation on this machine goes first: each chunk waits until it ends.
struct WaitForDictation<'a> {
    app: &'a AppHandle,
}

impl ChunkControl for WaitForDictation<'_> {
    fn proceed(&self) -> bool {
        while crate::tray::dictation_in_progress(self.app) {
            thread::sleep(DICTATION_POLL);
        }
        true
    }

    fn progress(&self, _done: usize, _total: usize) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_secret_compares_whole_values() {
        assert!(same_secret(b"abc123", b"abc123"));
        assert!(!same_secret(b"abc123", b"abc124"));
        assert!(!same_secret(b"abc", b"abc123"));
        assert!(!same_secret(b"", b"abc"));
    }

    #[test]
    fn new_tokens_are_long_and_differ() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert_eq!(a.0.len(), 32);
        assert!(a.0.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
