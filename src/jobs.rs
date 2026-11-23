//! Generic typed worker and reply plumbing for blocking application work.
//!
//! This module owns the runtime contract shared by dedicated workers: one concrete request type
//! per worker thread, typed sends, non-blocking reply polling, terminal health, and request-local
//! cancellation. It deliberately contains no domain requests, viewer state, or scheduling policy;
//! those belong to the worker's owning module.
//!
//! `Batch` and `Generation` provide latest-wins control for viewport work while preserving batch
//! identity, so generations from separate consumers cannot evict one another. A settled reply
//! remains deliverable after supersession: staleness means "stop waiting", not "discard data
//! already produced".
//!
//! Consumers must distinguish `Empty`, `Ready`, `Stale`, and `Disconnected`. Disconnection is
//! terminal and must not become a permanent loading state, while stale work is control flow rather
//! than an I/O failure. Dropping a cancellable handle can abandon queued work, but this boundary
//! never interrupts a closure that has already started.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;

/// A concrete worker's thread contract.
pub(crate) trait Worker: Send + 'static {
    type Request: Send + 'static;

    /// Runs until the sender is dropped or the worker stops.
    fn run(self, receiver: mpsc::Receiver<Self::Request>);
}

/// Health shared with the shell without exposing a concrete worker type.
#[derive(Clone)]
pub struct WorkerHealth {
    alive: Arc<AtomicBool>,
}

impl WorkerHealth {
    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::Relaxed)
    }

    /// A health handle already reporting death, for testing what the shell does about it.
    /// Killing a real worker would race the thread's own shutdown.
    #[cfg(test)]
    pub(crate) fn dead_for_test() -> Self {
        Self {
            alive: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// A typed request sender for one dedicated worker thread.
pub(crate) struct WorkerHandle<W: Worker> {
    sender: mpsc::Sender<W::Request>,
    alive: Arc<AtomicBool>,
}

impl<W: Worker> Clone for WorkerHandle<W> {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
            alive: Arc::clone(&self.alive),
        }
    }
}

impl<W: Worker> WorkerHandle<W> {
    /// Starts one worker thread.
    pub(crate) fn spawn(worker: W) -> Self {
        let (sender, receiver) = mpsc::channel();
        let alive = Arc::new(AtomicBool::new(true));
        let thread_alive = Arc::clone(&alive);
        std::thread::spawn(move || {
            let _guard = AliveGuard(thread_alive);
            worker.run(receiver);
        });
        Self { sender, alive }
    }

    pub(crate) fn health(&self) -> WorkerHealth {
        WorkerHealth {
            alive: Arc::clone(&self.alive),
        }
    }

    #[cfg(test)]
    pub(crate) fn is_alive(&self) -> bool {
        self.health().is_alive()
    }

    /// Sends a typed request and latches health as dead on failure.
    pub(crate) fn send(&self, request: W::Request) -> Result<(), mpsc::SendError<W::Request>> {
        match self.sender.send(request) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.alive.store(false, Ordering::Relaxed);
                Err(error)
            }
        }
    }
}

struct AliveGuard(Arc<AtomicBool>);

impl Drop for AliveGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

/// Shared sequence state for one source of viewport work.
pub(crate) struct Batch {
    latest: Arc<AtomicU64>,
}

