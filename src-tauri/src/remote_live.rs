//! Live remote dictation: while the recording goes on, each phrase is sent to
//! the server as soon as the speaker pauses, so its text comes back a moment
//! later instead of all at once at the end. Used by the `Preview` and `Type`
//! remote transcription modes (see [`RemoteTranscriptionMode`]).
//!
//! A pause is found from the recorder's frames themselves. With voice
//! activity detection on, the recorder stops sending frames a short time
//! after speech ends, so a gap in the frames is a pause. With it off, frames
//! keep coming, so a run of quiet frames counts as a pause.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use log::{debug, info, warn};
use once_cell::sync::Lazy;
use tauri::{AppHandle, Manager};
use tauri_specta::Event;

use crate::audio_toolkit::chunking::join_transcripts;
use crate::managers::transcription::{StreamTextEvent, TranscriptionManager};
use crate::settings::{AppSettings, RemoteTranscriptionMode};

const RATE: usize = 16_000;
/// A gap this long between frames is a pause. It comes on top of the voice
/// detector's hangover, so speech has been quiet for about 0.75 s by then.
const PAUSE_GAP: Duration = Duration::from_millis(300);
/// Without voice detection, this much quiet audio is a pause.
const QUIET_PAUSE_SAMPLES: usize = 700 * RATE / 1000;
/// A frame quieter than this (root mean square) counts as quiet.
const QUIET_RMS: f32 = 0.01;
/// A shorter phrase waits for the next pause: a word or two on its own is
/// easy to mishear.
const MIN_PHRASE_SAMPLES: usize = RATE;
/// Speech with no pause at all is cut here anyway, so the text doesn't stop
/// coming.
const MAX_PHRASE_SAMPLES: usize = 25 * RATE;
/// How often the worker checks for a gap when no frames arrive.
const POLL: Duration = Duration::from_millis(50);

/// The live session of the dictation in progress, from the start of the
/// recording until its stop takes it.
static ACTIVE: Lazy<Mutex<Option<LiveRemote>>> = Lazy::new(|| Mutex::new(None));

/// Whether dictations with these settings send phrases while recording.
pub fn wanted(settings: &AppSettings) -> bool {
    crate::remote_transcription::enabled(settings)
        && settings.remote_transcription_mode != RemoteTranscriptionMode::Whole
}

/// Starts sending phrases for the recording about to begin. Replaces (and
/// cancels) any session left over from an earlier recording.
pub fn begin(app: &AppHandle, settings: AppSettings) {
    let mode = settings.remote_transcription_mode;
    let typing = mode == RemoteTranscriptionMode::Type;
    let app_for_text = app.clone();
    let on_phrase = move |part: &str, all: &str| {
        let _ = StreamTextEvent {
            committed: all.to_string(),
            tentative: String::new(),
        }
        .emit(&app_for_text);
        if typing {
            if let Err(e) = crate::utils::paste_part(part.to_string(), app_for_text.clone()) {
                warn!("Couldn't type a phrase: {}", e);
            }
        }
    };
    let session = LiveRemote::start(app, settings, mode, Box::new(on_phrase));
    let old = lock_active().replace(session);
    drop(old);
}

/// Takes the session of the recording that just stopped, if it has one.
pub fn take() -> Option<LiveRemote> {
    lock_active().take()
}

/// Whether the dictation in progress sends phrases while recording.
pub fn is_active() -> bool {
    lock_active().is_some()
}

/// Stops sending phrases, for a cancelled or failed recording.
pub fn cancel() {
    drop(take());
}

fn lock_active() -> std::sync::MutexGuard<'static, Option<LiveRemote>> {
    ACTIVE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Called with each phrase's text as it comes back: the part to add after
/// what came before (with its leading space), and all the text so far.
type OnPhrase = Box<dyn FnMut(&str, &str) + Send>;

enum Msg {
    Frame(Instant, Vec<f32>),
    Finish,
}

/// What the phrases sent during a recording gave.
pub struct Finished {
    /// Every phrase's text in order, the last one included.
    pub phrases: Vec<String>,
    /// The last phrase's part, which [`OnPhrase`] wasn't called with: the
    /// caller pastes it, as the end of the dictation.
    pub last_part: String,
}

