//! The logger actor receives log messages and writes them to a log file.

use super::{ReducerId, WorkerId};
use error;
use git2;
use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::path;
use std::sync::mpsc;
use std::thread;

/// The different kinds of log messages that can be sent to the logger actor.
#[derive(Debug)]
enum LoggerMessage {
    SpawningWorker(WorkerId),
    SpawnedWorker(WorkerId),
    SpawningReducer(ReducerId),
    SpawnedReducer(ReducerId),
    ShutdownWorker(WorkerId),
    ShutdownReducer(ReducerId),
    WorkerPanicked(WorkerId, Box<Any + Send + 'static>),
    WorkerErrored(WorkerId, error::Error),
    ReducerPanicked(ReducerId, Box<Any + Send + 'static>),
    ReducerErrored(ReducerId, error::Error),
    BackingUpTestCase(String, String),
    StartJudgingInteresting(WorkerId),
    JudgedInteresting(WorkerId, u64),
    JudgedNotInteresting(WorkerId, String),
    NewSmallest(u64, u64, String),
    IsNotSmaller(String),
    StartGeneratingNextReduction(ReducerId),
    FinishGeneratingNextReduction(ReducerId),
    NoMoreReductions(ReducerId),
    FinalReducedSize(u64, u64),
    TryMerge(WorkerId, git2::Oid, git2::Oid),
    FinishedMerging(WorkerId, u64, u64),
}

impl fmt::Display for LoggerMessage {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            LoggerMessage::SpawningWorker(id) => write!(f, "Supervisor: Spawning worker {}", id),
            LoggerMessage::SpawnedWorker(id) => write!(f, "Worker {}: spawned", id),
            LoggerMessage::SpawningReducer(id) => write!(f, "Supervisor: Spawning reducer {}", id),
            LoggerMessage::SpawnedReducer(id) => write!(f, "Reducer {}: spawned", id),
            LoggerMessage::ShutdownWorker(id) => write!(f, "Worker {}: shutting down", id),
            LoggerMessage::ShutdownReducer(id) => write!(f, "Reducer {}: shutting down", id),
            LoggerMessage::WorkerErrored(id, ref err) => write!(f, "Worker {}: error: {}", id, err),
            LoggerMessage::WorkerPanicked(id, _) => write!(f, "Worker {}: panicked!", id),
            LoggerMessage::ReducerErrored(id, ref err) => {
                write!(f, "Reducer {}: error: {}", id, err)
            }
            LoggerMessage::ReducerPanicked(id, _) => write!(f, "Reducer {}: panicked!", id),
            LoggerMessage::BackingUpTestCase(ref from, ref to) => {
                write!(
                    f,
                    "Supervisor: backing up initial test case from {} to {}",
                    from,
                    to
                )
            }
            LoggerMessage::StartJudgingInteresting(id) => {
                write!(
                    f,
                    "Worker {}: judging a test case's interesting-ness...",
                    id
                )
            }
            LoggerMessage::JudgedInteresting(id, size) => {
                write!(
                    f,
                    "Worker {}: found an interesting test case of size {} bytes",
                    id,
                    size
                )
            }
            LoggerMessage::JudgedNotInteresting(id, ref provenance) => {
                write!(
                    f,
                    "Worker {}: found test case, generated by {}, not interesting",
                    id,
                    provenance
                )
            }
            LoggerMessage::NewSmallest(new_size, orig_size, ref provenance) => {
                assert!(new_size < orig_size);
                assert!(orig_size != 0);
                let percent = ((orig_size - new_size) as f64) / (orig_size as f64) * 100.0;
                write!(
                    f,
                    "Supervisor: new smallest interesting test case: {} bytes ({:.2}% reduced) -- generated by {}",
                    new_size,
                    percent,
                    provenance
                )
            }
            LoggerMessage::IsNotSmaller(ref provenance) => {
                write!(
                    f,
                    "Supervisor: interesting test case, generated by {}, is not new smallest; tell worker to try merging",
                    provenance
                )
            }
            LoggerMessage::StartGeneratingNextReduction(id) => {
                write!(f, "Reducer {}: generating next reduction...", id)
            }
            LoggerMessage::FinishGeneratingNextReduction(id) => {
                write!(f, "Reducer {}: finished generating next reduction", id)
            }
            LoggerMessage::NoMoreReductions(id) => write!(f, "Reducer {}: no more reductions", id),
            LoggerMessage::FinalReducedSize(final_size, orig_size) => {
                assert!(final_size <= orig_size);
                let percent = if orig_size == 0 {
                    100.0
                } else {
                    ((orig_size - final_size) as f64) / (orig_size as f64) * 100.0
                };
                write!(
                    f,
                    "Supervisor: final reduced size is {} bytes ({:.2}% reduced)",
                    final_size,
                    percent
                )
            }
            LoggerMessage::TryMerge(id, upstream_commit, worker_commit) => {
                write!(
                    f,
                    "Worker {}: trying to merge upstream's {} into our {}",
                    id,
                    upstream_commit,
                    worker_commit
                )
            }
            LoggerMessage::FinishedMerging(id, merged_size, upstream_size) => {
                if merged_size >= upstream_size {
                    write!(
                        f,
                        "Worker {}: finished merging; not worth it; merged size {} >= upstream size {}",
                        id,
                        merged_size,
                        upstream_size
                    )
                } else {
                    write!(
                        f,
                        "Worker {}: finished merging; was worth it; merged size {} < upstream size {}",
                        id,
                        merged_size,
                        upstream_size
                    )
                }
            }
        }
    }
}

