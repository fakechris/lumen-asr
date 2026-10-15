//! Serial queue for offline meeting processing.
//!
//! Every offline run (recording stop, file import, manual reprocess, crash
//! recovery) loads the diarization, ASR and LLM stacks. Running several at once
//! multiplies peak memory and makes each one slower, so they go through one
//! FIFO worker that processes a single meeting at a time.
//!
//! * **Dedupe** — a meeting already queued or running is not queued again.
//! * **Recording first** — while a meeting is recording the worker holds the
//!   next job back so model work does not compete with live capture. The gate
//!   is re-checked every [`GATE_POLL`] and given up after [`GATE_MAX_WAIT`], so
//!   a stale "recording" flag cannot park the queue forever.
//! * **Failure isolation** — a job that panics is reported to the caller's
//!   `on_panic` and the worker moves on to the next one.
//!
//! The queue is generic over the job payload so its semantics are unit-tested
//! with a fake runner; the desktop wiring lives in `meeting_cmd`.

use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use uuid::Uuid;

/// How often the worker re-checks the recording gate while holding a job back.
pub const GATE_POLL: Duration = Duration::from_secs(5);
/// Longest the worker holds a job back for a recording. Above the longest
/// meetings seen in practice (~7 h); past it the job runs anyway.
pub const GATE_MAX_WAIT: Duration = Duration::from_secs(8 * 60 * 60);

/// Result of [`ProcessingQueue::enqueue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Enqueued {
    /// Accepted; `ahead` jobs (including a running one) are before it.
    Queued { ahead: usize },
    /// The meeting is already waiting in the queue; the request is dropped.
    AlreadyQueued,
    /// The meeting is being processed right now; the request is dropped.
    AlreadyRunning,
}

struct State<J> {
    pending: VecDeque<(Uuid, J)>,
    running: Option<Uuid>,
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
            }),
            ready: Condvar::new(),
        }
    }
}

impl<J: Send + 'static> ProcessingQueue<J> {
    pub fn enqueue(&self, id: Uuid, job: J) -> Enqueued {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if state.running == Some(id) {
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
        state.running == Some(id) || state.pending.iter().any(|(queued, _)| *queued == id)
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

    fn finish(&self) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        state.running = None;
    }

    /// Start the single worker thread. `run` processes one job; a panic inside
    /// it is passed to `on_panic` and the worker continues with the next job.
    pub fn spawn_worker(
        self: &Arc<Self>,
        gate: impl Fn() -> bool + Send + 'static,
        run: impl Fn(Uuid, J) + Send + 'static,
        on_panic: impl Fn(Uuid) + Send + 'static,
    ) -> std::io::Result<()> {
        self.spawn_worker_with(gate, run, on_panic, GATE_POLL, GATE_MAX_WAIT)
    }

    fn spawn_worker_with(
        self: &Arc<Self>,
        gate: impl Fn() -> bool + Send + 'static,
        run: impl Fn(Uuid, J) + Send + 'static,
        on_panic: impl Fn(Uuid) + Send + 'static,
        poll: Duration,
        max_wait: Duration,
    ) -> std::io::Result<()> {
        let queue = Arc::clone(self);
        std::thread::Builder::new()
            .name("meeting-processing".into())
            .spawn(move || loop {
                let (id, job) = queue.take_next(&gate, poll, max_wait);
                if catch_unwind(AssertUnwindSafe(|| run(id, job))).is_err() {
                    tracing::warn!(meeting_id = %id, "meeting processing job panicked");
                    on_panic(id);
                }
                queue.finish();
            })
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
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
        queue.finish();
        assert!(!queue.is_pending(id(1)));
        assert_eq!(queue.enqueue(id(1), 0), Enqueued::Queued { ahead: 1 });
    }

    #[test]
    fn jobs_run_one_at_a_time_in_fifo_order_and_survive_a_panic() {
        let queue = Arc::new(ProcessingQueue::<u32>::default());
        let (done_tx, done_rx) = mpsc::channel();
        let (panic_tx, panic_rx) = mpsc::channel();
        let busy = Arc::new(AtomicBool::new(false));
        // Queue everything before the worker starts so ordering is deterministic.
        for n in 1..=4 {
            queue.enqueue(id(n), n as u32);
        }
        let busy_in_job = Arc::clone(&busy);
        queue
            .spawn_worker_with(
                || false,
                move |meeting, payload| {
                    assert!(!busy_in_job.swap(true, Ordering::SeqCst), "jobs overlapped");
                    std::thread::sleep(Duration::from_millis(5));
                    busy_in_job.store(false, Ordering::SeqCst);
                    if payload == 2 {
                        panic!("job 2 fails");
                    }
                    done_tx.send(meeting).unwrap();
                },
                move |meeting| panic_tx.send(meeting).unwrap(),
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
