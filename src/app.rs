//! App orchestration: tray + winit event loop + hotkey thread + ASR.
//!
//! The hotkey FSM runs on its own background thread now, completely
//! independent of the winit event loop. That way the tray menu spawning
//! the settings subprocess, or any other event loop weirdness, cannot
//! stall recording. The hotkey thread sends `Event::Start` / `Event::Stop`
//! over a crossbeam-style channel into the winit loop, which translates
//! them into recorder/tray/engine actions.

use crate::{
    audio::Recorder,
    cleanup::FallbackReason,
    diagnostics, enhance, hotkey, key_hook, media,
    meeting::{self, MeetingRecorder},
    model_manager::ModelState,
    overlay::{self, ListeningIntent, NoticeKind, OverlayViewState, ProcessingPhase},
    paste,
    settings::{CredentialHealth, EnhanceMode, MeetingNotesMode, Settings, SettingsRevision},
    sounds,
    transcript_window::{self, WindowMsg},
    tray,
    worker::{
        DeliveryTarget, DiarizationOutcome, JobConfig, JobStage, RecordingIntent, RepairPlan,
        Rerun, SessionPart, ShutdownMode, Worker, WorkerEvent,
    },
};
use anyhow::Result;
use std::collections::{BTreeMap, HashMap};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::mpsc;
use std::sync::{
    atomic::{AtomicBool, AtomicU32, Ordering},
    Arc,
};
use std::thread;
use std::time::{Duration, Instant};
use tracing::{info, warn};
use tray_icon::menu::MenuEvent;
use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy};
use winit::window::WindowId;

#[derive(Debug, Clone)]
pub enum UserEvent {
    HotkeyPress {
        epoch: u64,
        shift_down: bool,
        delivery_target: Option<DeliveryTarget>,
    },
    HotkeyShiftChanged {
        shift_down: bool,
    },
    HotkeyRelease,
    DiagnosticsExported,
    DiagnosticsExportFailed(String),
    /// A file picked from the tray menu was decoded to 16 kHz mono. `window`
    /// feeds the popup that was opened for it when the file was chosen.
    FileDecoded {
        path: PathBuf,
        samples: Vec<f32>,
        output: PathBuf,
        markdown: bool,
        window: mpsc::Sender<WindowMsg>,
    },
    /// The file dialog was dismissed, or decoding failed (`Some(reason)`).
    FileImportEnded(Option<String>),
    /// Answer to "also create meeting notes?" after a recording stopped.
    MeetingNotesChoice(bool),
    Tick,
}

// DETACHED_PROCESS | CREATE_NO_WINDOW — child fully decoupled from parent
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

enum EngineState {
    NotStarted,
    Loading,
    Ready { label: String },
    Failed { error: String },
}

/// Where a long-form (file or meeting) transcript goes, and how to label it.
struct FileJob {
    output: PathBuf,
    markdown: bool,
    /// Structured AI notes were requested; they go to `notes_output`.
    notes: bool,
    notes_output: Option<PathBuf>,
    title: String,
    details: String,
    /// Popup window for this job. `None` (meetings) keeps the overlay pill.
    window: Option<mpsc::Sender<WindowMsg>>,
    /// A non-final part of a rolling meeting transcription: it only feeds the
    /// worker's session and is never shown or delivered on its own.
    background: bool,
    /// Meeting membership, with the settings snapshotted when the meeting
    /// began so every part is processed the same way.
    session: Option<(SessionPart, Settings)>,
    /// The in-progress transcript of a meeting. Background parts append to it
    /// (it is their `output`); the final job deletes it once the finished
    /// transcript is saved.
    partial: Option<PathBuf>,
}

impl FileJob {
    /// Jobs that must stay out of the dictation overlay's job queue: the
    /// popup shows file jobs, and background meeting parts show nothing.
    fn overlay_free(&self) -> bool {
        self.window.is_some() || self.background
    }

    /// Tell the popup something; a failed send just means it was closed.
    fn tell_window(&self, message: WindowMsg) -> bool {
        self.window
            .as_ref()
            .is_some_and(|window| window.send(message).is_ok())
    }
}

/// What one background retry does for a delivered transcript.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepairStep {
    /// Clean piece `n` of the transcript again.
    Piece(usize),
    /// Generate the meeting notes that could not be generated before.
    Notes,
}

/// A delivered transcript whose AI cleanup (or notes) could not finish, e.g.
/// because the connection was down. The app keeps what is needed to try again
/// in the background and rewrites the saved files when it succeeds.
struct Repair {
    id: u64,
    name: String,
    plan: RepairPlan,
    output: PathBuf,
    notes_output: Option<PathBuf>,
    markdown: bool,
    title: String,
    details: String,
    /// Exactly what the transcript file held when we last wrote it: if the
    /// file has changed since (the user edited it), it is left alone.
    last_written: String,
    /// Most slices ever cleaned for each piece, to tell progress from repeats.
    best_cleaned: Vec<usize>,
    failed_rounds: u32,
    next_attempt: Instant,
    in_flight: Option<u64>,
}

/// First retry after two minutes, then slower; after five fruitless rounds
/// the text is left as it is.
const REPAIR_BACKOFF_SECONDS: [u64; 5] = [120, 300, 600, 1_200, 1_800];
/// Pause between steps of one productive round.
const REPAIR_STEP_PAUSE: Duration = Duration::from_secs(5);

/// How long to wait before the next round after `failed_rounds` fruitless
/// ones, or `None` to give up.
fn repair_delay(failed_rounds: u32) -> Option<Duration> {
    REPAIR_BACKOFF_SECONDS
        .get(failed_rounds.saturating_sub(1) as usize)
        .map(|seconds| Duration::from_secs(*seconds))
}

/// The next thing to retry: unfinished pieces in order, then the notes.
fn next_repair_step(plan: &RepairPlan) -> Option<RepairStep> {
    plan.pieces
        .iter()
        .position(|piece| piece.raw.is_some())
        .map(RepairStep::Piece)
        .or_else(|| plan.notes_failed.then_some(RepairStep::Notes))
}

/// File content for a transcript: Markdown with a title, or the plain text.
fn render_transcript(markdown: bool, title: &str, details: &str, text: &str) -> String {
    if markdown {
        media::to_markdown(title, details, text)
    } else {
        text.to_string()
    }
}

/// File content for meeting notes, which sit next to the transcript.
fn render_notes(title: &str, details: &str, transcript_name: &str, notes: &str) -> String {
    media::to_markdown(
        &title.replacen("Meeting", "Meeting notes", 1),
        &format!("{details} · transcript: {transcript_name}"),
        notes,
    )
}

fn write_file(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    path.parent().map_or(Ok(()), std::fs::create_dir_all)?;
    std::fs::write(path, content)
}

/// "cleaned 21 of 25 sections" when some of the text is still as recognised.
fn cleanup_summary(report: &crate::enhance::CleanupReport) -> Option<String> {
    (report.total > 0 && report.cleaned < report.total)
        .then(|| format!("cleaned {} of {} sections", report.cleaned, report.total))
}

/// A meeting being recorded: its parts are transcribed as they are cut.
struct MeetingSession {
    id: u64,
    started: meeting::LocalTime,
    /// The transcript so far, kept on disk while recording continues.
    partial: PathBuf,
    /// Settings as they were when the meeting began.
    settings: Settings,
}

/// A finished meeting recording waiting for the "create notes?" answer. The
/// final part is the audio recorded since the last rolling cut.
struct PendingMeeting {
    tail: meeting::Segment,
    session: MeetingSession,
    recorded: Duration,
}

/// What to do about meeting notes when a recording stops.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NotesDecision {
    /// Show the "create notes?" question.
    Ask,
    /// Create notes without asking.
    Create,
    /// Transcript only.
    Skip,
    /// The user wants notes (or would be asked) but no AI provider is set
    /// up: transcript only, with a hint.
    SkipNoProvider,
}

/// `may_ask` is false when no question can be shown (quitting the app).
fn notes_decision(mode: MeetingNotesMode, provider_ready: bool, may_ask: bool) -> NotesDecision {
    match mode {
        MeetingNotesMode::Never => NotesDecision::Skip,
        MeetingNotesMode::Always if provider_ready => NotesDecision::Create,
        MeetingNotesMode::Always => NotesDecision::SkipNoProvider,
        MeetingNotesMode::Ask if !may_ask => NotesDecision::Skip,
        MeetingNotesMode::Ask if provider_ready => NotesDecision::Ask,
        MeetingNotesMode::Ask => NotesDecision::SkipNoProvider,
    }
}

/// Rolling parts are queued as pieces of at most this long so that a
/// dictation job never waits behind more than one short transcription.
const MEETING_PIECE_SECONDS: usize = 300;
const MEETING_PIECE_SEARCH_SECONDS: usize = 10;

struct ActiveRecording {
    started: Instant,
    settings: Settings,
    intent: RecordingIntent,
    delivery_target: Option<DeliveryTarget>,
}

impl ActiveRecording {
    fn new(
        settings: Settings,
        intent: RecordingIntent,
        delivery_target: Option<DeliveryTarget>,
    ) -> Self {
        Self {
            started: Instant::now(),
            settings,
            intent,
            delivery_target,
        }
    }
}

#[derive(Clone)]
struct OverlayNotice {
    kind: NoticeKind,
    message: String,
    expires_at: Instant,
}

struct OverlayReducer {
    recording: Option<ListeningIntent>,
    jobs: BTreeMap<u64, ProcessingPhase>,
    active_job: Option<u64>,
    notice: Option<OverlayNotice>,
    last_view: OverlayViewState,
}

impl Default for OverlayReducer {
    fn default() -> Self {
        Self {
            recording: None,
            jobs: BTreeMap::new(),
            active_job: None,
            notice: None,
            last_view: OverlayViewState::Hidden,
        }
    }
}

impl OverlayReducer {
    fn recording_started(&mut self, intent: RecordingIntent) {
        self.recording = Some(match intent {
            RecordingIntent::Raw => ListeningIntent::Raw,
            RecordingIntent::Enhance => ListeningIntent::Enhance,
        });
    }

    fn recording_finished(&mut self) {
        self.recording = None;
    }

    fn job_queued(&mut self, id: u64) {
        self.jobs.entry(id).or_insert(ProcessingPhase::Queued);
    }

    fn job_started(&mut self, id: u64) -> bool {
        if self.active_job.is_some_and(|active| active != id) {
            return false;
        }
        let Some(phase) = self.jobs.get_mut(&id) else {
            return false;
        };
        if *phase != ProcessingPhase::Queued {
            return false;
        }
        *phase = ProcessingPhase::Transcribing;
        self.active_job = Some(id);
        true
    }

    fn enhancement_started(&mut self, id: u64) -> bool {
        if self.active_job != Some(id) {
            return false;
        }
        let Some(phase) = self.jobs.get_mut(&id) else {
            return false;
        };
        if *phase != ProcessingPhase::Transcribing {
            return false;
        }
        *phase = ProcessingPhase::Enhancing;
        true
    }

    fn job_finished(&mut self, id: u64) -> bool {
        if self.jobs.remove(&id).is_none() {
            return false;
        }
        if self.active_job == Some(id) {
            self.active_job = None;
        }
        true
    }

    fn show_notice(&mut self, kind: NoticeKind, message: impl Into<String>, now: Instant) {
        let duration = match kind {
            NoticeKind::Success => Duration::from_millis(2500),
            NoticeKind::Info => Duration::from_secs(3),
            NoticeKind::Warning
            | NoticeKind::RawFallback
            | NoticeKind::ProviderWarning
            | NoticeKind::DeliveryWarning => Duration::from_secs(4),
            NoticeKind::Error => Duration::from_secs(5),
        };
        self.notice = Some(OverlayNotice {
            kind,
            message: message.into(),
            expires_at: now + duration,
        });
    }

    fn desired_view(&mut self, now: Instant) -> OverlayViewState {
        if self
            .notice
            .as_ref()
            .is_some_and(|notice| now >= notice.expires_at)
        {
            self.notice = None;
        }
        if let Some(intent) = self.recording {
            return OverlayViewState::listening(intent);
        }
        if let Some(id) = self.active_job {
            if let Some(phase) = self.jobs.get(&id) {
                let queued = self
                    .jobs
                    .values()
                    .filter(|phase| **phase == ProcessingPhase::Queued)
                    .count();
                return OverlayViewState::processing(id, *phase, queued);
            }
        }
        let queued = self
            .jobs
            .iter()
            .filter(|(_, phase)| **phase == ProcessingPhase::Queued)
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        if let Some(id) = queued.first() {
            return OverlayViewState::processing(
                *id,
                ProcessingPhase::Queued,
                queued.len().saturating_sub(1),
            );
        }
        if let Some(notice) = &self.notice {
            return OverlayViewState::notice(notice.kind, notice.message.clone());
        }
        OverlayViewState::Hidden
    }

    fn sync(&mut self, controller: &overlay::OverlayController, now: Instant) {
        let view = self.desired_view(now);
        if view != self.last_view {
            if let Err(error) = controller.set_state(view.clone()) {
                warn!(error, "failed to update overlay state");
            } else {
                self.last_view = view;
            }
        }
    }
}

