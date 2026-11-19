# Handoff: Handy transcription server and remote client

This document is for a coding agent (or a person) who continues this work.
Read `AGENTS.md` first; its rules on settings, notices, translations and
bindings apply here.

## Goal

One computer (a MacBook with an M4 Max) runs Handy with a large model on its
GPU. Another computer (a Windows PC) dictates with Handy as usual —
shortcut, overlay, history, paste — but sends each recording to the Mac and
pastes the text that comes back, instead of running a local model.

Both sides are built into Handy, so one build can act as server or client:

- **Transcription Server** (Advanced settings): lets other computers send
  audio to this Handy. Built and tested.
- **Remote Transcription** (Advanced settings): sends this Handy's
  recordings to a server. Built and tested against the real server; not yet
  tried on the Windows PC.

## Repository state

- Fork: `https://github.com/valery-judah/Handy`, branch
  `feat/transcription-server`, on top of upstream `v0.10.1`. Upstream is
  `cjpais/Handy` (remote `origin`). The fork is remote `fork`, over HTTPS; in
  a fresh checkout, add it with
  `git remote add fork https://github.com/valery-judah/Handy.git`.
- Pushed commits:
  1. `feat: add a transcription server ...`
  2. `chore(ci): add unsigned Mac and Windows builds for this fork`
- The remote client and this document may not be committed yet. Check
  `git status` and `git log fork/feat/transcription-server`.

### Where the code is

| File                                                                     | What it holds                                                                                                                                                                                                         |
| ------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `src-tauri/src/transcription_server.rs`                                  | The server (axum): routes, token check, decoding the upload, transcribing it, its commands and tests                                                                                                                  |
| `src-tauri/src/remote_transcription.rs`                                  | The client: WAV encoding, the HTTP request, readable errors, the guard against using this computer's own server, its commands and tests                                                                               |
| `src-tauri/src/remote_live.rs`                                           | Live remote dictation: cuts the recording into phrases at pauses, sends each one, shows or types its text. Its tests include an ignored one that sends recordings to a real server (`HANDY_TEST_WAVS`).               |
| `src-tauri/src/settings.rs`                                              | `transcription_server_enabled`, `_port` (8765), `_token`; `remote_transcription_enabled`, `_url`, `_token`; and `ServerToken`, a string that `Debug` prints as `[REDACTED]`                                           |
| `src-tauri/src/actions.rs`                                               | Dictation. `TranscribeAction::start` (line 528 on) skips the local model and live streaming in remote mode. `TranscribeAction::stop` (line 796) sends the recording to the server, and a cancel stops waiting for it. |
| `src-tauri/src/managers/transcription_jobs.rs`                           | History "transcribe again" and file transcription: line 319 sends the whole recording to the server in remote mode                                                                                                    |
| `src-tauri/src/managers/transcription.rs`                                | `hold_for_server_request()`: keeps the model loaded while a server request runs                                                                                                                                       |
| `src-tauri/src/lib.rs`                                                   | Module declarations, `transcription_server::start_at_launch` in `initialize_core_logic`, the 8 commands in `collect_commands!`                                                                                        |
| `src/components/settings/TranscriptionServer.tsx`                        | Server settings: switch, port, server URL, token, new token                                                                                                                                                           |
| `src/components/settings/RemoteTranscription.tsx`                        | Client settings: switch, server URL, token (password field), Test Connection                                                                                                                                          |
| `src/components/settings/advanced/AdvancedSettings.tsx`                  | The two settings groups                                                                                                                                                                                               |
| `src/stores/settingsStore.ts`                                            | Updaters for the new settings                                                                                                                                                                                         |
| `src/i18n/locales/*/translation.json`                                    | `settings.advanced.transcriptionServer.*`, `settings.advanced.remoteTranscription.*`, `settings.advanced.groups.server`, `settings.advanced.groups.remote` (English text in every locale)                             |
| `.github/workflows/fork-build.yml`, `src-tauri/tauri.unsigned.conf.json` | Unsigned fork builds                                                                                                                                                                                                  |

## Server API

The server follows OpenAI's speech-to-text API. It listens on all network
interfaces, port 8765 by default. Plain HTTP.

**Base URL:** `http://<server IP>:<port>/v1`. The server's settings show it.

**Auth:** every endpoint except `/health` needs
`Authorization: Bearer <token>`. The token is 32 hex characters, created when
the server is first turned on. A missing or wrong token gets `401`.

| Endpoint                        | Request                                                                                                                                                                                                                       | Response                                                                                               |
| ------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| `GET /health`                   | —                                                                                                                                                                                                                             | `{"status":"ok"}`                                                                                      |
| `GET /v1/models`                | —                                                                                                                                                                                                                             | `{"object":"list","data":[{"id":"<selected model id>","object":"model","owned_by":"handy"}]}`          |
| `POST /v1/audio/transcriptions` | `multipart/form-data`: `file` (required; WAV, MP3, M4A, FLAC, OGG/Opus, ...), `response_format` (optional: `json` default, `verbose_json`, `text`). `model`, `language`, `prompt` and `temperature` are accepted and ignored. | `json`: `{"text":"..."}`. `verbose_json`: `{"task","language","duration","text"}`. `text`: plain text. |

