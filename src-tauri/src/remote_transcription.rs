//! Remote transcription: sends each recording to another Handy's
//! transcription server instead of the local model, so this computer can use
//! the other one's model and GPU. Any service with OpenAI's speech-to-text API
//! works too. See `transcription_server` for the other end.
//!
//! Dictation, History retries and file transcription all go to the server
//! while this is on. Everything after the text comes back (history,
//! post-processing, pasting) stays the same.

use std::io::Cursor;
use std::net::IpAddr;
use std::time::Duration;

use anyhow::{anyhow, Result};
use hound::{WavSpec, WavWriter};
use log::{debug, info};
use once_cell::sync::Lazy;
use reqwest::multipart::{Form, Part};
use serde::Deserialize;
use tauri::AppHandle;

use crate::settings::{self, AppSettings, RemoteTranscriptionMode, ServerToken};

/// How long to wait for the server to accept a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Time allowed for a request on top of the audio's own length. A GPU
/// transcribes far faster than real time; this covers a model that first
/// has to load.
const BASE_TIMEOUT: Duration = Duration::from_secs(60);

static CLIENT: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .expect("Failed to build the remote transcription HTTP client")
});

/// Whether recordings go to a server instead of the local model.
pub fn enabled(settings: &AppSettings) -> bool {
    settings.remote_transcription_enabled && !settings.remote_transcription_url.trim().is_empty()
}

/// The API's base URL, ending in `/v1`. Accepts the address with or without
/// `/v1`, the way Handy's server settings show it or not.
fn api_base(url: &str) -> String {
    let url = url.trim().trim_end_matches('/');
    if url.ends_with("/v1") {
        url.to_string()
    } else {
        format!("{url}/v1")
    }
}

