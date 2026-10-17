use crate::asr::{self, Engine};
use crate::cleanup::{EnhanceOutcome, FallbackReason};
use crate::enhance;
use crate::media;
use crate::model_manager::{CancellationToken, ModelManager, ModelState};
use crate::settings::Settings;
use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread;
use std::time::Instant;
use tracing::{info, warn};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShutdownMode {
    Wait,
    Discard,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecordingIntent {
    Raw,
    Enhance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeliveryTarget {
    pub hwnd: isize,
    pub process_id: u32,
    pub process_creation_time_100ns: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct JobConfig {
    settings: Settings,
    intent: RecordingIntent,
    delivery_target: Option<DeliveryTarget>,
    /// Imported files and meetings: transcribed in silence-aligned windows
    /// instead of one pass, never LLM-enhanced, never pasted.
    long_form: bool,
    /// Label speakers (long-form jobs only; needs the diarization models).
    diarize: bool,
    /// Set for the parts of a rolling meeting transcription.
    session: Option<SessionPart>,
    /// Set for text-only retries of cleanup / notes (no audio involved).
    rerun: Option<Rerun>,
    /// Also ask the provider for structured meeting notes (implies
    /// `long_form`); the notes are placed above the transcript.
    notes: bool,
}

impl JobConfig {
    pub fn new(
        settings: Settings,
        intent: RecordingIntent,
        delivery_target: Option<DeliveryTarget>,
        _enhancement_disabled: bool,
    ) -> Self {
        Self {
            settings,
            intent,
            delivery_target,
            long_form: false,
            diarize: false,
            session: None,
            rerun: None,
            notes: false,
        }
    }

    pub fn long_form(settings: Settings) -> Self {
        Self {
            diarize: settings.diarize_speakers,
            settings,
            intent: RecordingIntent::Raw,
            delivery_target: None,
            long_form: true,
            session: None,
            rerun: None,
            notes: false,
        }
    }

    /// Retry cleanup or notes on finished text. Uses no engine.
    pub fn rerun(settings: Settings, kind: Rerun) -> Self {
        Self {
            rerun: Some(kind),
            ..Self::long_form(settings)
        }
    }

    /// A meeting part: transcribed like a file, never delivered on its own.
    pub fn meeting_part(settings: Settings, part: SessionPart, notes: bool) -> Self {
        Self {
            session: Some(part),
            notes: notes && part.last,
            ..Self::long_form(settings)
        }
    }

    pub fn meeting_notes(settings: Settings) -> Self {
        Self {
            notes: true,
            ..Self::long_form(settings)
        }
    }

    fn should_enhance(&self) -> bool {
        if self.long_form {
            // Imported files and meetings have no Shift key to consult, so any
            // mode other than Never opts them into cleanup.
            return self.settings.enhance.mode.is_enabled();
        }
        self.intent == RecordingIntent::Enhance && self.settings.enhance.mode.is_enabled()
    }
}

/// Which long-running phase a [`WorkerEvent::JobProgress`] belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobStage {
    Diarizing,
    Transcribing,
    CleaningUp,
}

/// One piece of a meeting recording that is being transcribed in parts while
/// recording continues. All parts of a meeting share an `id`; the worker
/// keeps their texts and speaker identities until the `last` part arrives
/// and stitches everything together.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SessionPart {
    pub id: u64,
    /// Where this part starts on the meeting's timeline (16 kHz samples), so
    /// `[m:ss]` markers are meeting-global.
    pub offset_samples: usize,
    pub last: bool,
}

/// One stretch of a delivered transcript. `raw` is the text as recognised,
/// kept only for pieces whose AI cleanup is worth trying again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairPiece {
    pub text: String,
    pub raw: Option<String>,
}

/// What a delivered transcript still lacks, and how to put it together again
/// once it is fixed: the app retries in the background (a connection that was
/// down, say) and rewrites the saved files when cleanup or notes succeed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepairPlan {
    pub pieces: Vec<RepairPiece>,
    /// Put between pieces: a blank line for labelled blocks, a space for
    /// plain run-on text.
    pub separator: String,
    /// Remove speaker labels and markers after joining (one voice only).
    pub unlabel: bool,
    /// Notes were requested but the provider could not be reached.
    pub notes_failed: bool,
}

impl RepairPlan {
    /// The full transcript from the current pieces.
    pub fn assemble(&self) -> String {
        let text = self
            .pieces
            .iter()
            .map(|piece| piece.text.as_str())
            .filter(|text| !text.trim().is_empty())
            .collect::<Vec<_>>()
            .join(&self.separator);
        if self.unlabel {
            unlabel(&text)
        } else {
            text
        }
    }

    /// Nothing left worth retrying.
    pub fn is_complete(&self) -> bool {
        !self.notes_failed && self.pieces.iter().all(|piece| piece.raw.is_none())
    }
}

/// A text-only job that retries the AI steps of an already delivered
/// transcript. Needs no speech engine.
#[derive(Clone, Debug)]
pub enum Rerun {
    /// Clean this text (as recognised) again.
    Clean { raw: String },
    /// Generate meeting notes from this finished transcript.
    Notes { text: String },
}

/// What happened to the optional "label the speakers" step of a job.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DiarizationOutcome {
    /// Not requested (dictation, or the setting is off).
    NotRequested,
    /// The transcript carries `Speaker N:` labels for this many speakers.
    Labelled { speakers: usize },
    /// Only one speaker was found, so nothing was labelled.
    SingleSpeaker,
    /// Diarization failed; the transcript is unlabelled. Content-free reason.
    Failed(String),
}

pub enum WorkerEvent {
    ModelState {
        generation: u64,
        engine: String,
        state: ModelState,
    },
    EngineLoading {
        generation: u64,
        engine: String,
    },
    EngineReady {
        generation: u64,
        engine: String,
        label: String,
    },
    EngineFailed {
        generation: u64,
        engine: String,
        error: String,
    },
    JobStarted {
        id: u64,
    },
    EnhancementStarted {
        id: u64,
    },
    /// `done` of `total` windows (transcription) or slices (cleanup) finished.
    /// Only long-form jobs report progress.
    JobProgress {
        id: u64,
        stage: JobStage,
        done: usize,
        total: usize,
    },
    JobCompleted {
        id: u64,
        text: String,
        diarization: DiarizationOutcome,
        /// How AI cleanup went for a long-form job (`None` otherwise).
        cleanup: Option<enhance::CleanupReport>,
        /// Present when the delivered transcript can still be improved by
        /// retrying later.
        repair: Option<RepairPlan>,
        /// Structured meeting notes, kept apart from the transcript so the
        /// caller can save them as their own file. `Some` only for notes jobs
        /// whose generation succeeded.
        notes: Option<String>,
        auto_paste: bool,
        enhancement_warning: Option<FallbackReason>,
        delivery_target: Option<DeliveryTarget>,
    },
    JobFailed {
        id: u64,
        error: String,
    },
    JobDiscarded {
        id: u64,
    },
    ShutdownComplete,
}

struct Job {
    id: u64,
    generation: u64,
    samples: Vec<f32>,
    sample_rate: u32,
    config: JobConfig,
}

enum Command {
    Load {
        generation: u64,
        engine: String,
        cancellation: CancellationToken,
    },
    Transcribe(Job),
    Shutdown(ShutdownMode),
}

trait EngineLoader: Send + 'static {
    fn load(
        &mut self,
        engine: &str,
        cancellation: &CancellationToken,
        emit_model_state: &mut dyn FnMut(ModelState),
    ) -> Result<Box<dyn Engine>>;
}

struct RuntimeEngineLoader {
    models: ModelManager,
}

impl RuntimeEngineLoader {
    fn new() -> Result<Self> {
        Ok(Self {
            models: ModelManager::new().map_err(|error| anyhow!(error))?,
        })
    }
}

impl EngineLoader for RuntimeEngineLoader {
    fn load(
        &mut self,
        engine: &str,
        cancellation: &CancellationToken,
        emit_model_state: &mut dyn FnMut(ModelState),
    ) -> Result<Box<dyn Engine>> {
        asr::ensure_engine_supported(engine)?;
        let model_dir = self
            .models
            .ensure(engine, cancellation, emit_model_state)
            .map_err(|error| anyhow!(error))?;
        if cancellation.is_cancelled() {
            return Err(anyhow!("engine load cancelled"));
        }
        asr::load_from_dir(engine, model_dir)
    }
}

pub struct Worker {
    command_tx: Sender<Command>,
    event_rx: Receiver<WorkerEvent>,
    latest_generation: Arc<AtomicU64>,
    shutdown_requested: Arc<AtomicBool>,
    discard_requested: Arc<AtomicBool>,
    pending_jobs_by_generation: Arc<Mutex<HashMap<u64, usize>>>,
    job_events: Mutex<JobEventTracker>,
    synthetic_events: Mutex<VecDeque<WorkerEvent>>,
    transition_gate: Arc<Mutex<()>>,
    next_generation: u64,
    next_job: u64,
    load_cancellations: HashMap<u64, CancellationToken>,
}

impl Worker {
    pub fn spawn() -> Result<Self> {
        Self::spawn_with_loader(Box::new(RuntimeEngineLoader::new()?))
    }

    fn spawn_with_loader(loader: Box<dyn EngineLoader>) -> Result<Self> {
        let (command_tx, command_rx) = mpsc::channel();
        let (event_tx, event_rx) = mpsc::channel();
        let latest_generation = Arc::new(AtomicU64::new(0));
        let shutdown_requested = Arc::new(AtomicBool::new(false));
        let discard_requested = Arc::new(AtomicBool::new(false));
        let pending_jobs_by_generation = Arc::new(Mutex::new(HashMap::new()));
        let transition_gate = Arc::new(Mutex::new(()));
        let worker_generation = latest_generation.clone();
        let worker_shutdown = shutdown_requested.clone();
        let worker_discard = discard_requested.clone();
        let worker_pending_jobs = pending_jobs_by_generation.clone();
        let worker_transition_gate = transition_gate.clone();

        thread::Builder::new()
            .name("inference-worker".into())
            .stack_size(32 * 1024 * 1024)
            .spawn(move || {
                worker_main(
                    loader,
                    command_rx,
                    event_tx,
                    worker_generation,
                    worker_shutdown,
                    worker_discard,
                    worker_pending_jobs,
                    worker_transition_gate,
                )
            })
            .context("spawn inference worker")?;

        Ok(Self {
            command_tx,
            event_rx,
            latest_generation,
            shutdown_requested,
            discard_requested,
            pending_jobs_by_generation,
            job_events: Mutex::new(JobEventTracker::default()),
            synthetic_events: Mutex::new(VecDeque::new()),
            transition_gate,
            next_generation: 1,
            next_job: 1,
            load_cancellations: HashMap::new(),
        })
    }

    pub fn load(&mut self, engine: String) -> Result<u64> {
        if self.shutdown_requested.load(Ordering::Acquire) {
            return Err(anyhow!("inference worker is shutting down"));
        }
        let _transition = self.transition_gate.lock();
        let pending_jobs_by_generation = &self.pending_jobs_by_generation;
        self.load_cancellations.retain(|generation, cancellation| {
            let required = generation_has_pending_jobs(pending_jobs_by_generation, *generation);
            if !required {
                cancellation.cancel();
            }
            required
        });
        let generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        self.latest_generation.store(generation, Ordering::Release);
        let cancellation = CancellationToken::default();
        self.load_cancellations
            .insert(generation, cancellation.clone());
        if let Err(error) = self.command_tx.send(Command::Load {
            generation,
            engine,
            cancellation,
        }) {
            if let Some(cancellation) = self.load_cancellations.remove(&generation) {
                cancellation.cancel();
            }
            return Err(error).context("send engine load command");
        }
        Ok(generation)
    }

    pub fn cancel_load(&mut self) -> u64 {
        let _transition = self.transition_gate.lock();
        let current_generation = self.latest_generation.load(Ordering::Acquire);
        if let Some(cancellation) = self.load_cancellations.get(&current_generation) {
            cancellation.cancel();
        }
        let invalid_generation = self.next_generation;
        self.next_generation = self.next_generation.saturating_add(1);
        self.latest_generation
            .store(invalid_generation, Ordering::Release);
        invalid_generation
    }