impl Batch {
    pub(crate) fn new() -> Self {
        Self {
            latest: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Starts one request batch and returns the token every request in it shares.
    ///
    /// `Relaxed` suffices: the counter publishes nothing but itself, so there is
    /// no earlier write for a `Release` to order against.
    pub(crate) fn next(&self) -> Generation {
        let value = self.latest.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        Generation {
            batch: Arc::clone(&self.latest),
            value,
        }
    }
}

/// An opaque viewport generation tied to one [`Batch`].
#[derive(Clone)]
pub(crate) struct Generation {
    batch: Arc<AtomicU64>,
    value: u64,
}

impl Generation {
    pub(crate) fn is_current(&self) -> bool {
        self.batch.load(Ordering::Relaxed) == self.value
    }

    pub(crate) fn same_batch(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.batch, &other.batch)
    }

    /// Orders two generations. Meaningful only within one batch, so callers must
    /// establish [`Self::same_batch`] first — sequence numbers from different
    /// batches are unrelated counters.
    pub(crate) fn cmp_within_batch(&self, other: &Self) -> std::cmp::Ordering {
        debug_assert!(self.same_batch(other), "generations compare only within one batch");
        self.value.cmp(&other.value)
    }
}

/// The non-blocking state of a typed reply channel.
pub(crate) enum JobPoll<T> {
    Empty,
    Ready(T),
    Stale,
    Disconnected,
}

/// A request-local signal used to abandon work whose receiver went away.
///
/// Running work must opt in to polling this signal. Ordinary reply channels do not carry one.
#[derive(Clone)]
pub(crate) struct Cancellation {
    cancelled: Arc<AtomicBool>,
}

impl Cancellation {
    pub(crate) fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

/// A pending or finished worker request's reply channel.
pub(crate) struct JobHandle<T> {
    receiver: mpsc::Receiver<T>,
    generation: Option<Generation>,
    cancellation: Option<Cancellation>,
}

impl<T> JobHandle<T> {
    /// Tests of in-flight state use this with a sender they never fire: the handle stays `Empty`
    /// for good, and dropping the sender exercises `Disconnected`. Never assert that work on the
    /// real worker is still outstanding, since it can finish before the assertion runs.
    pub(crate) fn channel() -> (mpsc::Sender<T>, Self) {
        Self::channel_with_generation(None, None)
    }

    #[cfg(test)]
    pub(crate) fn channel_for(generation: Generation) -> (mpsc::Sender<T>, Self) {
        Self::channel_with_generation(Some(generation), None)
    }

    /// Creates a reply channel whose handle abandons queued work when dropped.
    ///
    /// The worker receives the paired [`Cancellation`] and decides where it can
    /// safely stop. The I/O scheduler checks before starting a task; it never
    /// interrupts a closure already executing.
    pub(crate) fn cancellable_channel(generation: Option<Generation>) -> (mpsc::Sender<T>, Self, Cancellation) {
        let cancellation = Cancellation::new();
        let (sender, handle) = Self::channel_with_generation(generation, Some(cancellation.clone()));
        (sender, handle, cancellation)
    }

    fn channel_with_generation(
        generation: Option<Generation>,
        cancellation: Option<Cancellation>,
    ) -> (mpsc::Sender<T>, Self) {
        let (sender, receiver) = mpsc::channel();
        (
            sender,
            Self {
                receiver,
                generation,
                cancellation,
            },
        )
    }

    /// Polls the reply, reporting [`JobPoll::Stale`] once a newer generation has
    /// superseded this request.
    ///
    /// A value that has *already arrived* is always delivered, stale or not: the
    /// work is paid for and a chunk of rows does not expire. Staleness only
    /// decides whether to keep *waiting* — which is what makes a scroll past a
    /// completed prefetch keep its rows instead of blanking back to a loading
    /// line. Superseding work that has not landed is the queue's job
    /// (`io::enqueue`), not this one's.
    pub(crate) fn try_recv(&mut self) -> JobPoll<T> {
        match self.receiver.try_recv() {
            Ok(value) => JobPoll::Ready(value),
            Err(_) if self.is_stale() => JobPoll::Stale,
            Err(mpsc::TryRecvError::Empty) => JobPoll::Empty,
            Err(mpsc::TryRecvError::Disconnected) => JobPoll::Disconnected,
        }
    }

    fn is_stale(&self) -> bool {
        self.generation
            .as_ref()
            .is_some_and(|generation| !generation.is_current())
    }