struct State {
    settings: Settings,
    credential_health: CredentialHealth,
    settings_error: Option<String>,
    recorder: Recorder,
    tray: tray::Tray,
    overlay: overlay::OverlayController,
    overlay_state: OverlayReducer,
    active_recording: Option<ActiveRecording>,
    worker: Worker,
    engine_state: EngineState,
    model_state: Option<ModelState>,
    load_generation: u64,
    pending_jobs: usize,
    active_job: Option<u64>,
    shutting_down: bool,
    discarding_jobs: bool,
    hk_stop: Arc<AtomicBool>,
    delivery_interlock: Arc<paste::DeliveryInterlock>,
    settings_revision: Option<SettingsRevision>,
    proxy: EventLoopProxy<UserEvent>,
    diagnostics_exporting: bool,
    meeting: Option<MeetingRecorder>,
    /// Live meeting input level feeding the recording indicator.
    meeting_level: Arc<AtomicU32>,
    /// The meeting pill: same position as the dictation pill, shown for the
    /// whole recording except while the dictation pill is on screen.
    meeting_overlay: overlay::OverlayController,
    meeting_overlay_view: OverlayViewState,
    meeting_overlay_enabled: bool,
    /// Tray toggle for this session only. `None` follows the saved setting;
    /// the tray process never rewrites settings.json itself.
    meeting_indicator_override: Option<bool>,
    /// Recording finished; the notes question is open.
    pending_meeting: Option<PendingMeeting>,
    /// The meeting being recorded, if any.
    meeting_session: Option<MeetingSession>,
    next_meeting_session: u64,
    /// Delivered transcripts waiting for an automatic retry of AI cleanup.
    repairs: Vec<Repair>,
    repair_jobs: HashMap<u64, (u64, RepairStep)>,
    next_repair_id: u64,
    /// Long-form jobs (imported files, meetings) mapped to their output file.
    file_jobs: HashMap<u64, FileJob>,
    file_import_running: bool,
}

pub fn run() -> Result<()> {
    let (settings, credential_health, settings_error, settings_revision) =
        match Settings::load_runtime() {
            Ok(loaded) => (
                loaded.settings,
                loaded.credential_health,
                loaded.settings_error,
                Some(loaded.revision),
            ),
            Err(error) => {
                warn!(error = %error, "settings load failed; using defaults until a valid file is saved");
                (
                    Settings::default(),
                    CredentialHealth::default(),
                    Some(format!("settings could not be loaded: {error}")),
                    Settings::revision().ok(),
                )
            }
        };
    if let Some(message) = &credential_health.message {
        warn!(message, "credential migration needs attention");
    }
    if let Some(error) = &settings_error {
        warn!(
            error,
            "settings validation failed; using defaults until a valid file is saved"
        );
    }
    info!(
        engine = %settings.engine,
        auto_paste = settings.auto_paste,
        overlay = settings.overlay,
        sounds = settings.sounds,
        "settings loaded"
    );
    let recorder = Recorder::new()?;
    let tray = tray::Tray::new(&settings)?;
    tray.set_meeting_indicator_visible(settings.show_meeting_indicator);
    let worker = Worker::spawn()?;

    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    let hk_stop = Arc::new(AtomicBool::new(false));
    let delivery_interlock = Arc::new(paste::DeliveryInterlock::default());
    spawn_hotkey_thread(
        settings.clone(),
        settings_revision.clone(),
        proxy.clone(),
        hk_stop.clone(),
        delivery_interlock.clone(),
    )?;
    spawn_tick_thread(proxy.clone());

    // Visual recording indicator on its own thread + own Win32 message loop.
    // It only reads atomics from the recorder, so there's no shared state with
    // the main winit/tray loop that could deadlock.
    let overlay = overlay::spawn(
        overlay::OverlayHandles {
            level_x10000: recorder.last_rms_x10000.clone(),
            stop: hk_stop.clone(),
        },
        settings.overlay,
    )?;

    let meeting_level = Arc::new(AtomicU32::new(0));
    let meeting_overlay = overlay::spawn(
        overlay::OverlayHandles {
            level_x10000: meeting_level.clone(),
            stop: hk_stop.clone(),
        },
        settings.show_meeting_indicator,
    )?;
    let meeting_overlay_enabled = settings.show_meeting_indicator;

    let state = State {
        settings,
        credential_health,
        settings_error,
        recorder,
        tray,
        overlay,
        overlay_state: OverlayReducer::default(),
        active_recording: None,
        worker,
        engine_state: EngineState::NotStarted,
        model_state: None,
        load_generation: 0,
        pending_jobs: 0,
        active_job: None,
        shutting_down: false,
        discarding_jobs: false,
        hk_stop,
        delivery_interlock,
        settings_revision,
        proxy,
        diagnostics_exporting: false,
        meeting: None,
        meeting_level,
        meeting_overlay,
        meeting_overlay_view: OverlayViewState::Hidden,
        meeting_overlay_enabled,
        meeting_indicator_override: None,
        pending_meeting: None,
        meeting_session: None,
        next_meeting_session: 1,
        repairs: Vec::new(),
        repair_jobs: HashMap::new(),
        next_repair_id: 1,
        file_jobs: HashMap::new(),
        file_import_running: false,
    };

    // Wait mode: loop sleeps until a message arrives (tray click, user event,
    // window event). The hotkey + tick threads wake it via EventLoopProxy.
    // No more thread::sleep inside the message pump → tray stays responsive.
    event_loop.set_control_flow(ControlFlow::Wait);

    let mut app = AppHandler { state };
    app.start_engine_load();
    event_loop.run_app(&mut app)?;
    Ok(())
}

fn spawn_hotkey_thread(
    initial: Settings,
    initial_revision: Option<SettingsRevision>,
    proxy: EventLoopProxy<UserEvent>,
    stop: Arc<AtomicBool>,
    delivery_interlock: Arc<paste::DeliveryInterlock>,
) -> Result<()> {
    thread::Builder::new()
        .name("hotkey".into())
        .spawn(move || hotkey_loop(initial, initial_revision, proxy, stop, delivery_interlock))?;
    Ok(())
}

fn spawn_tick_thread(proxy: EventLoopProxy<UserEvent>) {
    // Tick the event loop every 100 ms so periodic tasks (tray menu poll,
    // staged engine, settings reload, transcribe-done flag) get serviced
    // even when no input is happening.
    thread::Builder::new()
        .name("tick".into())
        .spawn(move || loop {
            thread::sleep(Duration::from_millis(100));
            if proxy.send_event(UserEvent::Tick).is_err() {
                break;
            }
        })
        .ok();
}

fn hotkey_loop(
    initial: Settings,
    initial_revision: Option<SettingsRevision>,
    proxy: EventLoopProxy<UserEvent>,
    stop: Arc<AtomicBool>,
    delivery_interlock: Arc<paste::DeliveryInterlock>,
) {
    let mut settings = initial;
    let mut settings_revision = initial_revision;
    let mut last_settings_error: Option<String> = None;
    let mut next_press_epoch = 0_u64;
    // Try OS-level registration so other apps know the combo is taken.
    // If that fails (e.g. Windows already reserved it), fall through to
    // physical key-hook polling, which does not require RegisterHotKey.
    let mut _mgr: Option<hotkey::HotkeyManager> = match hotkey::HotkeyManager::register(&settings) {
        Ok(m) => Some(m),
        Err(e) => {
            warn!(error = %e, "RegisterHotKey failed; using key-state polling only");
            None
        }
    };
    // Track combo vk codes manually so polling works without a HotkeyManager.
    let mut trigger_vk = hotkey::trigger_vk_for(&settings.hotkey_trigger);
    let mut mod_vks: Vec<u32> = settings
        .hotkey_modifiers
        .iter()
        .map(|m| hotkey::mod_vk_for(m))
        .collect();
    let mut poll_state = hotkey::PollState::default();
    let mut hook_health = key_hook::HealthMonitor::new(
        hotkey::configured_vks(trigger_vk, &mod_vks),
        hotkey::secondary_key_down,
    );
    let mut last_check = Instant::now();

    'polling: while !stop.load(Ordering::Relaxed) {
        if let Some(ev) = hotkey::poll_combo(trigger_vk, &mod_vks, &mut poll_state) {
            let mut posted_press_epoch = None;
            let user_ev = match ev {
                hotkey::Event::Press { shift_down } => {
                    let epoch =
                        mark_press_pending(delivery_interlock.as_ref(), &mut next_press_epoch);
                    posted_press_epoch = Some(epoch);
                    UserEvent::HotkeyPress {
                        epoch,
                        shift_down,
                        delivery_target: capture_delivery_target(),
                    }
                }
                hotkey::Event::ShiftChanged { shift_down } => {
                    UserEvent::HotkeyShiftChanged { shift_down }
                }
                hotkey::Event::Release => UserEvent::HotkeyRelease,
            };
            if proxy.send_event(user_ev).is_err() {
                if let Some(epoch) = posted_press_epoch {
                    consume_press_pending(delivery_interlock.as_ref(), epoch);
                }
                break;
            }
        }
        if hook_health.observe(
            Instant::now(),
            hotkey::secondary_key_down,
            hotkey::configured_key_down,
        ) {
            warn!(
                events_seen = key_hook::events_seen(),
                "configured key transitions were repeatedly absent from the keyboard hook; requesting reinstall"
            );
            if !emit_release_before_poll_reset(&proxy, &mut poll_state) {
                break;
            }
            key_hook::request_reinstall();
        }

        if last_check.elapsed() >= Duration::from_millis(500) {
            last_check = Instant::now();
            match Settings::revision() {
                Ok(revision) if settings_revision.as_ref() != Some(&revision) => {
                    settings_revision = Some(revision);
                    match Settings::load() {
                        Ok(new) => {
                            last_settings_error = None;
                            if new.hotkey_modifiers != settings.hotkey_modifiers
                                || new.hotkey_trigger != settings.hotkey_trigger
                            {
                                info!(
                                    "hotkey changed: {:?}+{} -> {:?}+{}",
                                    settings.hotkey_modifiers,
                                    settings.hotkey_trigger,
                                    new.hotkey_modifiers,
                                    new.hotkey_trigger
                                );
                                if !emit_release_before_poll_reset(&proxy, &mut poll_state) {
                                    break 'polling;
                                }
                                drop(_mgr.take());
                                _mgr = hotkey::HotkeyManager::register(&new).ok();
                                trigger_vk = hotkey::trigger_vk_for(&new.hotkey_trigger);
                                mod_vks = new
                                    .hotkey_modifiers
                                    .iter()
                                    .map(|modifier| hotkey::mod_vk_for(modifier))
                                    .collect();
                                hook_health = key_hook::HealthMonitor::new(
                                    hotkey::configured_vks(trigger_vk, &mod_vks),
                                    hotkey::secondary_key_down,
                                );
                            }
                            settings = new;
                        }
                        Err(error) => {
                            let message = error.to_string();
                            if last_settings_error.as_deref() != Some(message.as_str()) {
                                warn!(error = %message, "hotkey settings reload failed; keeping the last valid hotkey");
                                last_settings_error = Some(message);
                            }
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    let message = error.to_string();
                    if last_settings_error.as_deref() != Some(message.as_str()) {
                        warn!(error = %message, "hotkey settings revision check failed; keeping the last valid hotkey");
                        last_settings_error = Some(message);
                    }
                }
            }
        }

        thread::sleep(Duration::from_millis(8));
    }
    info!("hotkey thread exiting");
}

fn emit_release_before_poll_reset(
    proxy: &EventLoopProxy<UserEvent>,
    poll_state: &mut hotkey::PollState,
) -> bool {
    match hotkey::release_before_reset(poll_state) {
        Some(hotkey::Event::Release) => proxy.send_event(UserEvent::HotkeyRelease).is_ok(),
        Some(hotkey::Event::Press { .. } | hotkey::Event::ShiftChanged { .. }) => {
            unreachable!("reset can only synthesize release")
        }
        None => true,
    }
}

fn mark_press_pending(interlock: &paste::DeliveryInterlock, next_epoch: &mut u64) -> u64 {
    interlock.mark_press_pending(next_epoch)
}

fn consume_press_pending(interlock: &paste::DeliveryInterlock, epoch: u64) {
    interlock.consume_press(epoch);
}

fn recording_or_press_pending(
    recording_active: bool,
    interlock: &paste::DeliveryInterlock,
) -> bool {
    recording_active || interlock.press_pending()
}

fn capture_delivery_target() -> Option<DeliveryTarget> {
    use windows::Win32::UI::WindowsAndMessaging::{
        GetAncestor, GetForegroundWindow, GetWindowThreadProcessId, GA_ROOT,
    };

    let foreground = unsafe { GetForegroundWindow() };
    if foreground.0.is_null() {
        return None;
    }
    let hwnd = unsafe { GetAncestor(foreground, GA_ROOT) };
    if hwnd.0.is_null() {
        return None;
    }
    let mut process_id = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    }
    if process_id == 0 {
        return None;
    }
    let process_creation_time_100ns = process_creation_time_100ns(process_id)?;
    Some(DeliveryTarget {
        hwnd: hwnd.0 as isize,
        process_id,
        process_creation_time_100ns: Some(process_creation_time_100ns),
    })
}

fn delivery_target_is_current(target: DeliveryTarget) -> bool {
    capture_delivery_target().is_some_and(|current| current == target)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeliveryBlockReason {
    RecordingActive,
    TargetChanged,
    Cancelled,
    ClipboardChanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeliveryPlan {
    Paste,
    ClipboardExplicit,
    ClipboardSafety(DeliveryBlockReason),
}

fn decide_delivery(
    auto_paste: bool,
    recording_active: bool,
    target_present: bool,
    target_matches: bool,
    cancelled_or_tombstoned: bool,
) -> DeliveryPlan {
    if !auto_paste {
        return DeliveryPlan::ClipboardExplicit;
    }
    if cancelled_or_tombstoned {
        return DeliveryPlan::ClipboardSafety(DeliveryBlockReason::Cancelled);
    }
    if recording_active {
        return DeliveryPlan::ClipboardSafety(DeliveryBlockReason::RecordingActive);
    }
    if !target_present || !target_matches {
        return DeliveryPlan::ClipboardSafety(DeliveryBlockReason::TargetChanged);
    }
    DeliveryPlan::Paste
}

fn cleanup_notice_kind(reason: &FallbackReason) -> NoticeKind {
    match reason {
        FallbackReason::UnknownProvider
        | FallbackReason::MissingCredential
        | FallbackReason::CredentialTargetChanged
        | FallbackReason::InvalidEndpoint
        | FallbackReason::EmptyModelId
        | FallbackReason::RequestFailed
        | FallbackReason::ProviderRejected(_)
        | FallbackReason::ResponseUnparseable => NoticeKind::ProviderWarning,
        FallbackReason::EmptyCandidate
        | FallbackReason::IntegrityRejected(_)
        | FallbackReason::ValidatorError => NoticeKind::RawFallback,
    }
}

fn process_creation_time_100ns(process_id: u32) -> Option<u64> {
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
    use windows_sys::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };

    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, process_id) };
    if process.is_null() {
        return None;
    }
    let empty_time = || FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let mut creation = empty_time();
    let mut exit = empty_time();
    let mut kernel = empty_time();
    let mut user = empty_time();
    let success =
        unsafe { GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user) } != 0;
    unsafe {
        let _ = CloseHandle(process);
    }
    success.then(|| ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64)
}

