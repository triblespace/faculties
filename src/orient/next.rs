//! One call's owned wait and complete-output acceptance. The existing wait
//! state machine constructs reports and commits Presented; this frontend only
//! delays its Out acceptance until the caller publishes the correlated result.
use super::*;
use crate::out::Part;
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use triblespace::core::repo::async_store::{AcquiringReadControl, AcquiringReadInterrupted};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NextKind { Report, Timeout }

/// The outcome of joining the exact call, not acceptance of its output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NextCompletion { Completed, CancelledBeforeReport }

/// Exact complete output, not a clipped preview. Holding or dropping this
/// value does not acknowledge news. The owning PendingNext retains the worker.
pub struct PreparedReport {
    parts: Vec<Part>,
    kind: NextKind,
    accept: tokio::sync::oneshot::Sender<()>,
    owner: Arc<()>,
}

impl PreparedReport {
    pub fn parts(&self) -> &[Part] { &self.parts }
    pub fn kind(&self) -> NextKind { self.kind }
}

/// Configuration for one finite call; construction performs no I/O.
pub struct Next {
    orient: Orient,
    persona: String,
    options: WaitOptions,
    control: AcquiringReadControl,
    owner: Arc<()>,
    #[cfg(test)]
    quiet_snapshot: Option<quiet_snapshot_test::Rendezvous>,
    #[cfg(test)]
    registration: Option<crate::storage::registration_test::Rendezvous>,
}

/// The exact worker remains owned through preparation, publication, receipt
/// completion and cancellation. No task is detached when a timeout is observed.
pub struct PendingNext {
    control: AcquiringReadControl,
    owner: Arc<()>,
    ready: Receiver<PreparedReport>,
    worker: Option<std::thread::JoinHandle<Result<NextCompletion>>>,
    settled: Option<std::result::Result<NextCompletion, Arc<anyhow::Error>>>,
}

#[derive(Debug)]
struct JoinedNextError(Arc<anyhow::Error>);
impl std::fmt::Display for JoinedNextError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:#}", self.0)
    }
}
impl std::error::Error for JoinedNextError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}