pub struct LiveRemote {
    app: AppHandle,
    mode: RemoteTranscriptionMode,
    tx: Option<mpsc::Sender<Msg>>,
    worker: Option<thread::JoinHandle<anyhow::Result<Finished>>>,
    /// Set once the session is dropped: a phrase still coming back then is
    /// neither shown nor typed.
    stopped: Arc<AtomicBool>,
}

impl LiveRemote {
    fn start(
        app: &AppHandle,
        settings: AppSettings,
        mode: RemoteTranscriptionMode,
        on_phrase: OnPhrase,
    ) -> Self {
        let (tx, rx) = mpsc::channel();
        let tap_tx = Mutex::new(tx.clone());
        let router = app.state::<Arc<TranscriptionManager>>().stream_router();
        router.set_tap(Some(Box::new(move |frame: &[f32]| {
            let tx = tap_tx.lock().unwrap_or_else(|e| e.into_inner());
            let _ = tx.send(Msg::Frame(Instant::now(), frame.to_vec()));
        })));
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stopped = Arc::clone(&stopped);
        let worker = thread::Builder::new()
            .name("remote-live".into())
            .spawn(move || run(rx, settings, on_phrase, worker_stopped))
            .ok();
        info!("Live remote dictation started ({:?})", mode);
        Self {
            app: app.clone(),
            mode,
            tx: Some(tx),
            worker,
            stopped,
        }
    }

    pub fn mode(&self) -> RemoteTranscriptionMode {
        self.mode
    }

    /// Sends what's left of the recording and waits for every phrase. Blocks,
    /// so call it off the async runtime.
    pub fn finish(mut self) -> anyhow::Result<Finished> {
        self.stop_tap();
        if let Some(tx) = self.tx.take() {
            let _ = tx.send(Msg::Finish);
        }
        match self.worker.take() {
            Some(worker) => worker
                .join()
                .unwrap_or_else(|_| Err(anyhow::anyhow!("The live dictation crashed"))),
            None => Err(anyhow::anyhow!("The live dictation didn't start")),
        }
    }

    fn stop_tap(&self) {
        self.app
            .state::<Arc<TranscriptionManager>>()
            .stream_router()
            .set_tap(None);
    }
}

impl Drop for LiveRemote {
    /// Dropped without [`finish`](Self::finish): the worker stops once its
    /// channel closes, after any phrase it is sending.
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.stop_tap();
    }
}

fn run(
    rx: mpsc::Receiver<Msg>,
    settings: AppSettings,
    mut on_phrase: OnPhrase,
    stopped: Arc<AtomicBool>,
) -> anyhow::Result<Finished> {
    let mut cutter = PhraseCutter::default();
    let mut phrases: Vec<String> = Vec::new();
    // After the server fails once, the rest is sent together at the end, so
    // a down server doesn't cost a timeout at every pause.
    let mut server_failed = false;

    let mut send = |audio: Vec<f32>,
                    phrases: &mut Vec<String>,
                    last: bool|
     -> anyhow::Result<Option<String>> {
        let seconds = audio.len() as f64 / RATE as f64;
        let started = Instant::now();
        let text = tauri::async_runtime::block_on(crate::remote_transcription::transcribe(
            &settings, &audio,
        ))?;
        debug!(
            "Live remote phrase: {:.1}s of audio in {:?}",
            seconds,
            started.elapsed()
        );
        if text.is_empty() {
            return Ok(None);
        }
        let part = next_part(phrases, &text);
        phrases.push(text);
        if !last && !stopped.load(Ordering::Acquire) {
            on_phrase(&part, &join_transcripts(phrases));
        }
        Ok(Some(part))
    };

    loop {
        let phrase = match rx.recv_timeout(POLL) {
            Ok(Msg::Frame(at, pcm)) => cutter.push(at, &pcm),
            Err(RecvTimeoutError::Timeout) => cutter.poll(Instant::now()),
            Ok(Msg::Finish) => {
                let rest = cutter.finish();
                let last_part = if rest.is_empty() {
                    String::new()
                } else {
                    send(rest, &mut phrases, true)?.unwrap_or_default()
                };
                return Ok(Finished { phrases, last_part });
            }
            // Dropped without finishing: cancelled.
            Err(RecvTimeoutError::Disconnected) => {
                return Err(anyhow::anyhow!("The live dictation was cancelled"))
            }
        };
        let Some(audio) = phrase else { continue };
        if server_failed {
            cutter.keep(audio);
            continue;
        }
        let kept = audio.clone();
        if let Err(e) = send(audio, &mut phrases, false) {
            warn!("Live remote phrase failed; the rest goes at the end: {}", e);
            server_failed = true;
            cutter.keep(kept);
        }
    }
}