**Errors** use OpenAI's shape, `{"error":{"message":"...","type":"..."}}`:
`authentication_error` (401), `invalid_request_error` (400: no `file` field,
unreadable audio, unsupported `response_format`) or `server_error` (500, or
503 when no model is selected on the server).

**Server behavior:**

- It transcribes with its own settings: its selected model, language,
  custom words and filler-word removal. It doesn't run its LLM
  post-processing.
- One request at a time; others wait. Dictation on the server computer goes
  first.
- Upload limit 512 MB. Long audio is cut at pauses and transcribed in chunks.
- The first request after the model was unloaded also loads it (about a
  second for Qwen3-ASR 1.7B on the M4 Max). After that, 5 s of speech takes
  about 0.2 s.

## Client behavior

With Remote Transcription on and a URL set:

- **Dictation** records and trims silence locally as usual (Silero VAD
  ships with Handy), then posts the recording as 16 kHz mono 16-bit WAV to
  `<url>/audio/transcriptions`. The URL works with or without `/v1`. The
  text goes through Handy's usual steps: history, optional LLM
  post-processing on this computer, the transcription hook, paste.
- **No local model is loaded**, and recording starts even if none is
  downloaded.
- **Text Delivery** (`remote_transcription_mode`) picks how the text comes
  back:
  - **Whole Recording** (`whole`, the default): the recording is sent when it
    stops, and its text is pasted. The overlay shows the small pill.
  - **Live Preview** (`preview`): while recording, each phrase is sent at a
    pause and its text shown in the live overlay. When the recording stops,
    the whole recording is sent again, so the pasted text has the full
    context.
  - **Type As You Speak** (`type`): each phrase is typed into the focused
    app as soon as its text comes back, without the trailing space or
    auto-submit; the last phrase is pasted at the stop with them.
    Post-processing is skipped, because the text is already typed. History
    keeps the whole text and recording.
- **Phrases** (`src-tauri/src/remote_live.rs`): the recorder's frames reach
  the phrase sender through a tap on `StreamRouter`. With VAD on, frames
  stop shortly after speech ends, so a 300 ms gap in frames is a pause (about
  0.75 s of quiet in all). With VAD off, 0.7 s of quiet frames is a pause.
  Phrases shorter than 1 s wait for the next pause; speech with no pause is
  cut at 25 s. If the server fails once, the rest is sent together at the
  stop. After a cancel, a phrase still coming back is neither shown nor
  typed.
- **Timeouts:** 5 s to connect, then 60 s plus the audio's length.
- **Errors** show the existing "Transcription Failed" notice with a reason,
  such as "Can't reach the Handy server at ...". The recording stays in
  History, and "transcribe again" there also uses the server.
- **Cancel** (the cancel shortcut) stops waiting for the server.
- **Own server:** a URL that points at this computer's own running server is
  refused with an error. Without that check, the request would wait for its
  own dictation to finish and hang until the timeout.
- **Test Connection** calls `GET <url>/models` and shows the server's model
  or the error.

## What's tested and what isn't

Tested on the Mac:

- Server: 11 `curl` requests from the LAN address covering health, missing
  and wrong token, models, WAV and M4A uploads, all three response formats,
  an unsupported format, a missing file and random bytes. It used Metal on
  the M4 Max, and temporary upload files were removed.
- Client: unit tests for the URL handling, WAV encoding, error messages, the
  own-server guard, and a request to a stand-in server. The ignored test
  `transcribe_with_a_real_server` got the right text back from the running
  server in 1.4 s; a wrong token and a stopped server gave readable errors.
- `cargo build`, the Rust tests above, `bun run lint`, `bun run build`,
  `bun run check:translations` and `cargo fmt` all pass.

Not tested yet:

- Dictation in remote mode from start to finish. One computer can't be its
  own server (see above), so this needs the Windows PC or another computer.
- The two settings sections on screen.
- The Windows build. The first Fork Build run built only the server.
- `cargo clippy`, which isn't installed on the Mac.

## Next steps

