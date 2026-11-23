//! Persistent worker for opening and querying one indexed FASTA.
//!
//! The handle gives the reference service typed, asynchronous `Open` and `Fetch` requests while
//! this thread owns the mutable indexed reader. Keeping that reader alive between requests avoids
//! reopening the FASTA for every cache miss and serializes access to noodles' reader.
//!
//! Each request carries the reference generation that created it. An `Open` replaces the reader
//! and advances the worker's accepted generation, while a `Fetch` whose generation differs from
//! the accepted one is answered as interrupted instead of being run against the wrong reference.
//! The worker therefore owns reader lifetime and generation fencing; `ReferenceData` owns cache
//! state and decides how those replies affect the current frame.

use std::io;
use std::path::PathBuf;
use std::sync::mpsc;

use crate::basepair::BasePair;
use crate::jobs::{JobHandle, Worker, WorkerHandle};

use super::RefSequence;

struct ReferenceWorker;

enum ReferenceRequest {
    Open {
        generation: u64,
        path: PathBuf,
        reply: mpsc::Sender<io::Result<()>>,
    },
    Fetch {
        generation: u64,
        contig: String,
        start: BasePair,
        len: BasePair,
        reply: mpsc::Sender<io::Result<Vec<u8>>>,
    },
}

#[derive(Clone)]
pub struct ReferenceWorkerHandle {
    worker: WorkerHandle<ReferenceWorker>,
}

impl ReferenceWorkerHandle {
    pub fn spawn() -> Self {
        Self {
            worker: WorkerHandle::spawn(ReferenceWorker),
        }
    }

    pub(crate) fn open(&self, generation: u64, path: PathBuf) -> JobHandle<io::Result<()>> {
        let (reply, handle) = JobHandle::channel();
        let _ = self.worker.send(ReferenceRequest::Open {
            generation,
            path,
            reply,
        });
        handle
    }

    pub(crate) fn fetch(
        &self,
        generation: u64,
        contig: String,
        start: BasePair,
        len: BasePair,
    ) -> JobHandle<io::Result<Vec<u8>>> {
        let (reply, handle) = JobHandle::channel();
        let _ = self.worker.send(ReferenceRequest::Fetch {
            generation,
            contig,
            start,
            len,
            reply,
        });
        handle
    }
}

impl Worker for ReferenceWorker {
    type Request = ReferenceRequest;

    fn run(self, receiver: mpsc::Receiver<Self::Request>) {
        let mut reader: Option<RefSequence> = None;
        let mut generation = 0;
        while let Ok(request) = receiver.recv() {
            match request {
                ReferenceRequest::Open {
                    generation: requested_generation,
                    path,
                    reply,
                } => match RefSequence::from_fa_path(path) {
                    Ok(sequence) => {
                        generation = requested_generation;
                        reader = Some(sequence);
                        let _ = reply.send(Ok(()));
                    }
                    Err(error) => {
                        reader = None;
                        generation = requested_generation;
                        let _ = reply.send(Err(error));
                    }
                },
                ReferenceRequest::Fetch {
                    generation: requested_generation,
                    contig,
                    start,
                    len,
                    reply,
                } => {
                    let result = if requested_generation != generation {
                        Err(io::Error::new(io::ErrorKind::Interrupted, "stale reference request"))
                    } else {
                        reader.as_mut().map_or_else(
                            || Err(io::Error::new(io::ErrorKind::NotConnected, "no reference loaded")),
                            |sequence| sequence.fetch(&contig, start, len),
                        )
                    };
                    let _ = reply.send(result);
                }
            }
        }
    }
}
