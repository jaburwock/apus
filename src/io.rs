//! The domain-blind scheduler for blocking file I/O.
//!
//! Viewers share an application-wide I/O worker, and each local-area BAM summary source owns a
//! dedicated one. Callers submit closures through `IoWorkerHandle`, receive typed `JobHandle`s,
//! and choose whether work is foreground, FIFO, or tied to a viewport generation.
//!
//! Queue collapse is local to a generation batch: older viewport work is removed, newer work
//! evicts only the same batch, and equal generations append so one viewport can request several
//! windows. Foreground work can pass queued ordinary work, but a running closure is never
//! pre-empted.
//!
//! Dropping a handle prevents queued work from starting when cancellation is observed. Running
//! closures can opt in to polling that signal. Do not add a domain request enum or a default
//! scheduling policy here: both would move ownership decisions out of the caller.

use std::cmp::Ordering;
use std::collections::VecDeque;
use std::sync::mpsc;

use crate::jobs::{Cancellation, Generation, JobHandle, Worker, WorkerHandle, WorkerHealth};

struct IoWorker;

/// The queueing policy an I/O task chooses explicitly.
#[derive(Clone)]
pub(crate) enum IoSchedule {
    /// A visible load that should run before ordinary queued work.
    Foreground,
    /// Per-file or per-command work with no viewport lifetime.
    Fifo,
    /// Work made obsolete by a newer viewport from the same batch.
    Viewport(Generation),
}

impl IoSchedule {
    fn viewport_generation(&self) -> Option<&Generation> {
        match self {
            Self::Viewport(generation) => Some(generation),
            Self::Foreground | Self::Fifo => None,
        }
    }

    fn is_current(&self) -> bool {
        self.viewport_generation().is_none_or(Generation::is_current)
    }
}

/// Domain-owned work made queueable without exposing its request or result type.
struct QueuedTask {
    schedule: IoSchedule,
    cancellation: Cancellation,
    run: Box<dyn FnOnce() + Send>,
}

impl QueuedTask {
    fn is_runnable(&self) -> bool {
        !self.cancellation.is_cancelled() && self.schedule.is_current()
    }

    fn run(self) {
        if self.is_runnable() {
            (self.run)();
        }
    }
}

#[derive(Clone)]
pub struct IoWorkerHandle {
    worker: WorkerHandle<IoWorker>,
}

impl IoWorkerHandle {
    pub fn spawn() -> Self {
        Self {
            worker: WorkerHandle::spawn(IoWorker),
        }
    }

    pub fn health(&self) -> WorkerHealth {
        self.worker.health()
    }

    /// Queues domain-owned blocking work under an explicit scheduling policy.
    ///
    /// Dropping the returned handle prevents this task from starting while it is
    /// queued. A closure already running is allowed to finish.
    pub(crate) fn submit<T, F>(&self, schedule: IoSchedule, work: F) -> JobHandle<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        self.submit_cancellable(schedule, move |_| work())
    }

    /// Queues work that can stop between I/O operations when its handle is dropped.
    pub(crate) fn submit_cancellable<T, F>(&self, schedule: IoSchedule, work: F) -> JobHandle<T>
    where
        T: Send + 'static,
        F: FnOnce(&Cancellation) -> T + Send + 'static,
    {
        let (reply, handle, cancellation) = JobHandle::cancellable_channel(schedule.viewport_generation().cloned());
        let reply_schedule = schedule.clone();
        let reply_cancellation = cancellation.clone();
        let task = QueuedTask {
            schedule,
            cancellation,
            run: Box::new(move || {
                let result = work(&reply_cancellation);
                if !reply_cancellation.is_cancelled() && reply_schedule.is_current() {
                    let _ = reply.send(result);
                }
            }),
        };
        let _ = self.worker.send(task);
        handle
    }
}

impl Worker for IoWorker {
    type Request = QueuedTask;

    fn run(self, receiver: mpsc::Receiver<Self::Request>) {
        let mut pending = VecDeque::new();
        loop {
            if let Some(task) = take_next(&mut pending) {
                task.run();
                continue;
            }

            let Ok(task) = receiver.recv() else { break };
            enqueue(task, &mut pending);
            while let Ok(task) = receiver.try_recv() {
                enqueue(task, &mut pending);
            }
        }
    }
}