struct AppHandler {
    state: State,
}

impl AppHandler {
    fn sync_overlay(&mut self) {
        self.state
            .overlay_state
            .sync(&self.state.overlay, Instant::now());
    }

    fn show_overlay_notice(&mut self, kind: NoticeKind, message: impl Into<String>) {
        self.state
            .overlay_state
            .show_notice(kind, message, Instant::now());
        self.sync_overlay();
    }

    fn start_engine_load(&mut self) {
        if self.state.shutting_down {
            return;
        }
        let engine_name = self.state.settings.engine.clone();
        match self.state.worker.load(engine_name.clone()) {
            Ok(generation) => {
                self.state.load_generation = generation;
                self.state.engine_state = EngineState::Loading;
                self.state.model_state = None;
                self.state.tray.set_status(
                    tray::IconColor::Transcribing,
                    &format!("OpenWritr — loading {engine_name}"),
                );
            }
            Err(error) => {
                self.state.engine_state = EngineState::Failed {
                    error: error.to_string(),
                };
                self.state.tray.set_status(
                    tray::IconColor::Error,
                    "OpenWritr — could not start engine loader (see log)",
                );
                warn!(error = %error, "failed to start engine load");
            }
        }
    }
}

impl ApplicationHandler<UserEvent> for AppHandler {
    fn resumed(&mut self, _el: &ActiveEventLoop) {
        info!("event loop ready");
    }

    fn window_event(&mut self, _el: &ActiveEventLoop, _id: WindowId, _ev: WindowEvent) {}

    fn user_event(&mut self, el: &ActiveEventLoop, ev: UserEvent) {
        match ev {
            UserEvent::HotkeyPress {
                epoch,
                shift_down,
                delivery_target,
            } => {
                consume_press_pending(self.state.delivery_interlock.as_ref(), epoch);
                self.on_press(shift_down, delivery_target);
            }
            UserEvent::HotkeyShiftChanged { shift_down } => {
                self.on_shift_changed(shift_down);
            }
            UserEvent::HotkeyRelease => self.on_release(),
            UserEvent::DiagnosticsExported => {
                self.state.diagnostics_exporting = false;
                self.state
                    .tray
                    .set_tooltip("OpenWritr — diagnostics exported");
            }
            UserEvent::DiagnosticsExportFailed(error) => {
                self.state.diagnostics_exporting = false;
                warn!(error = %error, "diagnostics export failed");
                self.state
                    .tray
                    .set_tooltip("OpenWritr — diagnostics export failed (see log)");
            }
            UserEvent::FileDecoded {
                path,
                samples,
                output,
                markdown,
                window,
            } => {
                self.state.file_import_running = false;
                self.dispatch_file(path, samples, output, markdown, window);
            }
            UserEvent::MeetingNotesChoice(create_notes) => {
                self.finish_pending_meeting(create_notes);
            }
            UserEvent::FileImportEnded(error) => {
                self.state.file_import_running = false;
                if let Some(error) = error {
                    // The popup already shows the reason; no pill for files.
                    warn!(error = %error, "file import failed");
                    self.state.tray.set_status(
                        tray::IconColor::Error,
                        &format!("OpenWritr — could not read file: {}", short_error(&error)),
                    );
                } else {
                    self.update_job_status();
                }
            }
            UserEvent::Tick => self.tick(el),
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        // Wait mode: don't sleep here; tick thread + user events wake us.
        self.tick(el);
        el.set_control_flow(ControlFlow::Wait);
    }
}

impl AppHandler {
    fn tick(&mut self, el: &ActiveEventLoop) {
        while let Some(event) = self.state.worker.try_recv() {
            self.handle_worker_event(el, event);
        }
        self.sync_overlay();
        self.sync_meeting_indicator();

        if self.state.active_recording.is_some() {
            let stream_failed = self.state.recorder.stream_failed();
            let stream_stalled = self
                .state
                .recorder
                .callback_stalled(Duration::from_millis(500));
            if stream_failed || stream_stalled {
                let max_record_seconds = self
                    .state
                    .active_recording
                    .as_ref()
                    .map(|recording| recording.settings.max_record_seconds)
                    .expect("active recording disappeared");
                match self.state.recorder.recover_stream(max_record_seconds) {
                    Ok(capture) => {
                        warn!(
                            stream_failed,
                            stream_stalled,
                            device = %capture.device_name,
                            "capture stream recovered during active recording"
                        );
                    }
                    Err(error) => {
                        warn!(
                            stream_failed,
                            stream_stalled,
                            error = %error,
                            "capture stream recovery failed"
                        );
                        self.state.recorder.mark_stream_failed(error.to_string());
                        self.finish_recording(false);
                    }
                }
            } else {
                let active_recording = self
                    .state
                    .active_recording
                    .as_ref()
                    .expect("active recording disappeared");
                let max_seconds = active_recording.settings.max_record_seconds;
                let timer_limit_reached = self
                    .state
                    .active_recording
                    .as_ref()
                    .map(|recording| recording.started.elapsed().as_secs_f32() >= max_seconds)
                    .unwrap_or(false);
                if self.state.recorder.limit_reached() || timer_limit_reached {
                    self.finish_recording(true);
                }
            }
        }

        let meeting_status = self
            .state
            .meeting
            .as_ref()
            .map(|meeting| (meeting.limit_reached(), meeting.failure()));
        if let Some((limit_reached, failure)) = meeting_status {
            if limit_reached || failure.is_some() {
                if let Some(failure) = failure {
                    warn!(
                        failure,
                        "meeting capture failed; transcribing what was captured"
                    );
                }
                self.stop_meeting(true);
            }
        }

        self.run_due_repairs();

        // Rolling transcription: five minutes of finished audio go to the
        // transcriber while the recording carries on.
        if self
            .state
            .meeting
            .as_ref()
            .is_some_and(|recorder| !recorder.is_paused() && recorder.roll_due())
        {
            self.roll_meeting_part();
        }

        // Drain tray menu events.
        while let Ok(ev) = MenuEvent::receiver().try_recv() {
            if ev.id == self.state.tray.menu_quit_id {
                self.request_shutdown(el);
                return;
            }
            if ev.id == self.state.tray.menu_cancel_model_id {
                if matches!(self.state.engine_state, EngineState::Loading) {
                    self.state.load_generation = self.state.worker.cancel_load();
                    self.state.engine_state = EngineState::NotStarted;
                    self.state.model_state = Some(ModelState::Cancelled);
                    self.state.tray.set_status(
                        tray::IconColor::Error,
                        "OpenWritr — model download cancelled; choose Retry",
                    );
                    info!("model acquisition cancelled by user");
                }
                continue;
            }
            if ev.id == self.state.tray.menu_retry_engine_id {
                self.start_engine_load();
                continue;
            }
            if ev.id == self.state.tray.menu_open_logs_id {
                if let Err(e) = diagnostics::open_logs_dir() {
                    warn!(error = %e, "failed to open logs directory");
                    self.state.tray.set_status(
                        tray::IconColor::Error,
                        "OpenWritr — could not open logs (see log)",
                    );
                }
                continue;
            }
            if ev.id == self.state.tray.menu_export_diagnostics_id {
                if !self.state.diagnostics_exporting {
                    self.state.diagnostics_exporting = true;
                    self.state
                        .tray
                        .set_tooltip("OpenWritr — exporting diagnostics…");
                    let settings = self.state.settings.clone();
                    let proxy = self.state.proxy.clone();
                    let spawn = thread::Builder::new()
                        .name("diagnostics-export".into())
                        .spawn(move || {
                            let event = match diagnostics::export_bundle(&settings) {
                                Ok(path) => {
                                    info!(file = %path.display(), "diagnostics exported");
                                    if let Err(e) = diagnostics::reveal(&path) {
                                        warn!(error = %e, "failed to reveal diagnostics bundle");
                                    }
                                    UserEvent::DiagnosticsExported
                                }
                                Err(e) => UserEvent::DiagnosticsExportFailed(e.to_string()),
                            };
                            let _ = proxy.send_event(event);
                        });
                    if let Err(e) = spawn {
                        self.state.diagnostics_exporting = false;
                        warn!(error = %e, "failed to start diagnostics export");
                        self.state
                            .tray
                            .set_tooltip("OpenWritr — diagnostics export failed (see log)");
                    }
                } else {
                    self.state
                        .tray
                        .set_tooltip("OpenWritr — diagnostics export already running");
                }
                continue;
            }
            if ev.id == self.state.tray.menu_transcribe_file_id {
                self.start_file_import();
                continue;
            }
            if ev.id == self.state.tray.menu_meeting_id {
                if self.state.meeting.is_some() {
                    self.stop_meeting(true);
                } else {
                    self.start_meeting();
                }
                continue;
            }
            if ev.id == self.state.tray.menu_meeting_pause_id {
                self.toggle_meeting_pause();
                continue;
            }
            if ev.id == self.state.tray.menu_meeting_indicator_id {
                self.toggle_meeting_indicator();
                continue;
            }
            if ev.id == self.state.tray.menu_settings_id {
                info!("opening settings UI subprocess");
                // CRITICAL: spawn from a background thread. CreateProcessW on
                // Windows ARM64 (especially with Defender real-time scanning)
                // can block for several seconds. If we call it from inside the
                // winit pump, the tray's message queue stalls → app goes
                // "Not Responding" → hotkey dies. From a worker thread the
                // main pump keeps draining messages while the child boots.
                if let Ok(exe) = std::env::current_exe() {
                    thread::spawn(move || {
                        let _ = std::process::Command::new(exe)
                            .arg("--settings")
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
                            .spawn();
                    });
                }
                continue;
            }
            if ev.id == self.state.tray.menu_about_id {
                info!("opening About UI subprocess");
                if let Ok(exe) = std::env::current_exe() {
                    thread::spawn(move || {
                        let _ = std::process::Command::new(exe)
                            .arg("--about")
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
                            .spawn();
                    });
                }
            }
        }

        // Settings hot-reload compares content revisions so deletion is a
        // first-class change and both loops return to the same defaults.
        {
            match Settings::revision() {
                Ok(revision) if self.state.settings_revision.as_ref() != Some(&revision) => {
                    match Settings::load_runtime() {
                        Ok(loaded) => {
                            self.state.settings_revision = Some(loaded.revision.clone());
                            self.state.credential_health = loaded.credential_health;
                            if let Some(message) = &self.state.credential_health.message {
                                warn!(message, "credential migration needs attention");
                            }
                            if let Some(error) = loaded.settings_error {
                                warn!(error, "settings reload failed validation; keeping the last valid settings");
                                self.state.settings_error = Some(error);
                                self.update_job_status();
                            } else {
                                let old_engine = self.state.settings.engine.clone();
                                let old_overlay = self.state.settings.overlay;
                                let old_indicator = self.state.settings.show_meeting_indicator;
                                self.state.settings = loaded.settings;
                                self.state.settings_error = None;
                                if self.state.settings.show_meeting_indicator != old_indicator {
                                    // A saved change replaces any tray toggle.
                                    self.state.meeting_indicator_override = None;
                                    self.sync_meeting_indicator();
                                }
                                if self.state.settings.overlay != old_overlay {
                                    if let Err(error) =
                                        self.state.overlay.set_enabled(self.state.settings.overlay)
                                    {
                                        warn!(error, "failed to update overlay setting");
                                    }
                                    self.sync_overlay();
                                }
                                let new_engine = self.state.settings.engine.clone();
                                if new_engine != old_engine {
                                    info!(
                                        "engine changed: {old_engine} -> {new_engine}; reloading"
                                    );
                                    self.start_engine_load();
                                } else {
                                    self.update_job_status();
                                }
                            }
                        }
                        Err(error) => {
                            warn!(error = %error, "settings reload failed; keeping the last valid settings");
                            self.state.settings_error =
                                Some(format!("settings could not be reloaded: {error}"));
                            self.update_job_status();
                        }
                    }
                }
                Ok(_) => {}
                Err(error) => {
                    let message = format!("settings revision could not be read: {error}");
                    if self.state.settings_error.as_deref() != Some(message.as_str()) {
                        warn!(error = %error, "settings revision check failed; keeping the last valid settings");
                        self.state.settings_error = Some(message);
                        self.update_job_status();
                    }
                }
            }
        }
    }