/// 16 kHz mono 16-bit WAV, the same format Handy saves recordings in.
fn wav_bytes(samples: &[f32]) -> Result<Vec<u8>> {
    let spec = WavSpec {
        channels: 1,
        sample_rate: 16_000,
        bits_per_sample: 16,
        sample_format: hound::SampleFormat::Int,
    };
    let mut buffer = Cursor::new(Vec::with_capacity(44 + samples.len() * 2));
    {
        let mut writer = WavWriter::new(&mut buffer, spec)?;
        for sample in samples {
            writer.write_sample((sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)?;
        }
        writer.finalize()?;
    }
    Ok(buffer.into_inner())
}

/// A server on this same computer would wait for this dictation to finish
/// before transcribing it, so the request could never complete.
fn points_at_own_server(settings: &AppSettings) -> bool {
    if !settings.transcription_server_enabled {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(&api_base(&settings.remote_transcription_url)) else {
        return false;
    };
    if url.port_or_known_default() != Some(settings.transcription_server_port) {
        return false;
    }
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host
            .trim_matches(|c| c == '[' || c == ']')
            .parse::<IpAddr>()
            .is_ok_and(|ip| {
                ip.is_loopback()
                    || Some(ip) == crate::transcription_server::local_ip().map(IpAddr::V4)
            }),
        None => false,
    }
}

#[derive(Deserialize)]
struct TranscriptionResponse {
    text: String,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    message: String,
}

/// The server's own error message when it sent one.
fn error_message(status: reqwest::StatusCode, body: &str) -> String {
    match serde_json::from_str::<ErrorResponse>(body) {
        Ok(parsed) => format!(
            "The Handy server refused the request: {}",
            parsed.error.message
        ),
        Err(_) => {
            let body: String = body.trim().chars().take(200).collect();
            if body.is_empty() {
                format!("The Handy server answered with {status}")
            } else {
                format!("The Handy server answered with {status}: {body}")
            }
        }
    }
}

fn send_error(base: &str, e: reqwest::Error) -> anyhow::Error {
    if e.is_connect() {
        anyhow!("Can't reach the Handy server at {base}. Check that it's on and Handy is running there.")
    } else if e.is_timeout() {
        anyhow!("The Handy server at {base} didn't answer in time")
    } else {
        anyhow!("Can't talk to the Handy server at {base}: {e}")
    }
}

/// Sends 16 kHz mono `samples` to the server and returns the text.
pub async fn transcribe(settings: &AppSettings, samples: &[f32]) -> Result<String> {
    if points_at_own_server(settings) {
        return Err(anyhow!(
            "Remote transcription points at this computer's own transcription server. Enter the other computer's address."
        ));
    }
    let base = api_base(&settings.remote_transcription_url);
    let audio_secs = samples.len() as f64 / 16_000.0;
    let wav = wav_bytes(samples)?;
    debug!(
        "Sending {:.1}s of audio ({} bytes) to {}",
        audio_secs,
        wav.len(),
        base
    );

    let form = Form::new()
        .part(
            "file",
            Part::bytes(wav)
                .file_name("recording.wav")
                .mime_str("audio/wav")?,
        )
        .text("model", "handy")
        .text("response_format", "json");
    let started = std::time::Instant::now();
    let response = CLIENT
        .post(format!("{base}/audio/transcriptions"))
        .bearer_auth(&settings.remote_transcription_token.0)
        .timeout(BASE_TIMEOUT + Duration::from_secs_f64(audio_secs))
        .multipart(form)
        .send()
        .await
        .map_err(|e| send_error(&base, e))?;

    let status = response.status();
    let body = response.text().await.map_err(|e| send_error(&base, e))?;
    if !status.is_success() {
        return Err(anyhow!(error_message(status, &body)));
    }
    let parsed: TranscriptionResponse = serde_json::from_str(&body)
        .map_err(|e| anyhow!("The Handy server sent an answer Handy can't read: {e}"))?;
    info!(
        "Remote transcription of {:.1}s of audio took {:.2}s",
        audio_secs,
        started.elapsed().as_secs_f64()
    );
    Ok(parsed.text.trim().to_string())
}

// ---------------------------------------------------------------------------
// The server's microphone
// ---------------------------------------------------------------------------

/// How long a stop waits for the server's text: its recording, as long as
/// ten minutes, still has to be transcribed.
const MICROPHONE_STOP_TIMEOUT: Duration = Duration::from_secs(200);

#[derive(Deserialize)]
struct MicrophoneStarted {
    id: u64,
}

/// Posts `body` to `<base>/microphone/<action>` and returns the answer's body.
async fn microphone_request(
    settings: &AppSettings,
    action: &str,
    body: serde_json::Value,
    timeout: Duration,
) -> Result<String> {
    if points_at_own_server(settings) {
        return Err(anyhow!(
            "Remote transcription points at this computer's own transcription server. Enter the other computer's address."
        ));
    }
    let base = api_base(&settings.remote_transcription_url);
    let response = CLIENT
        .post(format!("{base}/microphone/{action}"))
        .bearer_auth(&settings.remote_transcription_token.0)
        .timeout(timeout)
        .json(&body)
        .send()
        .await
        .map_err(|e| send_error(&base, e))?;
    let status = response.status();
    let text = response.text().await.map_err(|e| send_error(&base, e))?;
    if status == reqwest::StatusCode::NOT_FOUND && text.trim().is_empty() {
        return Err(anyhow!(
            "The Handy server at {base} is too old to share its microphone. Update Handy there."
        ));
    }
    if !status.is_success() {
        return Err(anyhow!(error_message(status, &text)));
    }
    Ok(text)
}

/// Asks the server to start recording with its microphone. Returns the
/// recording's id, for [`stop_server_microphone`].
pub async fn start_server_microphone(settings: &AppSettings) -> Result<u64> {
    let body = microphone_request(
        settings,
        "start",
        serde_json::json!({}),
        Duration::from_secs(15),
    )
    .await?;
    let started: MicrophoneStarted = serde_json::from_str(&body)
        .map_err(|e| anyhow!("The Handy server sent an answer Handy can't read: {e}"))?;
    Ok(started.id)
}

/// Stops the server's recording and returns its text.
pub async fn stop_server_microphone(settings: &AppSettings, id: u64) -> Result<String> {
    let started = std::time::Instant::now();
    let body = microphone_request(
        settings,
        "stop",
        serde_json::json!({ "id": id }),
        MICROPHONE_STOP_TIMEOUT,
    )
    .await?;
    let parsed: TranscriptionResponse = serde_json::from_str(&body)
        .map_err(|e| anyhow!("The Handy server sent an answer Handy can't read: {e}"))?;
    info!(
        "The server's microphone recording came back in {:.2}s",
        started.elapsed().as_secs_f64()
    );
    Ok(parsed.text.trim().to_string())
}

/// Stops the server's recording and throws it away.
pub async fn cancel_server_microphone(settings: &AppSettings, id: u64) -> Result<()> {
    microphone_request(
        settings,
        "cancel",
        serde_json::json!({ "id": id }),
        Duration::from_secs(10),
    )
    .await
    .map(|_| ())
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

#[tauri::command]
#[specta::specta]
pub fn change_remote_transcription_server_microphone_setting(
    app: AppHandle,
    enabled: bool,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.remote_transcription_use_server_microphone = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_remote_transcription_enabled_setting(
    app: AppHandle,
    enabled: bool,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.remote_transcription_enabled = enabled;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_remote_transcription_url_setting(app: AppHandle, url: String) -> Result<(), String> {
    let url = url.trim().to_string();
    if !url.is_empty() {
        let parsed = reqwest::Url::parse(&url)
            .map_err(|_| "Enter a URL such as http://192.168.0.21:8765/v1".to_string())?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err("Enter a URL such as http://192.168.0.21:8765/v1".to_string());
        }
    }
    let mut settings = settings::get_settings(&app);
    settings.remote_transcription_url = url;
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_remote_transcription_token_setting(
    app: AppHandle,
    token: String,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.remote_transcription_token = ServerToken(token.trim().to_string());
    settings::write_settings(&app, settings);
    Ok(())
}

#[tauri::command]
#[specta::specta]
pub fn change_remote_transcription_mode_setting(
    app: AppHandle,
    mode: RemoteTranscriptionMode,
) -> Result<(), String> {
    let mut settings = settings::get_settings(&app);
    settings.remote_transcription_mode = mode;
    settings::write_settings(&app, settings);
    Ok(())
}

/// Checks the URL and token by asking the server for its model. Returns the
/// model's id.
#[tauri::command]
#[specta::specta]
pub async fn test_remote_transcription(app: AppHandle) -> Result<String, String> {
    let settings = settings::get_settings(&app);
    if settings.remote_transcription_url.trim().is_empty() {
        return Err("Enter the server URL first".to_string());
    }
    let base = api_base(&settings.remote_transcription_url);
    let response = CLIENT
        .get(format!("{base}/models"))
        .bearer_auth(&settings.remote_transcription_token.0)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| send_error(&base, e).to_string())?;
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(error_message(status, &body));
    }
    let model = serde_json::from_str::<serde_json::Value>(&body)
        .ok()
        .and_then(|v| v["data"][0]["id"].as_str().map(str::to_string))
        .unwrap_or_default();
    Ok(model)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::Multipart;
    use axum::http::HeaderMap;
    use axum::routing::post;
    use axum::{Json, Router};

    #[test]
    fn api_base_accepts_the_url_with_or_without_v1() {
        assert_eq!(api_base("http://mac:8765/v1"), "http://mac:8765/v1");
        assert_eq!(api_base("http://mac:8765/v1/"), "http://mac:8765/v1");
        assert_eq!(api_base(" http://mac:8765 "), "http://mac:8765/v1");
    }

    #[test]
    fn wav_bytes_is_a_16khz_mono_wav() {
        let wav = wav_bytes(&[0.0, 0.5, -0.5, 2.0]).unwrap();
        let reader = hound::WavReader::new(Cursor::new(wav)).unwrap();
        let spec = reader.spec();
        assert_eq!(
            (spec.sample_rate, spec.channels, spec.bits_per_sample),
            (16_000, 1, 16)
        );
        let samples: Vec<i16> = reader.into_samples().map(|s| s.unwrap()).collect();
        assert_eq!(samples, vec![0, 16383, -16383, i16::MAX]);
    }

    #[test]
    fn error_message_uses_the_servers_message() {
        let body =
            r#"{"error":{"message":"Missing or wrong token","type":"authentication_error"}}"#;
        assert_eq!(
            error_message(reqwest::StatusCode::UNAUTHORIZED, body),
            "The Handy server refused the request: Missing or wrong token"
        );
        assert_eq!(
            error_message(reqwest::StatusCode::BAD_GATEWAY, ""),
            "The Handy server answered with 502 Bad Gateway"
        );
    }

    #[test]
    fn own_server_is_detected() {
        let mut settings = settings::get_default_settings();
        settings.remote_transcription_url = "http://127.0.0.1:8765/v1".into();
        assert!(!points_at_own_server(&settings));
        settings.transcription_server_enabled = true;
        assert!(points_at_own_server(&settings));
        settings.remote_transcription_url = "http://192.0.2.50:8765/v1".into();
        assert!(!points_at_own_server(&settings));
    }

    /// Sends a recording to a stand-in server and checks what it received.
    #[tokio::test(flavor = "multi_thread")]
    async fn transcribe_sends_the_recording_and_reads_the_text() {
        async fn handler(headers: HeaderMap, mut form: Multipart) -> Json<serde_json::Value> {
            let auth = headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let mut file_len = 0;
            while let Some(field) = form.next_field().await.unwrap() {
                if field.name() == Some("file") {
                    file_len = field.bytes().await.unwrap().len();
                }
            }
            Json(serde_json::json!({ "text": format!(" {auth} {file_len} ") }))
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let app = Router::new().route("/v1/audio/transcriptions", post(handler));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut settings = settings::get_default_settings();
        settings.remote_transcription_enabled = true;
        settings.remote_transcription_url = format!("http://127.0.0.1:{port}");
        settings.remote_transcription_token = ServerToken("secret".into());

        let text = transcribe(&settings, &[0.1; 16_000]).await.unwrap();
        // 44-byte header plus 2 bytes per sample.
        assert_eq!(text, "Bearer secret 32044");
    }

    /// Starts and stops a recording with a real server's microphone. Needs
    /// `HANDY_TEST_SERVER_URL` and `HANDY_TEST_SERVER_TOKEN`, and the server
    /// sharing its microphone. Run with `--ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn server_microphone_with_a_real_server() {
        let mut settings = settings::get_default_settings();
        settings.remote_transcription_enabled = true;
        settings.remote_transcription_url = std::env::var("HANDY_TEST_SERVER_URL").unwrap();
        settings.remote_transcription_token =
            ServerToken(std::env::var("HANDY_TEST_SERVER_TOKEN").unwrap());
        let id = start_server_microphone(&settings).await.unwrap();
        tokio::time::sleep(Duration::from_secs(1)).await;
        let text = stop_server_microphone(&settings, id).await.unwrap();
        println!("text: {text:?}");
        let id = start_server_microphone(&settings).await.unwrap();
        cancel_server_microphone(&settings, id).await.unwrap();
        let err = stop_server_microphone(&settings, id).await.unwrap_err();
        println!("stop after cancel: {err}");
    }

    /// Against a real server: `HANDY_TEST_SERVER_URL`, `HANDY_TEST_SERVER_TOKEN`
    /// and `HANDY_TEST_WAV` (16 kHz mono 16-bit). Run with `--ignored`.
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn transcribe_with_a_real_server() {
        let mut settings = settings::get_default_settings();
        settings.remote_transcription_enabled = true;
        settings.remote_transcription_url = std::env::var("HANDY_TEST_SERVER_URL").unwrap();
        settings.remote_transcription_token =
            ServerToken(std::env::var("HANDY_TEST_SERVER_TOKEN").unwrap());
        let samples =
            crate::audio_toolkit::read_wav_samples(std::env::var("HANDY_TEST_WAV").unwrap())
                .unwrap();
        let text = transcribe(&settings, &samples).await.unwrap();
        println!("text: {text}");
        assert!(!text.is_empty());
    }
}