    pub fn enqueue(
        &mut self,
        samples: Vec<f32>,
        sample_rate: u32,
        config: JobConfig,
    ) -> Result<u64> {
        if self.shutdown_requested.load(Ordering::Acquire) {
            return Err(anyhow!("inference worker is shutting down"));
        }
        let id = self.next_job;
        self.next_job = self.next_job.saturating_add(1);
        let generation = self.latest_generation.load(Ordering::Acquire);
        register_pending_job(&self.pending_jobs_by_generation, generation);
        self.job_events.lock().register(id);
        if let Err(error) = self.command_tx.send(Command::Transcribe(Job {
            id,
            generation,
            samples,
            sample_rate,
            config,
        })) {
            unregister_pending_job(&self.pending_jobs_by_generation, generation);
            self.job_events.lock().forget(id);
            return Err(anyhow!("send transcription job: {error}"));
        }
        Ok(id)
    }

    pub fn shutdown(&self, mode: ShutdownMode) -> Result<()> {
        let _transition = self.transition_gate.lock();
        if mode == ShutdownMode::Discard {
            let discarded = self.job_events.lock().tombstone_all();
            self.synthetic_events.lock().extend(
                discarded
                    .into_iter()
                    .map(|id| WorkerEvent::JobDiscarded { id }),
            );
        }
        self.shutdown_requested.store(true, Ordering::Release);
        if mode == ShutdownMode::Discard {
            self.discard_requested.store(true, Ordering::Release);
        }
        for (generation, cancellation) in &self.load_cancellations {
            if mode == ShutdownMode::Discard
                || !generation_has_pending_jobs(&self.pending_jobs_by_generation, *generation)
            {
                cancellation.cancel();
            }
        }
        self.command_tx
            .send(Command::Shutdown(mode))
            .context("send worker shutdown command")
    }

    pub fn try_recv(&self) -> Option<WorkerEvent> {
        loop {
            if let Some(event) = self.synthetic_events.lock().pop_front() {
                return Some(event);
            }
            match self.event_rx.try_recv() {
                Ok(event) => {
                    if let Some(event) = self.normalize_event(event) {
                        return Some(event);
                    }
                }
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return None,
            }
        }
    }