    fn on_press(&mut self, shift_down: bool, delivery_target: Option<DeliveryTarget>) {
        if self.state.shutting_down || self.state.active_recording.is_some() {
            return;
        }
        if !matches!(self.state.engine_state, EngineState::Ready { .. }) {
            let (color, status) = self.blocked_recording_status();
            self.state.tray.set_status(color, &status);
            self.show_overlay_notice(
                NoticeKind::Error,
                status
                    .strip_prefix("OpenWritr — ")
                    .unwrap_or(&status)
                    .to_string(),
            );
            info!(status, "recording blocked while engine is unavailable");
            return;
        }

        if self.state.active_recording.is_none() {
            let settings = self.state.settings.clone();
            let shift_is_modifier = settings
                .hotkey_modifiers
                .iter()
                .any(|modifier| modifier == "shift");
            let intent = recording_intent(settings.enhance.mode, shift_is_modifier, shift_down);
            if settings.enhance.mode != crate::settings::EnhanceMode::Never
                && settings.enhance.provider == "openai_compatible"
            {
                crate::local_llm::prewarm(&settings.enhance.base_url, &settings.enhance.model);
            }
            match self.state.recorder.start(settings.max_record_seconds) {
                Ok(capture) => {
                    self.state.tray.set_status(
                        tray::IconColor::Recording,
                        &format!("OpenWritr — recording from {}", capture.device_name),
                    );
                    let active_recording = ActiveRecording::new(settings, intent, delivery_target);
                    if active_recording.settings.sounds {
                        sounds::play_start();
                    }
                    self.state.active_recording = Some(active_recording);
                    self.state.overlay_state.recording_started(intent);
                    self.sync_overlay();
                    info!(
                        device = %capture.device_name,
                        sample_rate = capture.sample_rate,
                        channels = capture.channels,
                        ?intent,
                        ?delivery_target,
                        "recording start"
                    );
                }
                Err(e) => {
                    self.state.tray.set_status(
                        tray::IconColor::Error,
                        "OpenWritr — microphone unavailable (see log)",
                    );
                    self.show_overlay_notice(NoticeKind::Error, "Microphone unavailable");
                    warn!(error = %e, "failed to start recording");
                }
            }
        }
    }

    fn on_release(&mut self) {
        if self.state.active_recording.is_some() {
            self.finish_recording(false);
        }
    }

    fn on_shift_changed(&mut self, shift_down: bool) {
        let Some(active_recording) = self.state.active_recording.as_mut() else {
            return;
        };
        let shift_is_modifier = active_recording
            .settings
            .hotkey_modifiers
            .iter()
            .any(|modifier| modifier == "shift");
        let intent = recording_intent(
            active_recording.settings.enhance.mode,
            shift_is_modifier,
            shift_down,
        );
        if intent == active_recording.intent {
            return;
        }

        active_recording.intent = intent;
        self.state.overlay_state.recording_started(intent);
        self.sync_overlay();
        info!(?intent, "recording enhancement intent changed");
    }

    fn finish_recording(&mut self, timer_limit_reached: bool) {
        let Some(active_recording) = self.state.active_recording.take() else {
            return;
        };
        self.state.overlay_state.recording_finished();
        let recording = match self.state.recorder.stop() {
            Ok(recording) => recording,
            Err(e) => {
                self.state.tray.set_status(
                    tray::IconColor::Error,
                    "OpenWritr — microphone error (see log)",
                );
                self.show_overlay_notice(NoticeKind::Error, "Microphone error");
                warn!(error = %e, "failed to stop recording");
                return;
            }
        };
        if active_recording.settings.sounds {
            sounds::play_stop();
        }
        if let Some(error) = recording.stream_error {
            self.state.tray.set_status(
                tray::IconColor::Error,
                "OpenWritr — microphone stream failed (see log)",
            );
            self.show_overlay_notice(NoticeKind::Error, "Microphone stream failed");
            warn!(error = %error, "recording aborted after stream failure");
            return;
        }

        self.state
            .tray
            .set_status(tray::IconColor::Idle, "OpenWritr");
        let reached_limit = timer_limit_reached || recording.reached_limit;
        if reached_limit {
            info!(
                max_seconds = active_recording.settings.max_record_seconds,
                "maximum recording duration reached"
            );
        }

        let dur = active_recording.started.elapsed();
        let min = active_recording.settings.min_record_seconds;
        if dur.as_secs_f32() < min {
            info!(secs = dur.as_secs_f32(), "below min — discarded");
            self.sync_overlay();
        } else {
            info!(
                device = %recording.device_name,
                sample_rate = recording.sample_rate,
                channels = recording.channels,
                samples = recording.samples.len(),
                drained_samples = recording.drained_samples,
                stream_recoveries = recording.stream_recoveries,
                reached_limit,
                "recording stop"
            );
            self.dispatch_transcribe(recording.samples, recording.sample_rate, active_recording);
        }
    }
}

fn recording_intent(
    mode: EnhanceMode,
    shift_is_modifier: bool,
    shift_is_down: bool,
) -> RecordingIntent {
    let additional_shift_is_down = shift_is_down && !shift_is_modifier;
    match mode {
        EnhanceMode::Never => RecordingIntent::Raw,
        EnhanceMode::WithShift if additional_shift_is_down => RecordingIntent::Enhance,
        EnhanceMode::WithShift => RecordingIntent::Raw,
        EnhanceMode::Always if additional_shift_is_down => RecordingIntent::Raw,
        EnhanceMode::Always => RecordingIntent::Enhance,
    }
}

impl AppHandler {
    fn dispatch_transcribe(
        &mut self,
        samples: Vec<f32>,
        sr: u32,
        active_recording: ActiveRecording,
    ) {
        if self.state.shutting_down {
            warn!("discarding completed recording because shutdown has started");
            self.sync_overlay();
            return;
        }
        let config = JobConfig::new(
            active_recording.settings,
            active_recording.intent,
            active_recording.delivery_target,
            false,
        );
        match self.state.worker.enqueue(samples, sr, config) {
            Ok(id) => {
                self.state.pending_jobs = self.state.pending_jobs.saturating_add(1);
                self.state.overlay_state.job_queued(id);
                info!(
                    id,
                    queue_depth = self.state.pending_jobs,
                    "transcription job queued"
                );
                self.update_job_status();
                self.sync_overlay();
            }
            Err(error) => {
                warn!(error = %error, "failed to queue transcription");
                self.state.tray.set_status(
                    tray::IconColor::Error,
                    "OpenWritr — could not queue transcription (see log)",
                );
                self.show_overlay_notice(NoticeKind::Error, "Could not queue transcription");
            }
        }
    }

    /// Gate shared by file import and meetings: they need a loaded engine.
    fn require_ready_engine(&mut self) -> bool {
        if self.state.shutting_down {
            return false;
        }
        if matches!(self.state.engine_state, EngineState::Ready { .. }) {
            return true;
        }
        let (color, status) = self.blocked_recording_status();
        self.state.tray.set_status(color, &status);
        self.show_overlay_notice(NoticeKind::Warning, "Engine is not ready yet");
        false
    }

    fn start_file_import(&mut self) {
        if self.state.file_import_running {
            return;
        }
        if !self.require_ready_engine() {
            return;
        }
        self.state.file_import_running = true;
        let proxy = self.state.proxy.clone();
        let markdown = self.state.settings.markdown_transcripts;
        let cleanup = self.state.settings.enhance.mode.is_enabled();
        let cleanup_engine = crate::enhance::cleanup_engine_label(&self.state.settings);
        let engine = match &self.state.engine_state {
            EngineState::Ready { label } => label.clone(),
            _ => self.state.settings.engine.clone(),
        };
        // The Open dialog blocks and decoding is CPU-heavy: neither may run
        // on the tray event loop.
        let spawned = thread::Builder::new()
            .name("file-import".into())
            .spawn(move || {
                let Some(path) = media::pick_audio_file() else {
                    let _ = proxy.send_event(UserEvent::FileImportEnded(None));
                    return;
                };
                let event = import_file(path, markdown, cleanup, cleanup_engine, engine);
                let _ = proxy.send_event(event);
            });
        if let Err(error) = spawned {
            self.state.file_import_running = false;
            warn!(error = %error, "failed to start file import");
        }
    }

    fn dispatch_file(
        &mut self,
        source: PathBuf,
        samples: Vec<f32>,
        output: PathBuf,
        markdown: bool,
        window: mpsc::Sender<WindowMsg>,
    ) {
        let name = source
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "audio file".into());
        let title = source
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Transcript".into());
        let seconds = samples.len() / media::TARGET_RATE as usize;
        let job = FileJob {
            output,
            markdown,
            notes: false,
            notes_output: None,
            title,
            details: format!(
                "Transcript of {name} ({}) · OpenWritr",
                format_duration(seconds)
            ),
            window: Some(window),
            background: false,
            session: None,
            partial: None,
        };
        self.dispatch_long_form(samples, job, "file");
    }

    fn dispatch_long_form(&mut self, samples: Vec<f32>, job: FileJob, kind: &str) {
        if self.state.shutting_down {
            job.tell_window(WindowMsg::Failed {
                error: "OpenWritr is shutting down".into(),
            });
            return;
        }
        let config = match &job.session {
            Some((part, settings)) => JobConfig::meeting_part(settings.clone(), *part, job.notes),
            None if job.notes => JobConfig::meeting_notes(self.state.settings.clone()),
            None => JobConfig::long_form(self.state.settings.clone()),
        };
        match self
            .state
            .worker
            .enqueue(samples, media::TARGET_RATE, config)
        {
            Ok(id) => {
                let windowed = job.overlay_free();
                job.tell_window(WindowMsg::Stage {
                    stage: transcript_window::Stage::Queued,
                    done: 0,
                    total: 0,
                });
                self.state.file_jobs.insert(id, job);
                self.state.pending_jobs = self.state.pending_jobs.saturating_add(1);
                if !windowed {
                    self.state.overlay_state.job_queued(id);
                }
                info!(id, kind, "long-form transcription job queued");
                self.update_job_status();
                self.sync_overlay();
            }
            Err(error) => {
                warn!(error = %error, "failed to queue long-form transcription");
                let reported = job.tell_window(WindowMsg::Failed {
                    error: format!("could not queue transcription: {error}"),
                });
                if reported {
                    self.state.tray.set_status(
                        tray::IconColor::Error,
                        "OpenWritr — could not queue transcription (see log)",
                    );
                } else {
                    self.show_job_warning("Could not queue transcription (see log)");
                }
            }
        }
    }

    /// Whether the meeting recording indicator should be on screen: the saved
    /// setting, unless the tray menu overrode it for this session.
    fn meeting_indicator_visible(&self) -> bool {
        self.state
            .meeting_indicator_override
            .unwrap_or(self.state.settings.show_meeting_indicator)
    }

    /// Push the indicator's visibility and current view to its pill. Cheap:
    /// only sends a command when something changed (about once a second
    /// while recording, for the clock).
    fn sync_meeting_indicator(&mut self) {
        let visible = self.meeting_indicator_visible();
        if visible != self.state.meeting_overlay_enabled {
            match self.state.meeting_overlay.set_enabled(visible) {
                Ok(()) => self.state.meeting_overlay_enabled = visible,
                Err(error) => warn!(error, "failed to update meeting indicator"),
            }
            self.state.tray.set_meeting_indicator_visible(visible);
        }
        // The meeting pill occupies the same spot as the dictation pill, so
        // it steps aside while that one is on screen (listening, writing, or
        // a notice) and returns as soon as it clears.
        let dictation_pill_showing =
            self.state.settings.overlay && !self.state.overlay_state.last_view.is_hidden();
        let desired = match &self.state.meeting {
            Some(recorder) if !dictation_pill_showing => {
                OverlayViewState::meeting(recorder.elapsed().as_secs(), recorder.is_paused())
            }
            _ => OverlayViewState::Hidden,
        };
        if desired != self.state.meeting_overlay_view {
            match self.state.meeting_overlay.set_state(desired.clone()) {
                Ok(()) => self.state.meeting_overlay_view = desired,
                Err(error) => warn!(error, "failed to update meeting indicator"),
            }
        }
    }

    fn toggle_meeting_indicator(&mut self) {
        let show = !self.meeting_indicator_visible();
        self.state.meeting_indicator_override = Some(show);
        self.state.tray.set_meeting_indicator_visible(show);
        self.sync_meeting_indicator();
        if self.state.meeting.is_some() {
            // Hiding must never be silent while a recording is running.
            let message = if show {
                "Recording indicator shown"
            } else {
                "Indicator hidden · still recording (tray icon is red)"
            };
            self.show_overlay_notice(NoticeKind::Info, message);
        }
    }

    fn start_meeting(&mut self) {
        if self.state.pending_meeting.is_some() {
            self.show_overlay_notice(
                NoticeKind::Warning,
                "Answer the meeting notes question first",
            );
            return;
        }
        if !self.require_ready_engine() {
            return;
        }
        match MeetingRecorder::start(self.state.meeting_level.clone()) {
            Ok(recorder) => {
                let sources = recorder.sources();
                let id = self.state.next_meeting_session;
                self.state.next_meeting_session += 1;
                let started = meeting::LocalTime::now();
                self.state.meeting_session = Some(MeetingSession {
                    id,
                    started,
                    partial: media::unique_path(meeting::partial_output_path(started)),
                    settings: self.state.settings.clone(),
                });
                self.state.meeting = Some(recorder);
                self.state
                    .tray
                    .set_meeting_state(tray::MeetingMenuState::Recording);
                self.state.tray.set_status(
                    tray::IconColor::Recording,
                    &format!("OpenWritr — recording meeting ({sources})"),
                );
                let hidden_note = if self.meeting_indicator_visible() {
                    ""
                } else {
                    " (indicator hidden)"
                };
                self.show_overlay_notice(
                    NoticeKind::Success,
                    format!("Recording meeting: {sources}{hidden_note}"),
                );
            }
            Err(error) => {
                warn!(error = %error, "could not start meeting capture");
                self.show_job_warning(&format!(
                    "Could not start meeting recording: {}",
                    short_error(&error.to_string())
                ));
            }
        }
    }

