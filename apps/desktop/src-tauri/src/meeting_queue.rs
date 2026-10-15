//! Serial queue for offline meeting processing.
//!
//! Every offline run (recording stop, file import, manual reprocess, crash and
//! startup recovery) loads the diarization, ASR and LLM stacks. Running several
//! at once multiplies peak memory and makes each one slower, so they go through
//! one FIFO worker that processes a single meeting at a time.
//!
//! * **Dedupe** — a meeting already queued or running is not queued again.
//! * **Recording first** — while a meeting is recording the worker holds the
//!   next job back so model work does not compete with live capture. The gate
//!   is re-checked every [`GATE_POLL`] and given up after [`GATE_MAX_WAIT`], so
//!   a stale "recording" flag cannot park the queue forever.
//! * **Failure isolation** — a job that panics is reported to the caller's
//!   `on_panic` and the worker moves on to the next one.
//! * **Hung-job guard** — each job runs on its own thread while the worker
//!   waits up to the job's budget ([`job_budget`]). A job past its budget is
//!   logged and left running detached (a pipeline thread cannot be killed
//!   safely); the worker moves on. A detached job still counts as running for
//!   dedupe, and its result is saved whenever it finishes.
//!
//! The queue is generic over the job payload so its semantics are unit-tested
//! with a fake runner; the desktop wiring lives in `meeting_cmd`.

use std::collections::{HashSet, VecDeque};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;

/// How often the worker re-checks the recording gate while holding a job back.
pub const GATE_POLL: Duration = Duration::from_secs(5);
/// Longest the worker holds a job back for a recording. Above the longest
/// meetings seen in practice (~7 h); past it the job runs anyway.
pub const GATE_MAX_WAIT: Duration = Duration::from_secs(8 * 60 * 60);
/// Floor of a job's budget before the worker stops waiting for it.
pub const JOB_BUDGET_FLOOR: Duration = Duration::from_secs(30 * 60);

/// How long the worker waits for one job before moving on: twice the audio
/// duration (processing normally takes ~15–20 % of it), never under
/// [`JOB_BUDGET_FLOOR`]. Unknown duration gets the floor.
pub fn job_budget(audio_seconds: Option<f64>) -> Duration {
    let scaled = audio_seconds
        .filter(|secs| secs.is_finite() && *secs > 0.0)
        .map(|secs| Duration::from_secs_f64(secs * 2.0))
        .unwrap_or(Duration::ZERO);
    scaled.max(JOB_BUDGET_FLOOR)
}

/// Result of [`ProcessingQueue::enqueue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    /// Accepted; `ahead` jobs (including a running one) are before it.
    Queued { ahead: usize },
    /// The meeting is already waiting in the queue; the request is dropped.
    AlreadyQueued,
    /// The meeting is being processed right now (possibly a detached job past
    /// its budget); the request is dropped.
    AlreadyRunning,
}

struct State<J> {
    pending: VecDeque<(Uuid, J)>,
    running: Option<Uuid>,
    /// Jobs past their budget that are still running on their own thread.
    detached: HashSet<Uuid>,
}

impl<J> State<J> {
    fn is_running(&self, id: Uuid) -> bool {
        self.running == Some(id) || self.detached.contains(&id)
    }
}

pub struct ProcessingQueue<J> {
    state: Mutex<State<J>>,
    ready: Condvar,
}

impl<J> Default for ProcessingQueue<J> {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                pending: VecDeque::new(),
                running: None,
                detached: HashSet::new(),
            }),
            ready: Condvar::new(),
        }
    }
}

type Runner<J> = Arc<dyn Fn(Uuid, J) + Send + Sync>;
type PanicHandler = Arc<dyn Fn(Uuid) + Send + Sync>;

