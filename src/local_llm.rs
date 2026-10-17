//! A language model that runs on this machine, for AI text cleanup and meeting
//! notes.
//!
//! OpenWritr drives Qualcomm's GenieX command-line tool (`geniex serve` is an
//! OpenAI-compatible server that runs AI Hub models on the Hexagon NPU) as a
//! private child process, and the normal OpenAI-compatible cleanup pipeline
//! talks to it over loopback. Nothing is bound in-process: GenieX is a
//! developer-preview SDK, so keeping it behind a process boundary means a
//! crash or an ABI change can never take the app down.
//!
//! What this module owns:
//! * finding (or, with the user's consent, installing) the official GenieX CLI;
//! * downloading / removing the model with `geniex pull` / `geniex remove`;
//! * starting `geniex serve` on a private port when a request needs it, and
//!   stopping it again when idle or when OpenWritr exits.
//!
//! The server is only ever started for the *managed* endpoint
//! ([`MANAGED_BASE_URL`]). Any other loopback address (Ollama, LM Studio, a
//! `geniex serve` the user runs themselves) is simply talked to.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use once_cell::sync::Lazy;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};

use crate::cleanup::EndpointScope;

/// Private port for the server OpenWritr starts. Deliberately not GenieX's
/// default (18181) so it never collides with a server the user runs.
pub const MANAGED_PORT: u16 = 18651;
pub const MANAGED_BASE_URL: &str = "http://127.0.0.1:18651/v1";
/// Qualcomm AI Hub bundle run on the NPU (4B instruct, no reasoning prefix).
pub const DEFAULT_MODEL: &str = "ai-hub-models/Qwen3-4B-Instruct-2507";

/// Pinned official installer (GenieX v0.8.0, Windows ARM64, Inno Setup).
const INSTALLER_URL: &str = "https://github.com/qualcomm/GenieX/releases/download/v0.8.0/geniex-cli-setup-windows-arm64-v0.8.0.exe";
const INSTALLER_SHA256: &str = "fcc4d932f371a9b3ad3d08a1b34912fe21a7fdf7c54d47c869250b50d81ec734";
const INSTALLER_BYTES: u64 = 65_196_910;

/// How long a freshly started server may take to answer.
const START_TIMEOUT: Duration = Duration::from_secs(90);
/// After a failed start, further requests fail fast for this long instead of
/// each waiting out [`START_TIMEOUT`].
const RETRY_AFTER_FAILURE: Duration = Duration::from_secs(60);
/// The server (and with it the NPU) is released after this much idleness.
/// GenieX drops an idle model after five minutes anyway, so keeping the
/// process longer only holds memory for a server that must reload first.
const IDLE_STOP: Duration = Duration::from_secs(5 * 60);

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Whether `endpoint` is the server OpenWritr manages itself.
pub fn is_managed(endpoint: &EndpointScope) -> bool {
    endpoint.is_loopback()
        && endpoint.port() == Some(MANAGED_PORT)
        && endpoint.scheme() == "http"
        && endpoint.path() == "/v1"
}

/// [`is_managed`] for a raw settings string.
pub fn is_managed_url(raw: &str) -> bool {
    EndpointScope::parse(raw).is_ok_and(|scope| is_managed(&scope))
}

/// Why the managed local model cannot be used right now.
#[derive(Debug, thiserror::Error)]
pub enum LocalLlmError {
    #[error("{0}")]
    Unsupported(String),
    #[error("the GenieX command-line tool is not installed")]
    CliMissing,
    #[error("the local model server did not start: {0}")]
    StartFailed(String),
    #[error("the local model server failed to start recently; not retrying yet")]
    RecentlyFailed,
}

/// The managed local model needs Qualcomm's NPU runtime, which only exists for
/// Windows on ARM64.
pub fn platform_support() -> std::result::Result<(), String> {
    platform_support_for(
        cfg!(target_arch = "aarch64"),
        std::env::var("PROCESSOR_ARCHITEW6432").ok().as_deref(),
    )
}

fn platform_support_for(
    native_arm64: bool,
    emulated_host: Option<&str>,
) -> std::result::Result<(), String> {
    if native_arm64 || emulated_host.is_some_and(|host| host.eq_ignore_ascii_case("ARM64")) {
        Ok(())
    } else {
        Err(
            "The built-in local model runs on Qualcomm Snapdragon (Windows on ARM). \
             On this PC use \"Another local server\" instead."
                .into(),
        )
    }
}

// ---------------------------------------------------------------------------
// Finding the CLI
// ---------------------------------------------------------------------------

