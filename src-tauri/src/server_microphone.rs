//! Lets another computer dictate with this computer's microphone: a client
//! asks the transcription server to start recording, then to stop, and gets
//! the text back. The recording runs as an ordinary dictation here, with
//! its overlay, sounds and history, so it's always visible when the
//! microphone is on. Only the paste is left out: the text goes to the
//! client instead.
//!
//! Off unless `transcription_server_microphone_enabled` is on, since anyone
//! with the token could otherwise listen through this computer.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use log::{info, warn};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tokio::sync::oneshot;

use crate::managers::audio::AudioRecordingManager;
use crate::settings;
use crate::transcription_server::{check_token, ApiError, ServerState};

/// A recording left running this long (say, the client lost its network) is
/// cancelled.
const MAX_RECORDING: Duration = Duration::from_secs(10 * 60);
/// How long a start waits for the microphone to open.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a stop waits for the text.
const STOP_TIMEOUT: Duration = Duration::from_secs(180);
const POLL: Duration = Duration::from_millis(25);

/// The text, or why there is none.
type TakeResult = Result<String, String>;

/// The one recording a client has asked for.
struct Take {
    id: u64,
    /// Set once this computer's dictation stops and is transcribing it.
    transcribing: bool,
    /// The waiting stop request, if it came before the text.
    reply: Option<oneshot::Sender<TakeResult>>,
    /// The text, if it came before the stop request (say, the recording was
    /// stopped here).
    result: Option<TakeResult>,
}

static TAKE: Lazy<Mutex<Option<Take>>> = Lazy::new(|| Mutex::new(None));
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn lock() -> MutexGuard<'static, Option<Take>> {
    TAKE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Called when a dictation here stops. When the recording was a client's,
/// its text goes to that client: the caller then doesn't paste it.
pub fn claim() -> Option<ClaimedTake> {
    let mut take = lock();
    let take = take.as_mut().filter(|t| !t.transcribing)?;
    take.transcribing = true;
    Some(ClaimedTake {
        id: take.id,
        delivered: false,
    })
}

/// A client's recording being transcribed. Dropped without
/// [`deliver`](Self::deliver) (cancelled, or no speech), the client gets
/// empty text.
pub struct ClaimedTake {
    id: u64,
    delivered: bool,
}

impl ClaimedTake {
    pub fn deliver(mut self, result: TakeResult) {
        self.delivered = true;
        deliver(self.id, result);
    }
}

impl Drop for ClaimedTake {
    fn drop(&mut self) {
        if !self.delivered {
            deliver(self.id, Ok(String::new()));
        }
    }
}

fn deliver(id: u64, result: TakeResult) {
    let mut slot = lock();
    let Some(take) = slot.as_mut().filter(|t| t.id == id) else {
        return;
    };
    match take.reply.take() {
        Some(reply) => {
            let _ = reply.send(result);
            *slot = None;
        }
        None => take.result = Some(result),
    }
}

fn recording(app: &AppHandle) -> bool {
    app.state::<Arc<AudioRecordingManager>>().is_recording()
}

/// Starts or stops this computer's dictation, the way `--toggle-transcription`
/// does.
fn toggle(app: &AppHandle) {
    crate::signal_handle::send_transcription_input(app, "transcribe", "transcription server");
}

fn check_allowed(app: &AppHandle) -> Result<(), ApiError> {
    if settings::get_settings(app).transcription_server_microphone_enabled {
        Ok(())
    } else {
        Err(ApiError::new(
            StatusCode::FORBIDDEN,
            "This computer's microphone isn't shared. Turn on \"Allow Other Computers to Use This Microphone\" in Handy's settings there.",
        ))
    }
}

#[derive(Serialize)]
pub(crate) struct Started {
    id: u64,
}

#[derive(Deserialize)]
pub(crate) struct TakeId {
    id: u64,
}

#[derive(Serialize)]
pub(crate) struct Text {
    text: String,
}