pub(super) fn is_cancelled(error: &anyhow::Error) -> bool {
    // These concrete wrappers each carry exactly one cause. Do not use
    // anyhow's recursive downcast or search arbitrary source/context chains:
    // a context can describe an independently failed close or settlement.
    type BorrowError = <crate::storage::BorrowedStore as SnapshotSource>::SnapshotError;
    let mut error: &(dyn std::error::Error + 'static) = error.as_ref();
    if let Some(registration) = error.downcast_ref::<OrientRegistrationError<BorrowError>>() {
        let CollectionRegistrationError::DependencyPut(borrow) = &registration.cause else { return false; };
        error = borrow;
    } else if let Some(snapshot) = error.downcast_ref::<WaitSnapshotError>() {
        error = snapshot.0.as_ref();
    }
    if let Some(borrow) = error.downcast_ref::<BorrowError>() {
        let Some(cause) = std::error::Error::source(borrow) else { return false; };
        error = cause;
    }
    matches!(error.downcast_ref::<AcquiringReadInterrupted>(),
        Some(AcquiringReadInterrupted::Cancelled))
}

pub(super) fn completion(result: Result<()>, offered: bool) -> Result<NextCompletion> {
    match result {
        Ok(()) => Ok(NextCompletion::Completed),
        Err(error) if !offered && is_cancelled(&error) => Ok(NextCompletion::CancelledBeforeReport),
        Err(error) => Err(error),
    }
}

pub(super) fn deadline(options: &WaitOptions) -> Result<Option<Instant>> {
    options.timeout.map(|duration| Instant::now().checked_add(duration)
        .ok_or_else(|| anyhow!("Orient wait deadline is out of range"))).transpose()
}

impl Orient {
    pub fn begin_next(&self, persona: &str, options: &WaitOptions) -> Result<Next> {
        if persona.trim().is_empty() { bail!("Orient next requires an explicit zooid persona"); }
        if options.timeout.is_none() { bail!("Orient next requires a finite timeout"); }
        Ok(Next { orient: self.clone(), persona: persona.to_owned(), options: options.clone(),
            control: AcquiringReadControl::new(deadline(options)?), owner: Arc::new(()),
            #[cfg(test)]
            quiet_snapshot: None,
            #[cfg(test)]
            registration: None,
        })
    }
}

impl Next {
    pub fn start(self) -> Result<PendingNext> {
        let control = self.control.clone();
        let owner = self.owner.clone();
        PendingNext::spawn(control, owner, move |send| self.run(send))
    }

    fn run(self, send: Sender<PreparedReport>) -> Result<NextCompletion> {
        self.orient.storage.scope(|storage| {
            let prepared = Arc::new(AtomicBool::new(false));
            let opened = crate::storage::BorrowedStore::open(
                storage.clone(), self.control.clone(), prepared.clone(),
            );
            let (mut store, signer, runtime) = match opened {
                Ok(opened) => opened,
                Err(error) if is_deadline(&error) => return offer_timeout(&send, self.owner.clone(),
                    vec![Part::Text { text: "Orient next reached its deadline before the shared store became available.\n".to_owned() }])
                    .map(|()| NextCompletion::Completed),
                Err(error) => return completion(Err(error), false),
            };
            #[cfg(test)]
            { store.registration = self.registration; }
            let mut parts = Vec::new();
            let offered = std::cell::Cell::new(false);
            let result = {
                let mut emit = |part| {
                    let complete = matches!(part, Part::Text { .. });
                    parts.push(part);
                    if complete {
                        // The whole report exists before ending acquisition's
                        // deadline. No fresh observation or timer is created.
                        self.control.check()?;
                        prepared.store(true, Ordering::Release);
                        offered.set(true);
                        offer(&send, self.owner.clone(), std::mem::take(&mut parts), &self.control.without_deadline())?;
                    }
                    Ok(())
                };
                runtime.block_on(observe_wait(
                    storage, &mut store, &signer, storage.path(), Some(&self.persona),
                    &self.options, self.orient.health_max_age, self.orient.service_events_collection,
                    false, self.control.deadline(), &mut Out::new(&mut emit),
                    #[cfg(test)]
                    self.quiet_snapshot.as_ref(),
                ))
            };
            match result {
                Ok(outcome) if !outcome.news_printed => {
                    parts.push(Part::Text { text: format!("{}\n", outcome.timeout_text()) });
                    offer_timeout(&send, self.owner.clone(), parts).map(|()| NextCompletion::Completed)
                }
                Ok(_) => Ok(NextCompletion::Completed),
                Err(error) if !offered.get() && is_deadline(&error) => {
                    parts.push(Part::Text { text:
                        "Orient next reached its deadline before a complete readable report was accepted.\n".to_owned() });
                    offer_timeout(&send, self.owner.clone(), parts).map(|()| NextCompletion::Completed)
                }
                // This is still inside scope: a subsequent storage close
                // failure cannot be mistaken for normal cancellation.
                Err(error) => completion(Err(error), offered.get()),
            }
        })
    }
}

pub(super) fn is_deadline(error: &anyhow::Error) -> bool {
    error.chain().any(|source| matches!(source.downcast_ref::<AcquiringReadInterrupted>(),
        Some(AcquiringReadInterrupted::DeadlineExceeded)))
}

fn offer(send: &Sender<PreparedReport>, owner: Arc<()>, parts: Vec<Part>, control: &AcquiringReadControl) -> Result<()> {
    control.check()?;
    let (accept, accepted) = tokio::sync::oneshot::channel();
    send.send(PreparedReport { parts, kind: NextKind::Report, accept, owner })
        .map_err(|_| anyhow!("Orient result owner closed before publication"))?;
    Err(super::super::delivery::PreparedAcceptance::new(accepted, control.clone()).into())
}

fn offer_timeout(send: &Sender<PreparedReport>, owner: Arc<()>, parts: Vec<Part>) -> Result<()> {
    // No pending Presented commit follows a quiet timeout. Its output remains
    // owned by the caller even though the observation worker can now finish.
    let (accept, _) = tokio::sync::oneshot::channel();
    send.send(PreparedReport { parts, kind: NextKind::Timeout, accept, owner })
        .map_err(|_| anyhow!("Orient result owner closed before timeout delivery"))
}

impl PendingNext {
    fn spawn(control: AcquiringReadControl, owner: Arc<()>,
        run: impl FnOnce(Sender<PreparedReport>) -> Result<NextCompletion> + Send + 'static,
    ) -> Result<Self> {
        let (send, ready) = mpsc::channel();
        let worker = std::thread::Builder::new().name("orient-next".to_owned())
            .spawn(move || run(send)).context("start owned Orient next call")?;
        Ok(Self { control, owner, ready, worker: Some(worker), settled: None })
    }

    /// A prepared complete result is available before worker completion. This
    /// lets Shell publish it before releasing Out's acceptance boundary.
    pub fn poll(&mut self) -> Result<Option<PreparedReport>> {
        if self.settled.is_some() { self.completed()?; }
        match self.ready.try_recv() {
            Ok(report) => Ok(Some(report)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                self.completed()?;
                Ok(None)
            }
        }
    }

    /// Call after complete ledger and native Archive publication, never after
    /// a clipped preview or merely buffering the worker's output. Join returns
    /// the real Presented/close outcome. Publication is not rolled back if
    /// cancellation, receipt writing or close then fails.
    pub fn acknowledge(&mut self, report: PreparedReport) -> Result<()> {
        if !Arc::ptr_eq(&self.owner, &report.owner) {
            bail!("prepared Orient report belongs to another pending call");
        }
        if self.settled.is_some() { self.completed()?; }
        if report.kind == NextKind::Report && report.accept.send(()).is_err() {
            self.completed()?;
            bail!("Orient receipt worker ended before acceptance");
        }
        self.completed()
    }

    pub fn cancel_and_join(&mut self) -> Result<NextCompletion> {
        self.control.cancel();
        self.join()
    }

    pub fn is_settled(&self) -> bool { self.worker.is_none() }

    fn completed(&mut self) -> Result<()> {
        match self.join()? {
            NextCompletion::Completed => Ok(()),
            NextCompletion::CancelledBeforeReport => Err(AcquiringReadInterrupted::Cancelled.into()),
        }
    }

    fn join(&mut self) -> Result<NextCompletion> {
        if self.settled.is_none() {
            let worker = self.worker.take().expect("unsettled Orient owns its worker");
            self.settled = Some(worker.join()
                .map_err(|panic| anyhow!("Orient next worker panicked: {}", super::panic_text(&panic)))
                .and_then(|result| result).map_err(Arc::new));
        }
        match self.settled.as_ref().expect("joined outcome retained") {
            Ok(outcome) => Ok(*outcome),
            Err(error) => Err(JoinedNextError(Arc::clone(error)).into()),
        }
    }
}

impl Drop for PendingNext {
    fn drop(&mut self) {
        if self.worker.is_some() {
            // Explicit shutdown reports errors. The fallback still owns the
            // call through settlement; synchronous queries can delay this join.
            if let Err(error) = self.cancel_and_join() {
                eprintln!("closing owned Orient next call: {error:#}");
            }
        }
    }
}

/// Private per-call test equipment. Neither the store nor a process-global
/// registry owns the rendezvous, and no shared-store borrow spans its wait.
#[cfg(test)]
pub(super) mod quiet_snapshot_test {
    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(in crate::orient::operations) enum Boundary { Health, Ordinary }

    pub(in crate::orient::operations) struct Rendezvous {
        boundary: Boundary,
        entered: Sender<Boundary>,
        release: std::sync::Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    }

    pub(super) struct Release(pub(super) Option<tokio::sync::oneshot::Sender<()>>);
    impl Drop for Release {
        fn drop(&mut self) {
            // Also release on a failed assertion or a timed-out observation.
            if let Some(send) = self.0.take() { let _ = send.send(()); }
        }
    }

    impl Rendezvous {
        pub(super) fn new(boundary: Boundary)
            -> (Self, Receiver<Boundary>, tokio::sync::oneshot::Sender<()>)
        {
            let (entered, ready) = mpsc::channel();
            let (release, go) = tokio::sync::oneshot::channel();
            (Self { boundary, entered, release: std::sync::Mutex::new(Some(go)) }, ready, release)
        }

        pub(in crate::orient::operations) async fn pause(&self, boundary: Boundary) -> Result<()> {
            if boundary != self.boundary { return Ok(()); }
            let go = self.release.lock().unwrap().take();
            let Some(go) = go else { return Ok(()); };
            self.entered.send(boundary).context("observe quiet snapshot rendezvous")?;
            tokio::time::timeout(Duration::from_secs(10), go).await
                .context("quiet snapshot rendezvous release timed out")?
                .context("quiet snapshot rendezvous release dropped")?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{CollectionRoutes, initialize_signer};
    use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};

    #[test]
    fn next_health_relations_registration_cancellation_settles_without_output() {
        let fixture = Fixture::new();
        let quiet_persona = genid().id;
        fixture.storage.with_store(|store, signer, runtime| {
            let sources = runtime.block_on(OrientSources::open(&fixture.storage, store, signer, true))?;
            let (person, _, _) = relations::person_fragment(quiet_persona,
                crate::relations::ProfileInput { label: "quiet-registration-zooid".into(), ..Default::default() })?;
            store.commit(sources.relations.source, signer, person)?;
            Ok(())
        }).unwrap();
        let (hook, ready, release) = crate::storage::registration_test::Rendezvous::new();
        let mut call = Orient::with_storage(fixture.storage.clone())
            .begin_next(&fmt_id(quiet_persona), &WaitOptions {
                timeout: Some(Duration::from_secs(30)), ..Default::default()
            }).unwrap();
        call.registration = Some(hook);
        let mut next = call.start().unwrap();
        // On unwind, release first, then let PendingNext own cancellation/join.
        let release = crate::storage::registration_test::Release(Some(release));
        // observe_wait opens HealthSources before OrientSources. This first
        // Relations Rank9 boundary is in health opening, not a quiet snapshot.
        assert_eq!(ready.recv_timeout(Duration::from_secs(10)).unwrap(), "Relations");
        assert!(!next.is_settled());
        assert!(matches!(next.ready.try_recv(), Err(TryRecvError::Empty)));
        fixture.storage.try_with_store(|_, _, _| Ok(())).unwrap()
            .expect("registration rendezvous must not hold the shared Storage borrow");
        next.control.cancel();
        drop(release); // The hook returns Ok; real attach/put sees cancellation.
        for _ in 0..2 {
            assert_eq!(next.cancel_and_join().unwrap(), NextCompletion::CancelledBeforeReport);
            assert!(next.is_settled());
            assert!(is_cancelled(&next.poll().err().expect("cancelled is not delivered output")));
            assert!(matches!(next.ready.try_recv(), Err(TryRecvError::Disconnected)));
        }
        fixture.storage.with_store(|store, signer, runtime| {
            let source = runtime.block_on(ReceiptSource::open(&fixture.storage, store, signer))?.source;
            let facts = store.snapshot()?.collection(source)?.view::<TribleSet>()?;
            assert!(!exists!(pattern!(&facts, [{ metadata::tag: &KIND_PRESENTED }])));
            Ok(())
        }).unwrap();
    }

    // Helper-level error fixtures, not injected failures of a real store.
    fn registration_error(cause: anyhow::Error) -> anyhow::Error {
        type BorrowError = <crate::storage::BorrowedStore as SnapshotSource>::SnapshotError;
        OrientRegistrationError {
            label: "Relations", projection: "Rank9",
            cause: CollectionRegistrationError::DependencyPut(BorrowError::for_classification_test(cause)),
        }.into()
    }

    #[test]
    fn registration_classifier_preserves_variants_context_deadline_and_offered_output() {
        type BorrowError = <crate::storage::BorrowedStore as SnapshotSource>::SnapshotError;
        let canceled = || registration_error(AcquiringReadInterrupted::Cancelled.into());
        assert!(is_cancelled(&canceled()));
        assert_eq!(completion(Err(canceled()), false).unwrap(), NextCompletion::CancelledBeforeReport);
        assert!(completion(Err(canceled()), true).is_err(), "offered output must not be retired");
        for error in [
            registration_error(anyhow!("unrelated put failure")),
            registration_error(anyhow::Error::from(AcquiringReadInterrupted::Cancelled)
                .context("independent inner failure")),
            canceled().context("independent outer failure"),
            // No permission to peel a bare core registration error either.
            anyhow::Error::from(CollectionRegistrationError::DependencyPut(
                BorrowError::for_classification_test(AcquiringReadInterrupted::Cancelled.into()))),
            anyhow::Error::from(OrientRegistrationError::<BorrowError> {
                label: "Relations", projection: "Rank9",
                cause: CollectionRegistrationError::WrongType(
                    triblespace::core::collection::CollectionTypeError::WrongEncoding { expected: genid().id }),
            }),
        ] {
            assert!(!is_cancelled(&error));
            assert!(completion(Err(error), false).is_err());
        }
        let deadline = registration_error(AcquiringReadInterrupted::DeadlineExceeded.into());
        assert!(!is_cancelled(&deadline));
        assert!(is_deadline(&deadline), "registration must preserve the existing timeout cause");
        assert_eq!(crate::orient::delivery::exit_code(&canceled()), 1,
            "non-Next delivery must not normalize cancellation");
        assert_eq!(crate::orient::delivery::exit_code(&deadline), 1);
        assert!(format!("{}", canceled()).starts_with("register Relations Rank9 collection:"));
    }

    #[test]
    fn registration_cancellation_preserves_independent_selection_failure() {
        for panic in [false, true] {
            crate::storage::runtime().unwrap().block_on(async {
                let (entered, ready) = mpsc::channel();
                let (release, go) = mpsc::channel();
                let task = tokio::task::spawn_blocking(move || -> Result<()> {
                    entered.send(()).unwrap();
                    go.recv_timeout(Duration::from_secs(10)).context("selection fixture release")?;
                    if panic { panic!("independent selection panic"); }
                    bail!("independent selection failure");
                });
                // Release also on assertion failure; the spawned task's wait is bounded.
                let release = crate::storage::registration_test::Release(Some(release));
                ready.recv_timeout(Duration::from_secs(5)).unwrap();
                drop(release);
                let joined = task.await.context("settle frozen Orient selection")
                    .and_then(|result| result);
                let error = finish_wait_selection::<()>(
                    Err(registration_error(AcquiringReadInterrupted::Cancelled.into())), joined).unwrap_err();
                assert!(!is_cancelled(&error));
                let diagnostic = format!("{error:#}");
                assert!(diagnostic.contains("register Relations Rank9 collection"));
                assert!(diagnostic.contains(if panic { "independent selection panic" }
                    else { "independent selection failure" }));
                assert!(completion(Err(error), false).is_err());
            });
        }
    }

    #[test]
    fn registration_cancellation_preserves_close_aggregation_errors() {
        // Exercise the actual helper, not a poisoned store or failed disk write.
        let canceled = || registration_error(AcquiringReadInterrupted::Cancelled.into());
        let error = crate::storage::finish_close_for_test::<()>(
            Err(canceled()), Err(anyhow!("independent close failure"))).unwrap_err();
        assert!(!is_cancelled(&error));
        let diagnostic = format!("{error:#}");
        assert!(diagnostic.contains("register Relations Rank9 collection"));
        assert!(diagnostic.contains("independent close failure"));
        assert!(completion(Err(error), false).is_err());
        // Next normalizes inside scope, so a later close can still fail.
        let normalized = completion(Err(canceled()), false);
        let error = crate::storage::finish_close_for_test(
            normalized, Err(anyhow!("close after normalized cancellation"))).unwrap_err();
        assert!(!is_cancelled(&error));
        assert_eq!(error.to_string(), "close after normalized cancellation");
    }

    fn cancel_after_quiet_frame(boundary: quiet_snapshot_test::Boundary) {
        let fixture = Fixture::new();
        let quiet_persona = genid().id;
        fixture.storage.with_store(|store, signer, runtime| {
            let sources = runtime.block_on(OrientSources::open(&fixture.storage, store, signer, true))?;
            let (person, _, _) = relations::person_fragment(quiet_persona,
                crate::relations::ProfileInput { label: "quiet-snapshot-zooid".into(), ..Default::default() })?;
            store.commit(sources.relations.source, signer, person)?;
            Ok(())
        }).unwrap();
        let (hook, ready, release) = quiet_snapshot_test::Rendezvous::new(boundary);
        let mut call = Orient::with_storage(fixture.storage.clone())
            .begin_next(&fmt_id(quiet_persona), &WaitOptions {
                timeout: Some(Duration::from_secs(30)),
                poll_interval: Duration::from_millis(1), ..Default::default()
            }).unwrap();
        call.quiet_snapshot = Some(hook);
        let mut next = call.start().unwrap();
        // Declared after Next so unwinding releases the rendezvous before
        // PendingNext's Drop cancels and joins. Both rendezvous waits are bounded.
        let release = quiet_snapshot_test::Release(Some(release));
        assert_eq!(ready.recv_timeout(Duration::from_secs(10)).unwrap(), boundary);
        assert!(!next.is_settled());
        assert!(matches!(next.ready.try_recv(), Err(TryRecvError::Empty)), "no offered output");
        assert!(!fixture.presented());
        next.control.cancel();
        drop(release);
        for _ in 0..2 {
            assert_eq!(next.cancel_and_join().unwrap(), NextCompletion::CancelledBeforeReport);
            assert!(next.is_settled());
            assert!(is_cancelled(&next.poll().err().expect("cancelled is not delivered output")));
            assert!(matches!(next.ready.try_recv(), Err(TryRecvError::Disconnected)),
                "neither a queued report nor a timeout may be silently retired");
        }
        // No receipt of any kind, not merely no receipt for fixture.event.
        fixture.storage.with_store(|store, signer, runtime| {
            let source = runtime.block_on(ReceiptSource::open(&fixture.storage, store, signer))?.source;
            let facts = store.snapshot()?.collection(source)?.view::<TribleSet>()?;
            assert!(!exists!(pattern!(&facts, [{ metadata::tag: &KIND_PRESENTED }])));
            Ok(())
        }).unwrap();
    }

    #[test]
    fn next_post_open_health_snapshot_cancellation_settles_without_output() {
        cancel_after_quiet_frame(quiet_snapshot_test::Boundary::Health);
    }

    #[test]
    fn next_post_open_ordinary_snapshot_cancellation_settles_without_output() {
        cancel_after_quiet_frame(quiet_snapshot_test::Boundary::Ordinary);
    }

    #[test]
    fn snapshot_interruption_recognition_rejects_context_and_unrelated_causes() {
        let fixture = Fixture::new();
        let control = AcquiringReadControl::new(None);
        let (mut store, _, _) = crate::storage::BorrowedStore::open(
            fixture.storage.clone(), control.clone(), Arc::new(AtomicBool::new(false)),
        ).unwrap();
        control.cancel();
        let wrapped = || anyhow::Error::from(AcquiringReadInterrupted::Cancelled);
        assert!(is_cancelled(&wrapped()));
        let borrowed = store.snapshot().err().expect("cancelled snapshot");
        assert!(is_cancelled(&borrowed.into()));
        let borrowed = store.snapshot().err().expect("cancelled snapshot");
        assert!(is_cancelled(&WaitSnapshotError(borrowed.into()).into()));
        let borrowed = store.snapshot().err().expect("cancelled snapshot");
        assert!(!is_cancelled(&anyhow::Error::from(borrowed).context("independent close failure")));
        let borrowed = store.snapshot().err().expect("cancelled snapshot");
        assert!(!is_cancelled(&WaitSnapshotError(
            anyhow::Error::from(borrowed).context("independent settlement failure")).into()));
        assert!(!is_cancelled(&WaitSnapshotError(anyhow!("unrelated snapshot failure")).into()));
        assert!(!is_cancelled(&WaitSnapshotError(wrapped().context("other failure")).into()));
        assert!(!is_cancelled(&WaitSnapshotError(
            AcquiringReadInterrupted::DeadlineExceeded.into()).into()));
        assert!(is_deadline(&WaitSnapshotError(AcquiringReadInterrupted::DeadlineExceeded.into()).into()),
            "the preserved snapshot deadline must still offer timeout output");
        for interruption in [AcquiringReadInterrupted::Cancelled, AcquiringReadInterrupted::DeadlineExceeded] {
            let error = anyhow::Error::from(WaitSnapshotError(interruption.into()));
            assert_eq!(crate::orient::delivery::exit_code(&error), 1,
                "non-Next frontends still report snapshot interruption as an error");
        }
        assert!(completion(Err(WaitSnapshotError(wrapped()).into()), true).is_err(),
            "even typed cancellation cannot retire an offered report");
    }

    #[test]
    fn borrowed_error_classifier_rejects_unrelated_and_context_wrapped_causes() {
        type BorrowError = <crate::storage::BorrowedStore as SnapshotSource>::SnapshotError;
        // Helper-level typed classification, not a real snapshot failure.
        // Storage deliberately keeps handler panics from poisoning its owner.
        for contextual in [false, true] {
            for snapshot_wrapper in [false, true] {
                let cause = if contextual {
                    anyhow::Error::from(AcquiringReadInterrupted::Cancelled)
                        .context("independent failure")
                } else { anyhow!("unrelated borrowed operation failure") };
                let borrowed: anyhow::Error = BorrowError::for_classification_test(cause).into();
                let error = if snapshot_wrapper { WaitSnapshotError(borrowed).into() }
                    else { borrowed };
                assert!(!is_cancelled(&error));
                assert!(completion(Err(error), false).is_err());
            }
        }
    }

    #[test]
    fn owned_join_retains_cancellation_error_and_panic_outcomes() {
        for fail in [false, true] {
            let control = AcquiringReadControl::new(None);
            let worker_control = control.clone();
            let (entered, ready) = mpsc::channel();
            let (release, go) = mpsc::channel();
            let mut next = PendingNext::spawn(control, Arc::new(()), move |_| {
                entered.send(()).unwrap();
                go.recv().unwrap();
                if fail { anyhow::bail!("independent worker failure"); }
                completion(worker_control.check().map_err(Into::into), false)
            }).unwrap();
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(!next.is_settled());
            next.control.cancel();
            release.send(()).unwrap();
            for _ in 0..2 {
                let result = next.cancel_and_join();
                if fail { assert!(result.unwrap_err().to_string().contains("independent worker failure")); }
                else { assert_eq!(result.unwrap(), NextCompletion::CancelledBeforeReport); }
                assert!(next.is_settled());
            }
            assert!(next.poll().is_err());
        }
        let (entered, ready) = mpsc::channel();
        let (release, go) = mpsc::channel();
        let mut next = PendingNext::spawn(AcquiringReadControl::new(None), Arc::new(()), move |_| {
            entered.send(()).unwrap();
            go.recv().unwrap();
            panic!("owned worker panic");
        }).unwrap();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        next.control.cancel();
        release.send(()).unwrap();
        for _ in 0..2 {
            assert!(next.cancel_and_join().unwrap_err().to_string().contains("owned worker panic"));
            assert!(next.is_settled());
        }
    }

    #[test]
    fn queued_or_polled_report_is_never_cancelled_before_report() {
        for poll_first in [false, true] {
            let control = AcquiringReadControl::new(None);
            let worker_control = control.clone();
            let owner = Arc::new(());
            let worker_owner = owner.clone();
            let (sent, ready) = mpsc::channel();
            let mut next = PendingNext::spawn(control, owner, move |send| {
                let mut emit = |part| {
                    let result = offer(&send, worker_owner.clone(), vec![part], &worker_control);
                    sent.send(()).unwrap(); // The complete report is already queued.
                    result
                };
                let result = crate::storage::runtime()?.block_on(
                    crate::orient::delivery::complete_report(&mut Out::new(&mut emit),
                        "complete report", "fixture", &mut |_| {}));
                completion(result, true)
            }).unwrap();
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            let report = if poll_first { next.poll().unwrap() } else { None };
            assert!(!next.is_settled());
            assert!(next.cancel_and_join().is_err());
            assert!(next.is_settled());
            assert!(next.cancel_and_join().is_err(), "second join must retain the failure");
            assert!(next.poll().is_err());
            let report = report.unwrap_or_else(|| next.ready.try_recv().unwrap());
            assert!(next.acknowledge(report).is_err(), "cancellation must not become acceptance");
        }
    }

    #[test]
    fn queued_timeout_is_completed_output_not_idle_cancellation() {
        let owner = Arc::new(());
        let worker_owner = owner.clone();
        let (sent, ready) = mpsc::channel();
        let mut next = PendingNext::spawn(AcquiringReadControl::new(None), owner, move |send| {
            offer_timeout(&send, worker_owner, vec![Part::Text { text: "timeout".into() }])?;
            sent.send(()).unwrap();
            Ok(NextCompletion::Completed)
        }).unwrap();
        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(next.cancel_and_join().unwrap(), NextCompletion::Completed);
        assert!(next.is_settled());
        let report = next.poll().unwrap().unwrap();
        assert_eq!(report.kind(), NextKind::Timeout);
        next.acknowledge(report).unwrap();
        assert!(next.poll().unwrap().is_none());
    }

    struct Fixture {
        storage: Storage,
        persona: Id,
        event: Id,
        _directory: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let pile = directory.path().join("wait.pile");
            let key = directory.path().join("wait.key");
            std::fs::File::create(&pile).unwrap();
            let signer = initialize_signer(&pile, Some(&key)).unwrap();
            let storage = Storage::shared_local(pile, Some(key));
            let routes = storage.with_store(|store, signer, _| {
                let mut routes = CollectionRoutes::new(); // Fixture's explicit config, not cached facts.
                for (scope, name) in crate::collection_names::table() {
                    let authority = AdmissionPolicy::direct(signer.verifying_key());
                    routes.insert(scope, store.collection(name,
                        CollectionPolicy::new(authority.clone(), authority))?.handle());
                }
                Ok(routes)
            }).unwrap();
            let storage = storage.with_collections(routes).with_expected_signer(signer.verifying_key());
            let persona = genid().id;
            let sender = genid().id;
            let event = storage.with_store(|store, signer, runtime| {
                let sources = runtime.block_on(OrientSources::open(&storage, store, signer, true))?;
                for (person, label) in [(persona, "isolated-zooid"), (sender, "sender")] {
                    let (fragment, _, _) = relations::person_fragment(person,
                        crate::relations::ProfileInput { label: label.to_owned(), ..Default::default() })?;
                    store.commit(sources.relations.source, signer, fragment)?;
                }
                let (fragment, event) = message::message_fragment(sender,
                    &message::Recipient::Person(persona), "complete native report\nwith second line",
                    clock::point_now()?);
                store.commit(sources.messages.source, signer, fragment)?;
                Ok(event)
            }).unwrap();
            Self { storage, persona, event, _directory: directory }
        }

        fn start(&self) -> PendingNext {
            Orient::with_storage(self.storage.clone()).begin_next(&fmt_id(self.persona),
                &WaitOptions { timeout: Some(Duration::from_secs(20)),
                    poll_interval: Duration::from_millis(10), ..Default::default() })
                .unwrap().start().unwrap()
        }

        fn presented(&self) -> bool {
            self.presented_event(self.event)
        }

        fn presented_event(&self, selected: Id) -> bool {
            self.storage.try_with_store(|store, signer, runtime| {
                let source = runtime.block_on(ReceiptSource::open(&self.storage, store, signer))?.source;
                let facts = store.snapshot()?.collection(source)?.view::<TribleSet>()?;
                Ok(find!(event: Id, pattern!(&facts, [{ presentation::event: ?event }]))
                    .any(|event| event == selected))
            }).unwrap().expect("prepared output must not hold the shared Storage borrow")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) { self.storage.close().unwrap(); }
    }

    #[test]
    fn native_catalog_observations_do_not_present_and_generic_delivery_still_does() {
        use crate::mcp::catalog::{Catalog, Config};
        for name in ["orient_show", "orient_wake", "orient_poll"] {
            let fixture = Fixture::new();
            let goal = fixture.storage.with_store(|store, signer, runtime| {
                let sources = runtime.block_on(OrientSources::open(&fixture.storage, store, signer, true))?;
                let (fragment, goal) = compass::goal_fragment("native overview goal",
                    vec!["isolated-zooid".into()], None, clock::point_now()?)?;
                store.commit(sources.compass.source, signer, fragment)?;
                Ok(goal)
            }).unwrap();
            let native = Catalog::with_native_storage(Config::new(fixture.storage.path()), fixture.storage.clone());
            let generic = Catalog::with_storage(Config::new(fixture.storage.path()), fixture.storage.clone());
            let args = |peek: bool| {
                let mut args = serde_json::json!({"persona": fmt_id(fixture.persona)});
                match name {
                    "orient_poll" => args["peek"] = peek.into(),
                    "orient_wake" => args["chars"] = 0.into(),
                    _ => {}
                }
                anybytes::Bytes::from(serde_json::to_vec(&args).unwrap())
            };
            let cancelled = AtomicBool::new(false);
            assert!(!fixture.presented());
            assert!(!fixture.presented_event(goal));
            let result = native.invoke(name, args(true), crate::mcp::Limits::default(), &cancelled).unwrap();
            assert!(!result.is_error);
            assert!(result.parts.iter().any(|part| matches!(part, Part::Text { text }
                if text.contains("native overview goal"))));
            assert!(!fixture.presented(), "native {name} must not consume message attention");
            assert!(!fixture.presented_event(goal), "native {name} must not consume goal attention");
            let result = generic.invoke(name, args(false), crate::mcp::Limits::default(), &cancelled).unwrap();
            assert!(!result.is_error);
            assert!(fixture.presented_event(goal), "generic {name} keeps its accepted-output receipt");
            assert_eq!(fixture.presented(), name != "orient_wake", "wake presents goals, not messages");
        }
    }

    #[test]
    fn native_catalog_rejects_consuming_poll_before_storage_or_output() {
        use crate::mcp::catalog::{Catalog, Config};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("must-not-open.pile");
        let catalog = Catalog::with_native_storage(Config::new(&path), Storage::shared(path.clone(), None));
        let (tool, schema) = catalog.tools().find(|(tool, _)| tool.name == "orient_poll").unwrap();
        let schema: serde_json::Value = serde_json::from_str(schema).unwrap();
        assert_eq!(schema["properties"]["peek"]["const"], true);
        assert!(tool.description.contains("Only peek=true"));
        let error = catalog.invoke("orient_poll", anybytes::Bytes::from(
            br#"{"persona":"fixture-zooid","peek":false}"#.to_vec()),
            crate::mcp::Limits::default(), &AtomicBool::new(false)).err().expect("consuming bypass must reject");
        assert!(error.is::<crate::mcp::InvalidArguments>());
        assert!(error.to_string().contains("only peek=true"));
        assert!(!path.exists(), "rejection must precede Storage/output");
    }

    #[test]
    fn next_wait_releases_shared_owner_and_presents_only_after_acceptance() {
        let fixture = Fixture::new();
        let mut next = fixture.start();
        let report = next.ready.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(report.kind(), NextKind::Report);
        assert!(report.parts().iter().any(|part| matches!(part, Part::Text { text }
            if text.contains("complete native report\nwith second line"))));
        assert!(!fixture.presented());
        // Simulate successful complete Archive/ledger publication. This is
        // deliberately after the concurrent ordinary same-store query above.
        next.acknowledge(report).unwrap();
        assert!(next.is_settled());
        assert!(fixture.presented());
    }

    #[test]
    fn cancel_and_join_after_accepted_report_is_completed() {
        let fixture = Fixture::new();
        let mut next = fixture.start();
        let report = next.ready.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(report.kind(), NextKind::Report);
        next.acknowledge(report).unwrap();
        assert!(next.is_settled());
        // The settled outcome is retained: a later cancel cannot rewrite it.
        for _ in 0..2 {
            assert_eq!(next.cancel_and_join().unwrap(), NextCompletion::Completed);
        }
        assert!(next.poll().unwrap().is_none());
        assert!(fixture.presented());
    }

    #[test]
    fn cancel_after_offered_report_is_never_cancelled_before_report() {
        let fixture = Fixture::new();
        let mut next = fixture.start();
        // The complete report is offered and held by the caller.
        let report = next.ready.recv_timeout(Duration::from_secs(10)).unwrap();
        assert_eq!(report.kind(), NextKind::Report);
        for _ in 0..2 {
            let result = next.cancel_and_join();
            assert!(!matches!(result, Ok(NextCompletion::CancelledBeforeReport)),
                "an offered report must not be retired as idle cancellation");
            assert!(result.is_err(), "cancellation after an offer is not completion either");
            assert!(next.is_settled());
        }
        assert!(next.acknowledge(report).is_err(), "cancellation must not become acceptance");
        assert!(!fixture.presented());
    }

    #[test]
    fn worker_error_that_is_not_cancellation_stays_an_error() {
        for error in [
            anyhow!("independent worker failure"),
            AcquiringReadInterrupted::DeadlineExceeded.into(),
            anyhow::Error::from(AcquiringReadInterrupted::Cancelled).context("independent close failure"),
        ] {
            let text = format!("{error:#}");
            let (entered, ready) = mpsc::channel();
            let (release, go) = mpsc::channel();
            // Not cancelled: the worker fails on its own.
            let mut next = PendingNext::spawn(AcquiringReadControl::new(None), Arc::new(()),
                move |_| {
                    entered.send(()).unwrap();
                    go.recv().unwrap();
                    completion(Err(error), false)
                }).unwrap();
            ready.recv_timeout(Duration::from_secs(5)).unwrap();
            release.send(()).unwrap();
            for _ in 0..2 {
                let error = next.cancel_and_join().unwrap_err();
                assert!(format!("{error:#}").contains(&text), "the retained error is the worker's own");
                assert!(!is_cancelled(&error));
                assert!(next.is_settled());
            }
            assert!(next.poll().is_err());
        }
    }

    #[test]
    fn next_rejected_or_cancelled_publication_never_presents() {
        let fixture = Fixture::new();
        let mut rejected = fixture.start();
        let report = rejected.ready.recv_timeout(Duration::from_secs(10)).unwrap();
        drop(report); // Archive publication failed; Out acceptance is withheld.
        assert!(rejected.join().is_err());
        assert!(!fixture.presented());
        let mut cancelled = fixture.start();
        let report = cancelled.ready.recv_timeout(Duration::from_secs(10)).unwrap();
        cancelled.control.cancel(); // Even if publication just succeeded, ack may fail.
        assert!(cancelled.acknowledge(report).is_err());
        assert!(cancelled.is_settled());
        assert!(!fixture.presented());
    }

    #[test]
    fn next_prepared_report_survives_selection_deadline_until_accepted() {
        let fixture = Fixture::new();
        let next = Orient::with_storage(fixture.storage.clone()).begin_next(
            &fmt_id(fixture.persona), &WaitOptions {
                timeout: Some(Duration::from_secs(5)),
                poll_interval: Duration::from_millis(10), ..Default::default()
            }).unwrap();
        let deadline = next.control.deadline().unwrap();
        let mut next = next.start().unwrap();
        let report = next.ready.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(report.kind(), NextKind::Report);
        std::thread::sleep(deadline.saturating_duration_since(Instant::now())
            + Duration::from_millis(10));
        assert!(!fixture.presented());
        // Simulate a slow tokenizer/rollover and publication. The same body
        // and worker survive; there is no second timeout or observation.
        assert!(matches!(next.ready.try_recv(), Err(TryRecvError::Empty)));
        next.acknowledge(report).unwrap();
        assert!(next.is_settled());
        assert!(fixture.presented());
    }

    #[test]
    fn next_busy_owner_can_cancel_without_entering_or_closing_another_store() {
        let fixture = Fixture::new();
        fixture.storage.with_store(|_, _, _| {
            let mut next = fixture.start();
            next.control.cancel();
            assert_eq!(next.cancel_and_join().unwrap(), NextCompletion::CancelledBeforeReport);
            assert!(next.is_settled());
            assert_eq!(next.cancel_and_join().unwrap(), NextCompletion::CancelledBeforeReport);
            assert!(next.poll().is_err(), "cancellation is not a delivered report");
            Ok(())
        }).unwrap();
        assert!(!fixture.presented());
    }

    #[test]
    fn prepared_call_yields_shared_runtime_to_another_wait_timeout_and_cancel() {
        let fixture = Fixture::new();
        let mut first = fixture.start();
        let report = first.ready.recv_timeout(Duration::from_secs(10)).unwrap();
        let quiet_persona = genid().id;
        fixture.storage.with_store(|store, signer, runtime| {
            let sources = runtime.block_on(OrientSources::open(&fixture.storage, store, signer, true))?;
            let (person, _, _) = relations::person_fragment(quiet_persona,
                crate::relations::ProfileInput { label: "quiet-other-zooid".into(), ..Default::default() })?;
            store.commit(sources.relations.source, signer, person)?;
            Ok(())
        }).unwrap();
        let start = |seconds| Orient::with_storage(fixture.storage.clone())
            .begin_next(&fmt_id(quiet_persona), &WaitOptions {
                timeout: Some(Duration::from_secs(seconds)), poll_interval: Duration::from_millis(10),
                ..Default::default()
            }).unwrap().start().unwrap();
        // The wait's deadline also bounds acquiring its snapshot, and under a
        // loaded whole suite one second was not always enough to acquire
        // (2026-11-12: "exact acquisition deadline exceeded"). Five seconds
        // still exercises a real timeout.
        let mut second = start(5);
        let timeout = second.ready.recv_timeout(Duration::from_secs(30)).unwrap();
        assert_eq!(timeout.kind(), NextKind::Timeout);
        second.acknowledge(timeout).unwrap();
        assert!(second.is_settled());
        let mut cancelled = start(20);
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(cancelled.cancel_and_join().unwrap(), NextCompletion::CancelledBeforeReport);
        assert!(cancelled.is_settled());
        assert_eq!(cancelled.cancel_and_join().unwrap(), NextCompletion::CancelledBeforeReport);
        assert!(cancelled.poll().is_err());
        assert!(!first.is_settled());
        assert!(!fixture.presented());
        first.acknowledge(report).unwrap();
        assert!(first.is_settled());
        assert!(fixture.presented());
    }

    #[test]
    fn prepared_report_cannot_acknowledge_a_different_pending_call() {
        let fixture = Fixture::new();
        let mut first = fixture.start();
        let first_report = first.ready.recv_timeout(Duration::from_secs(10)).unwrap();
        let mut second = fixture.start();
        let second_report = second.ready.recv_timeout(Duration::from_secs(10)).unwrap();
        assert!(first.acknowledge(second_report).unwrap_err().to_string().contains("another pending call"));
        assert!(second.join().is_err());
        assert!(second.is_settled());
        assert!(!fixture.presented());
        first.acknowledge(first_report).unwrap();
        assert!(first.is_settled());
        assert!(fixture.presented());
    }

    #[test]
    fn wait_snapshot_refresh_keeps_its_deadline_visible() {
        // Under a loaded suite a quiet wait's snapshot refresh met its
        // acquisition deadline, and formatting that error into a string hid
        // it from is_deadline: the worker ended with an error instead of a
        // timeout report (2026-11-17 on sky). This checks the property the
        // fix in operations.rs relies on: a deadline wrapped the way that
        // refresh wraps it (WaitSnapshotError, which keeps the source) stays
        // visible to is_deadline and is not read as a cancellation. It does
        // not drive operations.rs itself.
        use triblespace::core::repo::SnapshotSource;
        let fixture = Fixture::new();
        fixture.storage.scope(|storage| {
            let control = AcquiringReadControl::new(Some(Instant::now() + Duration::from_millis(200)));
            let (mut store, _, _) = crate::storage::BorrowedStore::open(storage.clone(), control, Arc::new(AtomicBool::new(false)))?;
            std::thread::sleep(Duration::from_millis(250));
            let error: anyhow::Error = store.snapshot()
                .map_err(|error| WaitSnapshotError(error.into())).err()
                .expect("a refresh past its deadline fails").into();
            assert!(is_deadline(&error), "{error:#}");
            assert!(!is_cancelled(&error), "{error:#}");
            assert!(completion(Err(error), false).is_err(), "a deadline is not a cancellation");
            Ok(())
        }).unwrap();
    }
}