fn cli_candidates(
    local_app_data: Option<&Path>,
    program_files: Option<&Path>,
    path_var: Option<&std::ffi::OsStr>,
) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(dir) = local_app_data {
        out.push(dir.join("GenieX CLI").join("geniex.exe"));
    }
    if let Some(dir) = program_files {
        out.push(dir.join("GenieX CLI").join("geniex.exe"));
    }
    if let Some(path) = path_var {
        out.extend(std::env::split_paths(path).map(|dir| dir.join("geniex.exe")));
    }
    out
}

/// The installed GenieX CLI, if any.
pub fn locate_cli() -> Option<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from);
    let path = std::env::var_os("PATH");
    cli_candidates(local.as_deref(), program_files.as_deref(), path.as_deref())
        .into_iter()
        .find(|candidate| candidate.is_file())
}

fn work_dir() -> PathBuf {
    crate::paths::models_dir()
        .parent()
        .map(|base| base.join("local-llm"))
        .unwrap_or_else(|| PathBuf::from("local-llm"))
}

fn hidden(command: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    command.creation_flags(CREATE_NO_WINDOW)
}

// ---------------------------------------------------------------------------
// Installing the CLI (explicit user action)
// ---------------------------------------------------------------------------

/// Download the pinned official GenieX installer, verify it, and run it
/// silently for the current user. The installer adds `geniex` to the user's
/// PATH; nothing is installed machine-wide and no administrator rights are
/// needed. `progress` receives a 0..=1 fraction while downloading.
pub fn install_cli(progress: &mut dyn FnMut(f32), cancel: &AtomicBool) -> Result<PathBuf> {
    platform_support().map_err(|reason| anyhow!(reason))?;
    let dir = work_dir();
    std::fs::create_dir_all(&dir).context("create the local model folder")?;
    let part = dir.join("geniex-cli-setup.exe.part");
    let installer = dir.join("geniex-cli-setup.exe");

    download_verified(
        INSTALLER_URL,
        INSTALLER_SHA256,
        INSTALLER_BYTES,
        &part,
        progress,
        cancel,
    )?;
    std::fs::rename(&part, &installer).context("finish the installer download")?;

    info!("running the GenieX installer");
    let status = hidden(Command::new(&installer).args([
        "/VERYSILENT",
        "/SUPPRESSMSGBOXES",
        "/NORESTART",
        "/NOICONS",
        "/TASKS=",
    ]))
    .status()
    .context("start the GenieX installer")?;
    let _ = std::fs::remove_file(&installer);
    if !status.success() {
        bail!("the GenieX installer exited with {status}");
    }
    locate_cli().ok_or_else(|| anyhow!("GenieX was installed but geniex.exe was not found"))
}

