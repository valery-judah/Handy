//! Dictating with the server's microphone: the shortcut here asks the
//! server to start recording, and to stop, and the text it sends back is
//! pasted here. This computer's microphone stays closed. See
//! `server_microphone` for the server's side.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{anyhow, Result};
use log::{info, warn};
use once_cell::sync::Lazy;
use tauri::AppHandle;

use crate::audio_feedback::{play_feedback_sound, SoundType};
use crate::notices::{self, Notice, NoticeCode, NoticeSource};
use crate::remote_transcription;
use crate::settings::{get_settings, AppSettings};

/// How long a stop waits for a start still on its way to the server.
const START_WAIT: Duration = Duration::from_secs(20);
const POLL: Duration = Duration::from_millis(25);

/// The recording on the server, from the shortcut's press until its stop.
struct Take {
    generation: u64,
    /// The server's id for the recording, once it has started.
    id: Option<u64>,
    /// Why the server didn't start, if it didn't.
    failed: Option<String>,
    /// A stop is waiting for the start; it reports a failure itself.
    stopping: bool,
}

static TAKE: Lazy<Mutex<Option<Take>>> = Lazy::new(|| Mutex::new(None));
static GENERATION: AtomicU64 = AtomicU64::new(0);

fn lock() -> MutexGuard<'static, Option<Take>> {
    TAKE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether dictations with these settings record with the server's
/// microphone.
pub fn wanted(settings: &AppSettings) -> bool {
    remote_transcription::enabled(settings) && settings.remote_transcription_use_server_microphone
}

/// Whether a recording on the server is going on for this computer.
pub fn is_active() -> bool {
    lock().is_some()
}

/// Asks the server to start recording. Returns at once; the request runs in
/// the background, and a failure ends the dictation with a notice.
pub fn begin(app: &AppHandle) {
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed) + 1;
    *lock() = Some(Take {
        generation,
        id: None,
        failed: None,
        stopping: false,
    });
    let app = app.clone();
    let settings = get_settings(&app);
    tauri::async_runtime::spawn(async move {
        let result = remote_transcription::start_server_microphone(&settings).await;
        // Decided under the lock, done after it: the lock can't be held
        // across an await.
        enum Next {
            Started(u64),
            ThrowAway(u64),
            Failed { message: String, report: bool },
            Nothing,
        }
        let next = {
            let mut slot = lock();
            let current = slot.as_mut().filter(|t| t.generation == generation);
            match (result, current) {
                (Ok(id), Some(take)) => {
                    take.id = Some(id);
                    Next::Started(id)
                }
                // Cancelled before the server answered.
                (Ok(id), None) => Next::ThrowAway(id),
                (Err(e), Some(take)) => {
                    let message = e.to_string();
                    take.failed = Some(message.clone());
                    // A stop already waiting reports the failure itself.
                    Next::Failed {
                        message,
                        report: !take.stopping,
                    }
                }
                (Err(_), None) => Next::Nothing,
            }
        };
        match next {
            Next::Started(id) => {
                info!("The server's microphone is recording (take {id})");
                crate::utils::emit_recording_ready(&app);
                play_feedback_sound(&app, SoundType::Start);
            }
            Next::ThrowAway(id) => {
                let _ = remote_transcription::cancel_server_microphone(&settings, id).await;
            }
            Next::Failed { message, report } => {
                warn!("The server's microphone didn't start: {message}");
                if report {
                    crate::utils::cancel_current_operation(&app);
                    notices::raise(
                        &app,
                        Notice::new(NoticeCode::TranscriptionFailed, NoticeSource::Dictation)
                            .with_detail(message),
                    );
                }
            }
            Next::Nothing => {}
        }
    });
}

/// Stops the server's recording and returns its text.
pub async fn finish(settings: &AppSettings) -> Result<String> {
    let deadline = tokio::time::Instant::now() + START_WAIT;
    let id = loop {
        {
            let mut slot = lock();
            let Some(take) = slot.as_mut() else {
                return Err(anyhow!("The recording on the server was cancelled"));
            };
            take.stopping = true;
            if let Some(message) = &take.failed {
                let message = message.clone();
                *slot = None;
                return Err(anyhow!(message));
            }
            if let Some(id) = take.id {
                *slot = None;
                break id;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            *lock() = None;
            return Err(anyhow!("The Handy server didn't start recording in time"));
        }
        tokio::time::sleep(POLL).await;
    };
    remote_transcription::stop_server_microphone(settings, id).await
}

/// Ends the recording on the server without its text.
pub fn cancel(app: &AppHandle) {
    let Some(take) = lock().take() else {
        return;
    };
    if let Some(id) = take.id {
        let settings = get_settings(app);
        tauri::async_runtime::spawn(async move {
            if let Err(e) = remote_transcription::cancel_server_microphone(&settings, id).await {
                warn!("Couldn't cancel the recording on the server: {e}");
            }
        });
    }
}