    fn toggle_meeting_pause(&mut self) {
        let Some(recorder) = self.state.meeting.as_mut() else {
            return;
        };
        if recorder.is_paused() {
            let resumed = recorder.resume().map(|()| recorder.sources());
            match resumed {
                Ok(_) => {
                    self.state
                        .tray
                        .set_meeting_state(tray::MeetingMenuState::Recording);
                    self.update_job_status();
                    self.show_overlay_notice(NoticeKind::Success, "Meeting recording resumed");
                }
                Err(error) => {
                    // Still paused: the user can retry or stop and keep what
                    // was recorded so far.
                    warn!(error = %error, "could not resume meeting capture");
                    self.show_job_warning(&format!(
                        "Could not resume the meeting: {}",
                        short_error(&error.to_string())
                    ));
                }
            }
        } else {
            recorder.pause();
            // Nothing more is recorded while paused, so use the time: hand
            // everything recorded so far to the transcriber now.
            let segment = recorder.take_segment(false, meeting::MIN_PAUSE_SEGMENT_SAMPLES);
            self.state
                .tray
                .set_meeting_state(tray::MeetingMenuState::Paused);
            self.update_job_status();
            self.show_overlay_notice(NoticeKind::Info, "Meeting recording paused");
            if let Some(segment) = segment {
                self.queue_meeting_part(segment);
            }
        }
    }

    /// Cut the finished stretch of the recording off for transcription.
    /// Called once an hour of audio is waiting (see `MeetingRecorder::roll_due`).
    fn roll_meeting_part(&mut self) {
        let segment = self
            .state
            .meeting
            .as_mut()
            .and_then(|recorder| recorder.take_segment(true, 1));
        if let Some(segment) = segment {
            info!(
                seconds = segment.samples.len() / media::TARGET_RATE as usize,
                "rolling meeting part cut"
            );
            self.queue_meeting_part(segment);
        }
    }

    /// Queue a finished part of the running meeting as background jobs.
    fn queue_meeting_part(&mut self, segment: meeting::Segment) {
        let Some(session) = &self.state.meeting_session else {
            return;
        };
        let (id, settings, partial) = (
            session.id,
            session.settings.clone(),
            session.partial.clone(),
        );
        let pieces = media::plan_windows(
            &segment.samples,
            media::TARGET_RATE,
            MEETING_PIECE_SECONDS,
            MEETING_PIECE_SEARCH_SECONDS,
        );
        for range in pieces {
            let part = SessionPart {
                id,
                offset_samples: segment.start + range.start,
                last: false,
            };
            let job = FileJob {
                // Where this part's text is appended as soon as it is ready.
                output: partial.clone(),
                markdown: false,
                notes: false,
                notes_output: None,
                title: String::new(),
                details: String::new(),
                window: None,
                background: true,
                session: Some((part, settings.clone())),
                partial: None,
            };
            self.dispatch_long_form(segment.samples[range].to_vec(), job, "meeting part");
        }
    }

    /// End the recording. With `ask_for_notes`, offer to also create AI notes
    /// (only when a provider is configured); otherwise queue the transcript.
    fn stop_meeting(&mut self, ask_for_notes: bool) {
        let Some(recorder) = self.state.meeting.take() else {
            return;
        };
        self.state
            .tray
            .set_meeting_state(tray::MeetingMenuState::Idle);
        let recorded = recorder.elapsed();
        let tail = recorder.finish();
        let session = self.state.meeting_session.take();
        let total_samples = tail.start + tail.samples.len();
        let Some(session) = session else {
            return;
        };
        if total_samples < media::TARGET_RATE as usize {
            self.show_job_warning("Meeting recording was too short to transcribe");
            self.update_job_status();
            return;
        }
        info!(seconds = recorded.as_secs(), "meeting capture stopped");
        let pending = PendingMeeting {
            tail,
            session,
            recorded,
        };

        let provider_ready = enhance::notes_configuration(&self.state.settings).is_ok();
        let decision = notes_decision(
            self.state.settings.meeting_notes,
            provider_ready,
            ask_for_notes,
        );
        match decision {
            NotesDecision::Create | NotesDecision::Skip | NotesDecision::SkipNoProvider => {
                if decision == NotesDecision::SkipNoProvider {
                    self.show_overlay_notice(
                        NoticeKind::Info,
                        "Set up an AI provider in Settings to get meeting notes",
                    );
                }
                self.state.pending_meeting = Some(pending);
                self.finish_pending_meeting(decision == NotesDecision::Create);
                return;
            }
            NotesDecision::Ask => {}
        }

        self.state.pending_meeting = Some(pending);
        self.state.tray.set_status(
            tray::IconColor::Transcribing,
            "OpenWritr — meeting recorded; waiting for your answer about notes",
        );
        let proxy = self.state.proxy.clone();
        let asked = thread::Builder::new()
            .name("meeting-notes-prompt".into())
            .spawn(move || {
                let choice = meeting::ask_create_notes(recorded);
                let _ = proxy.send_event(UserEvent::MeetingNotesChoice(choice));
            });
        if let Err(error) = asked {
            warn!(error = %error, "could not show the notes question; saving the transcript only");
            self.finish_pending_meeting(false);
        }
    }

    /// Queue the final part of the recording (and with it the stitched
    /// transcript, and notes if wanted) after the "create notes?" answer.
    fn finish_pending_meeting(&mut self, create_notes: bool) {
        let Some(pending) = self.state.pending_meeting.take() else {
            return;
        };
        let started = pending.session.started;
        let markdown = self.state.settings.markdown_transcripts;
        let mut output = meeting::default_output_path(started);
        if markdown {
            output.set_extension("md");
        }
        let notes_output =
            create_notes.then(|| media::unique_path(meeting::default_notes_path(started)));
        let part = SessionPart {
            id: pending.session.id,
            offset_samples: pending.tail.start,
            last: true,
        };
        let job = FileJob {
            output: media::unique_path(output),
            markdown,
            notes: create_notes,
            notes_output,
            window: None,
            background: false,
            partial: Some(pending.session.partial),
            session: Some((part, pending.session.settings)),
            title: format!(
                "Meeting {:04}-{:02}-{:02} {:02}:{:02}",
                started.year, started.month, started.day, started.hour, started.minute
            ),
            details: format!(
                "Recorded with OpenWritr · {}",
                format_duration(pending.recorded.as_secs() as usize)
            ),
        };
        self.dispatch_long_form(pending.tail.samples, job, "meeting");
    }

    /// Report a file/meeting outcome. Windowed jobs only touch the tray; the
    /// popup carries the details, so the overlay pill stays out of it.
    fn report_long_form(&mut self, windowed: bool, warning: bool, message: &str) {
        if windowed {
            let color = if warning {
                tray::IconColor::Error
            } else {
                tray::IconColor::Idle
            };
            self.state
                .tray
                .set_status(color, &format!("OpenWritr — {message}"));
        } else if warning {
            self.show_job_warning(message);
        } else {
            self.show_job_success(message);
        }
    }

    fn deliver_file_transcript(
        &mut self,
        job: FileJob,
        text: &str,
        notes: Option<String>,
        diarization: DiarizationOutcome,
        cleanup: Option<crate::enhance::CleanupReport>,
        repair: Option<RepairPlan>,
        cleanup_warning: Option<FallbackReason>,
    ) {
        let windowed = job.window.is_some();
        if text.trim().is_empty() {
            job.tell_window(WindowMsg::Failed {
                error: "No speech detected in the audio".into(),
            });
            self.report_long_form(windowed, true, "No speech detected in the audio");
            return;
        }
        let document = render_transcript(job.markdown, &job.title, &job.details, text);
        let output = job.output.clone();
        let text = document.as_str();
        let written = output
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::write(&output, text));
        if let Err(error) = written {
            warn!(error = %error, file = %output.display(), "failed to write transcript");
            // Never lose a long transcription to a bad path.
            let _ = paste::deliver(text, paste::DeliveryMode::Clipboard);
            job.tell_window(WindowMsg::Failed {
                error: format!(
                    "could not save {}: {error}. The transcript was copied to the clipboard.",
                    output.display()
                ),
            });
            self.report_long_form(
                windowed,
                true,
                &format!(
                    "Could not save transcript ({}); copied to clipboard",
                    short_error(&error.to_string())
                ),
            );
            return;
        }
        info!(file = %output.display(), chars = text.chars().count(), "transcript saved");
        // The finished transcript supersedes the in-progress one. (If saving
        // failed above we returned earlier and the partial file is kept.)
        if let Some(partial) = &job.partial {
            let _ = std::fs::remove_file(partial);
        }
        if let Err(error) = paste::deliver(text, paste::DeliveryMode::Clipboard) {
            warn!(error = %error, "failed to copy transcript to clipboard");
        }

        // Meeting notes go next to the transcript as their own file.
        let mut notes_saved = false;
        if let (Some(notes_text), Some(notes_path)) = (notes.as_deref(), job.notes_output.as_ref())
        {
            let transcript_name = output
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            let document = render_notes(&job.title, &job.details, &transcript_name, notes_text);
            let saved = write_file(notes_path, &document);
            match saved {
                Ok(()) => notes_saved = true,
                Err(error) => {
                    warn!(error = %error, file = %notes_path.display(), "failed to write notes");
                }
            }
        }