fn download_verified(
    url: &str,
    sha256: &str,
    expected_bytes: u64,
    dest: &Path,
    progress: &mut dyn FnMut(f32),
    cancel: &AtomicBool,
) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(None)
        .build()?;
    let mut response = client.get(url).send()?.error_for_status()?;
    let total = response.content_length().unwrap_or(expected_bytes).max(1);
    let mut file = std::fs::File::create(dest).context("create the download file")?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 256 * 1024];
    let mut done = 0_u64;
    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = std::fs::remove_file(dest);
            bail!("cancelled");
        }
        let read = response.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        file.write_all(&buffer[..read])?;
        done += read as u64;
        progress((done as f32 / total as f32).min(1.0));
    }
    file.flush()?;
    drop(file);
    let digest = hex(&hasher.finalize());
    if !digest.eq_ignore_ascii_case(sha256) {
        let _ = std::fs::remove_file(dest);
        bail!("the download does not match the expected checksum");
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

// ---------------------------------------------------------------------------
// Model management
// ---------------------------------------------------------------------------

/// Whether `model` is already downloaded, according to `geniex list`.
pub fn model_installed(cli: &Path, model: &str) -> Result<bool> {
    let output = hidden(Command::new(cli).args(["list", "--format", "json"]))
        .output()
        .context("run geniex list")?;
    if !output.status.success() {
        bail!("geniex list failed ({})", output.status);
    }
    Ok(lists_model(&String::from_utf8_lossy(&output.stdout), model))
}

/// Whether `listing` (`geniex list --format json`) contains `model`.
///
/// GenieX stores `ai-hub-models/<name>` under its canonical `qualcomm/<name>`,
/// so names are compared by their last path segment, ignoring case and any
/// `:precision` suffix.
fn lists_model(listing: &str, model: &str) -> bool {
    fn repo(name: &str) -> String {
        let name = name.split(':').next().unwrap_or(name);
        name.rsplit('/')
            .next()
            .unwrap_or(name)
            .trim()
            .to_ascii_lowercase()
    }
    let wanted = repo(model);
    if wanted.is_empty() {
        return false;
    }
    match serde_json::from_str::<serde_json::Value>(listing) {
        Ok(serde_json::Value::Array(items)) => items
            .iter()
            .filter_map(|item| item.get("name").and_then(|name| name.as_str()))
            .any(|name| repo(name) == wanted),
        // Unexpected output: fall back to a plain search for the bare name.
        _ => listing.to_ascii_lowercase().contains(&wanted),
    }
}

/// Download `model` with `geniex pull`. `progress` gets a percentage when the
/// tool prints one, plus the latest status line. Cancelling keeps what was
/// downloaded so far (GenieX resumes).
pub fn pull_model(
    cli: &Path,
    model: &str,
    progress: &mut dyn FnMut(Option<f32>, &str),
    cancel: &AtomicBool,
) -> Result<()> {
    let mut child = hidden(
        Command::new(cli)
            .args(["pull", model])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .spawn()
    .context("start geniex pull")?;

    let (tx, rx) = mpsc::channel::<String>();
    for stream in [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    {
        let tx = tx.clone();
        std::thread::spawn(move || split_progress_lines(stream, &tx));
    }
    drop(tx);

    // The last few lines the tool printed: when it fails, they say why.
    let mut recent: std::collections::VecDeque<String> = std::collections::VecDeque::new();
    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            let _ = child.wait();
            bail!("cancelled");
        }
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) => {
                let clean = strip_ansi(&line);
                let clean = clean.trim();
                if !clean.is_empty() {
                    progress(parse_percent(clean), clean);
                    if parse_percent(clean).is_none() {
                        info!("geniex pull: {clean}");
                        recent.push_back(clean.to_string());
                        if recent.len() > 4 {
                            recent.pop_front();
                        }
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    let status = child.wait()?;
    if !status.success() {
        let detail = recent.into_iter().collect::<Vec<_>>().join(" | ");
        if detail.is_empty() {
            bail!("geniex pull failed ({status})");
        }
        bail!("geniex pull failed ({status}): {detail}");
    }
    Ok(())
}

pub fn remove_model(cli: &Path, model: &str) -> Result<()> {
    let status = hidden(Command::new(cli).args(["remove", model, "--yes"]))
        .status()
        .context("run geniex remove")?;
    if !status.success() {
        bail!("geniex remove failed ({status})");
    }
    Ok(())
}

/// Emit text split on `\r` and `\n` (progress bars redraw with `\r`).
fn split_progress_lines(mut stream: Box<dyn Read + Send>, tx: &mpsc::Sender<String>) {
    let mut current = Vec::new();
    let mut byte = [0_u8; 1];
    while matches!(stream.read(&mut byte), Ok(1)) {
        if byte[0] == b'\r' || byte[0] == b'\n' {
            if !current.is_empty() {
                let _ = tx.send(String::from_utf8_lossy(&current).into_owned());
                current.clear();
            }
        } else {
            current.push(byte[0]);
        }
    }
    if !current.is_empty() {
        let _ = tx.send(String::from_utf8_lossy(&current).into_owned());
    }
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The last `NN%` / `NN.N%` figure in a progress line.
fn parse_percent(line: &str) -> Option<f32> {
    let end = line.rfind('%')?;
    let head = &line[..end];
    let start = head
        .rfind(|c: char| !(c.is_ascii_digit() || c == '.'))
        .map_or(0, |index| index + 1);
    head[start..]
        .parse::<f32>()
        .ok()
        .filter(|value| (0.0..=100.0).contains(value))
}

// ---------------------------------------------------------------------------
// The server process
// ---------------------------------------------------------------------------

struct Server {
    child: Option<Child>,
    last_used: Instant,
    failed_at: Option<Instant>,
    reaper_running: bool,
    /// The model has answered at least once since the server started.
    model_loaded: bool,
    /// The model has written at normal speed since the server started.
    was_fast: bool,
    /// Slow answers in a row from this server.
    slow_answers: u32,
}

static SERVER: Lazy<Mutex<Server>> = Lazy::new(|| {
    Mutex::new(Server {
        child: None,
        last_used: Instant::now(),
        failed_at: None,
        reaper_running: false,
        model_loaded: false,
        was_fast: false,
        slow_answers: 0,
    })
});

/// Make sure a request to `endpoint` can be answered. A no-op for every
/// endpoint other than the managed one.
pub fn ensure_running(endpoint: &EndpointScope) -> std::result::Result<(), LocalLlmError> {
    if !is_managed(endpoint) {
        return Ok(());
    }
    let mut server = SERVER.lock();
    let previous_use = server.last_used;
    server.last_used = Instant::now();

    let running = match server.child.as_mut().map(Child::try_wait) {
        Some(Ok(None)) => true,
        Some(_) => {
            warn!("local model server exited; restarting");
            server.child = None;
            server.model_loaded = false;
            server.was_fast = false;
            server.slow_answers = 0;
            false
        }
        None => false,
    };
    if !running {
        // Something else (a server the user started) already answers there.
        if !server_answers(Duration::from_millis(500)) {
            if server
                .failed_at
                .is_some_and(|at| at.elapsed() < RETRY_AFTER_FAILURE)
            {
                return Err(LocalLlmError::RecentlyFailed);
            }
            match start_server() {
                Ok(child) => {
                    server.child = Some(child);
                    server.model_loaded = false;
                    server.was_fast = false;
                    server.slow_answers = 0;
                    if !server.reaper_running {
                        server.reaper_running = true;
                        std::thread::spawn(reap_when_idle);
                    }
                }
                Err(error) => {
                    server.failed_at = Some(Instant::now());
                    return Err(error);
                }
            }
        }
    }

    // The server answers before its model is on the NPU, and GenieX unloads
    // the model after five idle minutes. The first request after either has
    // to wait for the load, which takes far longer than a cleanup request may
    // wait, so load it here, once, before any real request is sent.
    if !server.model_loaded || previous_use.elapsed() >= MODEL_RESIDENT {
        info!("loading the local model");
        let started = Instant::now();
        match load_model() {
            Ok(()) => {
                info!(took = ?started.elapsed(), "local model is loaded");
                server.model_loaded = true;
                server.failed_at = None;
            }
            Err(error) => {
                warn!(error = %error, took = ?started.elapsed(), "loading the local model failed");
                server.model_loaded = false;
                server.failed_at = Some(Instant::now());
                return Err(LocalLlmError::StartFailed(format!(
                    "the model did not load: {error}"
                )));
            }
        }
    }
    server.failed_at = None;
    Ok(())
}

/// How long GenieX keeps an idle model loaded (its default is 300 s); a
/// little less, so a request just before the unload is not trusted.
const MODEL_RESIDENT: Duration = Duration::from_secs(240);
/// The longest a model load may take.
const LOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// Make the server load [`DEFAULT_MODEL`] by asking it for one token.
fn load_model() -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(LOAD_TIMEOUT)
        .build()?;
    let response = client
        .post(format!("{MANAGED_BASE_URL}/chat/completions"))
        .json(&serde_json::json!({
            "model": DEFAULT_MODEL,
            "messages": [{"role": "user", "content": "Hi"}],
            "max_tokens": 1,
            "temperature": 0.0,
        }))
        .send()
        .context("no answer")?;
    if !response.status().is_success() {
        let status = response.status();
        let detail = response.text().unwrap_or_default();
        let detail: String = detail.chars().take(160).collect();
        bail!("the server answered with {status} {detail}");
    }
    Ok(())
}

fn start_server() -> std::result::Result<Child, LocalLlmError> {
    platform_support().map_err(LocalLlmError::Unsupported)?;
    let cli = locate_cli().ok_or(LocalLlmError::CliMissing)?;
    info!(cli = %cli.display(), "starting the local model server");
    let mut child = hidden(
        Command::new(&cli)
            .args(["serve", "--host", &format!("127.0.0.1:{MANAGED_PORT}")])
            .env("GENIEX_LOG", "info")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
    .spawn()
    .map_err(|error| LocalLlmError::StartFailed(error.to_string()))?;
    kill_with_parent(&child);
    // Everything the server prints goes into OpenWritr's log, so a model
    // that fails or stalls can be diagnosed from one file.
    if let Some(stream) = child.stdout.take() {
        forward_output(stream, "stdout");
    }
    if let Some(stream) = child.stderr.take() {
        forward_output(stream, "stderr");
    }

    let started = Instant::now();
    while started.elapsed() < START_TIMEOUT {
        if let Ok(Some(status)) = child.try_wait() {
            return Err(LocalLlmError::StartFailed(format!("exited with {status}")));
        }
        if server_answers(Duration::from_secs(2)) {
            info!(took = ?started.elapsed(), "local model server is ready");
            return Ok(child);
        }
        std::thread::sleep(Duration::from_millis(400));
    }
    let _ = child.kill();
    let _ = child.wait();
    Err(LocalLlmError::StartFailed("timed out".into()))
}

/// How loud a line of the server's output is in OpenWritr's log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LineLevel {
    /// Two lines per model step (hundreds per reply): only with
    /// `RUST_LOG=openwritr=debug`, where they time each part of the model.
    Debug,
    Info,
    /// The server or the NPU runtime reported an error.
    Warn,
}

fn server_line_level(text: &str) -> LineLevel {
    if text.contains("QnnGraph_execute") || text.contains("execution finished with result 0") {
        LineLevel::Debug
    } else if text.contains(" ERR ") || text.contains("<E>") {
        LineLevel::Warn
    } else {
        LineLevel::Info
    }
}

/// Copy a child's output into the log, one line at a time. Also keeps the
/// pipe drained so the server can never block on a full buffer.
fn forward_output(stream: impl Read + Send + 'static, name: &'static str) {
    let spawned = std::thread::Builder::new()
        .name(format!("geniex-{name}"))
        .spawn(move || {
            let mut reader = std::io::BufReader::new(stream);
            let mut line = Vec::new();
            loop {
                line.clear();
                match std::io::BufRead::read_until(&mut reader, b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let text = strip_ansi(&String::from_utf8_lossy(&line));
                        let text = text.trim();
                        if !text.is_empty() {
                            let text: String = text.chars().take(600).collect();
                            match server_line_level(&text) {
                                LineLevel::Debug => debug!(stream = name, "geniex: {text}"),
                                LineLevel::Info => info!(stream = name, "geniex: {text}"),
                                LineLevel::Warn => warn!(stream = name, "geniex: {text}"),
                            }
                        }
                    }
                }
            }
        });
    if let Err(error) = spawned {
        warn!(error = %error, "could not capture the local model server's output");
    }
}

fn server_answers(timeout: Duration) -> bool {
    reqwest::blocking::Client::builder()
        .timeout(timeout)
        .build()
        .and_then(|client| client.get(format!("{MANAGED_BASE_URL}/models")).send())
        .is_ok_and(|response| response.status().is_success())
}

fn reap_when_idle() {
    loop {
        std::thread::sleep(Duration::from_secs(30));
        let mut server = SERVER.lock();
        if server.child.is_none() {
            server.reaper_running = false;
            return;
        }
        if server.last_used.elapsed() >= IDLE_STOP {
            info!("local model server idle; stopping it");
            stop_locked(&mut server);
            server.reaper_running = false;
            return;
        }
    }
}

fn stop_locked(server: &mut Server) {
    if let Some(mut child) = server.child.take() {
        let _ = child.kill();
        let _ = child.wait();
    }
    server.model_loaded = false;
    server.was_fast = false;
    server.slow_answers = 0;
}

/// Stop the server OpenWritr started, now, so the NPU and its memory go back
/// to the rest of the system. The next request starts it again (and reloads
/// the model, about half a minute). Does nothing when OpenWritr did not start
/// a server, so a server the user runs is never touched. Also cancels
/// whatever the server is still generating, e.g. a reply nobody waits for
/// any more after a timeout.
///
/// Gives up quietly when another thread holds the server for longer than a
/// moment (it is loading the model for a new request); the idle stop then
/// releases it later.
pub fn release(reason: &str) {
    let Some(mut server) = SERVER.try_lock_for(Duration::from_secs(2)) else {
        info!(reason, "local model server is busy; leaving it running");
        return;
    };
    if server.child.is_some() {
        info!(reason, "stopping the local model server");
        stop_locked(&mut server);
    }
}

/// Writing speed (tokens per second) below which the built-in model counts as
/// slow. It normally writes about 18; in the slow state it wrote under 1.
pub(crate) const SLOW_WRITING_TOKENS_PER_SECOND: f64 = 4.0;

/// Whether a reply written at `tokens_per_second` should restart the server:
/// only when the same server wrote at normal speed before. That is the slow
/// state seen in practice (fast, then about 20 times slower for every later
/// request until restarted). A PC where the model is always slow gains
/// nothing from a restart, and each one costs a model load.
fn restart_for_speed(tokens_per_second: f64, was_fast: bool) -> bool {
    tokens_per_second < SLOW_WRITING_TOKENS_PER_SECOND && was_fast
}

/// Report how fast the built-in model wrote its last reply. Restarts the
/// server when it has fallen into the slow state, so the next request gets a
/// fresh one.
pub fn note_writing_speed(tokens_per_second: f64) {
    let Some(mut server) = SERVER.try_lock_for(Duration::from_secs(2)) else {
        return;
    };
    if server.child.is_none() {
        return;
    }
    if tokens_per_second >= SLOW_WRITING_TOKENS_PER_SECOND {
        server.was_fast = true;
        server.slow_answers = 0;
        return;
    }
    server.slow_answers += 1;
    if restart_for_speed(tokens_per_second, server.was_fast) {
        warn!(
            tokens_per_second,
            "the local model has become slow; restarting it"
        );
        stop_locked(&mut server);
    } else {
        info!(
            tokens_per_second,
            "the local model writes slowly on this PC"
        );
    }
}

/// Slow answers in a row after which a long cleanup stops: at under 1 token a
/// second a 48 minute transcript would take hours, holding the NPU and
/// memory all that time. What is left is retried later.
const SLOW_ANSWERS_TO_GIVE_UP: u32 = 2;

/// Whether the built-in model has kept writing slowly (a long cleanup then
/// stops instead of grinding on).
pub fn keeps_writing_slowly() -> bool {
    SERVER
        .try_lock_for(Duration::from_secs(2))
        .is_some_and(|server| server.slow_answers >= SLOW_ANSWERS_TO_GIVE_UP)
}

/// Stop the server (called when OpenWritr exits or the model is removed).
pub fn shutdown() {
    // A model load can hold the lock for minutes; do not make quitting wait
    // for it (Windows ends the server with the app anyway).
    if let Some(mut server) = SERVER.try_lock_for(Duration::from_secs(2)) {
        stop_locked(&mut server);
    }
}

/// Tie the child's life to ours: when OpenWritr exits for any reason, even a
/// crash, Windows ends the server too, so the NPU is never left occupied.
fn kill_with_parent(child: &Child) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    // The job handle is intentionally never closed: it lives as long as the
    // process, and closing it is what kills the children.
    static JOB: Lazy<usize> = Lazy::new(|| unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return 0;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if set == 0 {
            return 0;
        }
        job as usize
    });

    let job = *JOB;
    if job == 0 {
        warn!("could not create a job object; the server will not stop with the app");
        return;
    }
    let assigned =
        unsafe { AssignProcessToJobObject(job as HANDLE, child.as_raw_handle() as HANDLE) };
    if assigned == 0 {
        warn!("could not attach the local model server to the app's job");
    }
}

// ---------------------------------------------------------------------------
// Test connection (used by Settings)
// ---------------------------------------------------------------------------

/// Start the built-in server and load its model in the background, so the
/// cleanup that follows a dictation does not wait for the NPU to wake up.
/// Does nothing unless `base_url` is the built-in endpoint.
pub fn prewarm(base_url: &str, model: &str) {
    static RUNNING: AtomicBool = AtomicBool::new(false);
    if !is_managed_url(base_url) || RUNNING.swap(true, Ordering::AcqRel) {
        return;
    }
    let (base_url, model) = (base_url.to_string(), model.to_string());
    let spawned = std::thread::Builder::new()
        .name("local-llm-prewarm".into())
        .spawn(move || {
            if let Err(error) = test_chat(&base_url, &model) {
                warn!(error = %error, "local model prewarm failed");
            }
            RUNNING.store(false, Ordering::Release);
        });
    if spawned.is_err() {
        RUNNING.store(false, Ordering::Release);
    }
}

/// Models a server reports at `GET <base>/models` (OpenAI list format).
pub fn discover_models(base_url: &str) -> Result<Vec<String>> {
    let scope = EndpointScope::parse(base_url).map_err(|error| anyhow!(error.to_string()))?;
    if !scope.is_loopback() {
        bail!("only servers on this PC can be queried here");
    }
    ensure_running(&scope).map_err(|error| anyhow!(error.to_string()))?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let value: serde_json::Value = client
        .get(scope.join("/models"))
        .send()?
        .error_for_status()?
        .json()?;
    Ok(parse_model_ids(&value))
}

/// What a test request measured.
#[derive(Clone, Copy, Debug, Default)]
pub struct TestResult {
    pub elapsed: Duration,
    /// Reading speed, tokens per second, as reported by the server.
    pub prompt_per_second: Option<f64>,
    /// Writing speed, tokens per second, as reported by the server.
    pub predicted_per_second: Option<f64>,
}

impl TestResult {
    /// One line for Settings.
    pub fn summary(&self) -> String {
        let mut text = format!("Works: answered in {:.1} s", self.elapsed.as_secs_f32());
        if let Some(rate) = self.prompt_per_second {
            text.push_str(&format!(" · reads {rate:.0} tok/s"));
        }
        if let Some(rate) = self.predicted_per_second {
            text.push_str(&format!(" · writes {rate:.1} tok/s"));
        }
        text.push('.');
        text
    }
}

/// Send one short chat request to a server on this PC and measure it. The
/// first request to the built-in model includes loading it onto the NPU, so
/// the timeout is generous; run the test twice to see the steady speed.
pub fn test_chat(base_url: &str, model: &str) -> Result<TestResult> {
    let scope = EndpointScope::parse(base_url).map_err(|error| anyhow!(error.to_string()))?;
    if !scope.is_loopback() {
        bail!("only servers on this PC can be tested here");
    }
    if model.trim().is_empty() {
        bail!("enter a model ID first");
    }
    ensure_running(&scope).map_err(|error| anyhow!(error.to_string()))?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(240))
        .build()?;
    let started = Instant::now();
    let response = client
        .post(scope.join("/chat/completions"))
        .json(&serde_json::json!({
            "model": model.trim(),
            "messages": [{
                "role": "user",
                "content": "Write two short sentences about a quiet morning."
            }],
            "max_tokens": 48,
            "temperature": 0.0,
        }))
        .send()
        .context("the server did not answer")?;
    if !response.status().is_success() {
        bail!("the server answered with {}", response.status());
    }
    let value: serde_json::Value = response.json().context("the answer was not valid JSON")?;
    let has_text = value
        .pointer("/choices/0/message/content")
        .and_then(|content| content.as_str())
        .is_some();
    if !has_text {
        bail!("the answer had no text in the OpenAI chat format");
    }
    Ok(TestResult {
        elapsed: started.elapsed(),
        prompt_per_second: value
            .pointer("/timings/prompt_per_second")
            .and_then(|rate| rate.as_f64())
            .filter(|rate| *rate > 0.0),
        predicted_per_second: value
            .pointer("/timings/predicted_per_second")
            .and_then(|rate| rate.as_f64())
            .filter(|rate| *rate > 0.0),
    })
}