/// Queues a task, dropping viewport work made obsolete by a newer view.
///
/// A batch is identified by the `Generation`'s identity, not its sequence value,
/// so one viewer's generations only ever displace its own queued work. Within a
/// batch: an older generation is dropped, a newer one evicts the whole batch,
/// and an equal one appends, since a single viewport needs several windows.
fn enqueue(task: QueuedTask, pending: &mut VecDeque<QueuedTask>) {
    if !task.is_runnable() {
        return;
    }
    let Some(generation) = task.schedule.viewport_generation() else {
        pending.push_back(task);
        return;
    };

    let newest = pending
        .iter()
        .filter_map(|queued| {
            queued
                .schedule
                .viewport_generation()
                .filter(|queued| generation.same_batch(queued))
        })
        .max_by(|a, b| a.cmp_within_batch(b));

    match newest.map(|newest| generation.cmp_within_batch(newest)) {
        Some(Ordering::Less) => return,
        Some(Ordering::Greater) => pending.retain(|queued| match queued.schedule.viewport_generation() {
            Some(queued_generation) => !generation.same_batch(queued_generation),
            None => true,
        }),
        Some(Ordering::Equal) | None => {}
    }
    pending.push_back(task);
}

/// Takes the next task, letting foreground work jump queued FIFO work.
fn take_next(pending: &mut VecDeque<QueuedTask>) -> Option<QueuedTask> {
    pending.retain(QueuedTask::is_runnable);
    let foreground = pending
        .iter()
        .position(|task| matches!(task.schedule, IoSchedule::Foreground));
    foreground
        .and_then(|index| pending.remove(index))
        .or_else(|| pending.pop_front())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::JobPoll;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    fn task(schedule: IoSchedule) -> QueuedTask {
        QueuedTask {
            schedule,
            cancellation: Cancellation::new(),
            run: Box::new(|| {}),
        }
    }

    #[test]
    fn foreground_work_jumps_fifo_work() {
        let mut pending = VecDeque::new();
        enqueue(task(IoSchedule::Fifo), &mut pending);
        enqueue(task(IoSchedule::Foreground), &mut pending);

        assert!(matches!(
            take_next(&mut pending).map(|task| task.schedule),
            Some(IoSchedule::Foreground)
        ));
        assert!(matches!(
            take_next(&mut pending).map(|task| task.schedule),
            Some(IoSchedule::Fifo)
        ));
    }

    #[test]
    fn newer_viewport_work_drops_older_work_in_its_own_batch() {
        let batch = crate::jobs::Batch::new();
        let first = batch.next();
        let mut pending = VecDeque::new();
        enqueue(task(IoSchedule::Viewport(first.clone())), &mut pending);
        enqueue(task(IoSchedule::Viewport(first.clone())), &mut pending);
        enqueue(task(IoSchedule::Fifo), &mut pending);
        assert_eq!(pending.len(), 3, "equal-generation requests stay queued together");
        enqueue(task(IoSchedule::Viewport(batch.next())), &mut pending);

        assert_eq!(pending.len(), 2);
        assert!(pending.iter().any(|task| matches!(task.schedule, IoSchedule::Fifo)));
        assert!(pending.iter().any(|task| {
            matches!(&task.schedule, IoSchedule::Viewport(generation) if generation.cmp_within_batch(&first) == std::cmp::Ordering::Greater)
        }));
    }

    #[test]
    fn one_viewers_generation_never_evicts_anothers_work() {
        let left = crate::jobs::Batch::new();
        let right = crate::jobs::Batch::new();
        let mut pending = VecDeque::new();
        let left_generation = left.next();
        enqueue(task(IoSchedule::Viewport(left_generation.clone())), &mut pending);
        enqueue(task(IoSchedule::Viewport(right.next())), &mut pending);
        enqueue(task(IoSchedule::Viewport(right.next())), &mut pending);

        assert_eq!(pending.len(), 2);
        assert!(pending.iter().any(|task| {
            matches!(&task.schedule, IoSchedule::Viewport(generation) if generation.same_batch(&left_generation))
        }));
    }

    #[test]
    fn dropping_a_queued_handle_prevents_its_task_starting() {
        let worker = IoWorkerHandle::spawn();
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let mut running = worker.submit(IoSchedule::Fifo, move || {
            started_tx.send(()).expect("test must observe the running task");
            release_rx.recv().expect("test must release the running task");
        });
        started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("the first task must start and block the worker");

        let starts = Arc::new(AtomicUsize::new(0));
        let cancelled_starts = Arc::clone(&starts);
        let cancelled = worker.submit(IoSchedule::Fifo, move || {
            cancelled_starts.fetch_add(1, Ordering::Relaxed);
        });
        drop(cancelled);
        release_tx.send(()).expect("the running task must still be releasable");

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match running.try_recv() {
                JobPoll::Ready(()) => break,
                JobPoll::Empty => assert!(Instant::now() < deadline, "the running task did not finish"),
                JobPoll::Stale | JobPoll::Disconnected => panic!("the running task must complete normally"),
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        std::thread::sleep(Duration::from_millis(10));

        assert_eq!(
            starts.load(Ordering::Relaxed),
            0,
            "a dropped queued task must never start"
        );
    }
}