        let missing = if job.notes && !notes_saved {
            "AI notes"
        } else {
            "AI cleanup"
        };
        // What is still missing is retried in the background; say so.
        let retrying = repair.is_some();
        let summary = cleanup.as_ref().and_then(cleanup_summary);
        if let Some(plan) = repair {
            self.register_repair(&job, plan, text.to_string());
        }
        let (words, chars, preview) = transcript_window::summarize(text);
        // Warnings for the popup: AI problems and unavailable speaker labels.
        let mut warnings = Vec::new();
        if cleanup_warning.is_some() {
            warnings.push(format!(
                "Saved without {missing}: provider or credential problem."
            ));
        } else if let Some(summary) = &summary {
            warnings.push(format!("AI cleanup incomplete: {summary}."));
        }
        if retrying {
            warnings.push("OpenWritr will retry automatically and update the file.".into());
        }
        if let DiarizationOutcome::Failed(reason) = &diarization {
            warnings.push(format!("Speaker labels unavailable: {reason}"));
        }
        if let Some(note) = self.unlabelled_note(&diarization, text) {
            warnings.push(note);
        }
        let speakers = match &diarization {
            DiarizationOutcome::Labelled { speakers } => Some(*speakers),
            _ => None,
        };
        let shown = job.tell_window(WindowMsg::Done {
            output_path: output.display().to_string(),
            words,
            chars,
            preview,
            warning: (!warnings.is_empty()).then(|| warnings.join(" ")),
            speakers,
        });
        // The popup has "Show in folder"; only pop Explorer open ourselves
        // when nothing is showing the result (meetings, or popup closed).
        if !shown {
            if let Err(error) = diagnostics::reveal(&output) {
                warn!(error = %error, "failed to reveal transcript");
            }
        }
        let name = output
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "transcript".into());
        let retry_note = if retrying {
            "; retrying automatically"
        } else {
            ""
        };
        if let Some(reason) = cleanup_warning {
            warn!(?reason, "transcript saved without AI output");
            self.report_long_form(
                windowed,
                true,
                &format!(
                    "Saved {name} without {missing} (provider or credential problem){retry_note}"
                ),
            );
        } else if let Some(summary) = &summary {
            self.report_long_form(
                windowed,
                true,
                &format!("Saved {name} · {summary}{retry_note}"),
            );
        } else if let DiarizationOutcome::Failed(reason) = &diarization {
            self.report_long_form(
                windowed,
                true,
                &format!(
                    "Saved {name} without speaker labels ({})",
                    short_error(reason)
                ),
            );
        } else {
            let mut message = if notes_saved {
                format!("Saved {name} and meeting notes")
            } else {
                format!("Saved {name} and copied to clipboard")
            };
            if let DiarizationOutcome::Labelled { speakers } = &diarization {
                message.push_str(&format!(" · {speakers} speakers"));
            } else if let Some(note) = self.unlabelled_note(&diarization, text) {
                message.push_str(&format!(" · {}", note.trim_end_matches('.')));
            }
            self.report_long_form(windowed, false, &message);
        }
    }

    /// Why a finished file or meeting transcript has no speaker labels, when
    /// that is not an error (switched off, or only one voice heard).
    fn unlabelled_note(&self, diarization: &DiarizationOutcome, text: &str) -> Option<String> {
        match diarization {
            // A retried cleanup reports `NotRequested` for text that already
            // carries labels from the first pass.
            DiarizationOutcome::NotRequested
                if !self.state.settings.diarize_speakers && !text.contains("Speaker 1:") =>
            {
                Some("Speaker labels are off. Turn them on in Settings → Speaker labels.".into())
            }
            DiarizationOutcome::SingleSpeaker => {
                Some("Only one speaker was detected, so no labels were added.".into())
            }
            _ => None,
        }
    }

    /// Remember a delivered transcript whose AI steps can still be retried.
    fn register_repair(&mut self, job: &FileJob, plan: RepairPlan, written: String) {
        let id = self.state.next_repair_id;
        self.state.next_repair_id += 1;
        let name = job
            .output
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "transcript".into());
        info!(id, file = %job.output.display(), "AI cleanup will be retried in the background");
        self.state.repairs.push(Repair {
            id,
            name,
            best_cleaned: vec![0; plan.pieces.len()],
            plan,
            output: job.output.clone(),
            notes_output: job.notes_output.clone(),
            markdown: job.markdown,
            title: job.title.clone(),
            details: job.details.clone(),
            last_written: written,
            failed_rounds: 0,
            next_attempt: Instant::now() + Duration::from_secs(REPAIR_BACKOFF_SECONDS[0]),
            in_flight: None,
        });
    }

    /// Start the next retry of every delivered transcript that is due.
    fn run_due_repairs(&mut self) {
        if self.state.shutting_down || self.state.repairs.is_empty() {
            return;
        }
        let now = Instant::now();
        for index in 0..self.state.repairs.len() {
            let repair = &self.state.repairs[index];
            if repair.in_flight.is_some() || now < repair.next_attempt {
                continue;
            }
            let Some(step) = next_repair_step(&repair.plan) else {
                continue;
            };
            let kind = match step {
                RepairStep::Piece(piece) => Rerun::Clean {
                    raw: repair.plan.pieces[piece].raw.clone().unwrap_or_default(),
                },
                RepairStep::Notes => Rerun::Notes {
                    text: repair.plan.assemble(),
                },
            };
            let repair_id = repair.id;
            let config = JobConfig::rerun(self.state.settings.clone(), kind);
            match self
                .state
                .worker
                .enqueue(Vec::new(), media::TARGET_RATE, config)
            {
                Ok(job_id) => {
                    self.state.repairs[index].in_flight = Some(job_id);
                    self.state.repair_jobs.insert(job_id, (repair_id, step));
                }
                Err(error) => {
                    warn!(error = %error, "could not queue a cleanup retry");
                    self.state.repairs[index].next_attempt = now + Duration::from_secs(60);
                }
            }
        }
    }

    /// A background retry finished.
    fn finish_repair_step(
        &mut self,
        job_id: u64,
        text: String,
        notes: Option<String>,
        cleanup: Option<crate::enhance::CleanupReport>,
    ) {
        let Some((repair_id, step)) = self.state.repair_jobs.remove(&job_id) else {
            return;
        };
        let Some(index) = self
            .state
            .repairs
            .iter()
            .position(|repair| repair.id == repair_id)
        else {
            return;
        };
        let mut transcript_changed = false;
        let progressed;
        {
            let repair = &mut self.state.repairs[index];
            repair.in_flight = None;
            match step {
                RepairStep::Piece(piece) => {
                    let report = cleanup.unwrap_or_default();
                    let improved = report.cleaned > repair.best_cleaned[piece];
                    let complete = !report.needs_retry();
                    if improved || complete {
                        repair.plan.pieces[piece].text = text;
                        transcript_changed = true;
                    }
                    if improved {
                        repair.best_cleaned[piece] = report.cleaned;
                    }
                    if complete {
                        repair.plan.pieces[piece].raw = None;
                    }
                    progressed = improved || complete;
                }
                RepairStep::Notes => match (notes, repair.notes_output.clone()) {
                    (Some(notes), Some(path)) => {
                        let transcript_name = repair
                            .output
                            .file_name()
                            .map(|name| name.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        let document =
                            render_notes(&repair.title, &repair.details, &transcript_name, &notes);
                        match write_file(&path, &document) {
                            Ok(()) => {
                                repair.plan.notes_failed = false;
                                progressed = true;
                            }
                            Err(error) => {
                                warn!(error = %error, file = %path.display(), "could not write the retried notes");
                                progressed = false;
                            }
                        }
                    }
                    _ => progressed = false,
                },
            }
        }

        if transcript_changed {
            if let Err(reason) = self.rewrite_repaired_transcript(index) {
                // Most likely the file was edited or moved: leave it alone.
                let repair = self.state.repairs.remove(index);
                warn!(file = %repair.output.display(), reason, "stopping cleanup retries");
                self.show_job_warning(&format!(
                    "Left {} as it is ({reason}); automatic cleanup stopped",
                    repair.name
                ));
                return;
            }
        }

        let repair = &mut self.state.repairs[index];
        if repair.plan.is_complete() {
            let repair = self.state.repairs.remove(index);
            info!(file = %repair.output.display(), "transcript cleanup finished in the background");
            self.show_job_success(&format!("Finished cleaning up {}", repair.name));
        } else if progressed {
            repair.next_attempt = Instant::now() + REPAIR_STEP_PAUSE;
        } else {
            self.schedule_repair_retry(index);
        }
    }

    /// A background retry did not run to completion.
    fn abort_repair_step(&mut self, job_id: u64) {
        let Some((repair_id, _)) = self.state.repair_jobs.remove(&job_id) else {
            return;
        };
        if let Some(index) = self
            .state
            .repairs
            .iter()
            .position(|repair| repair.id == repair_id)
        {
            self.state.repairs[index].in_flight = None;
            self.schedule_repair_retry(index);
        }
    }

    /// Count a fruitless round and wait longer, or give up.
    fn schedule_repair_retry(&mut self, index: usize) {
        let repair = &mut self.state.repairs[index];
        repair.failed_rounds += 1;
        match repair_delay(repair.failed_rounds) {
            Some(delay) => repair.next_attempt = Instant::now() + delay,
            None => {
                let repair = self.state.repairs.remove(index);
                warn!(file = %repair.output.display(), "giving up on background cleanup");
                self.show_job_warning(&format!(
                    "Could not finish cleaning up {}; it stays as recognised",
                    repair.name
                ));
            }
        }
    }

    /// Rewrite the saved transcript from the repaired pieces, unless the file
    /// was changed by someone else since we wrote it.
    fn rewrite_repaired_transcript(&mut self, index: usize) -> Result<(), String> {
        let repair = &mut self.state.repairs[index];
        let current = std::fs::read_to_string(&repair.output)
            .map_err(|_| "the file was moved or deleted".to_string())?;
        if current != repair.last_written {
            return Err("you have edited it".into());
        }
        let document = render_transcript(
            repair.markdown,
            &repair.title,
            &repair.details,
            &repair.plan.assemble(),
        );
        write_file(&repair.output, &document).map_err(|error| error.to_string())?;
        repair.last_written = document;
        Ok(())
    }

    fn handle_worker_event(&mut self, el: &ActiveEventLoop, event: WorkerEvent) {
        match event {
            // Background retries of AI cleanup / notes never touch the
            // overlay, the job counters or the tray.
            WorkerEvent::JobStarted { id }
            | WorkerEvent::EnhancementStarted { id }
            | WorkerEvent::JobProgress { id, .. }
                if self.state.repair_jobs.contains_key(&id) => {}
            WorkerEvent::JobCompleted {
                id,
                text,
                cleanup,
                notes,
                ..
            } if self.state.repair_jobs.contains_key(&id) => {
                self.finish_repair_step(id, text, notes, cleanup);
            }
            WorkerEvent::JobFailed { id, .. } | WorkerEvent::JobDiscarded { id }
                if self.state.repair_jobs.contains_key(&id) =>
            {
                self.abort_repair_step(id);
            }
            WorkerEvent::ModelState {
                generation,
                engine,
                state,
            } => {
                if generation != self.state.load_generation || engine != self.state.settings.engine
                {
                    return;
                }
                let color = match &state {
                    ModelState::Failed { .. } | ModelState::Cancelled => tray::IconColor::Error,
                    ModelState::Ready => tray::IconColor::Transcribing,
                    ModelState::Missing
                    | ModelState::Downloading { .. }
                    | ModelState::Verifying => tray::IconColor::Transcribing,
                };
                let status = state.status_text(&engine);
                self.state.model_state = Some(state);
                self.state
                    .tray
                    .set_status(color, &format!("OpenWritr — {status}"));
            }
            WorkerEvent::EngineLoading { generation, engine } => {
                if generation != self.state.load_generation || engine != self.state.settings.engine
                {
                    return;
                }
                self.state.engine_state = EngineState::Loading;
                self.state.tray.set_status(
                    tray::IconColor::Transcribing,
                    &format!("OpenWritr — loading {engine}"),
                );
            }
            WorkerEvent::EngineReady {
                generation,
                engine,
                label,
            } => {
                if generation != self.state.load_generation || engine != self.state.settings.engine
                {
                    return;
                }
                info!(engine, label, "selected engine ready");
                self.state.engine_state = EngineState::Ready {
                    label: label.clone(),
                };
                self.state.model_state = Some(ModelState::Ready);
                if self.state.pending_jobs == 0 {
                    self.update_job_status();
                }
            }
            WorkerEvent::EngineFailed {
                generation,
                engine,
                error,
            } => {
                if generation != self.state.load_generation || engine != self.state.settings.engine
                {
                    return;
                }
                warn!(engine, error = %error, "selected engine failed to load");
                let short_error = short_error(&error);
                self.state.engine_state = EngineState::Failed { error };
                self.state.tray.set_status(
                    tray::IconColor::Error,
                    &format!("OpenWritr — {engine} failed: {short_error}; choose Retry"),
                );
            }
            WorkerEvent::JobStarted { id } if self.is_windowed_job(id) => {
                self.state.active_job = Some(id);
                self.tell_job_window(
                    id,
                    WindowMsg::Stage {
                        stage: transcript_window::Stage::Transcribing,
                        done: 0,
                        total: 0,
                    },
                );
                self.update_job_status();
            }
            WorkerEvent::EnhancementStarted { id } if self.is_windowed_job(id) => {
                self.tell_job_window(
                    id,
                    WindowMsg::Stage {
                        stage: transcript_window::Stage::CleaningUp,
                        done: 0,
                        total: 0,
                    },
                );
                self.state.tray.set_status(
                    tray::IconColor::Transcribing,
                    "OpenWritr — cleaning up transcript",
                );
            }
            WorkerEvent::JobProgress {
                id,
                stage,
                done,
                total,
            } => {
                let stage = match stage {
                    JobStage::Diarizing => transcript_window::Stage::Diarizing,
                    JobStage::Transcribing => transcript_window::Stage::Transcribing,
                    JobStage::CleaningUp => transcript_window::Stage::CleaningUp,
                };
                self.tell_job_window(id, WindowMsg::Stage { stage, done, total });
            }
            WorkerEvent::JobStarted { id } => {
                if !self.state.overlay_state.job_started(id) {
                    warn!(id, "ignoring start event for unknown or inactive job");
                    return;
                }
                self.state.active_job = Some(id);
                self.update_job_status();
                self.sync_overlay();
            }
            WorkerEvent::EnhancementStarted { id } => {
                if !self.state.overlay_state.enhancement_started(id) {
                    warn!(id, "ignoring enhancement phase for inactive job");
                    return;
                }
                self.state.tray.set_status(
                    tray::IconColor::Transcribing,
                    &format!(
                        "OpenWritr — enhancing; {} job(s) remaining",
                        self.state.pending_jobs
                    ),
                );
                self.sync_overlay();
            }
            WorkerEvent::JobCompleted {
                id,
                text,
                diarization,
                cleanup,
                repair,
                notes,
                auto_paste,
                enhancement_warning,
                delivery_target,
            } => {
                let file_output = self.state.file_jobs.remove(&id);
                let windowed = file_output.as_ref().is_some_and(FileJob::overlay_free);
                let finished = if windowed {
                    self.finish_windowed_job(id)
                } else {
                    self.finish_job(id)
                };
                if !finished {
                    warn!(id, "ignoring completion for tombstoned or unknown job");
                    return;
                }
                self.update_job_status();
                self.sync_overlay();
                if self.state.discarding_jobs {
                    warn!(id, "suppressing completed job during discard shutdown");
                    return;
                }
                if let Some(job) = file_output {
                    if job.background {
                        // A non-final meeting part. The worker keeps its text
                        // for the final stitch; here it only extends the
                        // in-progress transcript on disk.
                        if let Err(error) = meeting::append_partial(&job.output, &text) {
                            warn!(error = %error, "could not extend the in-progress transcript");
                        }
                        return;
                    }
                    self.deliver_file_transcript(
                        job,
                        &text,
                        notes,
                        diarization,
                        cleanup,
                        repair,
                        enhancement_warning,
                    );
                    return;
                }
                if text.is_empty() {
                    return;
                }
                let target_matches = delivery_target
                    .map(delivery_target_is_current)
                    .unwrap_or(false);
                let recording_active = recording_or_press_pending(
                    self.state.active_recording.is_some(),
                    self.state.delivery_interlock.as_ref(),
                );
                let plan = decide_delivery(
                    auto_paste,
                    recording_active,
                    delivery_target.is_some(),
                    target_matches,
                    false,
                );
                let (mode, safety_warning) = match plan {
                    DeliveryPlan::Paste => (paste::DeliveryMode::Paste, None),
                    DeliveryPlan::ClipboardExplicit => (paste::DeliveryMode::Clipboard, None),
                    DeliveryPlan::ClipboardSafety(reason) => {
                        (paste::DeliveryMode::Clipboard, Some(reason))
                    }
                };
                let delivery = match mode {
                    paste::DeliveryMode::Paste => paste::deliver_guarded(
                        &text,
                        mode,
                        Some(self.state.delivery_interlock.as_ref()),
                        || {
                            delivery_target
                                .map(delivery_target_is_current)
                                .unwrap_or(false)
                        },
                    ),
                    paste::DeliveryMode::Clipboard => paste::deliver(&text, mode),
                };
                match delivery {
                    Ok(paste::DeliveryOutcome::Pasted) => {
                        if let Some(reason) = enhancement_warning {
                            self.show_cleanup_fallback(reason, "pasted");
                        } else {
                            self.show_job_success("Transcript pasted");
                        }
                    }
                    Ok(paste::DeliveryOutcome::Copied) => {
                        let guarded_paste_rejected =
                            auto_paste && matches!(plan, DeliveryPlan::Paste);
                        if let Some(reason) = safety_warning.or_else(|| {
                            guarded_paste_rejected.then(|| {
                                if recording_or_press_pending(
                                    self.state.active_recording.is_some(),
                                    self.state.delivery_interlock.as_ref(),
                                ) {
                                    DeliveryBlockReason::RecordingActive
                                } else {
                                    DeliveryBlockReason::TargetChanged
                                }
                            })
                        }) {
                            self.show_delivery_fallback(reason);
                        } else if let Some(reason) = enhancement_warning {
                            self.show_cleanup_fallback(reason, "copied");
                        } else {
                            self.state.tray.set_status(
                                tray::IconColor::Idle,
                                "OpenWritr — copied transcript to clipboard",
                            );
                            self.show_overlay_notice(
                                NoticeKind::Success,
                                "Copied transcript to clipboard",
                            );
                        }
                    }
                    Ok(paste::DeliveryOutcome::CopiedWithWarning { warning, detail }) => {
                        warn!(?warning, detail, "automatic paste fell back to clipboard");
                        let summary = match warning {
                            paste::DeliveryWarning::ClipboardPreparationFailed => {
                                "Clipboard preservation was unavailable"
                            }
                            paste::DeliveryWarning::KeyInjectionFailed => {
                                "Keyboard paste was unavailable"
                            }
                        };
                        self.show_job_warning_kind(
                            NoticeKind::DeliveryWarning,
                            &format!(
                                "{summary}; transcript copied to clipboard: {}",
                                short_error(&detail)
                            ),
                        );
                    }
                    Ok(paste::DeliveryOutcome::CancelledClipboardChanged) => {
                        self.show_delivery_fallback(DeliveryBlockReason::ClipboardChanged);
                    }
                    Err(error) => self.show_job_warning(&format!(
                        "Transcript delivery failed: {}",
                        short_error(&error.to_string())
                    )),
                }
            }
            WorkerEvent::JobFailed { id, error } => {
                let file_job = self.state.file_jobs.remove(&id);
                let windowed = file_job.as_ref().is_some_and(FileJob::overlay_free);
                let finished = if windowed {
                    self.finish_windowed_job(id)
                } else {
                    self.finish_job(id)
                };
                if !finished {
                    warn!(id, "ignoring failure for tombstoned or unknown job");
                    return;
                }
                warn!(id, error = %error, "transcription job failed");
                if self.state.pending_jobs == 0 {
                    self.state.tray.set_status(
                        tray::IconColor::Error,
                        &format!("OpenWritr — transcription failed: {}", short_error(&error)),
                    );
                } else {
                    self.update_job_status();
                }
                if let Some(job) = file_job.filter(FileJob::overlay_free) {
                    job.tell_window(WindowMsg::Failed {
                        error: error.clone(),
                    });
                } else {
                    self.show_overlay_notice(
                        NoticeKind::Error,
                        format!("Transcription failed: {}", short_error(&error)),
                    );
                }
            }
            WorkerEvent::JobDiscarded { id } => {
                let file_job = self.state.file_jobs.remove(&id);
                let windowed = file_job.as_ref().is_some_and(FileJob::overlay_free);
                let finished = if windowed {
                    self.finish_windowed_job(id)
                } else {
                    self.finish_job(id)
                };
                if !finished {
                    return;
                }
                if let Some(job) = file_job {
                    job.tell_window(WindowMsg::Failed {
                        error: "Cancelled because OpenWritr is quitting".into(),
                    });
                }
                info!(id, "transcription job discarded");
                self.update_job_status();
                self.sync_overlay();
            }
            WorkerEvent::ShutdownComplete => {
                info!("worker shutdown complete");
                self.exit_after_clipboard_settles(el);
            }
        }
    }

    /// Whether `id` is a file job whose progress lives in a popup window
    /// rather than the overlay pill.
    fn is_windowed_job(&self, id: u64) -> bool {
        self.state
            .file_jobs
            .get(&id)
            .is_some_and(FileJob::overlay_free)
    }

    fn tell_job_window(&self, id: u64, message: WindowMsg) {
        if let Some(job) = self.state.file_jobs.get(&id) {
            job.tell_window(message);
        }
    }

    /// `finish_job` for jobs the overlay reducer never saw.
    fn finish_windowed_job(&mut self, id: u64) -> bool {
        self.state.pending_jobs = self.state.pending_jobs.saturating_sub(1);
        if self.state.active_job == Some(id) {
            self.state.active_job = None;
        }
        true
    }

    fn finish_job(&mut self, id: u64) -> bool {
        if !self.state.overlay_state.job_finished(id) {
            return false;
        }
        self.state.pending_jobs = self.state.pending_jobs.saturating_sub(1);
        if self.state.active_job == Some(id) {
            self.state.active_job = None;
        }
        true
    }

    fn update_job_status(&self) {
        // While a meeting is being recorded the tray shows *that*, however
        // many earlier parts are being transcribed in the background.
        if let Some(recorder) = &self.state.meeting {
            let (color, state) = if recorder.is_paused() {
                (
                    tray::IconColor::Transcribing,
                    "meeting paused, not recording",
                )
            } else {
                (tray::IconColor::Recording, "recording meeting")
            };
            let queued = if self.state.pending_jobs > 0 {
                format!(
                    " · transcribing earlier parts ({} queued)",
                    self.state.pending_jobs
                )
            } else {
                String::new()
            };
            self.state.tray.set_status(
                color,
                &format!("OpenWritr — {state} ({}){queued}", recorder.sources()),
            );
            return;
        }
        if self.state.pending_jobs > 0 {
            let noun = if self.state.pending_jobs == 1 {
                "job"
            } else {
                "jobs"
            };
            self.state.tray.set_status(
                tray::IconColor::Transcribing,
                &format!(
                    "OpenWritr — transcribing; {} {noun} remaining",
                    self.state.pending_jobs
                ),
            );
        } else if !self.state.shutting_down {
            if let EngineState::Failed { error } = &self.state.engine_state {
                self.state.tray.set_status(
                    tray::IconColor::Error,
                    &format!("OpenWritr — engine failed: {}", short_error(error)),
                );
            } else if let Some(issue) = self.configuration_issue() {
                self.state.tray.set_status(
                    tray::IconColor::Error,
                    &format!("OpenWritr — {}", short_error(issue)),
                );
            } else {
                match &self.state.engine_state {
                    EngineState::Ready { label } => self.state.tray.set_status(
                        tray::IconColor::Idle,
                        &format!("OpenWritr — ready: {label}"),
                    ),
                    EngineState::Loading => self
                        .state
                        .tray
                        .set_status(tray::IconColor::Transcribing, "OpenWritr — loading engine"),
                    EngineState::NotStarted => self
                        .state
                        .tray
                        .set_status(tray::IconColor::Error, "OpenWritr — engine not ready"),
                    EngineState::Failed { .. } => unreachable!(),
                }
            }
        }
    }

    fn show_job_success(&mut self, message: &str) {
        self.state
            .tray
            .set_status(tray::IconColor::Idle, &format!("OpenWritr — {message}"));
        self.show_overlay_notice(NoticeKind::Success, message);
    }

    fn show_job_warning(&mut self, message: &str) {
        self.show_job_warning_kind(NoticeKind::Warning, message);
    }

    fn show_job_warning_kind(&mut self, kind: NoticeKind, message: &str) {
        self.state
            .tray
            .set_status(tray::IconColor::Error, &format!("OpenWritr — {message}"));
        self.show_overlay_notice(kind, message);
    }

    fn show_cleanup_fallback(&mut self, reason: FallbackReason, action: &str) {
        let kind = cleanup_notice_kind(&reason);
        let message = match reason {
            FallbackReason::UnknownProvider => {
                format!("Provider is invalid; raw transcript was {action}")
            }
            FallbackReason::MissingCredential => {
                format!("Credential unavailable; raw transcript was {action}")
            }
            FallbackReason::CredentialTargetChanged => {
                format!("Provider changed after recording; raw transcript was {action}")
            }
            FallbackReason::InvalidEndpoint => {
                format!("Provider endpoint is invalid; raw transcript was {action}")
            }
            FallbackReason::EmptyModelId => {
                format!("Provider model is invalid; raw transcript was {action}")
            }
            FallbackReason::RequestFailed => {
                format!("Provider request failed; raw transcript was {action}")
            }
            FallbackReason::ProviderRejected(status) => format!(
                "Provider rejected the request (HTTP {status}, see the log); raw transcript was {action}"
            ),
            FallbackReason::ResponseUnparseable => {
                format!("Provider response was invalid; raw transcript was {action}")
            }
            FallbackReason::EmptyCandidate => {
                format!("Cleanup returned no text; raw transcript was {action}")
            }
            FallbackReason::IntegrityRejected(_) => {
                format!("Cleanup changed critical content; raw transcript was {action}")
            }
            FallbackReason::ValidatorError => {
                format!("Cleanup validation failed; raw transcript was {action}")
            }
        };
        self.show_job_warning_kind(kind, &message);
    }

    fn show_delivery_fallback(&mut self, reason: DeliveryBlockReason) {
        let message = match reason {
            DeliveryBlockReason::RecordingActive => {
                "Another recording is active; transcript copied to clipboard"
            }
            DeliveryBlockReason::TargetChanged => {
                "Target window changed; transcript copied to clipboard"
            }
            DeliveryBlockReason::Cancelled => {
                "Delivery was cancelled; transcript copied to clipboard"
            }
            DeliveryBlockReason::ClipboardChanged => {
                "Delivery cancelled because the clipboard changed; newer clipboard contents were preserved"
            }
        };
        self.show_job_warning_kind(NoticeKind::DeliveryWarning, message);
    }

    fn configuration_issue(&self) -> Option<&str> {
        self.state
            .settings_error
            .as_deref()
            .or(self.state.credential_health.message.as_deref())
    }

    fn blocked_recording_status(&self) -> (tray::IconColor, String) {
        if let Some(state) = &self.state.model_state {
            let color = match state {
                ModelState::Failed { .. } | ModelState::Cancelled => tray::IconColor::Error,
                _ => tray::IconColor::Transcribing,
            };
            return (
                color,
                format!(
                    "OpenWritr — recording blocked: {}",
                    state.status_text(&self.state.settings.engine)
                ),
            );
        }
        match &self.state.engine_state {
            EngineState::Loading => (
                tray::IconColor::Transcribing,
                "OpenWritr — recording blocked: engine is loading".into(),
            ),
            EngineState::Failed { error } => (
                tray::IconColor::Error,
                format!(
                    "OpenWritr — recording blocked: engine failed: {}",
                    short_error(error)
                ),
            ),
            EngineState::NotStarted => (
                tray::IconColor::Error,
                "OpenWritr — recording blocked: engine is not ready".into(),
            ),
            EngineState::Ready { .. } => (tray::IconColor::Idle, "OpenWritr — engine ready".into()),
        }
    }

    fn request_shutdown(&mut self, el: &ActiveEventLoop) {
        if self.state.shutting_down {
            return;
        }
        // Queue the meeting recorded so far so quitting never loses it; the
        // existing "finish queued jobs?" prompt then covers it.
        self.stop_meeting(false);
        if self.state.pending_meeting.is_some() {
            // Quit while the notes question was open: keep the transcript.
            self.finish_pending_meeting(false);
        }
        info!(pending_jobs = self.state.pending_jobs, "quit requested");
        self.state.shutting_down = true;
        self.state.hk_stop.store(true, Ordering::Relaxed);
        if self.state.active_recording.take().is_some() {
            let _ = self.state.recorder.stop();
            self.state.overlay_state.recording_finished();
            info!("active recording discarded during shutdown");
        }

        let mode = if self.state.pending_jobs > 0 {
            prompt_shutdown_mode(self.state.pending_jobs)
        } else {
            ShutdownMode::Discard
        };
        self.state.discarding_jobs = mode == ShutdownMode::Discard;
        if mode == ShutdownMode::Discard {
            paste::cancel_pending_restorations();
            // A cleanup request can wait minutes on a slow connection; quitting
            // must not wait for it.
            crate::enhance::cancel_requests();
        }
        let status = match mode {
            ShutdownMode::Wait => format!(
                "OpenWritr — finishing {} queued job(s) before exit",
                self.state.pending_jobs
            ),
            ShutdownMode::Discard => "OpenWritr — discarding queued work and exiting".into(),
        };
        self.state
            .tray
            .set_status(tray::IconColor::Transcribing, &status);
        if let Err(error) = self.state.worker.shutdown(mode) {
            warn!(error = %error, "worker shutdown command failed");
            self.exit_after_clipboard_settles(el);
        }
    }

    fn exit_after_clipboard_settles(&self, el: &ActiveEventLoop) {
        if !paste::wait_for_pending_restorations(Duration::from_secs(2)) {
            warn!("clipboard restoration did not settle before the shutdown timeout");
        }
        crate::local_llm::shutdown();
        el.exit();
    }
}