fn parse_model_ids(value: &serde_json::Value) -> Vec<String> {
    let mut ids: Vec<String> = value
        .get("data")
        .and_then(|data| data.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("id").and_then(|id| id.as_str()))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    ids.sort();
    ids.dedup();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(raw: &str) -> EndpointScope {
        EndpointScope::parse(raw).unwrap()
    }

    #[test]
    fn only_the_private_loopback_endpoint_is_managed() {
        assert!(is_managed(&scope(MANAGED_BASE_URL)));
        assert!(is_managed(&scope("http://localhost:18651/v1/")));
        assert!(!is_managed(&scope("http://127.0.0.1:18181/v1")));
        assert!(!is_managed(&scope("http://127.0.0.1:11434/v1")));
        assert!(!is_managed(&scope("https://127.0.0.1:18651/v1")));
        assert!(!is_managed(&scope("http://192.168.1.5:18651/v1")));
        assert!(!is_managed(&scope("http://127.0.0.1:18651")));
    }

    #[test]
    fn platform_support_needs_arm64() {
        assert!(platform_support_for(true, None).is_ok());
        assert!(platform_support_for(false, Some("ARM64")).is_ok());
        assert!(platform_support_for(false, Some("arm64")).is_ok());
        assert!(platform_support_for(false, None).is_err());
        assert!(platform_support_for(false, Some("AMD64")).is_err());
    }

    #[test]
    fn cli_search_order_is_install_dir_then_program_files_then_path() {
        let path = std::env::join_paths(["tools", "bin"]).unwrap();
        let found = cli_candidates(
            Some(Path::new("local")),
            Some(Path::new("programs")),
            Some(&path),
        );
        assert_eq!(
            found,
            [
                Path::new("local").join("GenieX CLI").join("geniex.exe"),
                Path::new("programs").join("GenieX CLI").join("geniex.exe"),
                Path::new("tools").join("geniex.exe"),
                Path::new("bin").join("geniex.exe"),
            ]
        );
        assert!(cli_candidates(None, None, None).is_empty());
    }

    #[test]
    fn model_listing_matches_the_canonical_name_geniex_stores() {
        let listing =
            r#"[{"name":"qualcomm/Qwen3-4B-Instruct-2507","size":123,"precisions":["N/A"]}]"#;
        assert!(lists_model(listing, DEFAULT_MODEL));
        assert!(lists_model(listing, "AI-HUB-MODELS/qwen3-4b-instruct-2507"));
        assert!(lists_model(listing, "qualcomm/Qwen3-4B-Instruct-2507:int4"));
        assert!(!lists_model(listing, "ai-hub-models/Other"));
        assert!(!lists_model(listing, "ai-hub-models/Qwen3-4B"));
        assert!(!lists_model(listing, "  "));
        assert!(!lists_model("[]", DEFAULT_MODEL));
        // Not JSON: plain search for the bare name.
        assert!(lists_model(
            "qualcomm/Qwen3-4B-Instruct-2507  2.3 GiB",
            DEFAULT_MODEL
        ));
    }

    #[test]
    fn percent_is_read_from_progress_lines() {
        assert_eq!(
            parse_percent("Downloading  42%  1.2 GiB/2.9 GiB"),
            Some(42.0)
        );
        assert_eq!(parse_percent("[=====>   ] 63.5% 12 MB/s"), Some(63.5));
        assert_eq!(parse_percent("100%"), Some(100.0));
        assert_eq!(parse_percent("step 1 of 3"), None);
        assert_eq!(parse_percent("up 250%"), None);
        assert_eq!(parse_percent("%"), None);
    }

    #[test]
    fn ansi_escapes_are_removed() {
        assert_eq!(strip_ansi("\u{1b}[32m✔  done\u{1b}[0m"), "✔  done");
        assert_eq!(strip_ansi("plain"), "plain");
    }

    #[test]
    fn progress_lines_split_on_carriage_returns() {
        let (tx, rx) = mpsc::channel();
        let data: Box<dyn Read + Send> =
            Box::new(std::io::Cursor::new(b"10%\r20%\r\n\nfinished".to_vec()));
        split_progress_lines(data, &tx);
        drop(tx);
        let lines: Vec<String> = rx.iter().collect();
        assert_eq!(lines, ["10%", "20%", "finished"]);
    }

    #[test]
    fn model_ids_are_read_sorted_and_deduplicated() {
        let value = serde_json::json!({
            "object": "list",
            "data": [{"id": "b"}, {"id": "a"}, {"id": "b"}, {"nope": 1}]
        });
        assert_eq!(parse_model_ids(&value), ["a", "b"]);
        assert!(parse_model_ids(&serde_json::json!({})).is_empty());
    }

    #[test]
    fn test_summary_lists_the_speeds_the_server_reported() {
        let full = TestResult {
            elapsed: Duration::from_millis(2500),
            prompt_per_second: Some(310.4),
            predicted_per_second: Some(14.26),
        };
        assert_eq!(
            full.summary(),
            "Works: answered in 2.5 s · reads 310 tok/s · writes 14.3 tok/s."
        );
        let bare = TestResult {
            elapsed: Duration::from_secs(1),
            ..TestResult::default()
        };
        assert_eq!(bare.summary(), "Works: answered in 1.0 s.");
    }

    #[test]
    fn server_output_is_logged_at_a_level_that_matches_it() {
        // Lines as GenieX printed them in a real run.
        let step = r"Oct 17 19:26:02.120 INF go\ml.go:233 [ML] [QNN]  <I> QnnGraph_execute started. graph = 0x4";
        let done = r"Oct 17 19:26:02.125 INF go\ml.go:233 [ML] [QNN]  <I> Graph token_ar1_cl512_1_of_4 execution finished with result 0";
        let error = r"Oct 17 19:25:34.739 ERR go\ml.go:237 [ML] [QNN]  <E> Failed to map buffer of size 538968064";
        let summary = r"Oct 17 19:26:02.625 INF handler\chat_response.go:185 Generation complete prompt_tokens=332 decode_tok_s=18.1";
        assert_eq!(server_line_level(step), LineLevel::Debug);
        assert_eq!(server_line_level(done), LineLevel::Debug);
        assert_eq!(server_line_level(error), LineLevel::Warn);
        assert_eq!(server_line_level(summary), LineLevel::Info);
        assert_eq!(
            server_line_level("Local hosting on http://127.0.0.1:18651/"),
            LineLevel::Info
        );
    }

    #[test]
    fn only_a_server_that_slowed_down_is_restarted() {
        assert!(restart_for_speed(0.9, true), "fast before, slow now");
        assert!(!restart_for_speed(0.9, false), "always slow here");
        assert!(!restart_for_speed(18.2, true));
        assert!(!restart_for_speed(SLOW_WRITING_TOKENS_PER_SECOND, true));
    }

    #[test]
    fn releasing_without_a_started_server_does_nothing() {
        release("test");
        assert!(SERVER.lock().child.is_none());
    }

    #[test]
    fn hex_is_lowercase_and_padded() {
        assert_eq!(hex(&[0, 15, 255]), "000fff");
    }

    #[test]
    fn unmanaged_endpoints_never_start_anything() {
        assert!(ensure_running(&scope("http://127.0.0.1:11434/v1")).is_ok());
        assert!(ensure_running(&scope("https://api.openai.com/v1")).is_ok());
    }
}