/// The text to add for `phrase` after `before`: with a space between them
/// unless either side is a script written without spaces.
fn next_part(before: &[String], phrase: &str) -> String {
    if before.is_empty() {
        return phrase.to_string();
    }
    let old = join_transcripts(before);
    let mut all = before.to_vec();
    all.push(phrase.to_string());
    let new = join_transcripts(&all);
    match new.strip_prefix(&old) {
        Some(part) => part.to_string(),
        None => format!(" {phrase}"),
    }
}

/// Cuts the recording into phrases at pauses.
#[derive(Default)]
struct PhraseCutter {
    phrase: Vec<f32>,
    last_frame: Option<Instant>,
    /// Quiet samples at the end of `phrase`.
    quiet: usize,
}

impl PhraseCutter {
    /// Adds a frame that arrived at `at`. Returns the phrase before it when
    /// the gap since the last frame was a pause.
    fn push(&mut self, at: Instant, pcm: &[f32]) -> Option<Vec<f32>> {
        let mut cut = None;
        if let Some(last) = self.last_frame {
            if at.saturating_duration_since(last) >= PAUSE_GAP {
                cut = self.cut();
            }
        }
        self.last_frame = Some(at);
        self.phrase.extend_from_slice(pcm);
        if rms(pcm) < QUIET_RMS {
            self.quiet += pcm.len();
        } else {
            self.quiet = 0;
        }
        if cut.is_none()
            && (self.quiet >= QUIET_PAUSE_SAMPLES || self.phrase.len() >= MAX_PHRASE_SAMPLES)
        {
            cut = self.cut();
        }
        cut
    }

    /// With no frame since the last one, whether the wait has become a pause.
    fn poll(&mut self, now: Instant) -> Option<Vec<f32>> {
        let last = self.last_frame?;
        if now.saturating_duration_since(last) >= PAUSE_GAP {
            self.cut()
        } else {
            None
        }
    }

    /// The phrase so far, if it's long enough to send on its own.
    fn cut(&mut self) -> Option<Vec<f32>> {
        if self.phrase.len().saturating_sub(self.quiet) < MIN_PHRASE_SAMPLES {
            return None;
        }
        self.quiet = 0;
        Some(std::mem::take(&mut self.phrase))
    }

    /// Puts a phrase that couldn't be sent back in front of what came after.
    fn keep(&mut self, mut audio: Vec<f32>) {
        audio.append(&mut self.phrase);
        self.phrase = audio;
    }

    /// Everything not yet cut.
    fn finish(&mut self) -> Vec<f32> {
        self.quiet = 0;
        std::mem::take(&mut self.phrase)
    }
}