/// Runs on the file-import thread after a file was picked: opens the popup,
/// decodes, and returns the event that hands everything to the tray loop.
fn import_file(
    path: PathBuf,
    markdown: bool,
    cleanup: bool,
    cleanup_engine: String,
    engine: String,
) -> UserEvent {
    let (window, messages) = mpsc::channel();
    if let Err(error) = transcript_window::spawn_window_thread(messages) {
        // The job still runs; the result is saved and revealed as usual.
        warn!(error = %error, "could not open the transcript window");
    }
    let output = media::unique_path(path.with_extension(if markdown { "md" } else { "txt" }));
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "audio file".into());
    let init = |duration: String| WindowMsg::Init {
        title: name.clone(),
        source_path: path.display().to_string(),
        duration,
        engine: engine.clone(),
        cleanup,
        cleanup_engine: cleanup_engine.clone(),
        markdown,
        output_path: output.display().to_string(),
    };
    let _ = window.send(init("reading…".into()));
    let _ = window.send(WindowMsg::Stage {
        stage: transcript_window::Stage::Decoding,
        done: 0,
        total: 0,
    });
    match media::decode_file(&path) {
        Ok(samples) => {
            let seconds = samples.len() / media::TARGET_RATE as usize;
            info!(seconds, "file decoded");
            let _ = window.send(init(format_duration(seconds)));
            UserEvent::FileDecoded {
                path,
                samples,
                output,
                markdown,
                window,
            }
        }
        Err(error) => {
            let message = format!("{error:#}");
            let _ = window.send(WindowMsg::Failed {
                error: message.clone(),
            });
            UserEvent::FileImportEnded(Some(message))
        }
    }
}