1. Commit the client and push the branch to the fork (see "Builds on GitHub
   Actions").
2. Install the Windows build from that run on the Windows PC. First-run
   onboarding still asks for a model: pick the smallest (Canary 180M).
3. On the Mac, turn on Transcription Server and copy its URL and token. On
   Windows, turn on Remote Transcription, paste both, and click Test
   Connection.
4. Dictate on Windows. Check the "Done when" list below.

Later, if wanted:

- **Onboarding:** a "use a server" choice in the model step, so the Windows
  PC doesn't need a model download. The step is in
  `src/components/onboarding/`.
- **Fallback** to the local model when the server can't be reached, in
  `TranscribeAction::stop`.
- **Sending a language** to the server. The server would have to accept the
  `language` field and pass it to the transcription; today it uses its own
  setting.
- **Finding the server by name:** the URL is an IP address, so reserve the
  Mac's address in the router.

## Build and test

### On the Mac (macOS, Apple Silicon)

This setup avoids installing tools system-wide:

- Bun is in `.tools.local/` (ignored by git). Put it first on `PATH`:
  `export PATH="$PWD/.tools.local/node_modules/.bin:$PATH"`.
- `rustfmt` and `clippy` aren't installed locally. Run `cargo fmt` in the
  Rust dev container image:
  `docker run --rm -v "$PWD":/w -w /w/src-tauri mcr.microsoft.com/devcontainers/rust:2-1-trixie cargo fmt`.
- Rust builds need `CMAKE_POLICY_VERSION_MINIMUM=3.5`.

```bash
cd src-tauri && CMAKE_POLICY_VERSION_MINIMUM=3.5 cargo build
```

```bash
cd src-tauri && CMAKE_POLICY_VERSION_MINIMUM=3.5 cargo test --lib -- remote_transcription transcription_server settings
```

```bash
bun run lint && bun run build && bun run check:translations
```

```bash
CMAKE_POLICY_VERSION_MINIMUM=3.5 bun run tauri dev
```

`src/bindings.ts` is generated. After changing Rust commands or settings,
run the debug binary once; `--list-models` writes the bindings and exits
without opening a window:

```bash
cd src-tauri && ./target/debug/handy --list-models
```

The dev build shares the app ID `com.pais.handy` and the settings file
(`~/Library/Application Support/com.pais.handy/settings_store.json`) with
an installed Handy. Quit the installed one first, or the dev build hands off
to it and exits. `tauri dev` also rebuilds and restarts the app whenever a
Rust file changes.

**Testing the client against the real server**, with the server running in
the dev build:

```bash
cd src-tauri && HANDY_TEST_SERVER_URL=http://192.168.0.21:8765/v1 HANDY_TEST_SERVER_TOKEN=<token> HANDY_TEST_WAV=/path/to/speech.wav cargo test --lib transcribe_with_a_real_server -- --ignored --nocapture
```

Make a test WAV with `say -o speech.aiff "..."` and
`afconvert -f WAVE -d LEI16@16000 -c 1 speech.aiff speech.wav`. Test the
server alone with:

```bash
curl -H "Authorization: Bearer $TOKEN" -F file=@speech.wav http://192.168.0.21:8765/v1/audio/transcriptions
```

### Builds on GitHub Actions

Every push to `feat/transcription-server` runs `Fork Build`
(`.github/workflows/fork-build.yml`). It builds an unsigned Apple Silicon
`.app`/`.dmg` and an x64 Windows `.msi`/`.exe`. It uses
`src-tauri/tauri.unsigned.conf.json`, which turns off Windows code signing
and updater signatures, and points the updater at the fork's releases so a
build never offers to replace itself with upstream Handy. A build takes
20–40 minutes, and a new push cancels the run in progress.

The local SSH key belongs to another GitHub account, so push over HTTPS with
the `gh` login:

```bash
git -c credential.helper= -c 'credential.helper=!gh auth git-credential' push fork feat/transcription-server
```

```bash
gh run list -R valery-judah/Handy --branch feat/transcription-server --limit 3
```

```bash
gh run download <run id> -R valery-judah/Handy -D ./artifacts
```

Don't push `main` to the fork: that starts `main-build.yml`, which needs
upstream's signing keys and fails.

**Unsigned apps:** on the Mac, run `xattr -dr com.apple.quarantine Handy.app`
before opening it. On Windows, SmartScreen asks; choose "More info → Run
anyway".

## Network setup

- On the Mac, allow incoming connections for Handy when macOS asks.
- Reserve the Mac's IP address in the router, so the URL on Windows keeps
  working.
- Traffic is plain HTTP, so the token and audio aren't encrypted. Keep it on
  the home network; don't forward the port to the internet.

## Done when

- With Remote Transcription on and the Mac's server on, dictating on the
  Windows PC pastes the Mac's transcription.
- With the Mac asleep or unreachable, dictation shows "Transcription Failed"
  with a reason, and "transcribe again" in History works once the Mac is
  back.
- With Remote Transcription off, Handy on Windows works exactly as before.
- The Fork Build run for the latest commit passes on both platforms.

## Prompt to start a coding agent

> Read `AGENTS.md` and `docs/remote-transcription-handoff.md` in this repo,
> on branch `feat/transcription-server`. The transcription server and the
> remote client are built. Check `git status`, commit anything left with
> conventional commit messages, and push the branch to the `fork` remote as
> the handoff describes. Then report the Fork Build result and help me test
> remote dictation on the Windows PC against the "Done when" list. Fix what
> fails, running the checks in "Build and test" before each push.