/// A client to the logger actor.
#[derive(Clone, Debug)]
pub struct Logger {
    sender: mpsc::Sender<LoggerMessage>,
}

/// Logger client implementation.
impl Logger {
    /// Spawn a `Logger` actor, writing logs to the given `Write`able.
    pub fn spawn<W>(to: W) -> error::Result<(Logger, thread::JoinHandle<()>)>
    where
        W: 'static + Send + Write,
    {
        let (sender, receiver) = mpsc::channel();
        let handle = thread::Builder::new()
            .name("preduce-logger".into())
            .spawn(move || Logger::run(to, receiver))?;
        Ok((Logger { sender: sender }, handle))
    }

    /// Log the start of spawning a worker.
    pub fn spawning_worker(&self, id: WorkerId) {
        let _ = self.sender.send(LoggerMessage::SpawningWorker(id));
    }

    /// Log the end of spawning a worker.
    pub fn spawned_worker(&self, id: WorkerId) {
        let _ = self.sender.send(LoggerMessage::SpawnedWorker(id));
    }

    /// Log that we are backing up the initial test case.
    pub fn backing_up_test_case<P, Q>(&self, from: P, to: Q)
    where
        P: AsRef<path::Path>,
        Q: AsRef<path::Path>,
    {
        let from = from.as_ref().display().to_string();
        let to = to.as_ref().display().to_string();
        self.sender
            .send(LoggerMessage::BackingUpTestCase(from, to))
            .unwrap();
    }

    /// Log that the worker with the given id is shutting down.
    pub fn shutdown_worker(&self, id: WorkerId) {
        let _ = self.sender.send(LoggerMessage::ShutdownWorker(id));
    }

    /// Log that the reducer with the given id is shutting down.
    pub fn shutdown_reducer(&self, id: ReducerId) {
        let _ = self.sender.send(LoggerMessage::ShutdownReducer(id));
    }

    /// Log that the worker with the given id is shutting down.
    pub fn worker_errored(&self, id: WorkerId, err: error::Error) {
        let _ = self.sender.send(LoggerMessage::WorkerErrored(id, err));
    }

    /// Log that the worker with the given id is shutting down.
    pub fn worker_panicked(&self, id: WorkerId, panic: Box<Any + Send + 'static>) {
        let _ = self.sender.send(LoggerMessage::WorkerPanicked(id, panic));
    }

    /// Log that the worker with the given id has started running an
    /// is-interesting predicate on its test case.
    pub fn start_judging_interesting(&self, id: WorkerId) {
        let _ = self.sender.send(LoggerMessage::StartJudgingInteresting(id));
    }

    /// Log that the worker with the given id has discovered a new interesting
    /// test case.
    pub fn judged_interesting(&self, id: WorkerId, size: u64) {
        let _ = self.sender.send(LoggerMessage::JudgedInteresting(id, size));
    }

    /// Log that the worker with the given id has discovered that its test case
    /// is not interesting.
    pub fn judged_not_interesting(&self, id: WorkerId, provenance: String) {
        let _ = self.sender
            .send(LoggerMessage::JudgedNotInteresting(id, provenance));
    }

    /// Log that the supervisor has a new globally smallest interesting test
    /// case.
    pub fn new_smallest(&self, new_size: u64, orig_size: u64, provenance: String) {
        assert!(new_size < orig_size);
        assert!(orig_size != 0);
        let _ = self.sender
            .send(LoggerMessage::NewSmallest(new_size, orig_size, provenance));
    }

    /// Log that the supervisor received a new interesting test case, but that
    /// it is not smaller than the current globally smallest interesting test
    /// case.
    pub fn is_not_smaller(&self, provenance: String) {
        let _ = self.sender.send(LoggerMessage::IsNotSmaller(provenance));
    }

    /// Log that this reducer actor has started generating its next potential
    /// reduction.
    pub fn start_generating_next_reduction(&self, id: ReducerId) {
        let _ = self.sender
            .send(LoggerMessage::StartGeneratingNextReduction(id));
    }

    /// Log that this reducer actor has completed generating its next potential
    /// reduction.
    pub fn finish_generating_next_reduction(&self, id: ReducerId) {
        let _ = self.sender
            .send(LoggerMessage::FinishGeneratingNextReduction(id));
    }

    /// Log that this reducer actor has exhuasted potential reductions for the
    /// current globally smallest interesting test case.
    pub fn no_more_reductions(&self, id: ReducerId) {
        let _ = self.sender.send(LoggerMessage::NoMoreReductions(id));
    }