fn rms(pcm: &[f32]) -> f32 {
    if pcm.is_empty() {
        return 0.0;
    }
    (pcm.iter().map(|s| s * s).sum::<f32>() / pcm.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: usize = 512;
    const FRAME_TIME: Duration = Duration::from_millis(32);

    fn speech() -> Vec<f32> {
        (0..FRAME).map(|i| ((i as f32) * 0.3).sin() * 0.2).collect()
    }

    /// Pushes `seconds` of speech frames in real time from `at`; returns the
    /// time after them and any phrases cut.
    fn talk(cutter: &mut PhraseCutter, mut at: Instant, seconds: f64) -> (Instant, Vec<usize>) {
        let mut cuts = Vec::new();
        for _ in 0..(seconds * RATE as f64 / FRAME as f64) as usize {
            if let Some(phrase) = cutter.push(at, &speech()) {
                cuts.push(phrase.len());
            }
            at += FRAME_TIME;
        }
        (at, cuts)
    }

    #[test]
    fn a_gap_in_frames_cuts_a_phrase() {
        let mut cutter = PhraseCutter::default();
        let (at, cuts) = talk(&mut cutter, Instant::now(), 2.0);
        assert!(cuts.is_empty());
        assert!(cutter.poll(at + Duration::from_millis(100)).is_none());
        let phrase = cutter.poll(at + PAUSE_GAP).expect("a pause cuts");
        assert!(phrase.len() >= 2 * RATE - FRAME);
        assert!(cutter.finish().is_empty());
    }

    #[test]
    fn a_short_phrase_waits_for_the_next_pause() {
        let mut cutter = PhraseCutter::default();
        let (at, _) = talk(&mut cutter, Instant::now(), 0.5);
        assert!(cutter.poll(at + PAUSE_GAP).is_none());
        let (at, cuts) = talk(&mut cutter, at + PAUSE_GAP * 2, 1.0);
        assert!(cuts.is_empty());
        let phrase = cutter.poll(at + PAUSE_GAP).expect("now long enough");
        assert!(phrase.len() >= (1.5 * RATE as f64) as usize - 2 * FRAME);
    }

    #[test]
    fn quiet_audio_cuts_without_a_gap() {
        let mut cutter = PhraseCutter::default();
        let (mut at, _) = talk(&mut cutter, Instant::now(), 1.5);
        let silence = vec![0.0; FRAME];
        let mut cut = None;
        for _ in 0..30 {
            cut = cut.or(cutter.push(at, &silence));
            at += FRAME_TIME;
        }
        assert!(cut.is_some());
    }

    #[test]
    fn long_speech_is_cut_anyway() {
        let mut cutter = PhraseCutter::default();
        let (_, cuts) = talk(&mut cutter, Instant::now(), 30.0);
        assert_eq!(cuts.len(), 1);
        assert!(cuts[0] >= MAX_PHRASE_SAMPLES);
    }

    #[test]
    fn kept_audio_goes_first() {
        let mut cutter = PhraseCutter::default();
        cutter.push(Instant::now(), &[0.5; 4]);
        cutter.keep(vec![0.1; 2]);
        assert_eq!(cutter.finish(), vec![0.1, 0.1, 0.5, 0.5, 0.5, 0.5]);
    }

    /// Sends the recordings in `HANDY_TEST_WAVS` (comma-separated) to a real
    /// server as one dictation, with a pause between them, the way a live
    /// dictation does. Also needs `HANDY_TEST_SERVER_URL` and
    /// `HANDY_TEST_SERVER_TOKEN`. Run with `--ignored --nocapture`.
    #[test]
    #[ignore]
    fn live_phrases_with_a_real_server() {
        let mut settings = crate::settings::get_default_settings();
        settings.remote_transcription_enabled = true;
        settings.remote_transcription_url = std::env::var("HANDY_TEST_SERVER_URL").unwrap();
        settings.remote_transcription_token =
            crate::settings::ServerToken(std::env::var("HANDY_TEST_SERVER_TOKEN").unwrap());
        let wavs = std::env::var("HANDY_TEST_WAVS").unwrap();

        let (tx, rx) = mpsc::channel();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let seen_by_callback = Arc::clone(&seen);
        let on_phrase: OnPhrase = Box::new(move |part: &str, all: &str| {
            println!("phrase part: {part:?}; all: {all:?}");
            seen_by_callback.lock().unwrap().push(part.to_string());
        });
        let worker =
            thread::spawn(move || run(rx, settings, on_phrase, Arc::new(AtomicBool::new(false))));

        let mut at = Instant::now();
        for wav in wavs.split(',') {
            let samples = crate::audio_toolkit::read_wav_samples(wav).unwrap();
            for frame in samples.chunks(FRAME) {
                tx.send(Msg::Frame(at, frame.to_vec())).unwrap();
                at += FRAME_TIME;
            }
            at += Duration::from_secs(1);
        }
        tx.send(Msg::Finish).unwrap();
        let finished = worker.join().unwrap().unwrap();
        println!("phrases: {:?}", finished.phrases);
        println!("last part: {:?}", finished.last_part);
        let count = wavs.split(',').count();
        assert_eq!(finished.phrases.len(), count);
        assert_eq!(seen.lock().unwrap().len(), count - 1);
    }

    #[test]
    fn parts_join_with_spaces() {
        assert_eq!(next_part(&[], "Hello."), "Hello.");
        assert_eq!(
            next_part(&["Hello.".into()], "How are you?"),
            " How are you?"
        );
        assert_eq!(next_part(&["你好".into()], "世界"), "世界");
    }
}
