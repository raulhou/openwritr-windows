# OpenWritr for Windows

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Windows 11](https://img.shields.io/badge/Windows-11_24H2-0078D4?logo=windows)](https://github.com/trsdn/openwritr-windows)
[![Rust](https://img.shields.io/badge/Rust-1.85-DEA584?logo=rust&logoColor=white)](https://www.rust-lang.org)
[![Snapdragon X Elite](https://img.shields.io/badge/Snapdragon-X_Elite-3253DC?logo=qualcomm&logoColor=white)](https://www.qualcomm.com/products/mobile/snapdragon/pcs-and-tablets/snapdragon-x-elite)
[![Hexagon NPU](https://img.shields.io/badge/Hexagon-NPU-FF2A00?logo=qualcomm&logoColor=white)](https://huggingface.co/trsdn/parakeet-tdt-0.6b-v3-htp-int8-8s)
[![Release](https://img.shields.io/github/v/release/trsdn/openwritr-windows)](https://github.com/trsdn/openwritr-windows/releases/latest)

Push-to-talk voice-to-text for Windows. Local transcription uses **NVIDIA
Parakeet TDT 0.6B v3** on CPU or Qualcomm Hexagon NPU, plus native
**Whisper Large v3 Turbo** on the Snapdragon X Elite NPU.
Optional LLM cleanup via **GitHub Copilot** (Claude Haiku 4.5, GPT-5 Mini,
GPT-4.1), any OpenAI-compatible endpoint, or a **model that runs on your PC**
(on the Snapdragon NPU via Qualcomm GenieX, or a local server such as LM Studio).

**[Microsoft Store](https://apps.microsoft.com/detail/9MSQWR701P2Q)** · **[Website](https://trsdn.github.io/openwritr-windows/)** · **[Releases](https://github.com/trsdn/openwritr-windows/releases)** · **[NPU model on HF](https://huggingface.co/trsdn/parakeet-tdt-0.6b-v3-htp-int8-8s)** · **[macOS sibling](https://github.com/trsdn/OpenWritr)**

## Quick start

**Easiest: [get it from the Microsoft Store](https://apps.microsoft.com/detail/9MSQWR701P2Q).**
One click, it installs the right build for your CPU automatically, keeps
itself updated, and there's no SmartScreen warning.

Prefer a direct download? Pick the build for your CPU, both per-user (no
admin / UAC required):

| Your machine | Build | Engine |
|---|---|---|
| **Snapdragon X Elite** (Surface Pro 11, etc.) | `…-arm64-…` | Parakeet CPU/NPU and Whisper NPU |
| **Intel / AMD** laptop | `…-x64-…` | CPU INT8 |

Not sure? Snapdragon laptops report "ARM-based processor" in Settings →
System → About. Everything else is x64.

**Installer.** Download
`openwritr-windows-<arch>-vX.Y.Z-setup.exe` from
[Releases](https://github.com/trsdn/openwritr-windows/releases/latest)
and run it. Sets up the Start Menu shortcut, an optional autostart-at-logon
entry, and a proper uninstaller you'll find under Settings → Apps. You can
turn autostart on or off any time later under Settings → Startup.

**Portable zip.** Download `openwritr-windows-<arch>-vX.Y.Z.zip` and unzip
it into **any folder you like** (e.g. `C:\Tools\OpenWritr\`), then run
`openwritr.exe`. The app finds its DLLs next to the exe — the install
location doesn't matter. Same binaries as the installer, just no shortcuts.
Autostart is available here too: enable "Run at startup" under Settings →
Startup, and OpenWritr repairs the entry automatically if you later move the
folder.

> **Note:** user data (settings, downloaded models, logs) always lives under
> `%LOCALAPPDATA%\OpenWritr\` — the app creates that folder automatically on
> first run, you never need to create it yourself. `AppData` is a hidden
> folder; if you want to look inside, paste `%LOCALAPPDATA%\OpenWritr` into
> the Explorer address bar.

Logs are under `%LOCALAPPDATA%\OpenWritr\logs`. Tray actions **Open logs**
and **Export diagnostics** provide bounded logs plus redacted runtime/model
status; diagnostics never include audio, transcript text, clipboard contents,
or API keys.

The x64 build runs Parakeet on the CPU (no Hexagon NPU on Intel/AMD); the
arm64 build adds the NPU engine. Both share the same multilingual model and
UX.

> **Windows SmartScreen warning.** The binaries are not code-signed (yet), so
> the first launch shows "Windows protected your PC". Click **More info →
> Run anyway**. To verify your download is authentic, compare its SHA-256
> against `SHA256SUMS.txt` attached to the release:
> `Get-FileHash .\openwritr-windows-<arch>-vX.Y.Z-setup.exe` in PowerShell.

The default engine is Parakeet CPU. Its verified model files are fetched on
first use into `%LOCALAPPDATA%\OpenWritr\models\` (~670 MB). Selecting an NPU
engine downloads its own pinned assets with visible progress and verification:
Parakeet NPU adds ~650 MB; Whisper NPU downloads a ~2.0 GB archive and uses
about 2.2 GB after extraction.

## Usage

| Combo | Action |
|---|---|
| **Hold Ctrl + Win** | Record. Release to transcribe and paste at the caret. |
| **Press or release Shift while recording** | Toggle cleanup live in **Additional Shift** mode; the overlay changes between Listening and Enhanced immediately. |
| **Press or release Shift while recording in Always mode** | Bypass or restore cleanup for the current recording. |
| **Tray right-click → Settings** | Change hotkey, engine, LLM provider. |

A small dark pill appears at the bottom-center of the primary monitor. Its
waveform and current state share one centered line, distinguishing raw and
enhanced listening, queued work, transcription, enhancement, success, safe raw
fallbacks, provider/credential warnings, and errors. Active recording always
takes priority over background job status. Settings changes take effect after
saving — no restart required.

### Transcribe files and meetings

Both live in the tray menu and run fully offline on the selected engine.

- **Transcribe audio or video file…** opens a file picker (WAV, MP3, M4A/AAC,
  FLAC, OGG, MP4/MOV/MKV with a supported audio track) and then a small
  progress window instead of the bottom-center indicator. The window shows the
  stage (reading audio, transcribing n/m, AI cleanup), elapsed time and an
  estimate, the source, length, engine, whether AI cleanup is on, the output
  format and file, and, when finished, the word count, a preview, and **Open
  file / Show in folder / Copy text** buttons. Minimize it like any window;
  closing it does not cancel the job. The transcript is saved next to the
  source as `<name>.txt` (or `.md`) and copied to the clipboard.
- **Start meeting recording / Stop meeting recording** captures your
  microphone and everything playing on the default output device (WASAPI
  loopback) into one timeline, up to 12 hours. **Pause meeting recording**
  stops capturing and releases the microphone (Windows' mic-in-use indicator
  turns off); **Resume** continues the same recording with no gap, and paused
  time is not recorded or counted. When you stop, OpenWritr asks whether to
  **also create meeting notes** (see below). The transcript is saved to
  `Documents\OpenWritr\Meetings\Meeting <date> <time>.txt` (named after when
  the meeting *started*) and copied to the clipboard. Quitting while a
  meeting is recording, or while the question is open, saves the transcript
  without notes.

  **Rolling transcription.** Meetings are transcribed while they are being
  recorded: every five minutes of audio, and whenever you pause, the finished
  part is cut off (at a quiet moment, so words are not split) and transcribed
  in the background while recording continues. As each part is ready its text
  is appended to `Meeting <date> <time> (in progress).txt` next to the final
  transcript, so you can read the meeting so far, and nothing is lost if the
  app or the PC dies mid-meeting. When you stop, only the last few minutes are
  left to transcribe; the parts are stuck together into one transcript, saved
  as the normal transcript file, and the in-progress file is deleted. Memory
  use stays around five minutes of audio however long the meeting runs, and
  the tray icon and recording indicator keep showing that you are recording
  (the tray tooltip mentions that earlier parts are being transcribed).
  Dictating with the hotkey still works during a meeting: it waits at most for
  the part being transcribed. Speaker labels stay consistent across parts
  (Speaker 1 in the first part is Speaker 1 in the last), and `[m:ss]` markers
  count from the start of the meeting. If a part cannot be transcribed, the
  transcript says so at that point and the rest is still delivered. If the
  engine is much slower than real time the backlog of untranscribed audio
  grows in memory (about 230 MB per hour of audio).

Long audio is split into windows of at most 30 s at the quietest nearby point
and transcribed in order. These jobs never paste. Transcription itself never
leaves the machine.

**Turning a meeting into a full report.** [`docs/meeting-report-prompt.md`](docs/meeting-report-prompt.md)
is a ready-made prompt for Claude: give it the saved transcript (with
`[m:ss] Speaker N:` markers) plus the agenda, slides and other material from
the meeting and it builds a single self-contained HTML file with an executive
summary, decisions, action items, a topic-by-topic digest, a materials
cross-reference, and the full verbatim transcript with a searchable index.

**Recording indicator.** While a meeting is being recorded, a pill in the
same style as the dictation indicator stays on screen, in exactly the same
place: red `Recording 12:34` with live level bars, or amber `Paused 12:34`.
When the dictation pill needs the spot (you dictate, a job is queued, or a
notice appears) the meeting pill steps aside and comes straight back when it
clears. Turn it off under *Settings → Output and feedback → Show a recording
indicator during meeting recordings*, or flip it for the current session with
**Hide / Show recording indicator** in the tray menu (the tray menu does not
change the saved setting). The tray icon still turns red (recording) or amber
(paused) either way, and hiding the indicator mid-meeting shows a short notice
so it is never silent.

**AI cleanup.** If text enhancement is set to anything other than **Never**
(Settings → Text enhancement), file and meeting transcripts are also cleaned up
by your configured provider (GitHub Copilot or an OpenAI-compatible API). The
transcript is sent in slices of up to ~3,000 characters, split at sentence ends
and returned as paragraphs. Each slice is checked by the same integrity
validator as dictation; a slice that fails keeps its raw text, and if no slice
can be cleaned the raw transcript is saved with a warning. Set enhancement to
**Never** to keep these transcripts fully offline. Note that cleanup sends the
transcript text to the provider you chose, so avoid it for confidential
meetings unless that provider is acceptable.

**Meeting notes.** Under *Settings → Output and feedback* choose what happens
when a meeting recording stops: **Ask every time** (the default), **Always**
create notes, or **Never**. With an AI provider configured (Settings → Text
enhancement), *Ask every time* shows *Also create meeting notes with AI?* **Yes** saves, next to the transcript, a separate
`Meeting notes <date> <time>.md` with a *Summary*, *Key points*, *Decisions*,
*Action items* (a `- [ ]` checklist) and *Open questions*. **No** saves the
transcript only. **Always** creates the notes without asking, also when you quit mid-meeting; **Never** never asks. Without a provider the question is skipped and a hint is shown. The prompt forbids
inventing names, owners, dates or decisions, but speakers are not labelled and
recognition errors can slip through, so review notes before acting on them.
**When the connection is unreliable.** Cleanup never holds up your transcript.
A request that cannot reach the provider keeps that section as recognised;
after three failures in a row the rest is skipped instead of waiting out each
timeout, and for the next minute no further requests are attempted (so
dictation is not held up either). Sections that could not be cleaned during a
meeting get a second try when you press Stop (for up to 90 seconds, and only
while the provider answers). Whatever is still unfinished is retried
**automatically in the background** after the transcript is saved, 2, 5, 10,
20 and 30 minutes later, and the saved transcript (and meeting notes, if they
could not be generated) are updated in place when it works. The saved-file
notice tells you what happened, for example *cleaned 21 of 25 sections;
retrying automatically*. OpenWritr leaves a file alone if you have edited,
moved or deleted it, and the retries stop when you quit the app (the
transcript stays as recognised; the in-progress and saved files are never
lost).

Notes work even when cleanup is set to **Never**; if cleanup is on it runs
first and the notes are built from the cleaned text. If the provider fails, the
transcript is still saved and the notice says the notes are missing. Very long
meetings are noted in parts and merged. The whole transcript is sent to the
provider, so answer *No* where that is not acceptable.

**In-person meetings.** Recording works without a call: with nothing playing,
only the microphone contributes. Use the best input device Windows offers (a
conference or USB mic beats a laptop mic), keep it near the speakers, and
expect quieter or distant voices to be missed. Overlapping speech and room
echo reduce accuracy, and speakers are not separated.

**Speaker labels.** Turn on *Label speakers in file and meeting transcripts*
(Settings → Output and feedback) and transcripts are split by who is talking:

```
[0:00] Speaker 1: Thanks everyone for joining…

[0:12] Speaker 2: Happy to be here. Can we start with the roadmap?
```

The `[m:ss]` marker is the time into the recording where that person starts
talking (paused time in a meeting is not counted).

OpenWritr finds the speaker turns with two small on-device models (pyannote
segmentation 3.0 and a WeSpeaker voice-print model, about 32 MB, downloaded the
first time you use it), then transcribes each turn separately. It runs on the
CPU, takes roughly a minute or two per hour of audio on a recent PC, and works
for files and meeting recordings alike. Labels never remove speech: audio the models can't
attribute (a short "yes", a first word at a segment edge) is transcribed with
the neighbouring speaker, so a labelled transcript has every word an unlabelled
one would. Labels are numbered in order of first
appearance and are **not** names. Accuracy drops with overlapping speech, very
short interjections, similar voices and poor audio, and a voice is sometimes
split into two labels. If only one speaker is found the transcript is left
unlabelled; if the models cannot be downloaded or run, you still get the normal
transcript and a note saying labels were unavailable. AI cleanup keeps the
labels and markers (the markers are held back from the AI request and put back
afterwards), and meeting notes may attribute action items to "Speaker N".

**Managing the speaker-label models.** *Settings → Speaker labels* holds the
on/off switch and the local model status: *Installed*, *Not downloaded* or
*Incomplete*, the size, and the folder they live in. **Download and install**
fetches them now (with a progress bar and **Cancel**) instead of waiting for
the first labelled transcript, **Verify files** re-checks sizes and SHA-256
hashes, and **Open model folder** opens the install location. For machines
without direct access, expand *Install manually*: it lists each file with its
exact name, size, hash and download link; save the files into the model folder
and click **Verify files**. Files that do not match the pinned hashes are
rejected.

**Markdown output.** Turn on *Save file and meeting transcripts as Markdown*
(Settings → Output and feedback) to get `.md` files instead of `.txt`: a `#`
title, an italic details line (source file or meeting length), and paragraphs.
The formatting is added by OpenWritr itself, not requested from the AI
provider, so the cleanup integrity checks are unaffected and the same words
are kept in both formats.

Record meetings only where everyone present has agreed to it.

## Settings

Tray icon → right-click → **Settings**. All fields:

- **Recording shortcut**: Ctrl / Shift / Alt / Win modifiers plus an optional
  trigger key. Modifier-only shortcuts require at least two modifiers; F13-F20
  may be used alone. The dialog shows a live shortcut preview and prevents
  ambiguous Shift-enhancement combinations. With Ctrl+Win held, Shift can be
  pressed or released during the recording to change the enhancement intent
  immediately. Default: Ctrl+Win.
- **Transcription engine**: Parakeet CPU INT8 (default), Parakeet NPU, or
  Whisper Large v3 Turbo NPU. Each choice shows hardware availability,
  first-use download size, and local cache status. NPU engines remain visible
  but disabled unless the ARM64 build is running on Snapdragon X Elite.
- **Output and feedback**: either paste into the active app or keep the
  transcript on the clipboard. Auto-paste is tied to the foreground top-level
  window captured when recording starts. If focus or the target process
  changes, or another recording is active when the job completes, OpenWritr
  does not inject keystrokes: it copies the final text to the clipboard and
  shows a warning. Copy-only mode always remains explicit. The overlay option
  covers both the recording waveform and typed job/status messages.
  Start/stop sounds are optional.
- **Text enhancement**: choose **Never**, **When Shift is additionally held**,
  or **Always**, then select GitHub Copilot, an OpenAI-compatible API, or a
  **Local model (on this PC)** (see below).
  Shift is monitored throughout the recording, so the mode and purple overlay
  indicator update immediately instead of being fixed at the initial key
  press. In **Always** mode, additional Shift bypasses cleanup while held. The
  model picker provides advisory presets while preserving
  arbitrary provider-specific model or deployment IDs and legacy settings.
  GitHub mode uses the account GitHub Copilot CLI is signed in with
  (`winget install GitHub.Copilot`, then `copilot login`), found in the same
  order the CLI uses: the `COPILOT_GITHUB_TOKEN`, `GH_TOKEN` or `GITHUB_TOKEN`
  variable, its Windows Credential Manager entry, its config file, and finally
  the GitHub CLI (`gh auth login`). OpenAI-compatible
  mode requires a valid base URL, model ID, and API key. API keys are stored in
  Windows Credential Manager, not in the settings file. Cleanup prompts can be
  customized per exact provider, endpoint, and model target; transcript text
  stays a separate untrusted user message. Provider/credential failures use
  the raw transcript with a provider warning. Empty or critically changed
  output, or an integrity-validator failure, also falls back to the raw
  transcript with a distinct safety warning.
- **Advanced**: set the maximum recording duration.
- Explanatory settings copy is kept behind immediate `i` hover help so the
  dialog remains compact; warnings, failures, and live availability remain
  visible.
- **About, credits & support** (also available directly from the tray menu):
  view the installed version and architecture,
  official website, Microsoft Store, source, releases, issue reporting,
  privacy and license links, model/runtime credits, local third-party notices,
  and guidance for exporting privacy-safe diagnostics.

Settings are stored at `%LOCALAPPDATA%\OpenWritr\settings.json`. The app polls
the file's mtime so external edits also take effect live. Writes use an atomic
replacement, and legacy plaintext API keys are migrated only after the secure
credential can be written and read back successfully. Unsaved changes require
confirmation before the Settings window closes.

### Local model for AI cleanup (NPU)

Settings → Text enhancement → **Local model (on this PC)** runs cleanup and
meeting notes on a model that never leaves your computer. Two sources:

- **Built-in model (NPU)** – on a Snapdragon (Windows on ARM) PC, OpenWritr
  uses Qualcomm's [GenieX](https://github.com/qualcomm/GenieX) to run
  `ai-hub-models/Qwen3-4B-Instruct-2507` on the Hexagon NPU. In Settings press
  **Install GenieX…** (downloads the pinned official v0.8.0 installer, checks its
  SHA-256, installs for your Windows user only, adds `geniex` to your PATH),
  then **Download model** (several GB, resumable), then **Test model**.
  OpenWritr starts `geniex serve` on a private port (18651) when a cleanup needs
  it and ties it to the app so the NPU is never left occupied. It is stopped as
  soon as a file or meeting transcript is finished, after 5 idle minutes
  following dictation, and whenever it stops answering in time or suddenly
  writes far slower than it did (the next cleanup starts a fresh one). If the
  server cannot start, cleanup falls back to the raw text exactly like an
  unreachable hosted provider.
- **Another local server** – any OpenAI-compatible server on this PC
  (a GenieX server you run yourself, LM Studio, Ollama, Foundry Local). Presets,
  **Find models**, **Test connection**, and a setup guide are in Settings. Only
  `localhost` / `127.0.0.1` / `::1` addresses are accepted here; no API key is
  needed unless the server asks for one.

Local models are slower and weaker than hosted ones, so OpenWritr sends them
smaller slices (about 1,200 characters for cleanup, 8,000 for meeting notes),
waits 8× longer for each answer, and removes `<think>…</think>` reasoning
blocks from replies. The same integrity checks as for hosted models apply.
Developer-preview note: GenieX is a Qualcomm developer preview; the pinned
version is isolated in `src/local_llm.rs`.

## Architecture

### Native Rust app (what ships in the release zip)

```
openwritr-windows/
├── src/
│   ├── main.rs              entry; dispatches `--settings` subprocess
│   ├── app.rs               winit event loop, tray, hotkey thread, ASR dispatch
│   ├── audio.rs             cpal WASAPI capture, multi-channel downmix
│   ├── credentials.rs       Windows Credential Manager API-key storage
│   ├── hotkey.rs            push-to-talk combo polling against key_hook state
│   ├── key_hook.rs          global WH_KEYBOARD_LL hook → atomic key bitmap
│   ├── overlay.rs           custom Win32 layered top-most window, GDI bars
│   ├── settings.rs          validation, migration, atomic JSON persistence
│   ├── settings_ui.rs       eframe/egui dialog (subprocess)
│   ├── asr/                 ONNX Runtime ASR pipelines
│   │   ├── parakeet.rs      CPU/NPU Parakeet pipeline
│   │   ├── whisper_npu.rs   native chunked Whisper NPU engine
│   │   ├── whisper_mel.rs   128-bin Whisper log-mel frontend
│   │   ├── whisper_decoder.rs  QNN encoder/decoder and KV-cache loop
│   │   └── qnn_ffi.rs       reusable typed QNN C-API sessions
│   ├── enhance.rs           Copilot / OpenAI / local-model cleanup pass
│   ├── local_llm.rs         GenieX install, model download, private `geniex serve`
│   ├── sounds.rs            G3/E3 tone synth (start/stop pings)
│   └── bin/
│       ├── package.rs       distributable-zip builder
│       └── cleanup_eval/    opt-in offline/live evaluator for the `cleanup`
│                            core (see Development section below)
└── Cargo.toml
```

Key design decisions:
- **Global low-level keyboard hook.** Push-to-talk detection reads physical
  key state from `WH_KEYBOARD_LL` instead of `GetAsyncKeyState`. The OS
  synthesises key-ups during focus changes (PowerShell launched mid-recording,
  UAC prompt, system shortcut handler), which the polling API faithfully
  reports — and which would abort the recording. The LL hook sees only
  physical events.
- **Settings UI as subprocess.** The settings dialog is the same exe re-launched
  with `--settings`. Spawning happens from a worker thread because
  `CreateProcessW` on Windows ARM64 with Defender real-time scanning can block
  several seconds — doing it inline would freeze the tray pump.
- **Overlay on its own message loop.** Layered top-most window with color-key
  transparency, painted with double-buffered GDI. It reads the recorder
  atomics and receives enable/status updates through a command channel, so
  settings changes apply without restarting the app.
- **Multi-channel downmix in the audio callback.** The Qualcomm Aqstic mic
  array on Surface Pro exposes 4-8 interleaved channels at 48 kHz; we average
  to mono before resampling to 16 kHz.

### Python toolchain (`scripts/`)

NPU model preparation lives in `scripts/`. `build_npu_encoder.py`,
`aihub_compile_encoder.py`, `wrap_qnn_context_binary.py`, and
`test_npu_encoder.py` are build-time tools used to produce the .bin
hosted on HF. They are NOT invoked by the shipped `openwritr.exe` at
runtime — the native build pulls the pre-compiled binary directly.

The Rust app does not call into Python at runtime.

### Build-time Python dependency

Python is used only at build time to fetch hash-pinned runtime wheels and
validate the canonical release manifest. The resulting packages are
self-contained and Python-free.

```powershell
.\scripts\envup.ps1
cargo build --release --bins
cargo run --release --bin package
```

The package command fetches the exact runtime tuple, validates versions,
SHA-256 hashes, PE architecture, required files, and licenses, then stages
from `release-manifest.json`. The ZIP lands in `target/dist/`.

## NPU pipeline

Parakeet runs its pre-compiled encoder on the Hexagon HTP; mel preprocessing
and the TDT decoder stay on CPU. Whisper runs both its encoder and
autoregressive KV-cache decoder as pre-compiled QNN sessions. It processes
sequential 30-second chunks, detects language once per recording, and reuses
that language for every later chunk.

The compiled binary expects a fixed **8-second audio window**. For
push-to-talk utterances ≤ 8 s the encoder is run once on a padded window.
For longer audio the encoder is run in chunks (8 s window with 1 s
overlap), feature streams are stitched at the seam, and the TDT decoder
runs once over the concatenated features. Tested up to ~23 s without
boundary doubling.

### Measured on Snapdragon X Elite (X1E80100)

| Audio length | Decode (preproc + encode + TDT) | × Realtime | Chunks |
|---|---|---|---|
| 3 s | 128 ms | 23× | 1 |
| 5.8 s | 221 ms | 26× | 1 |
| 16.4 s | 375 ms | 44× | 3 |
| 23.0 s | 626 ms | 37× | 4 |

The encoder itself is ~67 ms ± 0 ms per 8-second window, independent of
the actual audio length within the window.

### How the binary was built

1. `scripts/build_npu_encoder.py` constant-folds the encoder's
   dynamic-shape attention mask (`Shape → Gather → Range → Expand`) against
   a frozen `[1, 128, 801]` input — the HTP backend cannot evaluate that
   subgraph as-is.
2. `scripts/aihub_compile_encoder.py` submits the static-shape FP32 ONNX
   plus FLEURS calibration samples to Qualcomm AI Hub. Quantize job uses
   INT8 weights / INT16 activations (the standard HTP recipe for
   transformer encoders); compile job targets `snapdragon_x_elite_crd`
   with `--target_runtime qnn_context_binary --truncate_64bit_io`.
3. `scripts/wrap_qnn_context_binary.py` wraps the resulting `.bin` in a
   408-byte EPContext-node ONNX so ORT's QNN EP can consume it.
4. `src/asr/qnn_ffi.rs` loads the wrapper via direct `ort_sys` C-API calls,
   bypassing `ort` 2.0-rc.12's session builders (which crash inside QnnHtp
   when consuming EPContext-wrapper ONNX).

### Required helper DLLs

The QNN backend loads several sibling DLLs by name at session-create time.
`scripts/fetch_runtime.py` extracts the exact, hash-pinned set recorded in
`runtime-manifest.json`. Alongside `onnxruntime_providers_qnn.dll`, the
X Elite path requires `QnnHtp.dll`, `QnnHtpPrepare.dll`,
`QnnHtpNetRunExtensions.dll`, `QnnSystem.dll`, the V73 stub and skeleton,
and its catalog file. Without the SKEL + `.cat` pair, the stub fails
`LoadLibrary` with `ERROR_MOD_NOT_FOUND` (126) and QnnHtp later aborts
session creation with `STATUS_STACK_BUFFER_OVERRUN` (0xC0000409) without
a useful error.

## Models

| Model | Provider | Size | License | Auto-downloaded? |
|---|---|---|---|---|
| Parakeet TDT 0.6B v3 (CPU INT8 ONNX + companion files) | [istupakov/parakeet-tdt-0.6b-v3-onnx](https://huggingface.co/istupakov/parakeet-tdt-0.6b-v3-onnx) | ~670 MB | CC-BY-4.0 | Yes, on first run |
| Parakeet TDT 0.6B v3 NPU encoder (QAIRT context binary + wrapper) | [trsdn/parakeet-tdt-0.6b-v3-htp-int8-8s](https://huggingface.co/trsdn/parakeet-tdt-0.6b-v3-htp-int8-8s) | ~632 MB | CC-BY-4.0 | Yes, on first NPU launch |
| Whisper Large v3 Turbo (QNN encoder, decoder, tokenizer) | [qualcomm/Whisper-Large-V3-Turbo](https://huggingface.co/qualcomm/Whisper-Large-V3-Turbo) | ~2.2 GB extracted | Apache 2.0 + BSD-3 | Yes, on first Whisper launch |

The NPU encoder model is device-gated to Snapdragon X Elite (Hexagon V73).
It will not run on X Plus or any other Qualcomm chipset without
recompilation via AI Hub.

## Licenses

- **OpenWritr code**: MIT — see [`LICENSE`](LICENSE).
- **Parakeet model**: CC-BY-4.0 (NVIDIA). Attribution preserved when the
  model is downloaded.
- **Qualcomm QNN runtime DLLs** (`QnnHtp.dll`, QNN provider, V73 stub, etc.,
  bundled in the release zip): **Qualcomm AI Engine Direct redistributable
  license**. The full text ships inside every release zip under
  `third-party-licenses/Qualcomm_LICENSE.pdf`, alongside Microsoft's
  `ThirdPartyNotices.txt` for the `onnxruntime-qnn` PyPI package the DLLs
  come from. These DLLs are redistributable as part of applications targeting
  Qualcomm Snapdragon hardware, which is what OpenWritr does.
- **ONNX Runtime DLLs** (`onnxruntime.dll`, `onnxruntime_providers_qnn.dll`):
  MIT (Microsoft), bundled under their respective LICENSE files in the release
  zip.

## Repository layout

```
openwritr-windows/
├── src/                   Rust native app
├── scripts/               pinned runtime and release tooling
├── models-manifest.json   pinned model assets and hashes
├── runtime-manifest.json  pinned ORT/QNN wheels and compatibility tuple
├── release-manifest.json  canonical per-architecture package contents
└── target/                gitignored build and staging output
```

The shipped `openwritr.exe` does not call Python at runtime.

## Development

```powershell
git clone https://github.com/trsdn/openwritr-windows.git
cd openwritr-windows
.\scripts\envup.ps1
cargo build --release --bins
python scripts/fetch_runtime.py --arch arm64
python scripts/prepare_release.py --arch arm64
target\stage\arm64\openwritr.exe --self-check
```

For x64 packaging, stage the CPU runtime with:

```powershell
python scripts/fetch_runtime.py --arch x64
```

See [`docs/RUNTIME_COMPATIBILITY.md`](docs/RUNTIME_COMPATIBILITY.md) for
the shared ORT/QNN/QAIRT contract and its Snapdragon hardware gates.

### Cleanup evaluator (`cleanup_eval`)

`src/bin/cleanup_eval/` is a focused, opt-in evaluator for the `cleanup`
core (prompt resolution, request/response handling, normalization, and the
integrity validator) — it reuses those production modules directly rather
than reimplementing them. It replays a committed, versioned, fully
synthetic DE/EN fixture corpus
([`fixtures/cleanup-eval/v1/corpus.json`](fixtures/cleanup-eval/v1/corpus.json))
covering punctuation, fillers, repetition, negation, digits/dates/times/
versions, commands/code/acronyms, URLs/emails, language preservation,
filler-only/`[[EMPTY]]`, and known tricky cases. No private dictation is
included or ever required.

```powershell
# Deterministic offline mode (default): no network, safe for CI.
cargo run --bin cleanup_eval
cargo run --bin cleanup_eval -- --json --category punctuation

# Focused tests: corpus versioning, expected decisions, no private fields
# in the serialized report, and production-module reuse.
cargo test --bin cleanup_eval
```

Live-provider mode is entirely opt-in (`--live`) and never required by tests
or releases: it reads the same `settings.json` provider/model/base URL and
the same credential storage as `gh auth token` (the app itself also reads the Copilot CLI sign-in) or Windows
Credential Manager), but never prints secrets, transcripts, prompts, or raw
candidate text. Every report — offline or live — includes the model/
provider, canonical endpoint scope, prompt-profile/validator/corpus
version, latency, failure/integrity category counts, and punctuation/
filler/repetition health scores, with all content redacted by default:

```powershell
cargo run --bin cleanup_eval -- --live --provider github_copilot
```

Run `cargo run --bin cleanup_eval -- --help` for the full flag list.

## Releases

Tagged releases live at
[github.com/trsdn/openwritr-windows/releases](https://github.com/trsdn/openwritr-windows/releases).
Each architecture ships a ZIP and installer generated from the same validated
release stage. CI also builds an unsigned Store `.msixbundle` as a workflow
artifact for Partner Center ingestion, but does not attach it to the public
GitHub release. Tagged builds use the repository variables
`MSIX_IDENTITY_NAME` and `MSIX_PUBLISHER` when configured; otherwise the Store
bundle is skipped rather than publishing a placeholder identity. Every package
includes an `artifact-manifest.json`; CI verifies all required file hashes and
confirms that x64 artifacts contain no Qualcomm runtime files.