fn format_duration(total_seconds: usize) -> String {
    let (hours, minutes, seconds) = (
        total_seconds / 3600,
        total_seconds % 3600 / 60,
        total_seconds % 60,
    );
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

fn short_error(error: &str) -> String {
    const MAX_CHARS: usize = 140;
    let mut shortened = error.chars().take(MAX_CHARS).collect::<String>();
    if error.chars().count() > MAX_CHARS {
        shortened.push('…');
    }
    shortened
}

fn prompt_shutdown_mode(pending_jobs: usize) -> ShutdownMode {
    use windows::core::HSTRING;
    use windows::Win32::UI::WindowsAndMessaging::{
        MessageBoxW, IDNO, MB_DEFBUTTON1, MB_ICONQUESTION, MB_SETFOREGROUND, MB_YESNO,
    };

    let message = HSTRING::from(format!(
        "OpenWritr is still processing {pending_jobs} transcription job(s).\n\n\
         Choose Yes to wait for them to finish before exiting.\n\
         Choose No to discard queued results and exit after the current native call reaches a safe boundary."
    ));
    let title = HSTRING::from("OpenWritr");
    let response = unsafe {
        MessageBoxW(
            None,
            &message,
            &title,
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON1 | MB_SETFOREGROUND,
        )
    };
    if response == IDNO {
        ShutdownMode::Discard
    } else {
        ShutdownMode::Wait
    }
}

#[cfg(test)]
mod tests {
    use super::{
        cleanup_notice_kind, consume_press_pending, decide_delivery, mark_press_pending,
        recording_intent, recording_or_press_pending, ActiveRecording, DeliveryBlockReason,
        DeliveryPlan, OverlayReducer,
    };
    use crate::cleanup::{EndpointScope, FallbackReason, PromptSource, PromptTarget};
    use crate::overlay::{ListeningIntent, NoticeKind, OverlayViewState, ProcessingPhase};
    use crate::paste::DeliveryInterlock;
    use crate::settings::{EnhanceMode, Settings};
    use crate::worker::RecordingIntent;
    use std::time::Instant;

    fn file_job(window: bool, background: bool) -> super::FileJob {
        super::FileJob {
            output: std::path::PathBuf::new(),
            markdown: false,
            notes: false,
            notes_output: None,
            title: String::new(),
            details: String::new(),
            window: window.then(|| std::sync::mpsc::channel().0),
            background,
            session: None,
            partial: None,
        }
    }

    #[test]
    fn repairs_back_off_and_eventually_give_up() {
        let seconds: Vec<Option<u64>> = (1..=6)
            .map(|round| super::repair_delay(round).map(|d| d.as_secs()))
            .collect();
        assert_eq!(
            seconds,
            vec![
                Some(120),
                Some(300),
                Some(600),
                Some(1_200),
                Some(1_800),
                None
            ]
        );
        // Round zero (nothing failed yet) behaves like the first.
        assert_eq!(super::repair_delay(0).map(|d| d.as_secs()), Some(120));
    }

    #[test]
    fn repairs_fix_unfinished_pieces_in_order_then_the_notes() {
        use super::{next_repair_step, RepairStep};
        use crate::worker::{RepairPiece, RepairPlan};
        let piece = |raw: Option<&str>| RepairPiece {
            text: "t".into(),
            raw: raw.map(str::to_string),
        };
        let mut plan = RepairPlan {
            pieces: vec![piece(None), piece(Some("b")), piece(Some("c"))],
            separator: " ".into(),
            unlabel: false,
            notes_failed: true,
        };
        assert_eq!(next_repair_step(&plan), Some(RepairStep::Piece(1)));
        plan.pieces[1].raw = None;
        assert_eq!(next_repair_step(&plan), Some(RepairStep::Piece(2)));
        plan.pieces[2].raw = None;
        assert_eq!(next_repair_step(&plan), Some(RepairStep::Notes));
        plan.notes_failed = false;
        assert_eq!(next_repair_step(&plan), None);
    }

    #[test]
    fn documents_are_rendered_the_same_way_every_time() {
        use super::{render_notes, render_transcript};
        assert_eq!(render_transcript(false, "T", "d", "plain"), "plain");
        let markdown = render_transcript(true, "Meeting 1", "details", "body text.");
        assert!(markdown.starts_with("# Meeting 1\n\n*details*\n\n"));
        let notes = render_notes(
            "Meeting 2026-03-09 14:05",
            "Recorded",
            "Meeting.txt",
            "## Summary",
        );
        assert!(notes.starts_with("# Meeting notes 2026-03-09 14:05\n"));
        assert!(notes.contains("transcript: Meeting.txt"));
    }

    #[test]
    fn the_cleanup_summary_only_appears_when_something_is_left() {
        use super::cleanup_summary;
        use crate::enhance::CleanupReport;
        let report = |total, cleaned| CleanupReport {
            total,
            cleaned,
            unreachable: 0,
        };
        assert_eq!(
            cleanup_summary(&report(25, 21)).unwrap(),
            "cleaned 21 of 25 sections"
        );
        assert!(cleanup_summary(&report(25, 25)).is_none());
        assert!(cleanup_summary(&report(0, 0)).is_none());
    }

    #[test]
    fn notes_decision_covers_every_mode() {
        use super::notes_decision;
        use super::NotesDecision::{Ask as AskUser, Create, Skip, SkipNoProvider};
        use crate::settings::MeetingNotesMode::{Always, Ask, Never};
        // Never: no notes, no question, no hint.
        for ready in [true, false] {
            for may_ask in [true, false] {
                assert_eq!(notes_decision(Never, ready, may_ask), Skip);
            }
        }
        // Always: notes whenever a provider is ready, even while quitting.
        assert_eq!(notes_decision(Always, true, true), Create);
        assert_eq!(notes_decision(Always, true, false), Create);
        assert_eq!(notes_decision(Always, false, true), SkipNoProvider);
        // Ask: the question only when it can be shown and notes are possible.
        assert_eq!(notes_decision(Ask, true, true), AskUser);
        assert_eq!(notes_decision(Ask, true, false), Skip);
        assert_eq!(notes_decision(Ask, false, true), SkipNoProvider);
        assert_eq!(notes_decision(Ask, false, false), Skip);
    }

    #[test]
    fn popup_and_background_jobs_stay_out_of_the_dictation_overlay() {
        // Dictation-style meeting jobs (the final part, with the pill) use it.
        assert!(!file_job(false, false).overlay_free());
        // A file job shows in its popup, a background meeting part nowhere.
        assert!(file_job(true, false).overlay_free());
        assert!(file_job(false, true).overlay_free());
    }

    #[test]
    fn recording_intent_covers_the_full_shift_truth_table() {
        use RecordingIntent::{Enhance, Raw};

        let cases = [
            (EnhanceMode::Never, false, false, Raw),
            (EnhanceMode::Never, false, true, Raw),
            (EnhanceMode::Never, true, false, Raw),
            (EnhanceMode::Never, true, true, Raw),
            (EnhanceMode::WithShift, false, false, Raw),
            (EnhanceMode::WithShift, false, true, Enhance),
            (EnhanceMode::WithShift, true, false, Raw),
            (EnhanceMode::WithShift, true, true, Raw),
            (EnhanceMode::Always, false, false, Enhance),
            (EnhanceMode::Always, false, true, Raw),
            (EnhanceMode::Always, true, false, Enhance),
            (EnhanceMode::Always, true, true, Enhance),
        ];

        for (mode, shift_is_modifier, shift_is_down, expected) in cases {
            assert_eq!(
                recording_intent(mode, shift_is_modifier, shift_is_down),
                expected
            );
        }
    }

    #[test]
    fn active_recording_keeps_the_press_time_settings_snapshot() {
        let mut live = Settings::default();
        live.auto_paste = false;
        live.min_record_seconds = 0.5;
        live.max_record_seconds = 12.0;
        live.enhance.mode = EnhanceMode::Always;
        live.enhance.provider = "openai_compatible".into();
        live.enhance.base_url = "https://snapshot.example/v1".into();
        live.enhance.model = "snapshot-model".into();
        let target = PromptTarget::openai_compatible(
            EndpointScope::parse(&live.enhance.base_url).unwrap(),
            &live.enhance.model,
        )
        .unwrap();
        live.prompt_overrides
            .set(target.clone(), "snapshot prompt".into());

        let active = ActiveRecording::new(live.clone(), RecordingIntent::Enhance, None);
        live.auto_paste = true;
        live.min_record_seconds = 2.0;
        live.max_record_seconds = 30.0;
        live.enhance.mode = EnhanceMode::Never;
        live.enhance.provider = "github_copilot".into();
        live.enhance.base_url = "https://changed.example/v1".into();
        live.enhance.model = "changed-model".into();

        assert!(!active.settings.auto_paste);
        assert_eq!(active.settings.min_record_seconds, 0.5);
        assert_eq!(active.settings.max_record_seconds, 12.0);
        assert_eq!(active.settings.enhance.mode, EnhanceMode::Always);
        assert_eq!(active.settings.prompt_target().unwrap(), target);
        let resolved = active.settings.resolve_prompt(&target);
        assert_eq!(resolved.source, PromptSource::CustomOverride);
        assert_eq!(
            resolved.system,
            format!(
                "snapshot prompt\n\n{}",
                crate::cleanup::prompt::TRANSCRIPT_ONLY_POLICY
            )
        );
        assert_eq!(active.intent, RecordingIntent::Enhance);
    }

    #[test]
    fn overlay_reducer_enforces_recording_job_queue_notice_priority() {
        let now = Instant::now();
        let mut reducer = OverlayReducer::default();
        reducer.show_notice(NoticeKind::Warning, "old notice", now);
        reducer.job_queued(10);
        reducer.job_queued(11);
        assert_eq!(
            reducer.desired_view(now),
            OverlayViewState::processing(10, ProcessingPhase::Queued, 1)
        );

        assert!(reducer.job_started(10));
        assert_eq!(
            reducer.desired_view(now),
            OverlayViewState::processing(10, ProcessingPhase::Transcribing, 1)
        );

        reducer.recording_started(RecordingIntent::Enhance);
        assert_eq!(
            reducer.desired_view(now),
            OverlayViewState::listening(ListeningIntent::Enhance)
        );
        reducer.recording_finished();
        assert!(reducer.enhancement_started(10));
        assert_eq!(
            reducer.desired_view(now),
            OverlayViewState::processing(10, ProcessingPhase::Enhancing, 1)
        );

        assert!(reducer.job_finished(10));
        assert_eq!(
            reducer.desired_view(now),
            OverlayViewState::processing(11, ProcessingPhase::Queued, 0)
        );
        assert!(reducer.job_finished(11));
        assert_eq!(
            reducer.desired_view(now),
            OverlayViewState::notice(NoticeKind::Warning, "old notice")
        );
    }

    #[test]
    fn old_enhancement_and_terminal_events_cannot_clobber_newer_activity() {
        let now = Instant::now();
        let mut reducer = OverlayReducer::default();
        reducer.job_queued(1);
        reducer.job_queued(2);
        assert!(reducer.job_started(1));
        assert!(reducer.job_finished(1));
        assert!(reducer.job_started(2));

        assert!(!reducer.enhancement_started(1));
        assert!(!reducer.job_finished(1));
        assert_eq!(
            reducer.desired_view(now),
            OverlayViewState::processing(2, ProcessingPhase::Transcribing, 0)
        );
    }

    #[test]
    fn delivery_safety_falls_back_for_target_change_and_cancellation() {
        assert_eq!(
            decide_delivery(true, false, true, false, false),
            DeliveryPlan::ClipboardSafety(DeliveryBlockReason::TargetChanged)
        );
        assert_eq!(
            decide_delivery(true, false, true, true, true),
            DeliveryPlan::ClipboardSafety(DeliveryBlockReason::Cancelled)
        );
        assert_eq!(
            decide_delivery(true, true, true, true, false),
            DeliveryPlan::ClipboardSafety(DeliveryBlockReason::RecordingActive)
        );
        assert_eq!(
            decide_delivery(false, true, false, false, true),
            DeliveryPlan::ClipboardExplicit
        );
        assert_eq!(
            decide_delivery(true, false, true, true, false),
            DeliveryPlan::Paste
        );
    }

    #[test]
    fn pending_press_blocks_older_completion_until_app_consumes_its_epoch() {
        let pending = DeliveryInterlock::default();
        let mut next_epoch = 0;
        let epoch = mark_press_pending(&pending, &mut next_epoch);

        assert!(recording_or_press_pending(false, &pending));
        assert_eq!(
            decide_delivery(
                true,
                recording_or_press_pending(false, &pending),
                true,
                true,
                false,
            ),
            DeliveryPlan::ClipboardSafety(DeliveryBlockReason::RecordingActive)
        );

        consume_press_pending(&pending, epoch);
        assert!(!recording_or_press_pending(false, &pending));
    }

    #[test]
    fn consuming_an_older_press_epoch_does_not_clear_a_newer_pending_press() {
        let pending = DeliveryInterlock::default();
        let mut next_epoch = 0;
        let first = mark_press_pending(&pending, &mut next_epoch);
        let second = mark_press_pending(&pending, &mut next_epoch);

        consume_press_pending(&pending, first);

        assert!(pending.press_pending());
        consume_press_pending(&pending, second);
        assert!(!pending.press_pending());
    }

    #[test]
    fn cleanup_fallback_notices_distinguish_provider_and_validator_failures() {
        assert_eq!(
            cleanup_notice_kind(&FallbackReason::MissingCredential),
            NoticeKind::ProviderWarning
        );
        assert_eq!(
            cleanup_notice_kind(&FallbackReason::RequestFailed),
            NoticeKind::ProviderWarning
        );
        assert_eq!(
            cleanup_notice_kind(&FallbackReason::ValidatorError),
            NoticeKind::RawFallback
        );
        assert_eq!(
            cleanup_notice_kind(&FallbackReason::IntegrityRejected(vec![])),
            NoticeKind::RawFallback
        );
    }
}