/// `POST /v1/microphone/start`: starts recording with this computer's
/// microphone.
pub(crate) async fn start(
    State(state): State<ServerState>,
    headers: HeaderMap,
) -> Result<Json<Started>, ApiError> {
    let app = state.app.clone();
    check_token(&app, &headers)?;
    check_allowed(&app)?;

    let id = {
        let mut slot = lock();
        // A take whose recording was cancelled here, and never stopped.
        let stale = slot
            .as_ref()
            .is_some_and(|t| !t.transcribing && t.result.is_none() && !recording(&app));
        if stale {
            *slot = None;
        }
        if slot.is_some() || crate::tray::dictation_in_progress(&app) {
            return Err(ApiError::new(
                StatusCode::CONFLICT,
                "The microphone on the server is in use",
            ));
        }
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        *slot = Some(Take {
            id,
            transcribing: false,
            reply: None,
            result: None,
        });
        id
    };

    toggle(&app);
    let deadline = tokio::time::Instant::now() + START_TIMEOUT;
    while !recording(&app) {
        if tokio::time::Instant::now() >= deadline {
            forget(id);
            return Err(ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "The microphone on the server didn't start. Check Handy there for an error.",
            ));
        }
        tokio::time::sleep(POLL).await;
    }
    info!("Recording with this microphone for a client (take {id})");

    // Cancel a recording nobody stops.
    let watchdog_app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(MAX_RECORDING).await;
        let abandoned = lock()
            .as_ref()
            .is_some_and(|t| t.id == id && !t.transcribing);
        if abandoned && recording(&watchdog_app) {
            warn!("Client never stopped take {id}; cancelling it");
            forget(id);
            crate::utils::cancel_current_operation(&watchdog_app);
        }
    });

    Ok(Json(Started { id }))
}

/// `POST /v1/microphone/stop` with `{"id": ...}`: stops the recording and
/// returns its text.
pub(crate) async fn stop(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(body): Json<TakeId>,
) -> Result<Json<Text>, ApiError> {
    let app = state.app.clone();
    check_token(&app, &headers)?;

    let (rx, needs_stop) = {
        let mut slot = lock();
        let Some(take) = slot.as_mut().filter(|t| t.id == body.id) else {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                "No recording with that id is going on",
            ));
        };
        if let Some(result) = take.result.take() {
            *slot = None;
            return finish(result);
        }
        if !take.transcribing && !recording(&app) {
            // Cancelled here; a toggle now would start a new recording.
            *slot = None;
            return Ok(Json(Text {
                text: String::new(),
            }));
        }
        let (tx, rx) = oneshot::channel();
        take.reply = Some(tx);
        (rx, !take.transcribing)
    };

    if needs_stop {
        toggle(&app);
    }
    match tokio::time::timeout(STOP_TIMEOUT, rx).await {
        Ok(Ok(result)) => finish(result),
        // The take was dropped without a result.
        Ok(Err(_)) => Ok(Json(Text {
            text: String::new(),
        })),
        Err(_) => {
            forget(body.id);
            Err(ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "The server didn't finish the transcription in time",
            ))
        }
    }
}

/// `POST /v1/microphone/cancel` with `{"id": ...}`: stops the recording and
/// throws it away.
pub(crate) async fn cancel(
    State(state): State<ServerState>,
    headers: HeaderMap,
    Json(body): Json<TakeId>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let app = state.app.clone();
    check_token(&app, &headers)?;
    let ours = lock().as_ref().is_some_and(|t| t.id == body.id);
    if ours {
        forget(body.id);
        crate::utils::cancel_current_operation(&app);
        info!("Client cancelled take {}", body.id);
    }
    Ok(Json(serde_json::json!({})))
}

/// A cancel here ends a client's recording too, so a later dictation here
/// isn't taken for it. The client gets empty text.
pub fn abandon() {
    if let Some(take) = lock().take() {
        info!("Take {} was cancelled on this computer", take.id);
    }
}

fn forget(id: u64) {
    let mut slot = lock();
    if slot.as_ref().is_some_and(|t| t.id == id) {
        *slot = None;
    }
}

fn finish(result: TakeResult) -> Result<Json<Text>, ApiError> {
    result
        .map(|text| Json(Text { text }))
        .map_err(|message| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, message))
}