impl<J: Send + 'static> ProcessingQueue<J> {
    pub fn enqueue(&self, id: Uuid, job: J) -> Enqueued {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.is_running(id) {
            return Enqueued::AlreadyRunning;
        }
        if state.pending.iter().any(|(queued, _)| *queued == id) {
            return Enqueued::AlreadyQueued;
        }
        let ahead = state.pending.len() + usize::from(state.running.is_some());
        state.pending.push_back((id, job));
        self.ready.notify_one();
        Enqueued::Queued { ahead }
    }

    /// Whether the meeting is waiting in the queue or running now.
    pub fn is_pending(&self, id: Uuid) -> bool {
        let state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.is_running(id) || state.pending.iter().any(|(queued, _)| *queued == id)
    }

    /// Block until a job is available, then hold it back while `gate` reports a
    /// recording (bounded by `max_wait`, re-checked every `poll`). Marks the job
    /// as running; pair with [`Self::finish`].
    fn take_next(&self, gate: &dyn Fn() -> bool, poll: Duration, max_wait: Duration) -> (Uuid, J) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        while state.pending.is_empty() {
            state = self.ready.wait(state).unwrap_or_else(|p| p.into_inner());
        }
        drop(state);

        let held_since = Instant::now();
        let mut logged = false;
        while gate() {
            if held_since.elapsed() >= max_wait {
                tracing::warn!(
                    waited_s = held_since.elapsed().as_secs(),
                    "meeting processing queue: recording gate still closed; running anyway"
                );
                break;
            }
            if !logged {
                tracing::info!("meeting processing queue: waiting for the recording to end");
                logged = true;
            }
            std::thread::sleep(poll);
        }

        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let (id, job) = state
            .pending
            .pop_front()
            .expect("only the worker removes pending jobs");
        state.running = Some(id);
        (id, job)
    }

    /// The job for `id` ended (normally, by panic, or after being detached).
    fn finish(&self, id: Uuid) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.running == Some(id) {
            state.running = None;
        }
        state.detached.remove(&id);
    }

    /// The worker stops waiting for `id`: it stays "running" for dedupe until
    /// its thread calls [`Self::finish`], but no longer blocks the queue.
    fn detach(&self, id: Uuid) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.running == Some(id) {
            state.running = None;
            state.detached.insert(id);
        }
    }

    /// Start the single worker thread. `run` processes one job; a panic inside
    /// it is passed to `on_panic`. `budget` bounds how long the worker waits for
    /// one job before moving on (see the module docs).
    pub fn spawn_worker(
        self: &Arc<Self>,
        gate: impl Fn() -> bool + Send + 'static,
        budget: impl Fn(Uuid) -> Duration + Send + 'static,
        run: impl Fn(Uuid, J) + Send + Sync + 'static,
        on_panic: impl Fn(Uuid) + Send + Sync + 'static,
    ) -> std::io::Result<()> {
        self.spawn_worker_with(gate, budget, run, on_panic, GATE_POLL, GATE_MAX_WAIT)
    }

    fn spawn_worker_with(
        self: &Arc<Self>,
        gate: impl Fn() -> bool + Send + 'static,
        budget: impl Fn(Uuid) -> Duration + Send + 'static,
        run: impl Fn(Uuid, J) + Send + Sync + 'static,
        on_panic: impl Fn(Uuid) + Send + Sync + 'static,
        poll: Duration,
        max_wait: Duration,
    ) -> std::io::Result<()> {
        let queue = Arc::clone(self);
        let run: Runner<J> = Arc::new(run);
        let on_panic: PanicHandler = Arc::new(on_panic);
        std::thread::Builder::new()
            .name("meeting-processing".into())
            .spawn(move || loop {
                let (id, job) = queue.take_next(&gate, poll, max_wait);
                let limit = budget(id);
                let (done_tx, done_rx) = mpsc::channel::<()>();
                let job_queue = Arc::clone(&queue);
                let job_run = Arc::clone(&run);
                let job_on_panic = Arc::clone(&on_panic);
                let body = move || {
                    if catch_unwind(AssertUnwindSafe(|| job_run(id, job))).is_err() {
                        tracing::warn!(meeting_id = %id, "meeting processing job panicked");
                        job_on_panic(id);
                    }
                    job_queue.finish(id);
                    let _ = done_tx.send(());
                };
                let spawned = std::thread::Builder::new()
                    .name("meeting-processing-job".into())
                    .spawn(body);
                if let Err(e) = spawned {
                    // The body (and its job) is gone with the failed spawn.
                    tracing::warn!(meeting_id = %id, error = %e, "could not start meeting processing job");
                    on_panic(id);
                    queue.finish(id);
                    continue;
                }
                match done_rx.recv_timeout(limit) {
                    Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        tracing::warn!(
                            meeting_id = %id,
                            budget_s = limit.as_secs(),
                            "meeting processing job exceeded its budget; leaving it running and moving on"
                        );
                        queue.detach(id);
                    }
                }
            })
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    const GENEROUS: Duration = Duration::from_secs(60);

    #[test]
    fn budget_is_twice_the_audio_with_a_floor() {
        assert_eq!(job_budget(None), JOB_BUDGET_FLOOR);
        assert_eq!(job_budget(Some(60.0)), JOB_BUDGET_FLOOR);
        assert_eq!(job_budget(Some(f64::NAN)), JOB_BUDGET_FLOOR);
        assert_eq!(
            job_budget(Some(2.0 * 3600.0)),
            Duration::from_secs(4 * 3600)
        );
    }

    #[test]
    fn duplicates_of_a_queued_meeting_are_dropped() {
        let queue = ProcessingQueue::<u32>::default();
        assert_eq!(queue.enqueue(id(1), 0), Enqueued::Queued { ahead: 0 });
        assert_eq!(queue.enqueue(id(2), 0), Enqueued::Queued { ahead: 1 });
        assert_eq!(queue.enqueue(id(1), 9), Enqueued::AlreadyQueued);
    }

    #[test]
    fn a_running_meeting_is_not_queued_again() {
        let queue = ProcessingQueue::<u32>::default();
        queue.enqueue(id(1), 0);
        let (taken, _) = queue.take_next(&|| false, Duration::ZERO, Duration::ZERO);
        assert_eq!(taken, id(1));
        assert_eq!(queue.enqueue(id(1), 0), Enqueued::AlreadyRunning);
        assert_eq!(queue.enqueue(id(2), 0), Enqueued::Queued { ahead: 1 });
        assert!(queue.is_pending(id(1)) && queue.is_pending(id(2)));
        queue.finish(id(1));
        assert!(!queue.is_pending(id(1)));
        assert_eq!(queue.enqueue(id(1), 0), Enqueued::Queued { ahead: 1 });
    }

    #[test]
    fn a_detached_meeting_stays_deduped_until_it_finishes() {
        let queue = ProcessingQueue::<u32>::default();
        queue.enqueue(id(1), 0);
        queue.take_next(&|| false, Duration::ZERO, Duration::ZERO);
        queue.detach(id(1));
        assert_eq!(queue.enqueue(id(1), 0), Enqueued::AlreadyRunning);
        assert!(queue.is_pending(id(1)));
        // The worker is free: the next job does not count the detached one.
        assert_eq!(queue.enqueue(id(2), 0), Enqueued::Queued { ahead: 0 });
        queue.finish(id(1));
        assert!(!queue.is_pending(id(1)));
    }

    #[test]
    fn jobs_run_one_at_a_time_in_fifo_order_and_survive_a_panic() {
        let queue = Arc::new(ProcessingQueue::<u32>::default());
        let (done_tx, done_rx) = mpsc::channel();
        let done_tx = Mutex::new(done_tx);
        let (panic_tx, panic_rx) = mpsc::channel();
        let panic_tx = Mutex::new(panic_tx);
        let busy = Arc::new(AtomicBool::new(false));
        // Queue everything before the worker starts so ordering is deterministic.
        for n in 1..=4 {
            queue.enqueue(id(n), n as u32);
        }
        let busy_in_job = Arc::clone(&busy);
        queue
            .spawn_worker_with(
                || false,
                |_| GENEROUS,
                move |meeting, payload| {
                    assert!(!busy_in_job.swap(true, Ordering::SeqCst), "jobs overlapped");
                    std::thread::sleep(Duration::from_millis(5));
                    busy_in_job.store(false, Ordering::SeqCst);
                    if payload == 2 {
                        panic!("job 2 fails");
                    }
                    done_tx.lock().unwrap().send(meeting).unwrap();
                },
                move |meeting| panic_tx.lock().unwrap().send(meeting).unwrap(),
                Duration::ZERO,
                Duration::ZERO,
            )
            .unwrap();
        let wait = Duration::from_secs(5);
        let done: Vec<Uuid> = (0..3)
            .map(|_| done_rx.recv_timeout(wait).unwrap())
            .collect();
        assert_eq!(done, vec![id(1), id(3), id(4)]);
        assert_eq!(panic_rx.recv_timeout(wait).unwrap(), id(2));
    }

    #[test]
    fn a_job_past_its_budget_does_not_block_the_next_one() {
        let queue = Arc::new(ProcessingQueue::<u32>::default());
        let (release_tx, release_rx) = mpsc::channel::<()>();
        let release_rx = Mutex::new(release_rx);
        let (done_tx, done_rx) = mpsc::channel();
        let done_tx = Mutex::new(done_tx);
        queue.enqueue(id(1), 1);
        queue.enqueue(id(2), 2);
        queue
            .spawn_worker_with(
                || false,
                |meeting| {
                    if meeting == Uuid::from_u128(1) {
                        Duration::from_millis(20)
                    } else {
                        GENEROUS
                    }
                },
                move |meeting, payload| {
                    if payload == 1 {
                        // Hangs until the test releases it.
                        release_rx.lock().unwrap().recv().unwrap();
                    }
                    done_tx.lock().unwrap().send(meeting).unwrap();
                },
                |_| {},
                Duration::ZERO,
                Duration::ZERO,
            )
            .unwrap();
        let wait = Duration::from_secs(5);
        // Job 2 completes while job 1 is still hung.
        assert_eq!(done_rx.recv_timeout(wait).unwrap(), id(2));
        assert_eq!(queue.enqueue(id(1), 1), Enqueued::AlreadyRunning);
        // When the hung job eventually finishes, its result still lands.
        release_tx.send(()).unwrap();
        assert_eq!(done_rx.recv_timeout(wait).unwrap(), id(1));
        let deadline = Instant::now() + wait;
        while queue.is_pending(id(1)) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!queue.is_pending(id(1)));
    }

    #[test]
    fn a_recording_holds_the_next_job_until_it_ends() {
        let queue = ProcessingQueue::<u32>::default();
        queue.enqueue(id(1), 0);
        let checks = std::cell::Cell::new(0);
        // Gate reports "recording" for the first three checks.
        let gate = || {
            checks.set(checks.get() + 1);
            checks.get() <= 3
        };
        let (taken, _) = queue.take_next(&gate, Duration::ZERO, Duration::from_secs(60));
        assert_eq!(taken, id(1));
        assert_eq!(checks.get(), 4);
    }

    #[test]
    fn a_stuck_recording_gate_is_given_up_after_the_cap() {
        let queue = ProcessingQueue::<u32>::default();
        queue.enqueue(id(1), 0);
        let (taken, _) = queue.take_next(
            &|| true,
            Duration::from_millis(1),
            Duration::from_millis(20),
        );
        assert_eq!(taken, id(1));
    }
}
