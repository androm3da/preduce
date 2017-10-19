//! The supervisor actor manages workers, and brokers their access to new
//! reductions.

use super::{Logger, Reducer, ReducerId, Sigint, Worker, WorkerId};
use super::super::Options;
use error;
use oracle;
use queue::ReductionQueue;
use signposts;
use std::any::Any;
use std::cmp;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Read};
use std::path;
use std::sync::mpsc;
use std::thread;
use test_case::{self, TestCaseMethods};
use traits::{self, Oracle};

/// The messages that can be sent to the supervisor actor.
#[derive(Debug)]
enum SupervisorMessage {
    // From workers.
    WorkerPanicked(WorkerId, Box<Any + Send + 'static>),
    WorkerErrored(WorkerId, error::Error),
    RequestNextReduction(Worker, Option<test_case::PotentialReduction>),
    ReportInteresting(Worker, test_case::Interesting),

    // From reducers.
    ReducerPanicked(ReducerId, Box<Any + Send + 'static>),
    ReducerErrored(ReducerId, error::Error),
    ReplyNextReduction(Reducer, test_case::PotentialReduction),
    ReplyExhausted(Reducer, test_case::Interesting),

    // From the SIGINT actor.
    GotSigint,
}

/// A client handle to the supervisor actor.
#[derive(Clone, Debug)]
pub struct Supervisor {
    sender: mpsc::Sender<SupervisorMessage>,
}

/// Supervisor client API.
impl Supervisor {
    /// Spawn the supervisor thread, which will in turn spawn workers and start
    /// the test case reduction process.
    pub fn spawn<I>(
        opts: Options<I>,
    ) -> error::Result<(Supervisor, thread::JoinHandle<error::Result<()>>)>
    where
        I: 'static + traits::IsInteresting,
    {
        let (sender, receiver) = mpsc::channel();
        let sender2 = sender.clone();

        let handle = thread::Builder::new()
            .name(format!("preduce-supervisor"))
            .spawn(move || {
                let supervisor = Supervisor { sender: sender2 };
                SupervisorActor::run(opts, supervisor, receiver)
            })?;

        let sup = Supervisor { sender: sender };

        Ok((sup, handle))
    }

    // Messages sent to the supervisor from the workers.

    /// Request the next potentially-interesting test case reduction. The
    /// response will be sent back to the `who` worker.
    pub fn request_next_reduction(
        &self,
        who: Worker,
        not_interesting: Option<test_case::PotentialReduction>,
    ) {
        self.sender
            .send(SupervisorMessage::RequestNextReduction(
                who,
                not_interesting,
            ))
            .unwrap();
    }

    /// Notify the supervisor that the worker with the given id panicked.
    pub fn worker_panicked(&self, id: WorkerId, panic: Box<Any + Send + 'static>) {
        self.sender
            .send(SupervisorMessage::WorkerPanicked(id, panic))
            .unwrap();
    }

    /// Notify the supervisor that the worker with the given id errored out.
    pub fn worker_errored(&self, id: WorkerId, err: error::Error) {
        self.sender
            .send(SupervisorMessage::WorkerErrored(id, err))
            .unwrap();
    }

    /// Notify the supervisor that the given test case has been found to be
    /// interesting.
    pub fn report_interesting(&self, who: Worker, interesting: test_case::Interesting) {
        self.sender
            .send(SupervisorMessage::ReportInteresting(who, interesting))
            .unwrap();
    }

    // Messages sent to the supervisor from the reducer actors.

    /// Notify the supervisor that the reducer with the given id panicked.
    pub fn reducer_panicked(&self, id: ReducerId, panic: Box<Any + Send + 'static>) {
        self.sender
            .send(SupervisorMessage::ReducerPanicked(id, panic))
            .unwrap();
    }

    /// Notify the supervisor that the reducer with the given id errored out.
    pub fn reducer_errored(&self, id: ReducerId, err: error::Error) {
        self.sender
            .send(SupervisorMessage::ReducerErrored(id, err))
            .unwrap();
    }

    /// Tell the supervisor that there are no more reductions of the current
    /// test case.
    pub fn no_more_reductions(&self, reducer: Reducer, seed: test_case::Interesting) {
        self.sender
            .send(SupervisorMessage::ReplyExhausted(reducer, seed))
            .unwrap();
    }

    /// Give the supervisor the requested next reduction of the current test
    /// case.
    pub fn reply_next_reduction(&self, reducer: Reducer, reduction: test_case::PotentialReduction) {
        self.sender
            .send(SupervisorMessage::ReplyNextReduction(reducer, reduction))
            .unwrap();
    }

    // Messages sent to the supervisor from the SIGINT actor.

    pub fn got_sigint(&self) {
        self.sender.send(SupervisorMessage::GotSigint).unwrap();
    }
}

// Supervisor actor implementation.

struct SupervisorActor<I>
where
    I: 'static + traits::IsInteresting,
{
    opts: Options<I>,
    me: Supervisor,

    logger: Logger,
    logger_handle: thread::JoinHandle<()>,

    sigint: Sigint,
    sigint_handle: thread::JoinHandle<()>,

    worker_id_counter: usize,
    workers: HashMap<WorkerId, Worker>,
    idle_workers: Vec<Worker>,

    reducer_id_counter: usize,
    reducer_actors: HashMap<ReducerId, Reducer>,
    reducer_id_to_trait_object: HashMap<ReducerId, Box<traits::Reducer>>,
    reducers_without_actors: Vec<Box<traits::Reducer>>,
    exhausted_reducers: HashSet<ReducerId>,
    reduction_queue: ReductionQueue,

    oracle: oracle::Join3<
        oracle::InterestingRate,
        oracle::CreducePassPriorities,
        oracle::PercentReduced,
    >,
}

impl<I> SupervisorActor<I>
where
    I: 'static + traits::IsInteresting,
{
    fn run(
        mut opts: Options<I>,
        me: Supervisor,
        incoming: mpsc::Receiver<SupervisorMessage>,
    ) -> error::Result<()> {
        let num_workers = opts.num_workers();
        let num_reducers = opts.reducers().len();
        let reducers_without_actors = opts.take_reducers();

        let (logger, logger_handle) =
            Logger::spawn(fs::File::create("preduce.log")?, opts.print_histograms)?;
        let (sigint, sigint_handle) = Sigint::spawn(me.clone(), logger.clone())?;

        let mut supervisor = SupervisorActor {
            opts: opts,
            me: me,
            logger: logger,
            logger_handle: logger_handle,
            sigint: sigint,
            sigint_handle: sigint_handle,
            worker_id_counter: 0,
            workers: HashMap::with_capacity(num_workers),
            idle_workers: Vec::with_capacity(num_workers),
            reducer_id_counter: 0,
            reducer_actors: HashMap::with_capacity(num_reducers),
            reducer_id_to_trait_object: HashMap::with_capacity(num_reducers),
            reducers_without_actors,
            exhausted_reducers: HashSet::with_capacity(num_reducers),
            reduction_queue: ReductionQueue::with_capacity(num_reducers),
            oracle: Default::default(),
        };

        supervisor.backup_original_test_case()?;
        supervisor.spawn_reducers()?;

        let mut smallest_interesting = supervisor.verify_initially_interesting()?;

        let orig_size = smallest_interesting.size();

        loop {
            let last_iter_size = smallest_interesting.size();

            supervisor.reseed_reducers(&smallest_interesting)?;
            supervisor.spawn_workers()?;

            let should_continue = supervisor.reduction_loop_iteration(
                &incoming,
                &mut smallest_interesting,
                orig_size,
            )?;

            if !should_continue || smallest_interesting.size() >= last_iter_size {
                return supervisor.shutdown(smallest_interesting, orig_size);
            }
        }
    }

    /// Run the supervisor's main loop, serving reductions to workers, and
    /// keeping track of the globally smallest interesting test case.
    ///
    /// Returns `true` if we should continue another iteration if we've made any
    /// progress. Returns `false` if we should shutdown regardless whether we've
    /// made any progress.
    fn reduction_loop_iteration(
        &mut self,
        incoming: &mpsc::Receiver<SupervisorMessage>,
        smallest_interesting: &mut test_case::Interesting,
        orig_size: u64,
    ) -> error::Result<bool> {
        let _signpost = signposts::SupervisorRunLoop::new();

        for msg in incoming {
            match msg {
                // Messages from workers...
                SupervisorMessage::WorkerErrored(id, err) => {
                    self.logger.worker_errored(id, err);
                    self.restart_worker(id)?;
                }

                SupervisorMessage::WorkerPanicked(id, panic) => {
                    self.logger.worker_panicked(id, panic);
                    self.restart_worker(id)?;
                }

                SupervisorMessage::RequestNextReduction(who, not_interesting) => {
                    if let Some(not_interesting) = not_interesting {
                        self.oracle.observe_not_interesting(&not_interesting);
                    }
                    self.enqueue_worker_for_reduction(who);
                }

                SupervisorMessage::ReportInteresting(who, interesting) => {
                    self.handle_new_interesting_test_case(
                        who,
                        orig_size,
                        smallest_interesting,
                        interesting,
                    )?;
                }

                // Messages from reducer actors...
                SupervisorMessage::ReducerPanicked(id, panic) => {
                    assert!(self.reducer_actors.contains_key(&id));
                    assert!(self.reducer_id_to_trait_object.contains_key(&id));

                    self.logger.reducer_panicked(id, panic);
                    self.reducer_actors.remove(&id);

                    let reducer = self.reducer_id_to_trait_object.remove(&id).unwrap();
                    self.reducers_without_actors.push(reducer);
                }

                SupervisorMessage::ReducerErrored(id, err) => {
                    assert!(self.reducer_actors.contains_key(&id));
                    assert!(self.reducer_id_to_trait_object.contains_key(&id));

                    self.logger.reducer_errored(id, err);
                    self.reducer_actors.remove(&id);

                    let reducer = self.reducer_id_to_trait_object.remove(&id).unwrap();
                    self.reducers_without_actors.push(reducer);
                }

                SupervisorMessage::ReplyExhausted(reducer, seed) => {
                    assert!(self.reducer_actors.contains_key(&reducer.id()));
                    assert!(self.reducer_id_to_trait_object.contains_key(&reducer.id()));

                    // If the seed whose reductions are exhausted is our current
                    // smallest, then the reducer really is exhausted. If it
                    // isn't the current smallest interesting test case, then
                    // the following sequence of events happened:
                    //
                    // * We sent a message requesting the reducer's next
                    //   reduction
                    // * While waiting for its response, we received a new
                    //   interesting test case, and it became our new smallest.
                    // * Because we discovered a new smallest interesting test
                    //   case, we sent reseed messages to every reducer,
                    //   including the reducer we just sent a request to.
                    // * At the same time, it sent back a reply to the original
                    //   request, stating that its reductions are exhausted.
                    //
                    // Worker           Supervisor            Reducer
                    //   |                  |                    |
                    //   |                  |\                   |
                    //   |\ interesting     | \ request          |
                    //   | \                |  \ next            |
                    //   |  `---------------|   \ reduction      |
                    //   |                  |    \               |
                    //   |                  |     \              |
                    //   |                  |      `-------------|
                    //   |                  |\                   |
                    //   |                  | \ reseed           |
                    //   |                  |  \                /|
                    //   |                  |   \    exhausted / |
                    //   |                  |    \            /  |
                    //   |                  |     \          /   |
                    //   |                  |      \        /    |
                    //   |                  |       \      /     |
                    //   |                  |        \    /      |
                    //   |                  |         \  /       |
                    //   |                  |          \/        |
                    //   |                  |          /\        |
                    //   |                  |         /  \       |
                    //   |                  |        /    \      |
                    //   |                  |-------'      `-----|
                    //   |                  |                    |
                    //
                    // Therefore, if the seed that was exhausted is not our
                    // current smallest, than the reducer is not actually
                    // exhuasted, and is in the process of reseeding
                    // itself. Additionally, we need to re-request its next
                    // newly reseeded reduction; we usually do that for
                    // exhausted reducers when sending the initial reseed
                    // message, but didn't for this one because it wasn't in the
                    // exhausted set at that time.
                    if seed == *smallest_interesting {
                        let name = self.reducer_id_to_trait_object[&reducer.id()].name();
                        self.oracle.observe_exhausted(&name);
                        self.exhausted_reducers.insert(reducer.id());
                    } else {
                        reducer.request_next_reduction(None);
                    }
                }

                SupervisorMessage::ReplyNextReduction(reducer, reduction) => {
                    assert!(self.reducer_actors.contains_key(&reducer.id()));

                    if reduction.size() < smallest_interesting.size() {
                        let priority = self.oracle.predict(&reduction);
                        self.reduction_queue
                            .insert(reduction, reducer.id(), priority);
                        self.drain_queues();
                    } else {
                        reducer.not_interesting(reduction);
                        reducer.request_next_reduction(None);
                    }
                }

                SupervisorMessage::GotSigint => {
                    for (_, worker) in self.workers.drain() {
                        worker.shutdown();
                    }
                    self.reduction_queue.clear();
                    return Ok(false);
                }
            }

            // If all of our reducers are exhausted, and we are out of potential
            // reductions to test, then shutdown any idle workers, since we
            // don't have any work for them.
            if self.exhausted_reducers.len() == self.reducer_actors.len()
                && self.reduction_queue.is_empty()
            {
                for worker in self.idle_workers.drain(..) {
                    self.workers.remove(&worker.id());
                    worker.shutdown();
                }
            }

            if self.workers.is_empty() {
                break;
            }
        }

        Ok(true)
    }

    /// Consume this supervisor actor and perform shutdown.
    fn shutdown(
        self,
        smallest_interesting: test_case::Interesting,
        orig_size: u64,
    ) -> error::Result<()> {
        assert!(self.workers.is_empty());
        assert!(self.reduction_queue.is_empty());
        assert_eq!(self.exhausted_reducers.len(), self.reducer_actors.len());

        let _signpost = signposts::SupervisorShutdown::new();

        self.logger
            .final_reduced_size(smallest_interesting.size(), orig_size);

        self.sigint.shutdown();
        let _ = self.sigint_handle.join();

        // Tell all the reducer actors to shutdown, and then wait for them
        // finish their cleanup by joining the logger thread, which exits once
        // log messages can no longer be sent to it.
        for (_, r) in self.reducer_actors {
            r.shutdown();
        }
        drop(self.logger);
        self.logger_handle.join()?;

        println!(
            "====================================================================================="
        );

        // If the final, smallest interesting test case is small enough and its
        // contents are UTF-8, then print it to stdout.
        const TOO_BIG_TO_PRINT: u64 = 4096;
        let final_size = smallest_interesting.size();
        if final_size < TOO_BIG_TO_PRINT {
            let mut contents = String::with_capacity(final_size as usize);
            let mut file = fs::File::open(smallest_interesting.path())?;
            if let Ok(_) = file.read_to_string(&mut contents) {
                println!("{}", contents);
            }
        }

        Ok(())
    }

    /// Given that the worker with the given id panicked or errored out, clean
    /// up after it and spawn a replacement for it.
    fn restart_worker(&mut self, id: WorkerId) -> error::Result<()> {
        let old_worker = self.workers.remove(&id);
        assert!(old_worker.is_some());

        self.spawn_workers()
    }

    /// Generate the next reduction and send it to the given worker, or shutdown
    /// the worker if our reducer is exhausted.
    fn enqueue_worker_for_reduction(&mut self, who: Worker) {
        assert!(self.workers.contains_key(&who.id()));

        self.idle_workers.push(who);
        self.drain_queues();
    }

    /// Given that either we've generated new potential reductions to test, or a
    /// worker just became ready to test queued reductions, dispatch as many
    /// reductions to workers as possible.
    fn drain_queues(&mut self) {
        assert!(
            self.idle_workers.len() > 0 || self.reduction_queue.len() > 0,
            "Should only call drain_queues when we have potential to do new work"
        );

        let num_to_drain = cmp::min(self.idle_workers.len(), self.reduction_queue.len());
        let workers = self.idle_workers.drain(..num_to_drain);
        let reductions = self.reduction_queue.drain(..num_to_drain);

        for (worker, (reduction, reducer_id)) in workers.zip(reductions) {
            assert!(self.workers.contains_key(&worker.id()));
            assert!(self.reducer_actors.contains_key(&reducer_id));

            // Send the worker the next reduction from the queue to test for
            // interestingness.
            worker.next_reduction(reduction);

            // And pipeline the worker's is-interesting test with generating the
            // next reduction.
            if !self.exhausted_reducers.contains(&reducer_id) {
                self.reducer_actors[&reducer_id].request_next_reduction(None);
            }
        }
    }

    /// Given that the `who` worker just found a new interesting test case,
    /// either update our globally smallest interesting test case, or tell the
    /// worker to try testing a new reduction.
    fn handle_new_interesting_test_case(
        &mut self,
        who: Worker,
        orig_size: u64,
        smallest_interesting: &mut test_case::Interesting,
        interesting: test_case::Interesting,
    ) -> error::Result<()> {
        let _signpost = signposts::SupervisorHandleInteresting::new();

        let new_size = interesting.size();
        let old_size = smallest_interesting.size();

        if new_size < old_size {
            // We have a new globally smallest insteresting test case! First,
            // update the original test case file with the new interesting
            // reduction. The reduction process can take a LONG time, and if the
            // computation is interrupted for whatever reason, we DO NOT want to
            // lose this incremental progress!
            *smallest_interesting = interesting;
            fs::copy(smallest_interesting.path(), &self.opts.test_case)?;
            self.oracle
                .observe_smallest_interesting(&smallest_interesting);
            self.logger
                .new_smallest(smallest_interesting.clone(), orig_size);

            // Third, re-seed our reducer actors with the new test case, and
            // respawn any workers that might have shutdown because we exhausted
            // all possible reductions on the previous smallest interesting test
            // case.
            self.reseed_reducers(smallest_interesting)?;
            self.spawn_workers()?;

            // Fourth, clear out any queued potential reductions that are larger
            // than our new smallest interesting test case. We don't want to
            // waste time on them. For any reduction we don't end up
            // considering, tell its progenitor to generate its next reduction
            // from the new seed.
            {
                let reducers = &self.reducer_actors;
                self.reduction_queue.retain(|reduction, reducer_id| {
                    if reduction.size() < new_size {
                        return true;
                    }

                    reducers[&reducer_id].request_next_reduction(None);
                    false
                });
            }

            // Finaly send a new reduction to the worker that reported the new
            // smallest test case.
            self.enqueue_worker_for_reduction(who);
        } else {
            // Although the test case is interesting, it is not smaller. This is
            // the unlikely case where we find two new interesting test cases at
            // the same time, the smaller of the two reports back first, and
            // then the larger finishes and reports back before it is told to
            // abandon its current is-interesting test and move on to new work.
            self.oracle.observe_not_smallest_interesting(&interesting);
            self.logger.is_not_smaller(interesting);
            self.enqueue_worker_for_reduction(who);
        }

        Ok(())
    }

    /// Backup the original test case, just in case something goes wrong, or it
    /// is needed again to reduce a different issue from the one we're currently
    /// reducing, or...
    fn backup_original_test_case(&self) -> error::Result<()> {
        let mut backup_path = path::PathBuf::from(&self.opts.test_case);
        let mut file_name = self.opts
            .test_case
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .ok_or_else(|| {
                let e = io::Error::new(
                    io::ErrorKind::Other,
                    "test case path must exist and representable in utf-8",
                );
                error::Error::TestCaseBackupFailure(e)
            })?;
        file_name.push_str(".orig");
        backup_path.set_file_name(file_name);

        self.logger
            .backing_up_test_case(&self.opts.test_case, &backup_path);

        fs::copy(&self.opts.test_case, backup_path).map_err(error::Error::TestCaseBackupFailure)?;

        Ok(())
    }

    /// Verify that the initial, unreduced test case is itself interesting.
    fn verify_initially_interesting(&mut self) -> error::Result<test_case::Interesting> {
        let initial = test_case::Interesting::initial(&self.opts.test_case, self.opts.predicate())?;
        let initial = initial.ok_or(error::Error::InitialTestCaseNotInteresting)?;
        Ok(initial)
    }

    /// Spawn (or re-spawn) workers until we have the number of active,
    /// concurrent workers originally requested in the `Options`.
    fn spawn_workers(&mut self) -> error::Result<()> {
        assert!(self.workers.len() <= self.opts.num_workers());

        let new_workers: error::Result<Vec<_>> = (self.workers.len()..self.opts.num_workers())
            .map(|_| {
                let id = WorkerId::new(self.worker_id_counter);
                self.worker_id_counter += 1;

                let worker = Worker::spawn(
                    id,
                    self.opts.predicate().clone(),
                    self.me.clone(),
                    self.logger.clone(),
                )?;
                Ok((id, worker))
            })
            .collect();
        let new_workers = new_workers?;
        self.workers.extend(new_workers);
        Ok(())
    }

    /// Spawn a reducer actor for each reducer given to us in the options.
    fn spawn_reducers(&mut self) -> error::Result<()> {
        for reducer in self.reducers_without_actors.drain(..) {
            let id = ReducerId::new(self.reducer_id_counter);
            self.reducer_id_counter += 1;

            self.reducer_id_to_trait_object
                .insert(id, reducer.clone_boxed());
            let reducer_actor = Reducer::spawn(id, reducer, self.me.clone(), self.logger.clone())?;
            self.reducer_actors.insert(id, reducer_actor);
            self.exhausted_reducers.insert(id);
        }
        Ok(())
    }

    /// Reseed each of the reducer actors with the new smallest interesting test
    /// case.
    fn reseed_reducers(
        &mut self,
        smallest_interesting: &test_case::Interesting,
    ) -> error::Result<()> {
        // Re-spawn any reducers that may have panicked with the previous test
        // case as input.
        self.spawn_reducers()?;

        for (id, reducer_actor) in &self.reducer_actors {
            reducer_actor.set_new_seed(smallest_interesting.clone());

            // If the reducer was exhausted, put it back to work again by
            // requesting the next reduction. If it isn't exhausted, then we
            // will request its next reduction after we pull its most recently
            // generated (or currently being generated) reduction from the
            // reduction queue.
            if self.exhausted_reducers.contains(id) {
                reducer_actor.request_next_reduction(None);
                self.exhausted_reducers.remove(id);
            }
        }

        Ok(())
    }
}