    /// Whether this request still belongs to the newest batch. An unbatched
    /// handle is always current.
    #[cfg(test)]
    pub(crate) fn is_current(&self) -> bool {
        !self.is_stale()
    }
}

impl<T> Drop for JobHandle<T> {
    fn drop(&mut self) {
        if let Some(cancellation) = &self.cancellation {
            cancellation.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    enum Request {
        Add(u32, mpsc::Sender<u32>),
        Stop,
    }

    struct EchoWorker;

    impl Worker for EchoWorker {
        type Request = Request;

        fn run(self, receiver: mpsc::Receiver<Self::Request>) {
            while let Ok(request) = receiver.recv() {
                match request {
                    Request::Add(value, reply) => {
                        let _ = reply.send(value + 1);
                    }
                    Request::Stop => break,
                }
            }
        }
    }

    fn settle<T>(handle: &mut JobHandle<T>) -> T {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            match handle.try_recv() {
                JobPoll::Ready(value) => return value,
                JobPoll::Stale => panic!("worker reply became stale"),
                JobPoll::Disconnected => panic!("worker reply disconnected"),
                JobPoll::Empty => {
                    assert!(std::time::Instant::now() < deadline, "worker did not finish in time");
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }

    #[test]
    fn typed_worker_runs_a_request_and_returns_its_reply() {
        let worker = WorkerHandle::spawn(EchoWorker);
        let (reply, mut handle) = JobHandle::channel();
        worker
            .send(Request::Add(41, reply))
            .expect("worker must accept a request");

        assert_eq!(settle(&mut handle), 42);
        assert!(worker.is_alive());
    }

    #[test]
    fn a_stopped_worker_reports_disconnection_instead_of_pending_forever() {
        let worker = WorkerHandle::spawn(EchoWorker);
        worker.send(Request::Stop).expect("worker must accept stop");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while worker.is_alive() {
            assert!(std::time::Instant::now() < deadline, "worker never stopped");
            std::thread::sleep(Duration::from_millis(2));
        }

        let (reply, mut handle) = JobHandle::channel();
        assert!(worker.send(Request::Add(1, reply)).is_err());
        assert!(matches!(handle.try_recv(), JobPoll::Disconnected));
    }

    #[test]
    fn reply_polling_distinguishes_empty_disconnected_and_unbatched_handles() {
        let (reply, mut handle) = JobHandle::<u32>::channel();
        assert!(matches!(handle.try_recv(), JobPoll::Empty));
        drop(reply);
        assert!(matches!(handle.try_recv(), JobPoll::Disconnected));
    }

    #[test]
    fn only_a_batchs_newest_generation_is_current_and_they_order_by_age() {
        let batch = Batch::new();
        let mut issued: Vec<Generation> = Vec::new();
        for _ in 0..4 {
            let generation = batch.next();
            assert!(generation.is_current(), "the generation just issued is the current one");
            assert!(
                issued.iter().all(|older| !older.is_current()),
                "issuing a generation supersedes every earlier one"
            );
            assert!(
                issued
                    .iter()
                    .all(|older| older.cmp_within_batch(&generation) == std::cmp::Ordering::Less),
                "a later generation must order after every earlier one"
            );
            issued.push(generation);
        }
    }

    #[test]
    fn generations_from_separate_batches_are_never_the_same_batch() {
        let (left, right) = (Batch::new(), Batch::new());
        // Same sequence number, unrelated counters: identity, not value, decides.
        assert!(!left.next().same_batch(&right.next()));
    }

    #[test]
    fn a_reply_that_already_arrived_is_delivered_even_once_superseded() {
        // The work is paid for and its value has not expired. Discarding it here is
        // what blanked a completed VCF prefetch back to a loading line on scroll.
        let batch = Batch::new();
        let (reply, mut handle) = JobHandle::channel_for(batch.next());
        reply.send(42).expect("the test reply must be accepted");

        let _newer = batch.next();

        assert!(matches!(handle.try_recv(), JobPoll::Ready(42)));
    }

    #[test]
    fn a_superseded_request_still_waiting_reports_stale_rather_than_empty() {
        let batch = Batch::new();
        let (_reply, mut handle) = JobHandle::<u32>::channel_for(batch.next());
        assert!(matches!(handle.try_recv(), JobPoll::Empty));

        let _newer = batch.next();

        assert!(matches!(handle.try_recv(), JobPoll::Stale));
    }

    #[test]
    fn dropping_a_cancellable_handle_marks_its_queued_work_cancelled() {
        let (_reply, handle, cancellation) = JobHandle::<u32>::cancellable_channel(None);
        assert!(!cancellation.is_cancelled());

        drop(handle);

        assert!(cancellation.is_cancelled());
    }
}
