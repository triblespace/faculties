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

/// Exact complete output, not a clipped preview. Holding or dropping this
/// value does not acknowledge news. The owning PendingNext retains the worker.
pub struct PreparedReport {
    parts: Vec<Part>,
    kind: NextKind,
    accept: Sender<()>,
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
}

/// The exact worker remains owned through preparation, publication, receipt
/// completion and cancellation. No task is detached when a timeout is observed.
pub struct PendingNext {
    control: AcquiringReadControl,
    ready: Receiver<PreparedReport>,
    worker: Option<std::thread::JoinHandle<Result<()>>>,
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
            control: AcquiringReadControl::new(deadline(options)?) })
    }
}

impl Next {
    pub fn start(self) -> Result<PendingNext> {
        let (send, ready) = mpsc::channel();
        let control = self.control.clone();
        let worker = std::thread::Builder::new().name("orient-next".to_owned())
            .spawn(move || self.run(send)).context("start owned Orient next call")?;
        Ok(PendingNext { control, ready, worker: Some(worker) })
    }

    fn run(self, send: Sender<PreparedReport>) -> Result<()> {
        self.orient.storage.scope(|storage| {
            let prepared = Arc::new(AtomicBool::new(false));
            let opened = crate::storage::BorrowedStore::open(
                storage.clone(), self.control.clone(), prepared.clone(),
            );
            let (mut store, signer, runtime) = match opened {
                Ok(opened) => opened,
                Err(error) if is_deadline(&error) => return offer_timeout(&send,
                    vec![Part::Text { text: "Orient next reached its deadline before the shared store became available.\n".to_owned() }]),
                Err(error) => return Err(error),
            };
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
                        offer(&send, std::mem::take(&mut parts), &self.control.without_deadline())?;
                    }
                    Ok(())
                };
                runtime.block_on(observe_wait(
                    storage, &mut store, &signer, storage.path(), Some(&self.persona),
                    &self.options, self.orient.health_max_age, self.orient.service_events_collection,
                    false, self.control.deadline(), &mut Out::new(&mut emit),
                ))
            };
            match result {
                Ok(outcome) if !outcome.news_printed => {
                    parts.push(Part::Text { text: format!("{}\n", outcome.timeout_text()) });
                    offer_timeout(&send, parts)
                }
                Ok(_) => Ok(()),
                Err(error) if !offered.get() && is_deadline(&error) => {
                    parts.push(Part::Text { text:
                        "Orient next reached its deadline before a complete readable report was accepted.\n".to_owned() });
                    offer_timeout(&send, parts)
                }
                Err(error) => Err(error),
            }
        })
    }
}

pub(super) fn is_deadline(error: &anyhow::Error) -> bool {
    error.chain().any(|source| matches!(source.downcast_ref::<AcquiringReadInterrupted>(),
        Some(AcquiringReadInterrupted::DeadlineExceeded)))
}

fn offer(send: &Sender<PreparedReport>, parts: Vec<Part>, control: &AcquiringReadControl) -> Result<()> {
    control.check()?;
    let (accept, accepted) = mpsc::channel();
    send.send(PreparedReport { parts, kind: NextKind::Report, accept })
        .map_err(|_| anyhow!("Orient result owner closed before publication"))?;
    loop {
        control.check()?;
        match accepted.recv_timeout(Duration::from_millis(10)) {
            Ok(()) => { control.check()?; return Ok(()); }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                bail!("Orient complete report was not accepted for publication");
            }
        }
    }
}

fn offer_timeout(send: &Sender<PreparedReport>, parts: Vec<Part>) -> Result<()> {
    // No pending Presented commit follows a quiet timeout. Its output remains
    // owned by the caller even though the observation worker can now finish.
    let (accept, _) = mpsc::channel();
    send.send(PreparedReport { parts, kind: NextKind::Timeout, accept })
        .map_err(|_| anyhow!("Orient result owner closed before timeout delivery"))
}

impl PendingNext {
    /// A prepared complete result is available before worker completion. This
    /// lets Shell publish it before releasing Out's acceptance boundary.
    pub fn poll(&mut self) -> Result<Option<PreparedReport>> {
        match self.ready.try_recv() {
            Ok(report) => Ok(Some(report)),
            Err(TryRecvError::Empty) => Ok(None),
            Err(TryRecvError::Disconnected) => {
                self.join()?;
                Ok(None)
            }
        }
    }

    /// Call after complete ledger and native Archive publication, never after
    /// a clipped preview or merely buffering the worker's output. Join returns
    /// the real Presented/close outcome. Publication is not rolled back if
    /// cancellation, receipt writing or close then fails.
    pub fn acknowledge(&mut self, report: PreparedReport) -> Result<()> {
        if report.kind == NextKind::Report && report.accept.send(()).is_err() {
            self.join()?;
            bail!("Orient receipt worker ended before acceptance");
        }
        self.join()
    }

    pub fn cancel_and_join(&mut self) -> Result<()> {
        self.control.cancel();
        self.join()
    }

    pub fn is_settled(&self) -> bool { self.worker.is_none() }

    fn join(&mut self) -> Result<()> {
        match self.worker.take() {
            Some(worker) => worker.join().map_err(|_| anyhow!("Orient next worker panicked"))?,
            None => Ok(()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{CollectionRoutes, initialize_signer};
    use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};

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
            self.storage.try_with_store(|store, signer, runtime| {
                let source = runtime.block_on(ReceiptSource::open(&self.storage, store, signer))?.source;
                let facts = store.snapshot()?.collection(source)?.view::<TribleSet>()?;
                Ok(find!(event: Id, pattern!(&facts, [{ presentation::event: ?event }]))
                    .any(|event| event == self.event))
            }).unwrap().expect("prepared output must not hold the shared Storage borrow")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) { self.storage.close().unwrap(); }
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
            assert!(next.cancel_and_join().is_err());
            assert!(next.is_settled());
            Ok(())
        }).unwrap();
        assert!(!fixture.presented());
    }
}