    #[cfg(test)]
    fn recv_timeout(&self, timeout: std::time::Duration) -> Option<WorkerEvent> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(event) = self.synthetic_events.lock().pop_front() {
                return Some(event);
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let event = self.event_rx.recv_timeout(remaining).ok()?;
            if let Some(event) = self.normalize_event(event) {
                return Some(event);
            }
        }
    }

    fn normalize_event(&self, event: WorkerEvent) -> Option<WorkerEvent> {
        self.job_events
            .lock()
            .normalize(event, self.discard_requested.load(Ordering::Acquire))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JobPhase {
    Queued,
    Transcribing,
    Enhancing,
}

#[derive(Default)]
struct JobEventTracker {
    phases: HashMap<u64, JobPhase>,
    tombstones: HashSet<u64>,
    tombstone_order: VecDeque<u64>,
}

impl JobEventTracker {
    fn register(&mut self, id: u64) {
        self.tombstones.remove(&id);
        self.phases.insert(id, JobPhase::Queued);
    }

    fn forget(&mut self, id: u64) {
        self.phases.remove(&id);
        self.tombstones.remove(&id);
    }

    fn finish(&mut self, id: u64) -> bool {
        if self.phases.remove(&id).is_some() {
            self.add_tombstone(id);
            true
        } else {
            false
        }
    }

    fn add_tombstone(&mut self, id: u64) {
        const TOMBSTONE_LIMIT: usize = 1024;
        if self.tombstones.insert(id) {
            self.tombstone_order.push_back(id);
        }
        while self.tombstone_order.len() > TOMBSTONE_LIMIT {
            if let Some(expired) = self.tombstone_order.pop_front() {
                self.tombstones.remove(&expired);
            }
        }
    }

    fn tombstone_all(&mut self) -> Vec<u64> {
        let mut ids = self.phases.drain().map(|(id, _)| id).collect::<Vec<_>>();
        ids.sort_unstable();
        for id in &ids {
            self.add_tombstone(*id);
        }
        ids
    }

    fn normalize(&mut self, event: WorkerEvent, discard_requested: bool) -> Option<WorkerEvent> {
        if Self::event_job_id(&event).is_some_and(|id| self.tombstones.contains(&id)) {
            return None;
        }
        let event = if discard_requested {
            match event {
                WorkerEvent::JobCompleted { id, .. } | WorkerEvent::JobFailed { id, .. } => {
                    WorkerEvent::JobDiscarded { id }
                }
                WorkerEvent::JobStarted { .. }
                | WorkerEvent::EnhancementStarted { .. }
                | WorkerEvent::JobProgress { .. } => {
                    return None;
                }
                other => other,
            }
        } else {
            event
        };

        match event {
            WorkerEvent::JobStarted { id } => {
                let phase = self.phases.get_mut(&id)?;
                if *phase != JobPhase::Queued {
                    return None;
                }
                *phase = JobPhase::Transcribing;
                Some(WorkerEvent::JobStarted { id })
            }
            WorkerEvent::EnhancementStarted { id } => {
                let phase = self.phases.get_mut(&id)?;
                if *phase != JobPhase::Transcribing {
                    return None;
                }
                *phase = JobPhase::Enhancing;
                Some(WorkerEvent::EnhancementStarted { id })
            }
            event @ WorkerEvent::JobProgress { id, .. } => {
                // Progress is only meaningful while the job is still live.
                matches!(
                    self.phases.get(&id),
                    Some(JobPhase::Transcribing | JobPhase::Enhancing)
                )
                .then_some(event)
            }
            event @ WorkerEvent::JobCompleted { id, .. } => {
                if !matches!(
                    self.phases.get(&id),
                    Some(JobPhase::Transcribing | JobPhase::Enhancing)
                ) {
                    return None;
                }
                self.finish(id).then_some(event)
            }
            event @ WorkerEvent::JobFailed { id, .. }
            | event @ WorkerEvent::JobDiscarded { id } => self.finish(id).then_some(event),
            WorkerEvent::ShutdownComplete => {
                let unfinished = self.phases.drain().map(|(id, _)| id).collect::<Vec<_>>();
                for id in unfinished {
                    self.add_tombstone(id);
                }
                Some(WorkerEvent::ShutdownComplete)
            }
            other => Some(other),
        }
    }

    fn event_job_id(event: &WorkerEvent) -> Option<u64> {
        match event {
            WorkerEvent::JobStarted { id }
            | WorkerEvent::EnhancementStarted { id }
            | WorkerEvent::JobProgress { id, .. }
            | WorkerEvent::JobCompleted { id, .. }
            | WorkerEvent::JobFailed { id, .. }
            | WorkerEvent::JobDiscarded { id } => Some(*id),
            _ => None,
        }
    }
}

fn worker_main(
    mut loader: Box<dyn EngineLoader>,
    command_rx: Receiver<Command>,
    event_tx: Sender<WorkerEvent>,
    latest_generation: Arc<AtomicU64>,
    shutdown_requested: Arc<AtomicBool>,
    discard_requested: Arc<AtomicBool>,
    pending_jobs_by_generation: Arc<Mutex<HashMap<u64, usize>>>,
    transition_gate: Arc<Mutex<()>>,
) {
    let mut engine: Option<Box<dyn Engine>> = None;
    let mut engine_generation: Option<u64> = None;
    let mut engine_cancellation: Option<CancellationToken> = None;
    // Rolling meeting transcriptions in progress. Jobs run one at a time on
    // this thread, so the map needs no locking.
    let mut sessions: HashMap<u64, SessionState> = HashMap::new();
    // Whether the AI provider answered recently (see `CleanupGate`).
    let mut gate = CleanupGate::default();
    while let Ok(command) = command_rx.recv() {
        match command {
            Command::Load {
                generation,
                engine: engine_name,
                cancellation,
            } => {
                {
                    let _transition = transition_gate.lock();
                    if cancellation.is_cancelled()
                        || !should_process_load(
                            generation,
                            latest_generation.load(Ordering::Acquire),
                            shutdown_requested.load(Ordering::Acquire),
                            discard_requested.load(Ordering::Acquire),
                            &pending_jobs_by_generation,
                        )
                    {
                        continue;
                    }
                }
                engine = None;
                engine_generation = None;
                engine_cancellation = None;
                let _ = event_tx.send(WorkerEvent::EngineLoading {
                    generation,
                    engine: engine_name.clone(),
                });
                let mut emit_model_state = |state| {
                    let _ = event_tx.send(WorkerEvent::ModelState {
                        generation,
                        engine: engine_name.clone(),
                        state,
                    });
                };
                let loaded = loader.load(&engine_name, &cancellation, &mut emit_model_state);
                let _transition = transition_gate.lock();
                if cancellation.is_cancelled()
                    || !should_process_load(
                        generation,
                        latest_generation.load(Ordering::Acquire),
                        shutdown_requested.load(Ordering::Acquire),
                        discard_requested.load(Ordering::Acquire),
                        &pending_jobs_by_generation,
                    )
                {
                    drop(loaded);
                    continue;
                }
                match loaded {
                    Ok(loaded_engine) => {
                        let label = loaded_engine.label().to_string();
                        engine = Some(loaded_engine);
                        engine_generation = Some(generation);
                        engine_cancellation = Some(cancellation.clone());
                        let _ = event_tx.send(WorkerEvent::EngineReady {
                            generation,
                            engine: engine_name,
                            label,
                        });
                    }
                    Err(error) => {
                        engine_generation = None;
                        engine_cancellation = None;
                        let _ = event_tx.send(WorkerEvent::EngineFailed {
                            generation,
                            engine: engine_name,
                            error: error.to_string(),
                        });
                    }
                }
            }
            Command::Transcribe(job) => {
                let _pending_job =
                    PendingJobGuard::new(&pending_jobs_by_generation, job.generation);
                // However the job ends, a finished file or meeting transcript
                // gives the NPU back instead of keeping the text model loaded.
                let _release_local_model = ReleaseLocalModel::after(&job.config);
                {
                    let _transition = transition_gate.lock();
                    if discard_requested.load(Ordering::Acquire) {
                        let _ = event_tx.send(WorkerEvent::JobDiscarded { id: job.id });
                        continue;
                    }
                    // Text-only retries need no speech engine at all.
                    if job.config.rerun.is_some() {
                        let _ = event_tx.send(WorkerEvent::JobStarted { id: job.id });
                    } else {
                        if engine_generation != Some(job.generation)
                            || engine_cancellation
                                .as_ref()
                                .map(CancellationToken::is_cancelled)
                                .unwrap_or(true)
                        {
                            if let Some(part) = job.config.session {
                                // Never lose a whole meeting because one part
                                // met a reloading engine.
                                let state = sessions.entry(part.id).or_default();
                                let placeholder = failed_part_text(
                                    part.offset_samples,
                                    job.sample_rate,
                                    "the speech engine was not ready",
                                );
                                let (text, diarization, warning) = if part.last {
                                    let state = sessions.remove(&part.id).unwrap_or_default();
                                    let stitched = state.finish(
                                        PartText::plain(placeholder),
                                        None,
                                        DiarizationOutcome::NotRequested,
                                    );
                                    (stitched.text, stitched.outcome, stitched.warning)
                                } else {
                                    state.parts.push(PartText::plain(placeholder.clone()));
                                    (placeholder, DiarizationOutcome::NotRequested, None)
                                };
                                let _ = event_tx.send(WorkerEvent::JobStarted { id: job.id });
                                let _ = event_tx.send(WorkerEvent::JobCompleted {
                                    id: job.id,
                                    text,
                                    diarization,
                                    cleanup: None,
                                    repair: None,
                                    notes: None,
                                    auto_paste: false,
                                    enhancement_warning: warning,
                                    delivery_target: None,
                                });
                                continue;
                            }
                            let _ = event_tx.send(WorkerEvent::JobFailed {
                                id: job.id,
                                error: "selected engine is not ready".into(),
                            });
                            continue;
                        }
                        let _ = event_tx.send(WorkerEvent::JobStarted { id: job.id });
                    }
                }
                if let Some(rerun) = job.config.rerun.clone() {
                    run_rerun(&job, rerun, &mut gate, &event_tx, &discard_requested);
                    continue;
                }
                let active_engine = engine
                    .as_mut()
                    .expect("validated engine generation is missing");
                let started = Instant::now();
                let mut diarization = DiarizationOutcome::NotRequested;
                let transcription = if job.config.long_form {
                    let interrupted = || {
                        interrupted_job_event(job.id, &discard_requested, &engine_cancellation)
                            .is_some()
                    };
                    let send_progress = |stage: JobStage, done: usize, total: usize| {
                        let _ = event_tx.send(WorkerEvent::JobProgress {
                            id: job.id,
                            stage,
                            done,
                            total,
                        });
                    };
                    // Too little audio to identify anyone (e.g. the empty tail
                    // of a meeting that was stopped right after a cut).
                    let enough_audio = job.samples.len() >= job.sample_rate as usize;
                    let turns = if job.config.diarize && enough_audio {
                        send_progress(JobStage::Diarizing, 0, 0);
                        match asr::diarize_speakers(
                            &job.samples,
                            &interrupted,
                            &mut |done, total| send_progress(JobStage::Diarizing, done, total),
                        ) {
                            Ok(result) => match job.config.session {
                                // A part of a rolling meeting: speakers are
                                // matched to everyone heard so far, and the
                                // part is always labelled (the final job
                                // strips labels if only one voice was ever
                                // heard).
                                Some(part) => {
                                    let state = sessions.entry(part.id).or_default();
                                    let mapping = state.speakers.assign(&result.centroids);
                                    let mut turns = result.turns;
                                    asr::diarize::remap_speakers(&mut turns, &mapping);
                                    if turns.is_empty() {
                                        None
                                    } else {
                                        diarization = DiarizationOutcome::Labelled {
                                            speakers: state.speakers.len(),
                                        };
                                        Some(turns)
                                    }
                                }
                                None => {
                                    let speakers = asr::diarize::speaker_count(&result.turns);
                                    if speakers >= 2 {
                                        diarization = DiarizationOutcome::Labelled { speakers };
                                        Some(result.turns)
                                    } else {
                                        diarization = DiarizationOutcome::SingleSpeaker;
                                        None
                                    }
                                }
                            },
                            Err(error) => {
                                // An interruption is handled right below by
                                // the transcription step's own checks.
                                warn!(
                                    error = format!("{error:#}"),
                                    "speaker diarization failed; continuing without labels"
                                );
                                diarization =
                                    DiarizationOutcome::Failed(short_reason(&format!("{error:#}")));
                                None
                            }
                        }
                    } else {
                        None
                    };
                    match turns {
                        Some(turns) => transcribe_turns(
                            &mut **active_engine,
                            &job.samples,
                            job.sample_rate,
                            &turns,
                            job.config.session.map_or(0, |part| part.offset_samples),
                            interrupted,
                            |done, total| send_progress(JobStage::Transcribing, done, total),
                        ),
                        None => transcribe_long_form(
                            &mut **active_engine,
                            &job.samples,
                            job.sample_rate,
                            interrupted,
                            |done, total| send_progress(JobStage::Transcribing, done, total),
                        ),
                    }
                } else {
                    active_engine.transcribe(&job.samples, job.sample_rate)
                };
                let mut transcription_failed = false;
                let text = match transcription {
                    Ok(text) => text,
                    // A failed part of a meeting becomes a visible gap; the
                    // other parts are still delivered.
                    Err(error) if job.config.session.is_some() => {
                        warn!(error = %error, "meeting part failed; leaving a gap");
                        transcription_failed = true;
                        let offset = job.config.session.map_or(0, |part| part.offset_samples);
                        failed_part_text(offset, job.sample_rate, &error.to_string())
                    }
                    Err(error) => {
                        let _transition = transition_gate.lock();
                        let event = terminal_job_event(
                            job.id,
                            &discard_requested,
                            &engine_cancellation,
                            || WorkerEvent::JobFailed {
                                id: job.id,
                                error: error.to_string(),
                            },
                        );
                        let _ = event_tx.send(event);
                        continue;
                    }
                };
                {
                    let _transition = transition_gate.lock();
                    if let Some(event) =
                        interrupted_job_event(job.id, &discard_requested, &engine_cancellation)
                    {
                        let _ = event_tx.send(event);
                        continue;
                    }
                }

                let interrupted_now = || {
                    interrupted_job_event(job.id, &discard_requested, &engine_cancellation)
                        .is_some()
                };
                let send_cleaning_progress = |done: usize, total: usize| {
                    let _ = event_tx.send(WorkerEvent::JobProgress {
                        id: job.id,
                        stage: JobStage::CleaningUp,
                        done,
                        total,
                    });
                };

                // `raw_text` is what was recognised; it is kept so a failed
                // cleanup can be redone later.
                let raw_text = text;
                let mut cleanup_report: Option<enhance::CleanupReport> = None;
                let (final_text, enhancement_warning) = if job.config.should_enhance()
                    && !transcription_failed
                {
                    let _ = event_tx.send(WorkerEvent::EnhancementStarted { id: job.id });
                    info!("enhancement requested");
                    let cleaned = if job.config.long_form {
                        clean_transcript(
                            &raw_text,
                            &job.config.settings,
                            &mut gate,
                            &interrupted_now,
                            send_cleaning_progress,
                        )
                        .map(|cleaned| (cleaned.text, cleaned.warning, Some(cleaned.report)))
                    } else {
                        enhance::enhance_outcome(&raw_text, &job.config.settings, || {
                            !interrupted_now()
                        })
                        .map(|outcome| {
                            let (text, warning) = completed_cleanup(outcome);
                            (text, warning, None)
                        })
                    };
                    let Some((cleaned_text, warning, report)) = cleaned else {
                        let _transition = transition_gate.lock();
                        if let Some(event) =
                            interrupted_job_event(job.id, &discard_requested, &engine_cancellation)
                        {
                            let _ = event_tx.send(event);
                        }
                        continue;
                    };
                    cleanup_report = report;
                    (cleaned_text, warning)
                } else {
                    (raw_text.clone(), None)
                };

                // Parts of a rolling meeting: remember everything but the last
                // part's text; the last part carries the whole transcript.
                let own_report = cleanup_report.unwrap_or_default();
                let mut final_report = cleanup_report;
                let mut repair_plan: Option<RepairPlan> = None;
                let (final_text, enhancement_warning, diarization) = match job.config.session {
                    Some(part) => {
                        let own = PartText {
                            raw: raw_text,
                            text: final_text,
                            report: own_report,
                        };
                        let state = sessions.entry(part.id).or_default();
                        state.note_part(&diarization, &enhancement_warning);
                        if part.last {
                            let mut state = sessions.remove(&part.id).unwrap_or_default();
                            // A second chance for parts whose cleanup could not
                            // reach the provider while the meeting was running.
                            if job.config.should_enhance() {
                                retry_incomplete_parts(
                                    &mut state.parts,
                                    RETRY_BUDGET,
                                    |raw| {
                                        if !gate.open(Instant::now()) {
                                            return RetryAttempt::Unavailable;
                                        }
                                        match clean_transcript(
                                            raw,
                                            &job.config.settings,
                                            &mut gate,
                                            &interrupted_now,
                                            |_, _| {},
                                        ) {
                                            Some(cleaned) => RetryAttempt::Done(cleaned),
                                            None => RetryAttempt::Interrupted,
                                        }
                                    },
                                    send_cleaning_progress,
                                );
                            }
                            let stitched = state.finish(own, enhancement_warning, diarization);
                            final_report = Some(stitched.report);
                            repair_plan = Some(stitched.plan);
                            (stitched.text, stitched.warning, stitched.outcome)
                        } else {
                            let part_text = own.text.clone();
                            state.parts.push(own);
                            let _transition = transition_gate.lock();
                            // The part's text goes along so the app can keep
                            // an in-progress transcript on disk; the final
                            // delivery still comes from the stitched whole.
                            let event = terminal_job_event(
                                job.id,
                                &discard_requested,
                                &engine_cancellation,
                                || WorkerEvent::JobCompleted {
                                    id: job.id,
                                    text: part_text,
                                    diarization: DiarizationOutcome::NotRequested,
                                    cleanup: None,
                                    repair: None,
                                    notes: None,
                                    auto_paste: false,
                                    enhancement_warning: None,
                                    delivery_target: None,
                                },
                            );
                            let _ = event_tx.send(event);
                            continue;
                        }
                    }
                    None => {
                        if job.config.long_form {
                            repair_plan = Some(RepairPlan {
                                pieces: vec![RepairPiece {
                                    text: final_text.clone(),
                                    raw: own_report.needs_retry().then_some(raw_text),
                                }],
                                separator: " ".into(),
                                unlabel: false,
                                notes_failed: false,
                            });
                        }
                        (final_text, enhancement_warning, diarization)
                    }
                };

                let (final_text, enhancement_warning, notes_text) = if job.config.notes {
                    // Phase events are idempotent: a repeat is ignored by the
                    // tracker when cleanup already announced the phase.
                    let _ = event_tx.send(WorkerEvent::EnhancementStarted { id: job.id });
                    info!("meeting notes requested");
                    let outcome =
                        enhance::generate_meeting_notes(&final_text, &job.config.settings, || {
                            interrupted_now()
                        });
                    match outcome {
                        enhance::NotesOutcome::Notes(notes) => (
                            final_text,
                            enhancement_warning,
                            Some(notes.trim().to_string()),
                        ),
                        enhance::NotesOutcome::Failed(reason) => {
                            info!(?reason, "meeting notes unavailable; saving transcript only");
                            // A provider that could not be reached is worth
                            // asking again later; a refusal is not.
                            if enhance::is_unreachable(&reason) {
                                if let Some(plan) = repair_plan.as_mut() {
                                    plan.notes_failed = true;
                                }
                            }
                            (final_text, Some(reason), None)
                        }
                        enhance::NotesOutcome::Interrupted => {
                            let _transition = transition_gate.lock();
                            if let Some(event) = interrupted_job_event(
                                job.id,
                                &discard_requested,
                                &engine_cancellation,
                            ) {
                                let _ = event_tx.send(event);
                            }
                            continue;
                        }
                    }
                } else {
                    (final_text, enhancement_warning, None)
                };

                let chars = final_text.chars().count();
                let elapsed_ms = started.elapsed().as_millis() as u64;
                let _transition = transition_gate.lock();
                let event =
                    terminal_job_event(job.id, &discard_requested, &engine_cancellation, || {
                        WorkerEvent::JobCompleted {
                            id: job.id,
                            text: final_text,
                            diarization,
                            cleanup: final_report,
                            repair: repair_plan.filter(|plan| !plan.is_complete()),
                            notes: notes_text,
                            auto_paste: job.config.settings.auto_paste,
                            enhancement_warning,
                            delivery_target: job.config.delivery_target,
                        }
                    });
                if matches!(&event, WorkerEvent::JobCompleted { .. }) {
                    info!(id = job.id, chars, elapsed_ms, "transcription job complete");
                }
                let _ = event_tx.send(event);
            }
            Command::Shutdown(mode) => {
                info!(?mode, "inference worker stopping");
                drop(engine.take());
                let _ = event_tx.send(WorkerEvent::ShutdownComplete);
                break;
            }
        }
    }
}

/// Stops OpenWritr's own local text model when dropped, if the job it was made
/// for asks for that (see [`releases_local_model`]).
struct ReleaseLocalModel(bool);

impl ReleaseLocalModel {
    fn after(config: &JobConfig) -> Self {
        Self(releases_local_model(config))
    }
}

impl Drop for ReleaseLocalModel {
    fn drop(&mut self) {
        if self.0 {
            crate::local_llm::release("the transcript is finished");
        }
    }
}

/// Whether the local text model is unloaded right after this job: imported
/// files, finished meetings and background cleanup retries that used AI
/// cleanup or notes. Dictation keeps it for the next one (it is released after
/// a few idle minutes), and so does a meeting between its parts.
fn releases_local_model(config: &JobConfig) -> bool {
    config.long_form
        && config.session.map_or(true, |part| part.last)
        && (config.should_enhance() || config.notes)
}

/// Longest window handed to the engine in one call for long-form audio.
const LONG_FORM_WINDOW_SECONDS: usize = 30;
/// Window cuts are placed at the quietest 100 ms inside this trailing span.
const LONG_FORM_SEARCH_SECONDS: usize = 4;

/// Split `samples` into windows of at most 30 s, cutting each at the quietest
/// point near the end so words are rarely bisected.
fn plan_long_form_windows(samples: &[f32], sample_rate: u32) -> Vec<std::ops::Range<usize>> {
    media::plan_windows(
        samples,
        sample_rate,
        LONG_FORM_WINDOW_SECONDS,
        LONG_FORM_SEARCH_SECONDS,
    )
}

fn transcribe_long_form(
    engine: &mut dyn Engine,
    samples: &[f32],
    sample_rate: u32,
    interrupted: impl Fn() -> bool,
    mut progress: impl FnMut(usize, usize),
) -> Result<String> {
    let windows = plan_long_form_windows(samples, sample_rate);
    let total = windows.len();
    let mut transcript = String::new();
    progress(0, total);
    for (index, window) in windows.into_iter().enumerate() {
        // The caller re-checks interruption and discards the partial text.
        if interrupted() {
            break;
        }
        let text = engine.transcribe(&samples[window], sample_rate)?;
        let text = text.trim();
        if !text.is_empty() {
            if !transcript.is_empty() {
                transcript.push(' ');
            }
            transcript.push_str(text);
        }
        progress(index + 1, total);
        info!(window = index + 1, total, "long-form window transcribed");
    }
    Ok(transcript)
}

/// One engine call: a stretch of audio spoken by one speaker.
#[derive(Debug, PartialEq, Eq)]
struct TurnWindow {
    range: std::ops::Range<usize>,
    speaker: usize,
    /// Where speech in this stretch begins, for the `[m:ss]` marker.
    mark: usize,
}

/// Turn → engine windows that **partition the whole recording**.
///
/// Every sample ends up in exactly one window, so labelling can mislabel a
/// word but never drop one: speech the diarizer could not place (a short
/// "yes", a first word at a segmentation edge) is transcribed with the
/// neighbouring speaker. Each cut between two turns is placed at the quietest
/// 100 ms of the gap between them so it never splits a word, and anything
/// longer than 30 s is split again at quiet points for the engine.
fn plan_turn_windows(turns: &[asr::diarize::Turn], samples: &[f32], rate: u32) -> Vec<TurnWindow> {
    let len = samples.len();
    let frame = (rate as usize / 10).max(1);

    // cuts[i] separates turn i from turn i + 1.
    let mut cuts: Vec<usize> = Vec::with_capacity(turns.len().saturating_sub(1));
    for pair in turns.windows(2) {
        let low = pair[0].end.min(len);
        let high = pair[1].start.max(low).min(len);
        let mut cut = if high - low >= frame {
            media::quietest_point(samples, low..high, frame).unwrap_or((low + high) / 2)
        } else {
            (low + high) / 2
        };
        if let Some(&previous) = cuts.last() {
            cut = cut.max(previous);
        }
        cuts.push(cut);
    }

    let mut windows = Vec::new();
    for (index, turn) in turns.iter().enumerate() {
        let begin = if index == 0 { 0 } else { cuts[index - 1] };
        let end = cuts.get(index).copied().unwrap_or(len);
        if begin >= end {
            continue;
        }
        for sub in plan_long_form_windows(&samples[begin..end], rate) {
            let range = begin + sub.start..begin + sub.end;
            // The marker is when speech really starts: the turn's start for
            // the window it begins in, the window's own start otherwise.
            let mark = if range.end > turn.start {
                range.start.max(turn.start)
            } else {
                range.start
            };
            windows.push(TurnWindow {
                range,
                speaker: turn.speaker,
                mark,
            });
        }
    }
    windows
}

/// Transcribe speaker by speaker and label the result:
/// `[m:ss] Speaker 1: …` blocks separated by blank lines; consecutive
/// windows of the same speaker run together under the first one's time.
/// The windows cover the whole recording (see [`plan_turn_windows`]).
fn transcribe_turns(
    engine: &mut dyn Engine,
    samples: &[f32],
    sample_rate: u32,
    turns: &[asr::diarize::Turn],
    offset_samples: usize,
    interrupted: impl Fn() -> bool,
    mut progress: impl FnMut(usize, usize),
) -> Result<String> {
    let windows = plan_turn_windows(turns, samples, sample_rate);
    let total = windows.len();
    let mut transcript = String::new();
    let mut current: Option<usize> = None;
    progress(0, total);
    for (index, window) in windows.into_iter().enumerate() {
        // The caller re-checks interruption and discards the partial text.
        if interrupted() {
            break;
        }
        let text = engine.transcribe(&samples[window.range], sample_rate)?;
        let text = text.trim();
        if !text.is_empty() {
            if current == Some(window.speaker) {
                transcript.push(' ');
                transcript.push_str(text);
            } else {
                if !transcript.is_empty() {
                    transcript.push_str("\n\n");
                }
                let clock = crate::transcript_window::format_clock(
                    ((window.mark + offset_samples) / sample_rate.max(1) as usize) as u64,
                );
                transcript.push_str(&format!("[{clock}] Speaker {}: {text}", window.speaker + 1));
                current = Some(window.speaker);
            }
        }
        progress(index + 1, total);
    }
    Ok(transcript)
}

/// Split `[m:ss]` markers off the start of each block, returning the text
/// without them and the markers in block order.
///
/// AI cleanup must not see timestamps: its integrity check treats digits as
/// facts, so a reformatted `[12:03]` would reject the whole slice.
fn strip_timestamps(text: &str) -> (String, Vec<String>) {
    let mut stamps = Vec::new();
    let blocks: Vec<&str> = text
        .split("\n\n")
        .map(|block| {
            if let Some(rest) = block.strip_prefix('[') {
                if let Some((stamp, tail)) = rest.split_once("] ") {
                    let looks_like_clock = !stamp.is_empty()
                        && stamp.len() <= 8
                        && stamp.chars().all(|c| c.is_ascii_digit() || c == ':');
                    if looks_like_clock {
                        stamps.push(stamp.to_string());
                        return tail;
                    }
                }
            }
            block
        })
        .collect();
    (blocks.join("\n\n"), stamps)
}

/// Put markers back in front of the blocks of cleaned text. If cleanup
/// changed the number of blocks the markers can no longer be matched
/// reliably, so the labelled text is returned without them.
fn restore_timestamps(cleaned: &str, stamps: &[String]) -> String {
    if stamps.is_empty() {
        return cleaned.to_string();
    }
    let blocks: Vec<&str> = cleaned.split("\n\n").collect();
    if blocks.len() != stamps.len() {
        return cleaned.to_string();
    }
    blocks
        .iter()
        .zip(stamps)
        .map(|(block, stamp)| format!("[{stamp}] {block}"))
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// Short, single-line version of an error for user-facing notices.
fn short_reason(error: &str) -> String {
    let first_line = error.lines().next().unwrap_or_default();
    let mut text: String = first_line.chars().take(240).collect();
    if first_line.chars().count() > 240 {
        text.push('…');
    }
    text
}

/// A finished part of a rolling meeting: what was recognised, what cleanup
/// made of it, and how that went (so a second pass can redo only what failed).
#[derive(Clone, Debug, Default)]
struct PartText {
    raw: String,
    text: String,
    report: enhance::CleanupReport,
}

impl PartText {
    /// Text that is final as it stands (e.g. a placeholder for a lost part).
    fn plain(text: String) -> Self {
        Self {
            raw: text.clone(),
            text,
            report: enhance::CleanupReport::default(),
        }
    }
}

/// A rolling meeting transcription, put together.
struct Stitched {
    text: String,
    outcome: DiarizationOutcome,
    warning: Option<FallbackReason>,
    report: enhance::CleanupReport,
    plan: RepairPlan,
}

/// Worker-side memory of one rolling meeting transcription.
#[derive(Default)]
struct SessionState {
    speakers: asr::diarize::SpeakerBook,
    /// Finished texts of the earlier parts, in order.
    parts: Vec<PartText>,
    /// At least one part came back speaker-labelled.
    labelled: bool,
    diarization_failure: Option<String>,
    cleanup_warning: Option<FallbackReason>,
}

impl SessionState {
    /// Remember what a finished part contributed besides its text.
    fn note_part(&mut self, diarization: &DiarizationOutcome, warning: &Option<FallbackReason>) {
        match diarization {
            DiarizationOutcome::Labelled { .. } => self.labelled = true,
            DiarizationOutcome::Failed(reason) => {
                self.diarization_failure
                    .get_or_insert_with(|| reason.clone());
            }
            _ => {}
        }
        if self.cleanup_warning.is_none() {
            self.cleanup_warning = warning.clone();
        }
    }

    /// Stitch every earlier part and `own` (the last part) together.
    fn finish(
        self,
        own: PartText,
        own_warning: Option<FallbackReason>,
        own_diarization: DiarizationOutcome,
    ) -> Stitched {
        let labelled =
            self.labelled || matches!(own_diarization, DiarizationOutcome::Labelled { .. });
        let speakers = self.speakers.len();
        let mut report = enhance::CleanupReport::default();
        let mut pieces = Vec::new();
        for part in self.parts.into_iter().chain(std::iter::once(own)) {
            if part.text.trim().is_empty() {
                continue;
            }
            report.add(part.report);
            pieces.push(RepairPiece {
                raw: part.report.needs_retry().then_some(part.raw),
                text: part.text,
            });
        }
        // One person for the whole meeting: leave it unlabelled, exactly
        // like a single-speaker file.
        let unlabel = labelled && speakers < 2;
        let plan = RepairPlan {
            pieces,
            // Labelled transcripts are blocks; plain ones are one run-on
            // string that the Markdown writer later breaks into paragraphs.
            separator: if labelled { "\n\n" } else { " " }.to_string(),
            unlabel,
            notes_failed: false,
        };
        let outcome = if labelled && !unlabel {
            DiarizationOutcome::Labelled { speakers }
        } else if labelled {
            DiarizationOutcome::SingleSpeaker
        } else if let Some(reason) = self.diarization_failure {
            DiarizationOutcome::Failed(reason)
        } else {
            own_diarization
        };
        Stitched {
            text: plan.assemble(),
            outcome,
            warning: own_warning.or(self.cleanup_warning),
            report,
            plan,
        }
    }
}

/// Remembers that the AI provider could not be reached, so the next requests
/// are not even attempted for a minute. Without this every meeting part (and
/// every dictation queued behind it) would wait out a request timeout while
/// the connection is down.
#[derive(Default)]
struct CleanupGate {
    unreachable_until: Option<Instant>,
}

impl CleanupGate {
    const COOLDOWN: std::time::Duration = std::time::Duration::from_secs(60);

    fn open(&self, now: Instant) -> bool {
        self.unreachable_until.map_or(true, |until| now >= until)
    }

    fn record(&mut self, report: &enhance::CleanupReport, now: Instant) {
        if report.cleaned > 0 {
            self.unreachable_until = None;
        } else if report.unreachable > 0 {
            self.unreachable_until = Some(now + Self::COOLDOWN);
        }
    }
}

struct CleanedText {
    text: String,
    warning: Option<FallbackReason>,
    report: enhance::CleanupReport,
}

/// AI cleanup of a long-form transcript. `[m:ss]` markers are held back from
/// the request (its integrity check treats digits as facts) and put back
/// afterwards. Skips the network when the gate says the provider is down.
/// `None` when `interrupted` fires.
fn clean_transcript(
    raw: &str,
    settings: &Settings,
    gate: &mut CleanupGate,
    interrupted: &dyn Fn() -> bool,
    progress: impl FnMut(usize, usize),
) -> Option<CleanedText> {
    let (input, stamps) = strip_timestamps(raw);
    let attempt = gate.open(Instant::now());
    let result = if attempt {
        enhance::enhance_long_form(&input, settings, interrupted, progress)?
    } else {
        enhance::skipped_cleanup(&input, settings)
    };
    // Only a real attempt says anything about the connection; a skip must not
    // extend the cool-down it is waiting out.
    if attempt {
        gate.record(&result.report, Instant::now());
    }
    let (cleaned, warning) = completed_cleanup(result.outcome);
    Some(CleanedText {
        text: restore_timestamps(&cleaned, &stamps),
        warning,
        report: result.report,
    })
}

/// How long the second pass at Stop may spend on parts whose cleanup failed
/// during the meeting.
const RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(90);

/// One retry of a part's cleanup.
enum RetryAttempt {
    /// The provider was found unreachable moments ago: don't even try.
    Unavailable,
    Interrupted,
    Done(CleanedText),
}

/// A second chance for the parts whose cleanup could not reach the provider
/// during the meeting. Stops as soon as one attempt gets nothing through (the
/// connection is still down) or the time budget is spent; what is left is
/// retried later in the background by the app.
fn retry_incomplete_parts(
    parts: &mut [PartText],
    budget: std::time::Duration,
    mut attempt: impl FnMut(&str) -> RetryAttempt,
    mut progress: impl FnMut(usize, usize),
) {
    let started = Instant::now();
    let todo: Vec<usize> = (0..parts.len())
        .filter(|&index| parts[index].report.needs_retry())
        .collect();
    let total = todo.len();
    progress(0, total);
    for (done, index) in todo.into_iter().enumerate() {
        if started.elapsed() > budget {
            break;
        }
        let cleaned = match attempt(&parts[index].raw) {
            RetryAttempt::Done(cleaned) => cleaned,
            RetryAttempt::Unavailable | RetryAttempt::Interrupted => break,
        };
        let reached = cleaned.report.cleaned > 0;
        // Never trade earlier progress for a worse second attempt.
        if cleaned.report.cleaned >= parts[index].report.cleaned {
            parts[index].text = cleaned.text;
            parts[index].report = cleaned.report;
        }
        progress(done + 1, total);
        if !reached {
            break;
        }
    }
}

/// Run a text-only retry (see [`Rerun`]).
fn run_rerun(
    job: &Job,
    rerun: Rerun,
    gate: &mut CleanupGate,
    event_tx: &Sender<WorkerEvent>,
    discard_requested: &AtomicBool,
) {
    let interrupted = || discard_requested.load(Ordering::Acquire);
    let completed = |text: String,
                     notes: Option<String>,
                     cleanup: Option<enhance::CleanupReport>,
                     warning: Option<FallbackReason>| {
        WorkerEvent::JobCompleted {
            id: job.id,
            text,
            diarization: DiarizationOutcome::NotRequested,
            cleanup,
            repair: None,
            notes,
            auto_paste: false,
            enhancement_warning: warning,
            delivery_target: None,
        }
    };
    let event = match rerun {
        Rerun::Clean { raw } => {
            match clean_transcript(&raw, &job.config.settings, gate, &interrupted, |_, _| {}) {
                Some(cleaned) => {
                    completed(cleaned.text, None, Some(cleaned.report), cleaned.warning)
                }
                None => WorkerEvent::JobDiscarded { id: job.id },
            }
        }
        Rerun::Notes { text } => {
            match enhance::generate_meeting_notes(&text, &job.config.settings, &interrupted) {
                enhance::NotesOutcome::Notes(notes) => {
                    completed(String::new(), Some(notes.trim().to_string()), None, None)
                }
                enhance::NotesOutcome::Failed(reason) => {
                    completed(String::new(), None, None, Some(reason))
                }
                enhance::NotesOutcome::Interrupted => WorkerEvent::JobDiscarded { id: job.id },
            }
        }
    };
    let _ = event_tx.send(event);
}

/// Remove `[m:ss] Speaker N: ` prefixes and join the blocks into one run of
/// text (the shape an unlabelled transcript has).
fn unlabel(text: &str) -> String {
    let (without_stamps, _) = strip_timestamps(text);
    without_stamps
        .split("\n\n")
        .map(|block| {
            let block = block.trim();
            if let Some(rest) = block.strip_prefix("Speaker ") {
                let digits = rest.chars().take_while(char::is_ascii_digit).count();
                if digits > 0 {
                    if let Some(tail) = rest[digits..].strip_prefix(": ") {
                        return tail;
                    }
                }
            }
            block
        })
        .filter(|block| !block.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Visible stand-in for a part that could not be transcribed, so the rest of
/// the meeting is still delivered.
fn failed_part_text(offset_samples: usize, sample_rate: u32, reason: &str) -> String {
    let clock = crate::transcript_window::format_clock(
        (offset_samples / sample_rate.max(1) as usize) as u64,
    );
    format!(
        "[The part of the recording starting at {clock} could not be transcribed: {}]",
        short_reason(reason)
    )
}

fn completed_cleanup(outcome: EnhanceOutcome) -> (String, Option<FallbackReason>) {
    match outcome {
        EnhanceOutcome::Enhanced { text, .. } | EnhanceOutcome::Skipped { text, .. } => {
            (text, None)
        }
        EnhanceOutcome::RawFallback { text, reason } => (text, Some(reason)),
    }
}

fn interrupted_job_event(
    id: u64,
    discard_requested: &AtomicBool,
    engine_cancellation: &Option<CancellationToken>,
) -> Option<WorkerEvent> {
    if discard_requested.load(Ordering::Acquire) {
        Some(WorkerEvent::JobDiscarded { id })
    } else if engine_cancellation
        .as_ref()
        .map(CancellationToken::is_cancelled)
        .unwrap_or(true)
    {
        Some(WorkerEvent::JobFailed {
            id,
            error: "selected engine load was cancelled".into(),
        })
    } else {
        None
    }
}

fn terminal_job_event(
    id: u64,
    discard_requested: &AtomicBool,
    engine_cancellation: &Option<CancellationToken>,
    otherwise: impl FnOnce() -> WorkerEvent,
) -> WorkerEvent {
    interrupted_job_event(id, discard_requested, engine_cancellation).unwrap_or_else(otherwise)
}

fn should_process_load(
    generation: u64,
    latest_generation: u64,
    shutdown_requested: bool,
    discard_requested: bool,
    pending_jobs_by_generation: &Mutex<HashMap<u64, usize>>,
) -> bool {
    if discard_requested {
        return false;
    }
    let has_pending_jobs = generation_has_pending_jobs(pending_jobs_by_generation, generation);
    if shutdown_requested {
        has_pending_jobs
    } else {
        generation == latest_generation || has_pending_jobs
    }
}

fn register_pending_job(pending_jobs_by_generation: &Mutex<HashMap<u64, usize>>, generation: u64) {
    let mut pending = pending_jobs_by_generation.lock();
    *pending.entry(generation).or_default() += 1;
}

fn unregister_pending_job(
    pending_jobs_by_generation: &Mutex<HashMap<u64, usize>>,
    generation: u64,
) {
    let mut pending = pending_jobs_by_generation.lock();
    let Some(count) = pending.get_mut(&generation) else {
        return;
    };
    *count -= 1;
    if *count == 0 {
        pending.remove(&generation);
    }
}

fn generation_has_pending_jobs(
    pending_jobs_by_generation: &Mutex<HashMap<u64, usize>>,
    generation: u64,
) -> bool {
    pending_jobs_by_generation
        .lock()
        .get(&generation)
        .copied()
        .unwrap_or_default()
        > 0
}

struct PendingJobGuard<'a> {
    pending_jobs_by_generation: &'a Mutex<HashMap<u64, usize>>,
    generation: u64,
}

impl<'a> PendingJobGuard<'a> {
    fn new(pending_jobs_by_generation: &'a Mutex<HashMap<u64, usize>>, generation: u64) -> Self {
        Self {
            pending_jobs_by_generation,
            generation,
        }
    }
}

impl Drop for PendingJobGuard<'_> {
    fn drop(&mut self) {
        unregister_pending_job(self.pending_jobs_by_generation, self.generation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct FakeLoader {
        failed: HashSet<String>,
        slow_engine: Option<String>,
        loads: Arc<Mutex<Vec<String>>>,
        calls: Arc<Mutex<Vec<i32>>>,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
    }

    impl EngineLoader for FakeLoader {
        fn load(
            &mut self,
            engine: &str,
            _cancellation: &CancellationToken,
            emit_model_state: &mut dyn FnMut(ModelState),
        ) -> Result<Box<dyn Engine>> {
            self.loads.lock().push(engine.to_string());
            emit_model_state(ModelState::Ready);
            if self.slow_engine.as_deref() == Some(engine) {
                thread::sleep(Duration::from_millis(50));
            }
            if self.failed.contains(engine) {
                anyhow::bail!("injected load failure");
            }
            Ok(Box::new(FakeEngine {
                calls: self.calls.clone(),
                active: self.active.clone(),
                max_active: self.max_active.clone(),
                delay: Duration::from_millis(100),
            }))
        }
    }

    struct FakeEngine {
        calls: Arc<Mutex<Vec<i32>>>,
        active: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
        delay: Duration,
    }

    impl Engine for FakeEngine {
        fn transcribe(&mut self, samples: &[f32], _sample_rate: u32) -> Result<String> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            thread::sleep(self.delay);
            let value = samples.first().copied().unwrap_or_default() as i32;
            self.calls.lock().push(value);
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(value.to_string())
        }

        fn label(&self) -> &'static str {
            "fake"
        }
    }

    fn worker(
        failed: &[&str],
        slow_engine: Option<&str>,
    ) -> (
        Worker,
        Arc<Mutex<Vec<i32>>>,
        Arc<AtomicUsize>,
        Arc<Mutex<Vec<String>>>,
    ) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let loads = Arc::new(Mutex::new(Vec::new()));
        let max_active = Arc::new(AtomicUsize::new(0));
        let loader = FakeLoader {
            failed: failed.iter().map(|value| value.to_string()).collect(),
            slow_engine: slow_engine.map(str::to_string),
            loads: loads.clone(),
            calls: calls.clone(),
            active: Arc::new(AtomicUsize::new(0)),
            max_active: max_active.clone(),
        };
        (
            Worker::spawn_with_loader(Box::new(loader)).unwrap(),
            calls,
            max_active,
            loads,
        )
    }

    fn wait_until(
        worker: &Worker,
        mut predicate: impl FnMut(&WorkerEvent) -> bool,
    ) -> Vec<WorkerEvent> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut events = Vec::new();
        while Instant::now() < deadline {
            if let Some(event) = worker.recv_timeout(Duration::from_millis(50)) {
                let done = predicate(&event);
                events.push(event);
                if done {
                    return events;
                }
            }
        }
        panic!("timed out waiting for worker event");
    }

    fn wait_ready(worker: &Worker, engine: &str) {
        wait_until(worker, |event| {
            matches!(
                event,
                WorkerEvent::EngineReady {
                    engine: ready,
                    ..
                } if ready == engine
            )
        });
    }

    fn job_config(settings: Settings, intent: RecordingIntent) -> JobConfig {
        JobConfig::new(settings, intent, None, false)
    }

    #[test]
    fn stale_engine_load_is_never_published() {
        let (mut worker, _, _, _) = worker(&[], Some("slow"));
        worker.load("slow".into()).unwrap();
        thread::sleep(Duration::from_millis(5));
        worker.load("current".into()).unwrap();

        let events = wait_until(&worker, |event| {
            matches!(
                event,
                WorkerEvent::EngineReady { engine, .. } if engine == "current"
            )
        });

        assert!(!events.iter().any(|event| {
            matches!(
                event,
                WorkerEvent::EngineReady { engine, .. } if engine == "slow"
            )
        }));
    }

    #[test]
    fn load_failure_never_substitutes_another_engine() {
        let (mut worker, _, _, _) = worker(&["broken"], None);
        worker.load("broken".into()).unwrap();

        let events = wait_until(
            &worker,
            |event| matches!(event, WorkerEvent::EngineFailed { engine, .. } if engine == "broken"),
        );

        assert!(!events
            .iter()
            .any(|event| matches!(event, WorkerEvent::EngineReady { .. })));
    }

    #[test]
    fn jobs_run_fifo_without_concurrent_engine_calls() {
        let (mut worker, calls, max_active, _) = worker(&[], None);
        worker.load("ready".into()).unwrap();
        wait_ready(&worker, "ready");
        let settings = Settings::default();
        let first = worker
            .enqueue(
                vec![1.0],
                16_000,
                job_config(settings.clone(), RecordingIntent::Raw),
            )
            .unwrap();
        let second = worker
            .enqueue(
                vec![2.0],
                16_000,
                job_config(settings, RecordingIntent::Raw),
            )
            .unwrap();

        let events = wait_until(
            &worker,
            |event| matches!(event, WorkerEvent::JobCompleted { id, .. } if *id == second),
        );
        let completed = events
            .iter()
            .filter_map(|event| match event {
                WorkerEvent::JobCompleted { id, .. } => Some(*id),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(completed, vec![first, second]);
        assert_eq!(&*calls.lock(), &[1, 2]);
        assert_eq!(max_active.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn delivery_target_metadata_is_carried_to_completion() {
        let (mut worker, _, _, _) = worker(&[], None);
        worker.load("ready".into()).unwrap();
        wait_ready(&worker, "ready");
        let target = DeliveryTarget {
            hwnd: 17,
            process_id: 23,
            process_creation_time_100ns: Some(42),
        };
        let job = worker
            .enqueue(
                vec![1.0],
                16_000,
                JobConfig::new(
                    Settings::default(),
                    RecordingIntent::Raw,
                    Some(target),
                    false,
                ),
            )
            .unwrap();

        let events = wait_until(
            &worker,
            |event| matches!(event, WorkerEvent::JobCompleted { id, .. } if *id == job),
        );
        let completed_target = events.into_iter().find_map(|event| match event {
            WorkerEvent::JobCompleted {
                delivery_target, ..
            } => Some(delivery_target),
            _ => None,
        });

        assert_eq!(completed_target, Some(Some(target)));
    }

    #[test]
    fn job_snapshot_drives_provider_model_endpoint_and_prompt_resolution() {
        let mut live = Settings::default();
        live.enhance.mode = crate::settings::EnhanceMode::Always;
        live.enhance.provider = "openai_compatible".into();
        live.enhance.base_url = "https://snapshot.example/v1/".into();
        live.enhance.model = "private-deployment-42".into();
        let target = live.prompt_target().unwrap();
        live.prompt_overrides
            .set(target, "Use the frozen prompt.".into());

        let config = JobConfig::new(live.clone(), RecordingIntent::Enhance, None, false);
        live.enhance.provider = "github_copilot".into();
        live.enhance.base_url = "https://changed.example/v1".into();
        live.enhance.model = "changed-model".into();

        let prepared =
            crate::enhance::prepare_cleanup_request("raw transcript", &config.settings).unwrap();
        assert_eq!(prepared.url, "https://snapshot.example/v1/chat/completions");
        assert_eq!(prepared.body["model"], "private-deployment-42");
        let messages = prepared.body["messages"].as_array().unwrap();
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(
            messages[0]["content"],
            format!(
                "Use the frozen prompt.\n\n{}",
                crate::cleanup::prompt::TRANSCRIPT_ONLY_POLICY
            )
        );
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"], "raw transcript");
    }

    #[test]
    fn enhancement_failure_returns_the_raw_transcript_with_a_warning() {
        let (mut worker, _, _, _) = worker(&[], None);
        worker.load("ready".into()).unwrap();
        wait_ready(&worker, "ready");
        let mut settings = Settings::default();
        settings.enhance.mode = crate::settings::EnhanceMode::Always;
        settings.enhance.provider = "invalid-provider".into();
        let job = worker
            .enqueue(
                vec![7.0],
                16_000,
                job_config(settings, RecordingIntent::Enhance),
            )
            .unwrap();

        let events = wait_until(
            &worker,
            |event| matches!(event, WorkerEvent::JobCompleted { id, .. } if *id == job),
        );
        let completed = events.into_iter().find_map(|event| match event {
            WorkerEvent::JobCompleted {
                text,
                enhancement_warning,
                ..
            } => Some((text, enhancement_warning)),
            _ => None,
        });

        let (text, warning) = completed.unwrap();
        assert_eq!(text, "7");
        assert_eq!(warning, Some(FallbackReason::UnknownProvider));
    }

    #[test]
    fn validator_rejection_is_a_typed_raw_fallback_for_the_worker() {
        let outcome =
            crate::cleanup::pipeline::finalize("transfer 42 dollars", "Transfer 24 dollars.");
        let (text, warning) = completed_cleanup(outcome);
        assert_eq!(text, "transfer 42 dollars");
        assert!(matches!(
            warning,
            Some(FallbackReason::IntegrityRejected(_))
        ));
    }

    #[test]
    fn enhancement_phase_is_published_before_completion() {
        let (mut worker, _, _, _) = worker(&[], None);
        worker.load("ready".into()).unwrap();
        wait_ready(&worker, "ready");
        let mut settings = Settings::default();
        settings.enhance.mode = crate::settings::EnhanceMode::Always;
        settings.enhance.provider = "invalid-provider".into();
        let job = worker
            .enqueue(
                vec![7.0],
                16_000,
                job_config(settings, RecordingIntent::Enhance),
            )
            .unwrap();

        let events = wait_until(
            &worker,
            |event| matches!(event, WorkerEvent::JobCompleted { id, .. } if *id == job),
        );
        let phases = events
            .iter()
            .filter_map(|event| match event {
                WorkerEvent::JobStarted { id } if *id == job => Some("transcribing"),
                WorkerEvent::EnhancementStarted { id } if *id == job => Some("enhancing"),
                WorkerEvent::JobCompleted { id, .. } if *id == job => Some("completed"),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(phases, ["transcribing", "enhancing", "completed"]);
    }

    #[test]
    fn job_tombstones_suppress_late_and_backward_events() {
        let mut tracker = JobEventTracker::default();
        tracker.register(41);

        assert!(matches!(
            tracker.normalize(WorkerEvent::JobStarted { id: 41 }, false),
            Some(WorkerEvent::JobStarted { id: 41 })
        ));
        assert!(matches!(
            tracker.normalize(WorkerEvent::EnhancementStarted { id: 41 }, false),
            Some(WorkerEvent::EnhancementStarted { id: 41 })
        ));
        assert!(matches!(
            tracker.normalize(
                WorkerEvent::JobCompleted {
                    id: 41,
                    text: "done".into(),
                    diarization: DiarizationOutcome::NotRequested,
                    cleanup: None,
                    repair: None,
                    notes: None,
                    auto_paste: true,
                    enhancement_warning: None,
                    delivery_target: None,
                },
                false,
            ),
            Some(WorkerEvent::JobCompleted { id: 41, .. })
        ));
        assert!(tracker
            .normalize(WorkerEvent::EnhancementStarted { id: 41 }, false)
            .is_none());
        assert!(tracker
            .normalize(
                WorkerEvent::JobCompleted {
                    id: 41,
                    text: "late".into(),
                    diarization: DiarizationOutcome::NotRequested,
                    cleanup: None,
                    repair: None,
                    notes: None,
                    auto_paste: true,
                    enhancement_warning: None,
                    delivery_target: None,
                },
                false,
            )
            .is_none());
    }

    fn turn(start: usize, end: usize, speaker: usize) -> asr::diarize::Turn {
        asr::diarize::Turn {
            start,
            end,
            speaker,
        }
    }

    #[test]
    fn turn_windows_partition_the_recording() {
        let samples = vec![0.5f32; 16_000 * 10];
        let windows = plan_turn_windows(
            &[turn(16_000, 32_000, 0), turn(32_500, 48_000, 1)],
            &samples,
            16_000,
        );
        assert_eq!(windows.len(), 2);
        // Audio before the first turn belongs to it; the 500-sample gap
        // (shorter than a 100 ms frame) is split down the middle; the tail
        // after the last turn belongs to the last turn.
        assert_eq!(windows[0].range, 0..32_250);
        assert_eq!(windows[1].range, 32_250..samples.len());
        assert_eq!((windows[0].speaker, windows[1].speaker), (0, 1));
        // Markers show when speech really starts, not the window start.
        assert_eq!((windows[0].mark, windows[1].mark), (16_000, 32_500));
    }

    #[test]
    fn turn_cuts_land_in_the_quietest_part_of_the_gap() {
        let mut samples = vec![0.5f32; 16_000 * 4];
        // Silence only in 2.0 s..2.5 s of the 1 s..3 s gap.
        for s in &mut samples[32_000..40_000] {
            *s = 0.0;
        }
        let windows = plan_turn_windows(
            &[turn(0, 16_000, 0), turn(48_000, 64_000, 1)],
            &samples,
            16_000,
        );
        assert_eq!(windows.len(), 2);
        let cut = windows[0].range.end;
        assert!(
            (32_000..40_000).contains(&cut),
            "cut {cut} is not in the silence"
        );
        assert_eq!(windows[1].range.start, cut);
    }

    #[test]
    fn turn_windows_cover_every_sample_exactly_once() {
        // Deterministic pseudo-random layouts at a 1 kHz "sample rate" keep
        // the cases cheap while exercising 30 s splits (30 000 samples).
        let rate = 1_000u32;
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = |bound: usize| -> usize {
            state = state
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((state >> 33) as usize) % bound.max(1)
        };
        for case in 0..300 {
            let mut turns = Vec::new();
            let mut position = next(20_000);
            for _ in 0..(1 + next(12)) {
                let length = 300 + next(60_000);
                turns.push(turn(position, position + length, next(4)));
                position += length + next(40_000);
            }
            let len = position + next(30_000);
            let samples: Vec<f32> = (0..len).map(|_| next(2_000) as f32 / 1_000.0).collect();

            let windows = plan_turn_windows(&turns, &samples, rate);
            assert!(!windows.is_empty(), "case {case}");
            assert_eq!(windows.first().unwrap().range.start, 0, "case {case}");
            assert_eq!(windows.last().unwrap().range.end, len, "case {case}");
            for pair in windows.windows(2) {
                assert_eq!(pair[0].range.end, pair[1].range.start, "case {case}");
            }
            assert!(
                windows.iter().all(|w| w.range.len() <= 30_000),
                "case {case}"
            );
            assert!(windows.iter().all(|w| !w.range.is_empty()), "case {case}");
        }
    }

    #[test]
    fn long_turns_are_split_for_the_engine() {
        let samples = vec![0.5f32; 16_000 * 90];
        let windows = plan_turn_windows(&[turn(0, 16_000 * 80, 2)], &samples, 16_000);
        assert!(windows.len() >= 3);
        assert!(windows.iter().all(|w| w.range.len() <= 16_000 * 30));
        assert!(windows.iter().all(|w| w.speaker == 2));
    }

    #[test]
    fn timestamps_survive_cleanup_by_block_position() {
        let labelled = "[0:00] Speaker 1: hello there\n\n[1:02:05] Speaker 2: hi";
        let (plain, stamps) = strip_timestamps(labelled);
        assert_eq!(plain, "Speaker 1: hello there\n\nSpeaker 2: hi");
        assert_eq!(stamps, vec!["0:00", "1:02:05"]);
        assert_eq!(restore_timestamps(&plain, &stamps), labelled);

        // Cleanup that edits words inside blocks keeps the markers aligned.
        let cleaned = "Speaker 1: Hello there.\n\nSpeaker 2: Hi!";
        assert_eq!(
            restore_timestamps(cleaned, &stamps),
            "[0:00] Speaker 1: Hello there.\n\n[1:02:05] Speaker 2: Hi!"
        );
    }

    #[test]
    fn timestamps_are_dropped_when_cleanup_changes_the_block_count() {
        let stamps = vec!["0:00".to_string(), "0:05".to_string()];
        let merged = "Speaker 1: one two";
        assert_eq!(restore_timestamps(merged, &stamps), merged);
        // No markers to strip from ordinary text.
        let (plain, none) = strip_timestamps("just text [with brackets] here");
        assert_eq!(plain, "just text [with brackets] here");
        assert!(none.is_empty());
    }

    struct LevelsEngine;

    impl Engine for LevelsEngine {
        fn transcribe(&mut self, samples: &[f32], _sample_rate: u32) -> Result<String> {
            // Reports every distinct integer level present in the audio.
            let mut levels: Vec<i32> = samples
                .iter()
                .map(|s| *s as i32)
                .filter(|level| *level != 0)
                .collect();
            levels.sort_unstable();
            levels.dedup();
            Ok(levels
                .iter()
                .map(|level| format!("L{level}"))
                .collect::<Vec<_>>()
                .join(" "))
        }

        fn label(&self) -> &'static str {
            "levels"
        }
    }

    #[test]
    fn speech_between_turns_is_never_dropped() {
        let mut samples = vec![0.0f32; 16_000 * 7];
        for (range, value) in [
            (0..16_000, 1.0),              // speaker 1
            (24_000..27_200, 9.0),         // an unattributed word in the gap
            (48_000..64_000, 3.0),         // speaker 2
            (96_000..96_000 + 1_600, 7.0), // a trailing word after the last turn
        ] {
            for s in &mut samples[range] {
                *s = value;
            }
        }
        let turns = [turn(0, 16_000, 0), turn(48_000, 64_000, 1)];
        let text = transcribe_turns(
            &mut LevelsEngine,
            &samples,
            16_000,
            &turns,
            0,
            || false,
            |_, _| {},
        )
        .unwrap();
        for level in ["L1", "L3", "L7", "L9"] {
            assert_eq!(
                text.matches(level).count(),
                1,
                "{level} must be transcribed exactly once in: {text}"
            );
        }
    }

    struct EchoEngine;

    impl Engine for EchoEngine {
        fn transcribe(&mut self, samples: &[f32], _sample_rate: u32) -> Result<String> {
            // The loudest sample encodes which turn this is (windows include
            // a little silence of padding).
            let peak = samples.iter().copied().fold(0.0f32, f32::max);
            Ok(format!("w{}", peak as i32))
        }

        fn label(&self) -> &'static str {
            "echo"
        }
    }

    #[test]
    fn turn_transcripts_are_labelled_and_merge_same_speaker_runs() {
        let mut samples = vec![0.0f32; 16_000 * 6];
        // Turns: spk0 [0,1s), spk0 [1.2s,2s), spk1 [3s,4s), spk0 [5s,6s)
        for (range, value) in [
            (0..16_000, 1.0),
            (19_200..32_000, 2.0),
            (48_000..64_000, 3.0),
            (80_000..96_000, 4.0),
        ] {
            for s in &mut samples[range] {
                *s = value;
            }
        }
        let turns = [
            turn(0, 16_000, 0),
            turn(19_200, 32_000, 0),
            turn(48_000, 64_000, 1),
            turn(80_000, 96_000, 0),
        ];
        let mut seen = Vec::new();
        let text = transcribe_turns(
            &mut EchoEngine,
            &samples,
            16_000,
            &turns,
            0,
            || false,
            |done, total| seen.push((done, total)),
        )
        .unwrap();
        assert_eq!(
            text,
            "[0:00] Speaker 1: w1 w2\n\n[0:03] Speaker 2: w3\n\n[0:05] Speaker 1: w4"
        );
        assert_eq!(seen.first(), Some(&(0, 4)));
        assert_eq!(seen.last(), Some(&(4, 4)));
    }

    fn state_with_speakers(count: usize) -> SessionState {
        let mut state = SessionState::default();
        // Orthogonal voice prints are always distinct people.
        let voices: Vec<Vec<f32>> = (0..count)
            .map(|i| {
                let mut v = vec![0.0f32; count.max(1)];
                v[i] = 1.0;
                v
            })
            .collect();
        state.speakers.assign(&voices);
        state
    }

    fn parts(texts: &[&str]) -> Vec<PartText> {
        texts
            .iter()
            .map(|text| PartText::plain((*text).to_string()))
            .collect()
    }

    fn report(total: usize, cleaned: usize, unreachable: usize) -> enhance::CleanupReport {
        enhance::CleanupReport {
            total,
            cleaned,
            unreachable,
        }
    }

    #[test]
    fn session_stitches_labelled_parts_as_blocks_with_global_speakers() {
        let mut state = state_with_speakers(2);
        state.labelled = true;
        state.parts = parts(&["[0:00] Speaker 1: hello", ""]);
        let stitched = state.finish(
            PartText::plain("[1:00:00] Speaker 2: hi again".into()),
            None,
            DiarizationOutcome::Labelled { speakers: 2 },
        );
        assert_eq!(
            stitched.text,
            "[0:00] Speaker 1: hello\n\n[1:00:00] Speaker 2: hi again"
        );
        assert_eq!(
            stitched.outcome,
            DiarizationOutcome::Labelled { speakers: 2 }
        );
        assert!(stitched.warning.is_none());
        assert_eq!(stitched.plan.separator, "\n\n");
        assert!(stitched.plan.is_complete());
    }

    #[test]
    fn a_meeting_with_one_voice_comes_out_unlabelled() {
        let mut state = state_with_speakers(1);
        state.labelled = true;
        state.parts = parts(&["[0:00] Speaker 1: one two"]);
        let stitched = state.finish(
            PartText::plain("[1:00:00] Speaker 1: three".into()),
            None,
            DiarizationOutcome::Labelled { speakers: 1 },
        );
        assert_eq!(stitched.text, "one two three");
        assert_eq!(stitched.outcome, DiarizationOutcome::SingleSpeaker);
        // A later repair must strip the labels the same way.
        assert!(stitched.plan.unlabel);
        assert_eq!(stitched.plan.assemble(), "one two three");
    }

    #[test]
    fn unlabelled_parts_run_together_and_empty_parts_vanish() {
        let state = SessionState {
            parts: parts(&["one two", "   ", "three"]),
            ..Default::default()
        };
        let stitched = state.finish(
            PartText::plain("four".into()),
            None,
            DiarizationOutcome::NotRequested,
        );
        assert_eq!(stitched.text, "one two three four");
        assert_eq!(stitched.outcome, DiarizationOutcome::NotRequested);

        // A silent tail adds nothing.
        let state = SessionState {
            parts: parts(&["only part"]),
            ..Default::default()
        };
        let stitched = state.finish(
            PartText::plain(String::new()),
            None,
            DiarizationOutcome::NotRequested,
        );
        assert_eq!(stitched.text, "only part");
    }

    #[test]
    fn session_reports_diarization_failure_and_keeps_the_first_warning() {
        let mut state = SessionState::default();
        state.note_part(
            &DiarizationOutcome::Failed("model missing".into()),
            &Some(FallbackReason::RequestFailed),
        );
        state.note_part(
            &DiarizationOutcome::Failed("later".into()),
            &Some(FallbackReason::MissingCredential),
        );
        let stitched = state.finish(
            PartText::plain("x".into()),
            None,
            DiarizationOutcome::Failed("last".into()),
        );
        assert_eq!(
            stitched.outcome,
            DiarizationOutcome::Failed("model missing".into())
        );
        assert_eq!(stitched.warning, Some(FallbackReason::RequestFailed));
    }

    #[test]
    fn pieces_that_could_not_be_cleaned_keep_their_raw_text_for_a_retry() {
        let state = SessionState {
            parts: vec![
                PartText {
                    raw: "first as heard".into(),
                    text: "First, cleaned.".into(),
                    report: report(2, 2, 0),
                },
                PartText {
                    raw: "second as heard".into(),
                    text: "second as heard".into(),
                    report: report(2, 0, 2),
                },
            ],
            ..Default::default()
        };
        let stitched = state.finish(
            PartText {
                raw: "third as heard".into(),
                text: "Third, half cleaned".into(),
                report: report(2, 1, 1),
            },
            None,
            DiarizationOutcome::NotRequested,
        );
        assert_eq!(stitched.report, report(6, 3, 3));
        assert!(stitched.report.needs_retry());
        let raws: Vec<Option<&str>> = stitched
            .plan
            .pieces
            .iter()
            .map(|piece| piece.raw.as_deref())
            .collect();
        assert_eq!(
            raws,
            vec![None, Some("second as heard"), Some("third as heard")]
        );
        assert!(!stitched.plan.is_complete());
        assert_eq!(
            stitched.plan.assemble(),
            "First, cleaned. second as heard Third, half cleaned"
        );
    }

    #[test]
    fn a_plan_is_complete_only_when_nothing_is_left_to_retry() {
        let mut plan = RepairPlan {
            pieces: vec![RepairPiece {
                text: "t".into(),
                raw: Some("r".into()),
            }],
            separator: " ".into(),
            unlabel: false,
            notes_failed: false,
        };
        assert!(!plan.is_complete());
        plan.pieces[0].raw = None;
        assert!(plan.is_complete());
        plan.notes_failed = true;
        assert!(!plan.is_complete());
    }

    #[test]
    fn the_cleanup_gate_closes_when_nothing_gets_through_and_reopens_on_success() {
        let now = Instant::now();
        let mut gate = CleanupGate::default();
        assert!(gate.open(now));

        // Some slices worked: the provider is evidently reachable.
        gate.record(&report(4, 2, 2), now);
        assert!(gate.open(now));

        // Nothing got through: stay away for the cool-down.
        gate.record(&report(4, 0, 4), now);
        assert!(!gate.open(now));
        assert!(!gate.open(now + CleanupGate::COOLDOWN - std::time::Duration::from_secs(1)));
        assert!(gate.open(now + CleanupGate::COOLDOWN));

        // A rejected answer is not a connection problem.
        let mut gate = CleanupGate::default();
        gate.record(&report(4, 0, 0), now);
        assert!(gate.open(now));

        // One success clears an earlier outage.
        let mut gate = CleanupGate::default();
        gate.record(&report(4, 0, 4), now);
        gate.record(&report(2, 1, 1), now);
        assert!(gate.open(now));
    }

    fn cleaned(text: &str, report: enhance::CleanupReport) -> RetryAttempt {
        RetryAttempt::Done(CleanedText {
            text: text.into(),
            warning: None,
            report,
        })
    }

    fn pending_part(raw: &str) -> PartText {
        PartText {
            raw: raw.into(),
            text: raw.into(),
            report: report(2, 0, 2),
        }
    }

    #[test]
    fn the_retry_pass_fixes_parts_and_skips_those_already_clean() {
        let mut list = vec![
            PartText {
                raw: "ok raw".into(),
                text: "Ok.".into(),
                report: report(1, 1, 0),
            },
            pending_part("needs it"),
        ];
        let mut asked = Vec::new();
        retry_incomplete_parts(
            &mut list,
            std::time::Duration::from_secs(60),
            |raw| {
                asked.push(raw.to_string());
                cleaned("Needs it.", report(2, 2, 0))
            },
            |_, _| {},
        );
        assert_eq!(asked, vec!["needs it"], "only the failed part is retried");
        assert_eq!(list[0].text, "Ok.");
        assert_eq!(list[1].text, "Needs it.");
        assert!(!list[1].report.needs_retry());
    }

    #[test]
    fn the_retry_pass_gives_up_at_once_while_still_offline() {
        let mut list = vec![pending_part("a"), pending_part("b"), pending_part("c")];
        let mut calls = 0;
        retry_incomplete_parts(
            &mut list,
            std::time::Duration::from_secs(60),
            |_| {
                calls += 1;
                // Nothing got through.
                cleaned("unchanged", report(2, 0, 2))
            },
            |_, _| {},
        );
        assert_eq!(calls, 1, "one failed attempt is enough to know");

        // The gate being closed means not even one attempt is made.
        let mut calls = 0;
        retry_incomplete_parts(
            &mut list,
            std::time::Duration::from_secs(60),
            |_| {
                calls += 1;
                RetryAttempt::Unavailable
            },
            |_, _| {},
        );
        assert_eq!(calls, 1);
        assert!(list.iter().all(|part| part.report.needs_retry()));
    }

    #[test]
    fn a_worse_retry_never_replaces_earlier_progress() {
        let mut list = vec![PartText {
            raw: "raw".into(),
            text: "partly cleaned".into(),
            report: report(4, 3, 1),
        }];
        retry_incomplete_parts(
            &mut list,
            std::time::Duration::from_secs(60),
            |_| cleaned("raw", report(4, 1, 3)),
            |_, _| {},
        );
        assert_eq!(list[0].text, "partly cleaned");
        assert_eq!(list[0].report, report(4, 3, 1));
    }

    #[test]
    fn an_exhausted_budget_stops_the_pass() {
        let mut list = vec![pending_part("a"), pending_part("b")];
        let mut calls = 0;
        retry_incomplete_parts(
            &mut list,
            std::time::Duration::ZERO,
            |_| {
                calls += 1;
                cleaned("x", report(2, 2, 0))
            },
            |_, _| {},
        );
        assert!(calls <= 1, "a zero budget allows at most the first attempt");
    }

    #[test]
    fn rerun_jobs_need_no_engine_and_report_how_cleanup_went() {
        // The harness worker never loads an engine here: a rerun is text only.
        let (mut worker, _, _, _) = worker(&[], None);
        let mut settings = Settings::default();
        settings.enhance.mode = crate::settings::EnhanceMode::Never;
        let job = worker
            .enqueue(
                Vec::new(),
                16_000,
                JobConfig::rerun(
                    settings,
                    Rerun::Clean {
                        raw: "[0:05] Speaker 1: as recognised".into(),
                    },
                ),
            )
            .unwrap();
        let events = wait_until(
            &worker,
            |event| matches!(event, WorkerEvent::JobCompleted { id, .. } if *id == job),
        );
        let Some(WorkerEvent::JobCompleted {
            text,
            cleanup,
            repair,
            notes,
            ..
        }) = events.last()
        else {
            panic!("the rerun should complete");
        };
        // Cleanup is switched off, so the text comes back untouched, with
        // its marker, and nothing is left to retry.
        assert_eq!(text, "[0:05] Speaker 1: as recognised");
        assert!(cleanup.is_some_and(|report| !report.needs_retry()));
        assert!(repair.is_none() && notes.is_none());
    }

    #[test]
    fn unlabel_removes_markers_and_labels() {
        assert_eq!(
            unlabel("[0:00] Speaker 1: hi\n\nSpeaker 12: there\n\n[1:02:05] Speaker 3: you"),
            "hi there you"
        );
        assert_eq!(unlabel("no labels here"), "no labels here");
        // Text that merely starts with the word is left alone.
        assert_eq!(
            unlabel("Speaker notes: important"),
            "Speaker notes: important"
        );
    }

    #[test]
    fn failed_parts_leave_a_visible_gap_with_the_meeting_time() {
        let text = failed_part_text(16_000 * 3_725, 16_000, "engine exploded\nsecond line");
        assert_eq!(
            text,
            "[The part of the recording starting at 1:02:05 could not be transcribed: engine exploded]"
        );
    }

    #[test]
    fn meeting_part_configs_only_ask_for_notes_on_the_last_part() {
        let part = |last| SessionPart {
            id: 1,
            offset_samples: 0,
            last,
        };
        let settings = Settings::default();
        assert!(!JobConfig::meeting_part(settings.clone(), part(false), true).notes);
        assert!(JobConfig::meeting_part(settings.clone(), part(true), true).notes);
        assert!(!JobConfig::meeting_part(settings, part(true), false).notes);
    }

    #[test]
    fn rolling_parts_report_their_own_text_and_the_last_one_carries_the_transcript() {
        let (mut worker, _, _, _) = worker(&[], None);
        worker.load("ready".into()).unwrap();
        wait_ready(&worker, "ready");
        let mut settings = Settings::default();
        settings.enhance.mode = crate::settings::EnhanceMode::Never;

        let mut ids = Vec::new();
        for (index, level) in [1.0f32, 2.0, 3.0].into_iter().enumerate() {
            let part = SessionPart {
                id: 7,
                offset_samples: index * 16_000,
                last: index == 2,
            };
            ids.push(
                worker
                    .enqueue(
                        vec![level; 16_000],
                        16_000,
                        JobConfig::meeting_part(settings.clone(), part, false),
                    )
                    .unwrap(),
            );
        }

        let events = wait_until(
            &worker,
            |event| matches!(event, WorkerEvent::JobCompleted { id, .. } if *id == ids[2]),
        );
        let completed: Vec<(u64, String)> = events
            .iter()
            .filter_map(|event| match event {
                WorkerEvent::JobCompleted { id, text, .. } => Some((*id, text.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            completed,
            vec![
                (ids[0], "1".to_string()),
                (ids[1], "2".to_string()),
                (ids[2], "1 2 3".to_string()),
            ]
        );
    }

    #[test]
    fn long_form_jobs_report_ordered_progress_before_completion() {
        let (mut worker, _, _, _) = worker(&[], None);
        worker.load("ready".into()).unwrap();
        wait_ready(&worker, "ready");
        let mut settings = Settings::default();
        settings.enhance.mode = crate::settings::EnhanceMode::Never;
        let job = worker
            .enqueue(
                vec![1.0; 65 * 16_000],
                16_000,
                JobConfig::long_form(settings),
            )
            .unwrap();

        let events = wait_until(
            &worker,
            |event| matches!(event, WorkerEvent::JobCompleted { id, .. } if *id == job),
        );
        let progress = events
            .iter()
            .filter_map(|event| match event {
                WorkerEvent::JobProgress {
                    id,
                    stage: JobStage::Transcribing,
                    done,
                    total,
                } if *id == job => Some((*done, *total)),
                _ => None,
            })
            .collect::<Vec<_>>();

        let total = progress.first().expect("progress was reported").1;
        assert!(total >= 2);
        let expected = (0..=total).map(|done| (done, total)).collect::<Vec<_>>();
        assert_eq!(progress, expected);
    }

    #[test]
    fn dictation_jobs_never_report_progress() {
        let (mut worker, _, _, _) = worker(&[], None);
        worker.load("ready".into()).unwrap();
        wait_ready(&worker, "ready");
        let job = worker
            .enqueue(
                vec![1.0],
                16_000,
                job_config(Settings::default(), RecordingIntent::Raw),
            )
            .unwrap();
        let events = wait_until(
            &worker,
            |event| matches!(event, WorkerEvent::JobCompleted { id, .. } if *id == job),
        );
        assert!(!events
            .iter()
            .any(|event| matches!(event, WorkerEvent::JobProgress { .. })));
    }

    #[test]
    fn wait_shutdown_drains_jobs_before_stopping() {
        let (mut worker, calls, _, _) = worker(&[], None);
        worker.load("ready".into()).unwrap();
        wait_ready(&worker, "ready");
        let settings = Settings::default();
        worker
            .enqueue(
                vec![1.0],
                16_000,
                job_config(settings.clone(), RecordingIntent::Raw),
            )
            .unwrap();
        worker
            .enqueue(
                vec![2.0],
                16_000,
                job_config(settings, RecordingIntent::Raw),
            )
            .unwrap();
        worker.shutdown(ShutdownMode::Wait).unwrap();

        let events = wait_until(&worker, |event| {
            matches!(event, WorkerEvent::ShutdownComplete)
        });
        let completed = events
            .iter()
            .filter(|event| matches!(event, WorkerEvent::JobCompleted { .. }))
            .count();

        assert_eq!(completed, 2);
        assert_eq!(&*calls.lock(), &[1, 2]);
    }

    #[test]
    fn wait_shutdown_skips_a_queued_engine_load() {
        let (mut worker, calls, _, loads) = worker(&[], None);
        worker.load("ready".into()).unwrap();
        wait_ready(&worker, "ready");
        let job = worker
            .enqueue(
                vec![1.0],
                16_000,
                job_config(Settings::default(), RecordingIntent::Raw),
            )
            .unwrap();
        wait_until(
            &worker,
            |event| matches!(event, WorkerEvent::JobStarted { id } if *id == job),
        );
        worker.load("unused".into()).unwrap();
        worker.shutdown(ShutdownMode::Wait).unwrap();

        let events = wait_until(&worker, |event| {
            matches!(event, WorkerEvent::ShutdownComplete)
        });

        assert!(events
            .iter()
            .any(|event| matches!(event, WorkerEvent::JobCompleted { id, .. } if *id == job)));
        assert_eq!(&*calls.lock(), &[1]);
        assert_eq!(&*loads.lock(), &["ready"]);
    }

    #[test]
    fn wait_shutdown_finishes_a_job_that_depends_on_an_active_load() {
        let (mut worker, calls, _, loads) = worker(&[], Some("required"));
        worker.load("ready".into()).unwrap();
        wait_ready(&worker, "ready");
        worker.load("required".into()).unwrap();
        let job = worker
            .enqueue(
                vec![2.0],
                16_000,
                job_config(Settings::default(), RecordingIntent::Raw),
            )
            .unwrap();
        worker.shutdown(ShutdownMode::Wait).unwrap();

        let events = wait_until(&worker, |event| {
            matches!(event, WorkerEvent::ShutdownComplete)
        });

        assert!(events
            .iter()
            .any(|event| matches!(event, WorkerEvent::JobCompleted { id, .. } if *id == job)));
        assert_eq!(&*calls.lock(), &[2]);
        assert_eq!(&*loads.lock(), &["ready", "required"]);
    }

    #[test]
    fn newer_engine_request_preserves_an_older_load_needed_by_a_job() {
        let (mut worker, calls, _, loads) = worker(&[], Some("required"));
        worker.load("ready".into()).unwrap();
        wait_ready(&worker, "ready");
        worker.load("required".into()).unwrap();
        let job = worker
            .enqueue(
                vec![2.0],
                16_000,
                job_config(Settings::default(), RecordingIntent::Raw),
            )
            .unwrap();
        worker.load("latest".into()).unwrap();

        let events = wait_until(
            &worker,
            |event| matches!(event, WorkerEvent::EngineReady { engine, .. } if engine == "latest"),
        );

        assert!(events
            .iter()
            .any(|event| matches!(event, WorkerEvent::JobCompleted { id, .. } if *id == job)));
        assert_eq!(&*calls.lock(), &[2]);
        assert_eq!(&*loads.lock(), &["ready", "required", "latest"]);
    }

    #[test]
    fn cancelled_load_is_not_revived_by_a_pending_job() {
        let (mut worker, _, _, _) = worker(&[], Some("cancelled"));
        worker.load("ready".into()).unwrap();
        wait_ready(&worker, "ready");
        worker.load("cancelled".into()).unwrap();
        let job = worker
            .enqueue(
                vec![2.0],
                16_000,
                job_config(Settings::default(), RecordingIntent::Raw),
            )
            .unwrap();
        worker.cancel_load();

        let events = wait_until(
            &worker,
            |event| matches!(event, WorkerEvent::JobFailed { id, .. } if *id == job),
        );

        assert!(!events
            .iter()
            .any(|event| matches!(event, WorkerEvent::JobCompleted { id, .. } if *id == job)));
    }

    #[test]
    fn cancelling_an_active_engine_suppresses_job_completion() {
        let (mut worker, _, _, _) = worker(&[], None);
        worker.load("ready".into()).unwrap();
        wait_ready(&worker, "ready");
        let job = worker
            .enqueue(
                vec![2.0],
                16_000,
                job_config(Settings::default(), RecordingIntent::Raw),
            )
            .unwrap();
        wait_until(
            &worker,
            |event| matches!(event, WorkerEvent::JobStarted { id } if *id == job),
        );
        worker.cancel_load();

        let events = wait_until(
            &worker,
            |event| matches!(event, WorkerEvent::JobFailed { id, .. } if *id == job),
        );

        assert!(!events
            .iter()
            .any(|event| matches!(event, WorkerEvent::JobCompleted { id, .. } if *id == job)));
    }

    #[test]
    fn discard_suppresses_a_completion_already_waiting_in_the_event_queue() {
        let (mut worker, _, _, _) = worker(&[], None);
        worker.load("ready".into()).unwrap();
        wait_ready(&worker, "ready");
        let job = worker
            .enqueue(
                vec![2.0],
                16_000,
                job_config(Settings::default(), RecordingIntent::Raw),
            )
            .unwrap();
        thread::sleep(Duration::from_millis(150));
        worker.shutdown(ShutdownMode::Discard).unwrap();

        let events = wait_until(&worker, |event| {
            matches!(event, WorkerEvent::ShutdownComplete)
        });

        assert!(events
            .iter()
            .any(|event| matches!(event, WorkerEvent::JobDiscarded { id } if *id == job)));
        assert!(!events
            .iter()
            .any(|event| matches!(event, WorkerEvent::JobCompleted { id, .. } if *id == job)));
    }

    #[test]
    fn discard_shutdown_suppresses_results_and_skips_queued_jobs() {
        let (mut worker, _, _, _) = worker(&[], None);
        worker.load("ready".into()).unwrap();
        wait_ready(&worker, "ready");
        let settings = Settings::default();
        let first = worker
            .enqueue(
                vec![1.0],
                16_000,
                job_config(settings.clone(), RecordingIntent::Raw),
            )
            .unwrap();
        let second = worker
            .enqueue(
                vec![2.0],
                16_000,
                job_config(settings, RecordingIntent::Raw),
            )
            .unwrap();
        wait_until(
            &worker,
            |event| matches!(event, WorkerEvent::JobStarted { id } if *id == first),
        );
        worker.shutdown(ShutdownMode::Discard).unwrap();

        let events = wait_until(&worker, |event| {
            matches!(event, WorkerEvent::ShutdownComplete)
        });
        let discarded = events
            .iter()
            .filter_map(|event| match event {
                WorkerEvent::JobDiscarded { id } => Some(*id),
                _ => None,
            })
            .collect::<Vec<_>>();

        assert!(discarded.contains(&first));
        assert!(discarded.contains(&second));
        assert!(!events
            .iter()
            .any(|event| matches!(event, WorkerEvent::JobCompleted { .. })));
    }

    #[test]
    fn long_form_windows_cover_the_audio_and_cut_at_quiet_points() {
        let rate = 16_000;
        // 70 s of loud audio with a silent gap at 28.5 s (inside the first
        // window's search span) and another at 57 s.
        let mut samples = vec![0.5f32; 70 * rate];
        for sample in &mut samples[28 * rate + 8_000..28 * rate + 12_000] {
            *sample = 0.0;
        }
        for sample in &mut samples[57 * rate..57 * rate + 4_000] {
            *sample = 0.0;
        }
        let windows = plan_long_form_windows(&samples, rate as u32);
        assert_eq!(windows.first().unwrap().start, 0);
        assert_eq!(windows.last().unwrap().end, samples.len());
        for pair in windows.windows(2) {
            assert_eq!(pair[0].end, pair[1].start);
        }
        assert!(windows.iter().all(|w| w.len() <= 30 * rate));
        let first_cut = windows[0].end;
        assert!((28 * rate + 8_000..28 * rate + 12_000).contains(&first_cut));
    }

    #[test]
    fn short_audio_is_a_single_long_form_window() {
        assert_eq!(
            plan_long_form_windows(&vec![0.1; 16_000 * 10], 16_000).len(),
            1
        );
        assert!(plan_long_form_windows(&[], 16_000).is_empty());
    }

    #[test]
    fn local_text_model_is_released_after_transcripts_only() {
        let mut settings = Settings::default();
        settings.enhance.mode = crate::settings::EnhanceMode::Always;

        assert!(releases_local_model(&JobConfig::long_form(
            settings.clone()
        )));
        assert!(releases_local_model(&JobConfig::rerun(
            settings.clone(),
            Rerun::Clean { raw: "text".into() }
        )));
        let part = |last| SessionPart {
            id: 1,
            offset_samples: 0,
            last,
        };
        assert!(!releases_local_model(&JobConfig::meeting_part(
            settings.clone(),
            part(false),
            false
        )));
        assert!(releases_local_model(&JobConfig::meeting_part(
            settings.clone(),
            part(true),
            false
        )));
        assert!(!releases_local_model(&JobConfig::new(
            settings.clone(),
            RecordingIntent::Enhance,
            None,
            false
        )));

        let mut off = settings.clone();
        off.enhance.mode = crate::settings::EnhanceMode::Never;
        assert!(!releases_local_model(&JobConfig::long_form(off.clone())));
        assert!(releases_local_model(&JobConfig::meeting_notes(off)));
    }
}