    /// Log the final reduced test case's size once the reduction process has
    /// completed.
    pub fn final_reduced_size(&self, final_size: u64, orig_size: u64) {
        assert!(final_size <= orig_size);
        let _ = self.sender
            .send(LoggerMessage::FinalReducedSize(final_size, orig_size));
    }

    /// Log that the worker with the given id is attempting a merge.
    pub fn try_merging(&self, id: WorkerId, upstream_commit: git2::Oid, worker_commit: git2::Oid) {
        let _ = self.sender
            .send(LoggerMessage::TryMerge(id, upstream_commit, worker_commit));
    }

    /// Log that the worker with the given id is attempting a merge.
    pub fn finished_merging(&self, id: WorkerId, merged_size: u64, upstream_size: u64) {
        let _ = self.sender.send(LoggerMessage::FinishedMerging(
            id,
            merged_size,
            upstream_size,
        ));
    }

    /// Log that the reducer with the given id is spawning.
    pub fn spawning_reducer(&self, id: ReducerId) {
        let _ = self.sender.send(LoggerMessage::SpawningReducer(id));
    }

    /// Log that the reducer with the given id has completed spawning.
    pub fn spawned_reducer(&self, id: ReducerId) {
        let _ = self.sender.send(LoggerMessage::SpawnedReducer(id));
    }

    /// Log that the reducer with the given id errored out.
    pub fn reducer_errored(&self, id: ReducerId, err: error::Error) {
        let _ = self.sender.send(LoggerMessage::ReducerErrored(id, err));
    }

    /// Log that the reducer with the given id is shutting down.
    pub fn reducer_panicked(&self, id: ReducerId, panic: Box<Any + Send + 'static>) {
        let _ = self.sender.send(LoggerMessage::ReducerPanicked(id, panic));
    }
}

/// Logger actor implementation.
impl Logger {
    fn run<W>(mut to: W, incoming: mpsc::Receiver<LoggerMessage>)
    where
        W: Write,
    {
        let mut smallest_size = 0;

        // Reduction provenance -> (new smallest interesting count,
        //                          interesting-but-not-smallest count,
        //                          not interesting count)
        let mut stats: BTreeMap<String, (usize, usize, usize)> = BTreeMap::new();

        for log_msg in incoming {
            writeln!(&mut to, "{}", log_msg).expect("Should write to log file");

            match log_msg {
                msg @ LoggerMessage::ReducerErrored(_, _) |
                msg @ LoggerMessage::WorkerErrored(_, _) |
                msg @ LoggerMessage::ReducerPanicked(_, _) |
                msg @ LoggerMessage::WorkerPanicked(_, _) => {
                    println!("{}", msg);
                }

                LoggerMessage::NewSmallest(new_size, orig_size, provenance) => {
                    smallest_size = new_size;

                    println!(
                        "({:.2}%, {} bytes)",
                        if orig_size == 0 {
                            100.0
                        } else {
                            ((orig_size - new_size) as f64) / (orig_size as f64) * 100.0
                        },
                        new_size
                    );

                    stats.entry(provenance).or_insert((0, 0, 0)).0 += 1;
                }
                LoggerMessage::IsNotSmaller(provenance) => {
                    stats.entry(provenance).or_insert((0, 0, 0)).1 += 1;
                }
                LoggerMessage::JudgedNotInteresting(_, provenance) => {
                    stats.entry(provenance).or_insert((0, 0, 0)).2 += 1;
                }
                LoggerMessage::FinishedMerging(_, merged_size, upstream_size)
                    if merged_size >= upstream_size => {
                    stats.entry("merge".into()).or_insert((0, 0, 0)).2 += 1;
                }
                _ => {}
            }
        }

        println!("Final size is {}", smallest_size);
        println!();

        let mut stats: Vec<_> = stats.into_iter().collect();
        stats.sort_by(|&(_, s), &(_, t)| {
            use std::cmp::Ordering;
            match (s.0.cmp(&t.0), s.1.cmp(&t.1), s.2.cmp(&t.2)) {
                (Ordering::Equal, Ordering::Equal, o) |
                (Ordering::Equal, o, _) |
                (o, _, _) => o,
            }
        });
        stats.reverse();

        println!("{:=<85}", "");
        println!(
            "{:<50.50} {:>10.10}  {:>10.10}  {:>10.10}",
            "Reducer",
            "smallest",
            "intrstng",
            "not intrstng"
        );
        println!("{:-<85}", "");
        for (ref reducer, (smallest, not_smallest, not_interesting)) in stats {
            // Take the last 50 characters of the reducer name, not the first
            // 50.
            let reducer: String = reducer
                .chars()
                .rev()
                .take_while(|&c| c != '/')
                .take(50)
                .collect();
            let reducer: String = reducer.chars().rev().collect();
            println!(
                "{:<50.50} {:>10}  {:>10}  {:>10}",
                reducer,
                smallest,
                not_smallest,
                not_interesting
            );
        }
        println!("{:=<85}", "");
    }
}
