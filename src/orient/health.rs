//! Resident-only health input with admitted foreground maintenance.
//! Opening its configured descriptors may acquire their exact bytes; report
//! maintenance and passive attention remain local and never fetch health payloads.
//! An explicit `show` acquires the fixed selected report and its queried labels.
//!
//! The maintained targets remain the query sources. The values built below
//! are just one report's output and attention IDs, never a second health store.

use super::*;
use crate::schemas::orient::{
    KIND_NEWS_PATH_BEAT, KIND_NEWS_PATH_DELIVERED, KIND_NEWS_PATH_EXIT, KIND_NEWS_PATH_RETRY,
};
use crate::schemas::swarm_health::{self as schema, attrs};
use std::sync::{Arc, Mutex};
use triblespace::core::collection::descriptor;
use triblespace::macros::id_hex;
use triblespace_net::telemetry;

pub(super) struct HealthSources {
    health: OrientSource,
    latest: Collection<LwwRegisterBlob>,
    relations: OrientSource,
    service_events: Option<OrientSource>,
    pub(super) presentations: ReceiptSource,
    max_age: Duration,
    // The prefix before successful health upkeep and an optional receipt
    // attempt, not its final query snapshot: an append racing upkeep must
    // remain changed on the next poll.
    maintained_view: Option<(ed25519_dalek::VerifyingKey, FacultySnapshot)>,
    // A failed receipt attempt is not a freshness claim. Retain its note with
    // that input/signer so unchanged polls report it without repeating upkeep.
    receipt_maintenance_note: Option<String>,
    // One process-local observation, not another persisted health model. The
    // cached views and watermark belong to exactly the same sampled prefix.
    poll_view: Option<(FacultySnapshot, HealthObservation)>,
    // Present only for a continuous observer, which is a window's news path.
    news_path: Option<NewsPathBeat>,
    #[cfg(test)]
    poll_observations: usize,
    #[cfg(test)]
    maintenance_passes: std::cell::Cell<usize>,
}

impl HealthSources {
    pub(super) async fn open(
        routing: &Storage,
        pile: &mut FacultyStore,
        signer: &SigningKey,
        max_age: Duration,
    ) -> Result<Self> {
        let health = OrientSource::open(
            routing,
            pile,
            signer,
            schema::DEFAULT_SCOPE_ID,
            "Swarm health",
        )
        .await?;
        let relations =
            OrientSource::open(routing, pile, signer, RELATIONS_SCOPE_ID, "Relations").await?;
        let presentations = ReceiptSource::open(routing, pile, signer).await?;
        let mut local = pile.store();
        let latest = local.attach::<LwwRegisterBlob>(
            health.source,
            (attrs::node.id(), metadata::created_at.id()),
        )?;
        drop(local);
        Ok(Self {
            health,
            latest,
            relations,
            service_events: None,
            presentations,
            max_age,
            maintained_view: None,
            receipt_maintenance_note: None,
            poll_view: None,
            news_path: None,
            #[cfg(test)]
            poll_observations: 0,
            #[cfg(test)]
            maintenance_passes: std::cell::Cell::new(0),
        })
    }

    pub(super) async fn with_service_events(
        mut self,
        pile: &mut FacultyStore,
        signer: &SigningKey,
        handle: Option<CollectionHandle>,
    ) -> Result<Self> {
        if let Some(handle) = handle {
            self.service_events = Some(service_events::open(pile, signer, handle).await?);
        }
        Ok(self)
    }

    fn maintain(&self, pile: &FacultyStore, signer: &SigningKey) -> Result<Option<String>> {
        #[cfg(test)]
        self.maintenance_passes
            .set(self.maintenance_passes.get() + 1);
        let mut local = pile.store();
        // Pile acquisition is immediately resident-only. Run these local
        // mapping futures to completion without yielding a Peer store guard
        // across network I/O or re-entering a Peer operation.
        pollster::block_on(async {
            for source in [&self.health, &self.relations]
                .into_iter()
                .chain(self.service_events.as_ref())
            {
                // A report left unattached is read from its own bytes.
                crate::storage::tolerate_own_lag(
                    local.maintain_attached(source.succinct, signer).await,
                )?;
                crate::storage::tolerate_own_lag(
                    local.maintain_attached(source.rank9, signer).await,
                )?;
            }
            crate::storage::tolerate_own_lag(local.maintain_attached(self.latest, signer).await)?;
            // As on the ordinary Orient path, receipt freshness avoids a
            // repeat but is not a precondition of reporting resident health.
            // Only this optional projection is best effort; health, Relations
            // and latest-target failures above still propagate unchanged. It
            // is the ordinary path's own upkeep on the resident-only store,
            // so a receipt left unattached is read from its own bytes here
            // too.
            let receipt_result = self.presentations.maintain(&mut *local, signer).await;
            Ok(receipt_result.err().map(|error| {
                format!(
                    "note: Orient receipt membership not refreshed ({error:#}); a recent event may repeat"
                )
            }))
        })
    }

    pub(super) fn observe(
        &self,
        pile: &mut FacultyStore,
        signer: &SigningKey,
    ) -> Result<HealthObservation> {
        // Baseline/show select health, not presentation freshness. Poll has an
        // Out through which it reports an optional receipt-attempt failure.
        let _receipt_note = self.maintain(pile, signer)?;
        let now = clock::now()?;
        self.at(pile.snapshot()?, now)
    }

    /// The explicitly requested dashboard may fetch missing authority and
    /// selected report bytes. This first selection uses the same frozen
    /// records and proofs throughout; passive attention uses `at` instead.
    pub(super) async fn observe_acquiring(
        &self,
        pile: &mut FacultyStore,
        signer: &SigningKey,
    ) -> Result<HealthObservation> {
        let _receipt_note = self.maintain(pile, signer)?;
        let snapshot = pile.snapshot()?;
        let now = clock::now()?;
        let health = self.health;
        let relations = self.relations;
        let latest_target = self.latest;
        let presentations_target = self.presentations.rank9;
        let max_age = self.max_age;
        // Service observations remain resident-only even for a requested
        // health dashboard. Missing code bytes are not evidence of a cause.
        let service_events = self
            .service_events
            .map(|source| source.observe(&snapshot))
            .transpose()?;
        let runtime = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            let reader = crate::storage::AcquiringReader::with_handle(snapshot.clone(), runtime);
            let observe = |source: OrientSource| -> Result<OrientFact> {
                let collection = reader.attached_acquiring(source.rank9)?;
                let view = crate::storage::acquire_attached_facts(&collection)?;
                Ok(OrientFact {
                    collection: collection.into_frozen(),
                    view,
                })
            };
            let latest_collection = reader.attached_acquiring(latest_target)?;
            let latest = crate::storage::require_complete_attached_read(
                latest_collection.read_acquiring::<LwwIndex>()?,
            )?
            .query()?;
            let presentations = reader.attached(presentations_target)?;
            Ok(HealthObservation {
                snapshot,
                evaluated_at: now,
                facts: observe(health)?,
                latest,
                latest_collection: latest_collection.into_frozen(),
                relations: observe(relations)?,
                // Show presents this dashboard regardless of old receipts.
                // Do not fetch historical receipt bytes it never consults.
                presentations: ReceiptObservation {
                    collection: presentations.into_frozen(),
                    view: FactArchive::new(vec![
                        triblespace::core::blob::encodings::succinctarchive::SuccinctArchive::from(
                            &TribleSet::new(),
                        ),
                    ]),
                },
                max_age,
                service_events,
            })
        })
        .await
        .context("join frozen health input selection")?
    }

    fn maintain_if_changed(
        &mut self,
        pile: &mut FacultyStore,
        signer: &SigningKey,
    ) -> Result<bool> {
        let before = pile.snapshot()?;
        let subject = signer.verifying_key();
        if self
            .maintained_view
            .as_ref()
            .is_some_and(|(prior_subject, prior)| {
                *prior_subject == subject && before.changes_since(prior).is_empty()
            })
        {
            return Ok(false);
        }
        let receipt_note = self.maintain(pile, signer)?;
        // Our own publications can cause one extra quiet upkeep. Advancing to
        // a post-upkeep snapshot instead could silently cover a concurrent
        // append to a source whose mapping has already run.
        self.maintained_view = Some((subject, before));
        self.receipt_maintenance_note = receipt_note;
        Ok(true)
    }

    /// Health is delivered before any ordinary source or payload acquisition.
    /// An unresolved resident persona leaves the normal resolution path intact.
    pub(super) async fn poll(
        &mut self,
        pile: &mut FacultyStore,
        signer: &SigningKey,
        input: &str,
        peek: bool,
        output: &mut Out<'_>,
    ) -> Result<(bool, Option<Epoch>)> {
        self.poll_in(pile, signer, input, peek, NewsForm::Report, output)
            .await
    }

    /// [`Self::poll`], writing any report in `form`.
    pub(super) async fn poll_in(
        &mut self,
        pile: &mut FacultyStore,
        signer: &SigningKey,
        input: &str,
        peek: bool,
        form: NewsForm,
        output: &mut Out<'_>,
    ) -> Result<(bool, Option<Epoch>)> {
        self.maintain_if_changed(pile, signer)?;
        if let Some(note) = &self.receipt_maintenance_note {
            output.line(note)?;
        }
        let now = clock::now()?;
        let sampled = pile.snapshot()?;
        self.refresh_poll_view(sampled, now)?;
        self.beat_if_due(pile, signer, input, now, output)?;
        let (_, observation) = self
            .poll_view
            .as_ref()
            .expect("poll selected a health view");
        let report = observation.attention();
        if report.attention.is_empty() {
            return Ok((false, report.next_change));
        }
        let persona = match observation.persona(input) {
            Ok(persona) => persona,
            Err(error) if is_payload_pending(&error) || is_persona_not_found(&error) => {
                return Ok((false, report.next_change));
            }
            Err(error) => return Err(error),
        };
        let news = observation.news(persona, &report);
        let fired = matches!(news, News::Report { .. });
        apply_prepared_news(
            pile,
            signer,
            self.presentations.source,
            peek,
            &news,
            "",
            form,
            output,
            &mut path_observer(self.news_path_target()),
        )
        .await?;
        Ok((fired, report.next_change))
    }

    fn refresh_poll_view(&mut self, sampled: FacultySnapshot, now: Epoch) -> Result<()> {
        let probe = RefreshProbe::begin(
            "health",
            &sampled,
            self.poll_view.as_ref().map(|(watermark, _)| watermark),
            None,
        );
        let changed = self
            .poll_view
            .as_ref()
            .map_or(true, |(_, observation)| !observation.is_current(&sampled));
        if changed {
            let observation = self.at(sampled.clone(), now);
            if let Some(probe) = &probe {
                if let Ok(current) = &observation {
                    let previous = self.poll_view.as_ref().map(|(_, observation)| observation);
                    trace_refresh_line(format_args!(
                        "event=baseline refresh={} scope=health baseline={}",
                        probe.id,
                        if previous.is_some() {
                            "previous_observation"
                        } else {
                            "absent"
                        },
                    ));
                    probe.fact("Swarm health", previous.map(|p| &p.facts), &current.facts);
                    probe.fact(
                        "Relations",
                        previous.map(|p| &p.relations),
                        &current.relations,
                    );
                    probe.cover(
                        "Orient receipts",
                        previous.map(|p| p.presentations.collection.cover()),
                        current.presentations.collection.cover(),
                    );
                    probe.cover(
                        "Swarm health latest",
                        previous.map(|p| p.latest_collection.cover()),
                        current.latest_collection.cover(),
                    );
                }
                probe.finish(if observation.is_ok() {
                    "ready"
                } else {
                    "error"
                });
            }
            let observation = observation?;
            self.poll_view = Some((sampled, observation));
            #[cfg(test)]
            {
                self.poll_observations += 1;
            }
        } else {
            // Unrelated appends and time alone leave these target views valid.
            // Refresh the payload reader and health evaluation time so newly
            // resident labels, expiry and clock rollback need no reattachment.
            let observation = &mut self
                .poll_view
                .as_mut()
                .expect("an unchanged poll has a selected health view")
                .1;
            observation.snapshot = sampled;
            observation.evaluated_at = now;
            if let Some(probe) = &probe {
                probe.finish("retained");
            }
        }
        Ok(())
    }

    fn at(&self, snapshot: FacultySnapshot, now: Epoch) -> Result<HealthObservation> {
        let facts = self.health.observe(&snapshot)?;
        let latest_collection = trace_refresh_call("Swarm health latest", "attach", || {
            snapshot.attached(self.latest)
        })?;
        // The register over the same foundations the health facts read: a
        // report no attachment reaches yet is built in memory.
        let latest_index = trace_refresh_call("Swarm health latest", "view", || {
            latest_collection
                .read::<LwwIndex>()
                .map(|read| read.into_value())
        })?;
        let latest = trace_refresh_call("Swarm health latest", "query", || latest_index.query())?;
        let relations = self.relations.observe(&snapshot)?;
        let presentations = self.presentations.observe(&snapshot)?;
        let service_events = self
            .service_events
            .map(|source| source.observe(&snapshot))
            .transpose()?;
        Ok(HealthObservation {
            snapshot,
            evaluated_at: now,
            facts,
            latest,
            latest_collection,
            relations,
            presentations,
            max_age: self.max_age,
            service_events,
        })
    }
}

/// Writer cadence of a continuous observer's news-path beat (writer policy).
pub(super) const NEWS_PATH_BEAT_EVERY: Duration = Duration::from_secs(5 * 60);
/// Reader policy: a news path whose latest beat is older than this is silent.
/// So is one whose latest RETRY's next try is overdue by this much.
pub(super) const NEWS_PATH_MAX_AGE: Duration = Duration::from_secs(20 * 60);
/// Reader policy: a stretch still degraded, silent or exited this long after
/// it began is said to peers once more.
pub(super) const NEWS_PATH_ESCALATE_AFTER: Duration = Duration::from_secs(60 * 60);
/// Tag of the escalation event that follows a stretch's first event.
///
/// Minted with `trible genid` on 2026-10-29.
const NEWS_PATH_ESCALATION: Id = id_hex!("5FEBE690EA87EC872B5A16396D11F631");
/// Tag of the silence event that follows a window's latest beat or
/// acceptance.
///
/// Minted with `trible genid` on 2026-10-29.
const NEWS_PATH_SILENCE: Id = id_hex!("C17110071EBCFEC8581BC6AFCFB11184");
/// Tag of the silence event that follows an overdue RETRY.
///
/// Minted with `trible genid` on 2026-10-29.
const NEWS_PATH_OVERDUE: Id = id_hex!("BF769540B346A0B226F41700C40ECCE5");
/// The exit reason is one line of an error chain, not a log.
const NEWS_PATH_REASON_CHARS: usize = 400;

/// One observer process's beat state: process-local, never a loaded model.
struct NewsPathBeat {
    session: Id,
    window: Option<Id>,
    /// The last publication attempt, successful or not: the beat interval
    /// throttles failures too, so a refused write is not retried every poll.
    last_attempt: Option<Epoch>,
    /// Whether this session's first beat has been published. Only a
    /// published first beat consumes the recovery marker.
    published: bool,
    delivery: Arc<Mutex<DeliveryState>>,
}

/// What this process last observed of its own deliveries. Shared with the
/// delivery seam through [`NewsPathTarget`], so a retry or an acceptance is
/// recorded by the store that seam already owns.
#[derive(Debug, Default)]
struct DeliveryState {
    /// An accepted delivery since the session began or since the last retry.
    proven: bool,
    /// The exit, silence or retry the next DELIVERED record resolves.
    resolves: Option<Id>,
    /// When the current run of retries began, or the start of the run a
    /// carried retry belongs to: only an acceptance ends a run.
    degraded_since: Option<Epoch>,
}

/// Everything a news-path record needs, cloned out of the observer: the
/// health collection, persona, session and this process's delivery state.
#[derive(Clone, Debug)]
pub(super) struct NewsPathTarget {
    collection: Collection<SimpleArchive>,
    persona: Id,
    session: Id,
    delivery: Arc<Mutex<DeliveryState>>,
}

/// Record one deferred attempt before its backoff. Called by the delivery
/// seam with the store it already owns; writes nothing else and polls nothing.
pub(super) fn record_news_path_retry(
    store: &mut FacultyStore,
    signer: &SigningKey,
    path: &NewsPathTarget,
    attempt: u64,
    reason: &str,
    delay: Duration,
) -> Result<()> {
    let now = clock::now()?;
    let mut delivery = path
        .delivery
        .lock()
        .expect("delivery state is not poisoned");
    // The observation stands whether or not its record lands: the next
    // acceptance is a transition either way.
    delivery.proven = false;
    let since = *delivery.degraded_since.get_or_insert(now);
    let reason = store
        .put::<blobencodings::UTF8String, _>(one_line(reason))
        .map_err(|error| anyhow!("store retry reason: {error}"))?;
    let retry = entity! {
        metadata::tag: &KIND_NEWS_PATH_RETRY,
        attrs::node: &path.persona,
        attrs::session: &path.session,
        metadata::created_at: clock::point(now)?,
        metadata::started_at: clock::point(since)?,
        metadata::expires_at: clock::point(now + span(delay))?,
        metadata::description: reason,
        telemetry::attrs::failed: u128::from(attempt),
    };
    let id = retry.root().expect("one retry entity");
    publish(store, signer, path.collection, retry)?;
    delivery.resolves = Some(id);
    Ok(())
}

/// One delivery observation from the complete-report seam, in plain fields
/// so it is independent of the seam's own notice type.
#[derive(Clone, Copy, Debug)]
pub(super) enum NewsPathNotice<'a> {
    /// The callback deferred this attempt; the next try follows `delay`.
    Retry {
        attempt: u64,
        reason: &'a str,
        delay: Duration,
    },
    /// The callback returned a successful delivery. Not a claim that the news
    /// was handled, nor that its receipt commit settled.
    Accepted,
}

/// The single wiring point for the delivery seam: each notice maps to exactly
/// one of the two record functions.
pub(super) fn on_delivery_notice(
    store: &mut FacultyStore,
    signer: &SigningKey,
    path: &NewsPathTarget,
    notice: NewsPathNotice<'_>,
) -> Result<()> {
    match notice {
        NewsPathNotice::Retry {
            attempt,
            reason,
            delay,
        } => record_news_path_retry(store, signer, path, attempt, reason, delay),
        NewsPathNotice::Accepted => record_news_path_delivered(store, signer, path),
    }
}

/// The observer a complete-report seam calls with the store it already owns.
/// It captures only a clone of health state (no Storage owner), maps each
/// notice to its record, and never fails or delays the delivery: a record
/// that cannot be written is noted on stderr. No news path, no records.
pub(super) fn path_observer(
    target: Option<NewsPathTarget>,
) -> impl FnMut(&mut FacultyStore, &SigningKey, crate::orient::delivery::DeliveryNotice) {
    use crate::orient::delivery::DeliveryNotice;
    move |store: &mut FacultyStore, signer: &SigningKey, notice: DeliveryNotice| {
        let Some(target) = target.as_ref() else {
            return;
        };
        let notice = match notice {
            DeliveryNotice::Retry(retry) => NewsPathNotice::Retry {
                attempt: retry.attempt,
                reason: retry.reason,
                delay: retry.delay,
            },
            DeliveryNotice::Accepted => NewsPathNotice::Accepted,
        };
        if let Err(error) = on_delivery_notice(store, signer, target, notice) {
            eprintln!("note: news-path delivery record not written ({error:#})");
        }
    }
}

/// Record an accepted delivery, but only on a transition: the first in the
/// session, or the first after a retry. Any complete accepted report counts.
pub(super) fn record_news_path_delivered(
    store: &mut FacultyStore,
    signer: &SigningKey,
    path: &NewsPathTarget,
) -> Result<()> {
    let mut delivery = path
        .delivery
        .lock()
        .expect("delivery state is not poisoned");
    if delivery.proven {
        return Ok(());
    }
    let delivered = entity! {
        metadata::tag: &KIND_NEWS_PATH_DELIVERED,
        attrs::node: &path.persona,
        attrs::session: &path.session,
        metadata::created_at: clock::point_now()?,
        metadata::supersedes?: delivery.resolves,
    };
    publish(store, signer, path.collection, delivered)?;
    *delivery = DeliveryState {
        proven: true,
        ..DeliveryState::default()
    };
    Ok(())
}

/// Commit one news-path record, refusing first when this key is not admitted
/// to WRITE: a record readers would ignore is not a publication.
fn publish(
    store: &mut FacultyStore,
    signer: &SigningKey,
    collection: Collection<SimpleArchive>,
    record: Fragment,
) -> Result<()> {
    if !collection
        .writer_is_admitted(&store.snapshot()?, signer.verifying_key())
        .context("check news-path WRITE admission")?
    {
        bail!("this key is not admitted to WRITE the swarm-health collection");
    }
    store
        .commit(collection, signer, record)
        .map_err(|error| anyhow!("commit news-path record: {error}"))?;
    Ok(())
}

impl HealthSources {
    /// Mark this observer as a window's news path: it beats while it polls.
    pub(super) fn start_beating(&mut self) {
        self.news_path = Some(NewsPathBeat {
            session: genid().id,
            window: None,
            last_attempt: None,
            published: false,
            delivery: Arc::default(),
        });
    }

    /// The target a delivery seam records retries and acceptances against,
    /// once this observer's persona has resolved.
    pub(super) fn news_path_target(&self) -> Option<NewsPathTarget> {
        let path = self.news_path.as_ref()?;
        Some(NewsPathTarget {
            collection: self.health.source,
            persona: path.window?,
            session: path.session,
            delivery: Arc::clone(&path.delivery),
        })
    }

    /// Record, best effort, that this news path ended on `error`. The caller
    /// returns its error unchanged whatever happens here: this annotates an
    /// exit, it never decides or alters one.
    pub(super) fn record_exit(
        &self,
        pile: &mut FacultyStore,
        signer: &SigningKey,
        error: &anyhow::Error,
        output: &mut Out<'_>,
    ) {
        let Some(NewsPathBeat {
            session,
            window: Some(window),
            ..
        }) = self.news_path.as_ref()
        else {
            return;
        };
        let mut record = || -> Result<()> {
            let reason = pile
                .put::<blobencodings::UTF8String, _>(one_line(&format!("{error:#}")))
                .map_err(|error| anyhow!("store exit reason: {error}"))?;
            let exit = entity! {
                metadata::tag: &KIND_NEWS_PATH_EXIT,
                attrs::node: window,
                attrs::session: session,
                metadata::created_at: clock::point_now()?,
                metadata::description: reason,
            };
            publish(pile, signer, self.health.source, exit)
        };
        if let Err(note) = record() {
            let _ = output.line(format!("note: news-path exit not recorded ({note:#})"));
        }
    }

    /// One beat per interval from the poll every loop of a continuous
    /// observer already runs; no timer, task or retry of its own.
    fn beat_if_due(
        &mut self,
        pile: &mut FacultyStore,
        signer: &SigningKey,
        input: &str,
        now: Epoch,
        output: &mut Out<'_>,
    ) -> Result<()> {
        let Some(path) = self.news_path.as_mut() else {
            return Ok(());
        };
        // A clock that went backwards beats again rather than waiting it out.
        if path
            .last_attempt
            .is_some_and(|last| last <= now && now < last + span(NEWS_PATH_BEAT_EVERY))
        {
            return Ok(());
        }
        let (_, observation) = self
            .poll_view
            .as_ref()
            .expect("poll selected a health view");
        let window = match path.window {
            Some(window) => window,
            // An unresolved persona is not yet a news path anyone could name.
            None => match observation.persona(input) {
                Ok(window) => window,
                Err(_) => return Ok(()),
            },
        };
        path.window = Some(window);
        // A session's first beat names what it follows (`session_prior`),
        // from the register's latest record and that record's session: a few
        // lookups here instead of a scan of the history by every reader.
        let facts = observation.facts.view();
        let prior = match path.published {
            true => None,
            false => observation
                .latest
                .winner(window)
                .and_then(|record| session_prior(facts, window, record, now)),
        };
        // A carried retry's stretch continues in this session until an
        // acceptance, so this session's retries keep its start.
        let carried_since = prior.and_then(|prior| {
            find!(
                since: (Epoch, Epoch),
                pattern!(facts, [{ prior @
                    metadata::tag: &KIND_NEWS_PATH_RETRY,
                    metadata::started_at: ?since,
                }])
            )
            .map(|since| since.1)
            .min()
        });
        path.last_attempt = Some(now);
        let tags =
            std::iter::once(KIND_NEWS_PATH_BEAT).chain(prior.map(|_| schema::KIND_RECOVERED));
        let beat = entity! {
            metadata::tag*: tags,
            attrs::node: &window,
            attrs::session: &path.session,
            metadata::created_at: clock::point(now)?,
            metadata::supersedes?: prior,
        };
        match publish(pile, signer, self.health.source, beat) {
            Ok(()) => {
                path.published = true;
                if let Some(prior) = prior {
                    // Polling resumed after this record; the session's first
                    // accepted delivery is what resolves it.
                    let mut delivery = path
                        .delivery
                        .lock()
                        .expect("delivery state is not poisoned");
                    if !delivery.proven {
                        delivery.resolves.get_or_insert(prior);
                        if let Some(since) = carried_since {
                            delivery.degraded_since.get_or_insert(since);
                        }
                    }
                }
            }
            Err(error) => output.line(format!("note: news-path beat not recorded ({error:#})"))?,
        }
        Ok(())
    }
}

/// What a new session's first beat follows, given the persona's latest record
/// before it: an exit; an unresolved retry, so a clean restart during a
/// deferral keeps the evidence; or else a beat or DELIVERED a reader would
/// already call silent. A retry is unresolved when it is the latest record,
/// when it is newer than the latest DELIVERED of that record's session, or
/// when that session never had a report accepted and its first beat followed
/// the retry. That last one is carried as the original record, so a chain of
/// restarts that never reach an acceptance stays one hop deep. An unresolved
/// retry outranks silence: the new session's polling ends a silence, only an
/// acceptance ends a deferral. A clean, current predecessor is followed by
/// nothing.
fn session_prior(facts: &FactArchive, window: Id, record: Id, now: Epoch) -> Option<Id> {
    let tagged = |kind: Id| exists!(pattern!(facts, [{ record @ metadata::tag: &kind }]));
    if tagged(KIND_NEWS_PATH_EXIT) || tagged(KIND_NEWS_PATH_RETRY) {
        return Some(record);
    }
    if !(tagged(KIND_NEWS_PATH_BEAT) || tagged(KIND_NEWS_PATH_DELIVERED)) {
        return None;
    }
    let silent = find!(
        created: (Epoch, Epoch),
        pattern!(facts, [{ record @ metadata::created_at: ?created }])
    )
    .any(|created| created.1 + span(NEWS_PATH_MAX_AGE) <= now)
    .then_some(record);
    let Some(session) = find!(
        session: Id,
        pattern!(facts, [{ record @ attrs::session: ?session }])
    )
    .next() else {
        return silent;
    };
    let newest = |kind: Id| {
        find!(
            (event: Id, at: (Epoch, Epoch)),
            pattern!(facts, [{ ?event @
                metadata::tag: &kind,
                attrs::node: &window,
                attrs::session: &session,
                metadata::created_at: ?at,
            }])
        )
        .max_by_key(|(event, at)| (at.1, *event))
    };
    let unresolved = match (
        newest(KIND_NEWS_PATH_RETRY),
        newest(KIND_NEWS_PATH_DELIVERED),
    ) {
        (Some((retry, retried)), delivered)
            if delivered.is_none_or(|(id, at)| (retried.1, retry) > (at.1, id)) =>
        {
            Some(retry)
        }
        (None, None) => find!(
            carried: Id,
            pattern!(facts, [
                { _?first @
                    metadata::tag: &KIND_NEWS_PATH_BEAT,
                    metadata::tag: &schema::KIND_RECOVERED,
                    attrs::node: &window,
                    attrs::session: &session,
                    metadata::supersedes: ?carried,
                },
                { ?carried @ metadata::tag: &KIND_NEWS_PATH_RETRY },
            ])
        )
        .max(),
        _ => None,
    };
    unresolved.or(silent)
}

fn span(duration: Duration) -> hifitime::Duration {
    hifitime::Duration::from_total_nanoseconds(duration.as_nanos() as i128)
}

fn one_line(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(NEWS_PATH_REASON_CHARS) {
        Some((cut, _)) => format!("{}...", &line[..cut]),
        None => line,
    }
}

fn utc(at: Epoch) -> String {
    let (year, month, day, hour, minute, second, _) = at.to_gregorian_utc();
    format!("{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02} UTC")
}

fn person_label(people: &FactArchive, snapshot: &impl BlobStoreGet, person: Id) -> String {
    let heads = profile_heads(people, person);
    let [head] = heads.as_slice() else {
        return fmt_id(person);
    };
    let head = *head;
    find!(
        handle: relations::TextHandle,
        pattern!(people, [{ head @ metadata::name: ?handle }])
    )
    .min_by_key(|handle| handle.raw)
    .and_then(|handle| {
        snapshot
            .get::<View<str>, blobencodings::UTF8String>(handle)
            .ok()
    })
    .map(|label| label.to_string())
    .unwrap_or_else(|| fmt_id(person))
}

fn exit_reason(facts: &FactArchive, snapshot: &impl BlobStoreGet, exit: Id) -> String {
    find!(
        reason: Inline<inlineencodings::Handle<blobencodings::UTF8String>>,
        pattern!(facts, [{ exit @ metadata::description: ?reason }])
    )
    .filter_map(|reason| {
        snapshot
            .get::<View<str>, blobencodings::UTF8String>(reason)
            .ok()
    })
    .map(|reason| reason.to_string())
    .min()
    // Passive attention fetches nothing; an absent reason is still news.
    .unwrap_or_else(|| "reason not yet resident".to_owned())
}

/// Each persona's latest news-path record, through the maintained register:
/// readers never scan the beat history, only the latest record's session.
/// Events about the reader's own persona are dropped later, in `news`, where
/// the reader is known.
fn render_news_paths(
    report: &mut HealthReport,
    facts: &FactArchive,
    latest: &LwwQuery,
    people: &FactArchive,
    snapshot: &impl BlobStoreGet,
    now: Epoch,
    detail: Detail,
) {
    use std::fmt::Write as _;
    let tagged =
        |record: Id, kind: Id| exists!(pattern!(facts, [{ record @ metadata::tag: &kind }]));
    let now_key = now.to_tai_duration().total_nanoseconds();
    let mut lines = Vec::new();
    // Health reports share this register (their node is an observer, never a
    // persona); only the news-path kinds below are read here.
    for (record, window, session, created) in find!(
        (record: Id, window: Id, session: Id, created: (Epoch, Epoch)),
        and!(latest.has(record), pattern!(facts, [{ ?record @
            attrs::node: ?window,
            attrs::session: ?session,
            metadata::created_at: ?created,
        }]))
    ) {
        let exit = tagged(record, KIND_NEWS_PATH_EXIT);
        let retry = tagged(record, KIND_NEWS_PATH_RETRY);
        if !(exit
            || retry
            || tagged(record, KIND_NEWS_PATH_BEAT)
            || tagged(record, KIND_NEWS_PATH_DELIVERED))
        {
            continue;
        }
        let label = person_label(people, snapshot, window);
        if exit {
            let reason = exit_reason(facts, snapshot, record);
            let exited = format!("exited at {}: {reason}", utc(created.1));
            lines.push(format!("{label}: {exited}"));
            report.attention.insert(AttentionEvent::NewsPath {
                event: record,
                window,
                detail: format!(
                    "news path of {label} {exited}. Nothing restarts it automatically."
                ),
            });
            escalate(report, window, record, created.1, now, || {
                format!(
                    "news path of {label} still exited after {}: {exited}. Nothing restarts it automatically.",
                    escalation_span()
                )
            });
            continue;
        }
        if retry {
            // Beats stop while a delivery is deferred, so a latest RETRY is
            // degraded until DELIVERED or an exit follows it, or until its
            // next try is overdue by the silence threshold: then no newer
            // retry has been observed, and the path reads as silent with no
            // new record. Only a RETRY that is the window's latest record
            // can be overdue; a newer beat reaches `degraded` below instead.
            let next_try = find!(
                next: (Epoch, Epoch),
                pattern!(facts, [{ record @ metadata::expires_at: ?next }])
            )
            .map(|next| next.1)
            .max();
            match next_try {
                Some(next) if now >= next + span(NEWS_PATH_MAX_AGE) => {
                    let silence = Silence {
                        retry: record,
                        created: created.1,
                        next,
                    };
                    overdue(
                        report, &mut lines, facts, snapshot, &label, window, silence, now,
                    );
                }
                _ => {
                    report.next_change = earliest(
                        report.next_change,
                        next_try.map(|next| next + span(NEWS_PATH_MAX_AGE)),
                    );
                    degraded(
                        report, &mut lines, facts, snapshot, &label, window, record, now,
                    );
                }
            }
            continue;
        }
        let age = format_age(now_key, created.1.to_tai_duration().total_nanoseconds());
        if now < created.1 {
            report.next_change = earliest(report.next_change, Some(created.1));
            lines.push(format!("{label}: latest beat is in the future"));
            continue;
        }
        let stale_at = created.1 + span(NEWS_PATH_MAX_AGE);
        if now >= stale_at {
            lines.push(format!("{label}: silent since {}", utc(created.1)));
            report.attention.insert(AttentionEvent::NewsPath {
                // Not the record's own id: an acceptance already named its
                // "accepting again" event with it, and a presented id would
                // swallow the silence that follows.
                event: follow_up(NEWS_PATH_SILENCE, record),
                window,
                detail: format!(
                    "news path of {label} has not beaten since {} ({age} ago); that window may not be hearing news. Nothing restarts it automatically.",
                    utc(created.1)
                ),
            });
            escalate(report, window, record, created.1, now, || {
                format!(
                    "news path of {label} still silent after {}: has not beaten since {} ({age} ago). Nothing restarts it automatically.",
                    escalation_span(),
                    utc(created.1)
                )
            });
            continue;
        }
        report.next_change = earliest(report.next_change, Some(stale_at));
        // The latest record is a fresh beat or acceptance. What the session
        // last established: a RETRY newer than its latest DELIVERED is still
        // degraded; an older DELIVERED never clears a newer RETRY.
        let newest = |kind: Id| {
            find!(
                (event: Id, at: (Epoch, Epoch)),
                pattern!(facts, [{ ?event @
                    metadata::tag: &kind,
                    attrs::node: &window,
                    attrs::session: &session,
                    metadata::created_at: ?at,
                }])
            )
            .max_by_key(|(event, at)| (at.1, *event))
        };
        let last_retry = newest(KIND_NEWS_PATH_RETRY);
        let last_delivered = newest(KIND_NEWS_PATH_DELIVERED);
        match (last_retry, last_delivered) {
            (Some((retry, retried)), delivered)
                if delivered.is_none_or(|(id, at)| (retried.1, retry) > (at.1, id)) =>
            {
                degraded(
                    report, &mut lines, facts, snapshot, &label, window, retry, now,
                );
            }
            (_, Some((delivered, _))) => {
                lines.push(format!("{label}: callback accepting, last beat {age} ago"));
                for prior in find!(
                    prior: Id,
                    pattern!(facts, [{ delivered @ metadata::supersedes: ?prior }])
                ) {
                    report.attention.insert(AttentionEvent::NewsPath {
                        event: delivered,
                        window,
                        detail: format!(
                            "news path of {label}: callback accepting again (session [{}]) after {}",
                            fmt_id(session),
                            resolved(facts, snapshot, prior)
                        ),
                    });
                }
            }
            _ => {
                lines.push(format!(
                    "{label}: polling, last beat {age} ago, callback has not accepted a report yet"
                ));
                for prior in find!(
                    prior: Id,
                    pattern!(facts, [{ _?first @
                        metadata::tag: &KIND_NEWS_PATH_BEAT,
                        metadata::tag: &schema::KIND_RECOVERED,
                        attrs::node: &window,
                        attrs::session: &session,
                        metadata::supersedes: ?prior,
                    }])
                ) {
                    report.attention.insert(AttentionEvent::NewsPath {
                        event: session,
                        window,
                        detail: format!(
                            "news path of {label}: polling resumed (session [{}]) after {}; callback has not accepted a report yet",
                            fmt_id(session),
                            resolved(facts, snapshot, prior)
                        ),
                    });
                }
            }
        }
    }
    if detail == Detail::Full && !lines.is_empty() {
        lines.sort();
        report.text.push_str("\nNews paths (shared beats):\n");
        for line in lines {
            writeln!(report.text, "- {line}").unwrap();
        }
    }
}

/// Say one deferred attempt: since when, which attempt, why, and when next.
/// The dashboard line follows the attempt; the attention events are the
/// stretch's.
#[allow(clippy::too_many_arguments)]
fn degraded(
    report: &mut HealthReport,
    lines: &mut Vec<String>,
    facts: &FactArchive,
    snapshot: &impl BlobStoreGet,
    label: &str,
    window: Id,
    retry: Id,
    now: Epoch,
) {
    let key = |at: (Epoch, Epoch)| at.1.to_tai_duration().total_nanoseconds();
    let summary = retry_summary(facts, snapshot, retry);
    let next = find!(
        (created: (Epoch, Epoch), next: (Epoch, Epoch)),
        pattern!(facts, [{ retry @
            metadata::created_at: ?created,
            metadata::expires_at: ?next,
        }])
    )
    .map(|(created, next)| format!(", next try in {}", format_age(key(next), key(created))))
    .min()
    .unwrap_or_default();
    lines.push(format!("{label}: degraded {summary}{next}"));
    let (episode, since) = stretch(facts, window, retry);
    report.attention.insert(AttentionEvent::NewsPath {
        event: episode,
        window,
        detail: format!("news path of {label} degraded {summary}{next}"),
    });
    if let Some(since) = since {
        escalate(report, window, episode, since, now, || {
            format!(
                "news path of {label} still degraded after {} (degraded {summary})",
                escalation_span()
            )
        });
    }
}

/// A latest RETRY whose next try has not been observed.
struct Silence {
    retry: Id,
    created: Epoch,
    next: Epoch,
}

/// Say a latest RETRY overdue by the silence threshold as silence: no newer
/// retry has been observed. That is missing evidence, not proof the daemon
/// died. Its time is the RETRY's own `created_at`, the last instant the path
/// is known to have been alive; the next try was only an intention. The
/// silence stays in the RETRY's stretch, so it escalates on that start.
#[allow(clippy::too_many_arguments)]
fn overdue(
    report: &mut HealthReport,
    lines: &mut Vec<String>,
    facts: &FactArchive,
    snapshot: &impl BlobStoreGet,
    label: &str,
    window: Id,
    silence: Silence,
    now: Epoch,
) {
    let key = |at: Epoch| at.to_tai_duration().total_nanoseconds();
    let Silence {
        retry,
        created,
        next,
    } = silence;
    let age = format_age(key(now), key(created));
    let summary = retry_summary(facts, snapshot, retry);
    lines.push(format!(
        "{label}: silent, no retry since {} (next try was due {})",
        utc(created),
        utc(next)
    ));
    report.attention.insert(AttentionEvent::NewsPath {
        // Not the RETRY's own id: a stretch of one attempt already used that
        // for its degraded event, which this silence must not hide behind.
        event: follow_up(NEWS_PATH_OVERDUE, retry),
        window,
        detail: format!(
            "news path of {label} has not beaten, and no retry since {} ({age} ago): the next try due at {} has not been observed, after delivery degraded {summary}; that window may not be hearing news. Nothing restarts it automatically.",
            utc(created),
            utc(next)
        ),
    });
    let (episode, since) = stretch(facts, window, retry);
    if let Some(since) = since {
        escalate(report, window, episode, since, now, || {
            format!(
                "news path of {label} still silent after {}: no beat, and no retry since {} (delivery degraded {summary}). Nothing restarts it automatically.",
                escalation_span(),
                utc(created)
            )
        });
    }
}

/// The stretch a RETRY belongs to: the stretch's first RETRY, which names
/// its events, and when the stretch began.
fn stretch(facts: &FactArchive, window: Id, retry: Id) -> (Id, Option<Epoch>) {
    let since = find!(
        at: (Epoch, Epoch),
        pattern!(facts, [{ retry @ metadata::started_at: ?at }])
    )
    .map(|at| at.1)
    .min();
    // Every attempt of one degraded stretch carries its start, across
    // restarts that never accept too, so the stretch's first RETRY names the
    // event: peers hear of a stretch once, not of each attempt.
    let episode = find!(
        (first: Id, created: (Epoch, Epoch)),
        pattern!(facts, [
            { retry @ metadata::started_at: _?since },
            { ?first @
                metadata::tag: &KIND_NEWS_PATH_RETRY,
                attrs::node: &window,
                metadata::started_at: _?since,
                metadata::created_at: ?created,
            },
        ])
    )
    .min_by_key(|(first, created)| (created.1, *first))
    .map_or(retry, |(first, _)| first);
    (episode, since)
}

/// Tell peers once more about a stretch still unresolved
/// [`NEWS_PATH_ESCALATE_AFTER`] after `start`, whatever it reads as now.
/// Before then, wake readers at that instant, so a daemon that writes
/// nothing more is escalated by the deadline alone.
///
/// The stretch is the episode its first event already names; nothing here
/// carries one further. A degraded stretch (and the silence of its overdue
/// RETRY) runs from the RETRY's `started_at`, which restarts carry until an
/// acceptance. An EXIT is its own episode: its hour runs from the EXIT, and
/// it neither inherits nor stops the clock of a stretch it ended (one that
/// already escalated stays said). A silent beat or acceptance runs from that
/// record. A restart's resumed polling is not escalated; its next RETRY
/// rejoins the carried stretch. An acceptance ends a degraded stretch, so
/// the reader no longer reaches this for it.
fn escalate(
    report: &mut HealthReport,
    window: Id,
    episode: Id,
    start: Epoch,
    now: Epoch,
    detail: impl FnOnce() -> String,
) {
    let due = start + span(NEWS_PATH_ESCALATE_AFTER);
    if now < due {
        report.next_change = earliest(report.next_change, Some(due));
        return;
    }
    report.attention.insert(AttentionEvent::NewsPath {
        event: follow_up(NEWS_PATH_ESCALATION, episode),
        window,
        detail: detail(),
    });
}

fn escalation_span() -> String {
    format!("{} min", NEWS_PATH_ESCALATE_AFTER.as_secs() / 60)
}

/// The id of an attention event that has no record of its own: the
/// escalation of a stretch, or a silence after a beat, an acceptance or an
/// overdue RETRY. It folds
/// `tag` and the record it follows into an intrinsic id that is never
/// written, so every render names the event the same way and a presented
/// one is not said again. That idempotence is its only purpose: a fresh
/// random id would break nothing but the once-only wake, and nothing
/// computes it to look a record up.
fn follow_up(tag: Id, record: Id) -> Id {
    entity! {
        metadata::tag: &tag,
        metadata::supersedes: &record,
    }
    .root()
    .expect("one follow-up entity")
}

/// "since T: attempt n, <reason>" for one retry record.
fn retry_summary(facts: &FactArchive, snapshot: &impl BlobStoreGet, retry: Id) -> String {
    let since = find!(
        at: (Epoch, Epoch),
        pattern!(facts, [{ retry @ metadata::started_at: ?at }])
    )
    .min_by_key(|at| at.1)
    .map(|at| utc(at.1))
    .unwrap_or_else(|| "an unknown time".to_owned());
    let attempt = find!(
        attempt: u128,
        pattern!(facts, [{ retry @ telemetry::attrs::failed: ?attempt }])
    )
    .max()
    .map_or_else(
        || "an attempt".to_owned(),
        |attempt| format!("attempt {attempt}"),
    );
    let reason = exit_reason(facts, snapshot, retry);
    format!("since {since}: {attempt}, {reason}")
}

/// Describe the record an acceptance or a resumption resolves.
fn resolved(facts: &FactArchive, snapshot: &impl BlobStoreGet, prior: Id) -> String {
    if exists!(pattern!(facts, [{ prior @ metadata::tag: &KIND_NEWS_PATH_EXIT }])) {
        format!("an exit: {}", exit_reason(facts, snapshot, prior))
    } else if exists!(pattern!(facts, [{ prior @ metadata::tag: &KIND_NEWS_PATH_RETRY }])) {
        format!(
            "delivery degraded {}",
            retry_summary(facts, snapshot, prior)
        )
    } else {
        find!(
            created: (Epoch, Epoch),
            pattern!(facts, [{ prior @ metadata::created_at: ?created }])
        )
        .map(|created| format!("silence since {}", utc(created.1)))
        .min()
        .unwrap_or_else(|| "a silence".to_owned())
    }
}

pub(super) fn until(deadline: Option<Epoch>, now: Epoch) -> Option<Duration> {
    deadline.map(|deadline| {
        let ns = (deadline - now).total_nanoseconds().max(0);
        Duration::from_nanos(ns.min(u64::MAX as i128) as u64)
    })
}

/// The reader's freshness deadline, not a retry timeout. Dropping ordinary reads at
/// this boundary lets the caller refresh local health before trying them again.
pub(super) async fn deadline(deadline: Option<Epoch>) -> Result<()> {
    match until(deadline, clock::now()?) {
        Some(delay) => tokio::time::sleep(delay).await,
        None => std::future::pending::<()>().await,
    }
    Ok(())
}

pub(super) struct HealthObservation {
    snapshot: FacultySnapshot,
    // Health freshness is evaluated at this explicit application time, not a
    // property of the immutable storage observation.
    evaluated_at: Epoch,
    facts: OrientFact,
    latest: LwwQuery,
    latest_collection: AttachedSnapshot<FacultySnapshot, LwwRegisterBlob>,
    relations: OrientFact,
    presentations: ReceiptObservation,
    max_age: Duration,
    service_events: Option<OrientFact>,
}

pub(super) struct HealthReport {
    pub(super) text: String,
    pub(super) attention: AttentionView,
    pub(super) next_change: Option<Epoch>,
}

impl HealthObservation {
    /// A requested dashboard may wait for its exact selected facts and labels.
    /// Passive health attention deliberately does not: a disconnected observer
    /// must be able to report the outage before ordinary network acquisition.
    pub(super) async fn report_acquiring(&self) -> Result<HealthReport> {
        let facts = self.facts.view.clone();
        let latest = self.latest.clone();
        let snapshot = self.snapshot.clone();
        let runtime = tokio::runtime::Handle::current();
        let now = self.evaluated_at;
        let max_age = self.max_age;
        let service_events = self
            .service_events
            .as_ref()
            .map(|source| source.view.clone());
        let people = self.relations.view.clone();
        tokio::task::spawn_blocking(move || {
            let reader = crate::storage::AcquiringReader::with_handle(snapshot.clone(), runtime);
            let mut report = render_health(&facts, &latest, &reader, now, max_age, Detail::Full);
            if let Some(facts) = service_events {
                service_events::append_report(&mut report, &facts, &snapshot, true);
            }
            render_news_paths(
                &mut report,
                &facts,
                &latest,
                &people,
                &snapshot,
                now,
                Detail::Full,
            );
            report
        })
        .await
        .context("join requested health report acquisition")
    }

    pub(super) async fn persona_acquiring(
        &self,
        pile: &mut FacultyStore,
        input: &str,
    ) -> Result<Id> {
        read(pile, &self.snapshot, |reader| {
            resolve_resident_persona(self.relations.view(), reader, input)
        })
        .await
    }

    fn is_current(&self, snapshot: &FacultySnapshot) -> bool {
        self.facts.is_current(snapshot)
            && self.latest_collection.is_current(snapshot)
            && self.relations.is_current(snapshot)
            && self.presentations.is_current(snapshot)
            && self
                .service_events
                .as_ref()
                .is_none_or(|source| source.is_current(snapshot))
    }

    /// The complete dashboard, for `show`.
    pub(super) fn report(&self) -> HealthReport {
        self.render(Detail::Full)
    }

    /// What can wake someone, for wait, poll and baseline: the attention set
    /// and the next deadline, with empty text.
    pub(super) fn attention(&self) -> HealthReport {
        let attention = self.render(Detail::Attention);
        // Every test that reaches wait, poll or baseline also proves the two
        // renders agree on everything but the text.
        #[cfg(test)]
        {
            let full = self.render(Detail::Full);
            assert_eq!(attention.attention, full.attention);
            assert_eq!(attention.next_change, full.next_change);
        }
        attention
    }

    fn render(&self, detail: Detail) -> HealthReport {
        let stage = match detail {
            Detail::Full => "render dashboard",
            Detail::Attention => "render attention",
        };
        let mut report = trace_refresh_call("Swarm health", stage, || {
            Ok::<_, std::convert::Infallible>(render_health(
                self.facts.view(),
                &self.latest,
                &self.snapshot,
                self.evaluated_at,
                self.max_age,
                detail,
            ))
        })
        .unwrap_or_else(|never| match never {});
        if let Some(source) = &self.service_events {
            service_events::append_report(
                &mut report,
                source.view(),
                &self.snapshot,
                detail == Detail::Full,
            );
        }
        render_news_paths(
            &mut report,
            self.facts.view(),
            &self.latest,
            self.relations.view(),
            &self.snapshot,
            self.evaluated_at,
            detail,
        );
        report
    }

    pub(super) fn persona(&self, input: &str) -> Result<Id> {
        resolve_resident_persona(self.relations.view(), &self.snapshot, input)
    }

    pub(super) fn news(&self, persona: Id, report: &HealthReport) -> News {
        let mut pending = report
            .attention
            .pending_health(self.facts.view(), self.presentations.view());
        // A window is not told about its own news path: it is the one party
        // that cannot hear it, and its peers are the ones who can act.
        let own = IdentityIndex::from_relations(self.relations.view())
            .component(persona)
            .unwrap_or_else(|_| BTreeSet::from([persona]));
        pending.events.retain(|_, event| {
            !matches!(event, AttentionEvent::NewsPath { window, .. } if own.contains(window))
        });
        if pending.is_empty() {
            return News::Quiet;
        }
        use std::fmt::Write as _;
        let mut text = String::new();
        let mut stream = String::new();
        // Wait/poll are an attention channel, not a health dashboard. Each
        // reason already identifies the changed subject and actionable state;
        // the complete resident snapshot remains available through `show`.
        let mut collection_groups: BTreeMap<CollectionSyncGroup, Vec<&AttentionEvent>> =
            BTreeMap::new();
        for event in pending.events.values() {
            match event {
                AttentionEvent::Health {
                    collection_group: Some(group),
                    ..
                } => collection_groups
                    .entry(group.clone())
                    .or_default()
                    .push(event),
                _ => {
                    writeln!(text, "News: {}", event.reason()).unwrap();
                    push_stream_line(&mut stream, &event.reason());
                }
            }
        }
        for (group, events) in collection_groups {
            let reason = if events.len() == 1 {
                events[0].reason()
            } else {
                group.reason(events.len())
            };
            writeln!(text, "News: {reason}").unwrap();
            push_stream_line(&mut stream, &reason);
        }
        News::Report {
            text,
            stream,
            events: pending.ids().collect(),
        }
    }
}

/// Collection-sync conditions do not wake anyone, and that is the rule this file
/// already states a few lines above: "Wait/poll are an attention channel, not a
/// health dashboard." The collection component slipped through it.
///
/// What this deliberately does NOT touch, because a peer cut off from the swarm
/// must still be able to notice and reconnect -- if health cannot wake it, its
/// silence reads as health and it stalls forever:
///
///   * the observer-stale event, a separate insertion in the `!fresh` branch,
///     which is exactly the "this node can no longer see the swarm" signal;
///   * every non-collection component, DHT publication among them;
///   * `show`, which still renders every condition. Nothing is hidden here;
///     these simply stop being a reason to interrupt someone.
///
/// Measured 2026-09-21: one `orient show` carried 517 condition lines -- 314
/// converged, 179 unknown, 16 stalled -- against three piles that verify 26/26
/// complete with replication quiescent. Delivered as attention, that filled a
/// waiting session's callback queue to its 100-submission cap and killed its
/// delivery daemon. Whether those unknowns and stalls are real, or an instrument
/// that cannot tell quiescence from a stall, is a separate open investigation;
/// until it lands they are not worth a wake.
///
/// Set TRIBLESPACE_ORIENT_COLLECTION_SYNC_NEWS=1 to restore the old behaviour.
fn collection_sync_may_wake(component: Id) -> bool {
    component != schema::COLLECTION
        || matches!(
            std::env::var("TRIBLESPACE_ORIENT_COLLECTION_SYNC_NEWS").as_deref(),
            Ok("1")
        )
}

fn component_name(component: Id) -> Option<&'static str> {
    match component {
        schema::HOST => Some("event loop"),
        schema::STORE => Some("serving snapshot"),
        schema::COLLECTION => Some("collection sync"),
        schema::DHT => Some("DHT publication"),
        _ => None,
    }
}

fn state_name(component: Id, state: Id) -> &'static str {
    match state {
        schema::CURRENT if component == schema::COLLECTION => {
            "converged at observed pairwise roots"
        }
        schema::CURRENT => "current",
        schema::PROGRESSING => "catching up",
        schema::STALLED => "stalled",
        _ => "unknown",
    }
}

fn attention_state_name(component: Id, state: Id, recovered: bool) -> &'static str {
    if recovered {
        return if component == schema::COLLECTION {
            "recovered; converged at observed pairwise roots"
        } else {
            "recovered; current"
        };
    }
    match state {
        schema::UNKNOWN if component == schema::COLLECTION => {
            "pairwise-root comparison unavailable beyond progress grace"
        }
        schema::STALLED if component == schema::COLLECTION => {
            "pairwise roots remain divergent without observed progress beyond grace"
        }
        _ => state_name(component, state),
    }
}

/// Has this observer already seen this qualitative health episode?
///
/// The receipt names the exact condition entity that was delivered, while the
/// health producer may replace that entity to publish fresh counters. Join the
/// two collections on the stable episode fields and deliberately leave those
/// counters out. This is why receipts are a queryable Rank9 projection rather
/// than an entity-id membership set.
fn health_episode_presented(facts: &FactArchive, presented: &FactArchive, event: Id) -> bool {
    if event_presented(presented, event) {
        return true;
    }

    for (node, session, state, started_at) in find!(
        (node: Id, session: Id, state: Id, started_at: IntervalValue),
        pattern!(facts, [{ event @
            metadata::tag: &schema::KIND_CONDITION,
            attrs::node: ?node,
            attrs::session: ?session,
            attrs::state: ?state,
            metadata::started_at: ?started_at,
        }])
    ) {
        for component in [schema::HOST, schema::STORE, schema::COLLECTION, schema::DHT] {
            if !exists!(pattern!(facts, [{ event @ metadata::tag: &component }])) {
                continue;
            }
            for lifecycle in [schema::KIND_ALERT, schema::KIND_RECOVERED] {
                if !exists!(pattern!(facts, [{ event @ metadata::tag: &lifecycle }])) {
                    continue;
                }
                if component != schema::COLLECTION {
                    if exists!((prior: Id), and!(
                        pattern!(facts, [{ ?prior @
                            metadata::tag: &schema::KIND_CONDITION,
                            metadata::tag: &component,
                            metadata::tag: &lifecycle,
                            attrs::node: &node,
                            attrs::session: &session,
                            attrs::state: &state,
                            metadata::started_at: &started_at,
                        }]),
                        pattern!(presented, [{ _?receipt @ presentation::event: ?prior }]),
                    )) {
                        return true;
                    }
                    continue;
                }

                for collection in find!(
                    collection: Inline<inlineencodings::Handle<SimpleArchive>>,
                    pattern!(facts, [{ event @ attrs::collection: ?collection }])
                ) {
                    let peers: Vec<_> = find!(
                        peer: ed25519_dalek::VerifyingKey,
                        pattern!(facts, [{ event @ attrs::peer: ?peer }])
                    )
                    .collect();
                    if peers.is_empty() {
                        for prior in find!(
                            prior: Id,
                            and!(
                                pattern!(facts, [{ ?prior @
                                    metadata::tag: &schema::KIND_CONDITION,
                                    metadata::tag: &component,
                                    metadata::tag: &lifecycle,
                                    attrs::node: &node,
                                    attrs::session: &session,
                                    attrs::state: &state,
                                    attrs::collection: &collection,
                                    metadata::started_at: &started_at,
                                }]),
                                pattern!(presented, [{ _?receipt @ presentation::event: ?prior }]),
                            )
                        ) {
                            if !exists!(pattern!(facts, [{ prior @ attrs::peer: _?peer }])) {
                                return true;
                            }
                        }
                    } else {
                        for peer in peers {
                            if exists!((prior: Id), and!(
                                pattern!(facts, [{ ?prior @
                                    metadata::tag: &schema::KIND_CONDITION,
                                    metadata::tag: &component,
                                    metadata::tag: &lifecycle,
                                    attrs::node: &node,
                                    attrs::session: &session,
                                    attrs::state: &state,
                                    attrs::collection: &collection,
                                    attrs::peer: peer,
                                    metadata::started_at: &started_at,
                                }]),
                                pattern!(presented, [{ _?receipt @ presentation::event: ?prior }]),
                            )) {
                                return true;
                            }
                        }
                    }
                }
            }
        }
    }
    false
}

impl AttentionView {
    fn pending_health(&self, facts: &FactArchive, presented: &FactArchive) -> Self {
        Self {
            events: self
                .events
                .iter()
                .filter(|(event, _)| !health_episode_presented(facts, presented, **event))
                .map(|(event, detail)| (*event, detail.clone()))
                .collect(),
        }
    }
}

fn collection_label(
    snapshot: &impl BlobStoreGet,
    handle: Inline<inlineencodings::Handle<SimpleArchive>>,
) -> String {
    let encoded = hex::encode(handle.raw);
    let prefix = &encoded[..12];
    let name = BlobStoreGet::get::<TribleSet, SimpleArchive>(snapshot, handle)
        .ok()
        .and_then(|facts| descriptor::name(&facts).ok().flatten())
        .and_then(|name| {
            BlobStoreGet::get::<View<str>, blobencodings::UTF8String>(snapshot, name).ok()
        });
    match name {
        Some(name) => format!("{} [{prefix}]", &*name),
        None => format!("[{prefix}]"),
    }
}

/// How much of a health observation to render.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Detail {
    /// The complete dashboard, as `show` prints it.
    Full,
    /// Only what can wake someone: the attention set and the next deadline,
    /// with empty text. Wait, poll and baseline are an attention channel, not a
    /// dashboard, and they used to build the whole dashboard only to discard
    /// it. The text is most of the cost when a view is wide: on 2026-09-26 it
    /// was 137 s of a 200 s `show` against a 10372-member health cover, and
    /// 1010 of its 1150 condition lines belonged to stale reports, whose
    /// conditions can never wake anyone.
    Attention,
}

/// Query the current report and its conditions directly from maintained facts.
/// Missing/unknown rows do not invalidate other reports or invent a green bit.
fn render_health(
    facts: &FactArchive,
    latest: &LwwQuery,
    snapshot: &impl BlobStoreGet,
    now: Epoch,
    max_age: Duration,
    detail: Detail,
) -> HealthReport {
    use std::fmt::Write as _;

    let now_key = now.to_tai_duration().total_nanoseconds();
    let mut text = String::from("\nSwarm health (local observations):\n");
    let mut attention = AttentionView::default();
    let mut next_change: Option<Epoch> = None;
    let mut observed = false;
    for (report, node, session, endpoint, created) in find!(
        (report: Id, node: Id, session: Id, endpoint: ed25519_dalek::VerifyingKey,
         created: (Epoch, Epoch)),
        and!(latest.has(report), pattern!(facts, [
            { ?report @ metadata::tag: &schema::KIND_REPORT, attrs::node: ?node,
              attrs::session: ?session, metadata::created_at: ?created },
            { ?node @ attrs::endpoint: ?endpoint },
        ]))
    ) {
        observed = true;
        let endpoint = hex::encode(endpoint.to_bytes());
        let observer = &endpoint[..12];
        let age = format_age(now_key, created.1.to_tai_duration().total_nanoseconds());
        let stale_at =
            created.1 + hifitime::Duration::from_total_nanoseconds(max_age.as_nanos() as i128);
        let fresh = created.1 <= now && now < stale_at;
        let timing = if now < created.1 {
            "unknown (sample is in the future)"
        } else if !fresh {
            "unknown (report too old)"
        } else {
            "fresh"
        };
        writeln!(text, "- observer [{observer}], sample {age} ago: {timing}").unwrap();
        if fresh {
            next_change = Some(next_change.map_or(stale_at, |seen| seen.min(stale_at)));
        } else if now < created.1 {
            next_change = Some(next_change.map_or(created.1, |seen| seen.min(created.1)));
        } else {
            attention.insert(AttentionEvent::Health {
                event: report,
                detail: format!("observer [{observer}] report exceeds reader maximum age; current health unknown (last sample {age} ago)"),
                collection_group: None,
            });
        }
        // A condition wakes someone only while its report is fresh, so an
        // attention render has nothing to find in a stale report's conditions.
        if detail == Detail::Attention && !fresh {
            continue;
        }
        let mut conditions = BTreeSet::new();
        for (condition, component, state) in find!(
            (condition: Id, component: Id, state: Id),
            pattern!(facts, [
                { report @ attrs::condition: ?condition },
                { ?condition @ metadata::tag: &schema::KIND_CONDITION,
                  metadata::tag: ?component, attrs::node: &node, attrs::session: &session,
                  attrs::state: ?state },
            ])
        ) {
            let Some(component_name) = component_name(component) else {
                continue;
            };
            let alert = exists!(pattern!(facts, [{
                condition @ metadata::tag: &schema::KIND_ALERT
            }]));
            let recovered = exists!(pattern!(facts, [{
                condition @ metadata::tag: &schema::KIND_RECOVERED
            }]));
            let wakes = fresh && (alert || recovered) && collection_sync_may_wake(component);
            // Same rows, same order as the dashboard, so the first-inserted
            // detail of an event is the same in both renders; only the labels
            // nobody will read are skipped.
            if detail == Detail::Attention && !wakes {
                continue;
            }
            let mut collections: Vec<String> = find!(
                collection: Inline<inlineencodings::Handle<SimpleArchive>>,
                pattern!(facts, [{ condition @ attrs::collection: ?collection }])
            )
            .map(|collection| collection_label(snapshot, collection))
            .collect();
            collections.sort();
            let mut peers: Vec<String> = find!(
                peer: ed25519_dalek::VerifyingKey,
                pattern!(facts, [{ condition @ attrs::peer: ?peer }])
            )
            .map(|peer| hex::encode(peer.to_bytes())[..12].to_owned())
            .collect();
            peers.sort();
            let collection_scope = collections
                .iter()
                .map(|collection| format!(" {collection}"))
                .collect::<String>();
            let peer_scope = peers
                .iter()
                .map(|peer| format!(" via [{peer}]"))
                .collect::<String>();
            let scope = format!("{collection_scope}{peer_scope}");
            let condition_line =
                format!("{component_name}{scope}: {}", state_name(component, state));
            conditions.insert(condition_line);
            if wakes {
                attention.insert(AttentionEvent::Health {
                    event: condition,
                    detail: format!(
                        "observer [{observer}] {component_name}{scope}: {}",
                        attention_state_name(component, state, recovered)
                    ),
                    collection_group: if component == schema::COLLECTION && collections.len() == 1 {
                        let issue = if recovered {
                            Some(CollectionSyncIssue::Recovered)
                        } else {
                            match state {
                                schema::UNKNOWN => Some(CollectionSyncIssue::ComparisonUnavailable),
                                schema::STALLED => Some(CollectionSyncIssue::DivergenceStalled),
                                _ => None,
                            }
                        };
                        issue.map(|issue| CollectionSyncGroup {
                            observer: observer.to_owned(),
                            peer_scope,
                            issue,
                        })
                    } else {
                        None
                    },
                });
            }
        }
        if conditions.is_empty() {
            writeln!(text, "  scope not observed in this report").unwrap();
        }
        for condition in conditions {
            writeln!(
                text,
                "  {condition}{}",
                if fresh { "" } else { " (last sample only)" }
            )
            .unwrap();
        }
    }
    if !observed {
        text.push_str("- not observed / not configured; no health conclusion\n");
    } else {
        text.push_str("  Scope is the reported observer/peer/collection pairs, not the whole swarm.\n  DHT publication and record convergence do not prove blob availability.\n");
    }
    HealthReport {
        // An attention render never hands out a partial dashboard.
        text: match detail {
            Detail::Full => text,
            Detail::Attention => String::new(),
        },
        attention,
        next_change,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl HealthSources {
        // Synthetic synchronous callers only; production poll remains async.
        fn poll_test(
            &mut self,
            pile: &mut FacultyStore,
            signer: &SigningKey,
            input: &str,
            peek: bool,
            output: &mut Out<'_>,
        ) -> Result<(bool, Option<Epoch>)> {
            test_block_on(self.poll(pile, signer, input, peek, output))
        }
    }
    use schema::{Component, Condition, Evidence, Measurement, Recorder, State};
    use triblespace::core::blob::Blob;
    use triblespace::core::collection::{
        records::empty_metadata_handle, CollectionCommit, CollectionRecord, CollectionStore,
    };
    use triblespace::core::repo::{BlobStorePut, StorageClose, WantRead};

    #[test]
    fn explicit_workspace_health_retains_all_selected_roots() {
        use crate::schemas::orient::RECEIPTS_SCOPE_ID;
        use triblespace::core::collection::{AdmissionPolicy, CollectionPolicy};

        test_block_on(async {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("workspace-health.pile");
            std::fs::File::create(&path).unwrap();
            let signer = SigningKey::from_bytes(&[72; 32]);
            let mut store = open_store_as(&path, signer.verifying_key()).unwrap();
            let mut routes = crate::storage::CollectionRoutes::new();
            for scope in [
                schema::DEFAULT_SCOPE_ID,
                RELATIONS_SCOPE_ID,
                RECEIPTS_SCOPE_ID,
            ] {
                let source = store
                    .collection(
                        crate::collection_names::require_name(scope),
                        CollectionPolicy::new(
                            AdmissionPolicy::Open,
                            AdmissionPolicy::direct(signer.verifying_key()),
                        ),
                    )
                    .unwrap();
                routes.insert(scope, source.handle());
            }
            let routing = test_storage().with_collections(routes.clone());
            let sources =
                HealthSources::open(&routing, &mut store, &signer, Duration::from_secs(60))
                    .await
                    .unwrap();
            assert_eq!(
                sources.health.source.handle(),
                routes[&schema::DEFAULT_SCOPE_ID]
            );
            assert_eq!(
                sources.relations.source.handle(),
                routes[&RELATIONS_SCOPE_ID]
            );
            assert_eq!(
                sources.presentations.source.handle(),
                routes[&RECEIPTS_SCOPE_ID]
            );
            assert_eq!(
                sources.latest,
                store
                    .attach::<LwwRegisterBlob>(
                        sources.health.source,
                        (attrs::node.id(), metadata::created_at.id()),
                    )
                    .unwrap()
            );
            assert!(store.health().started_at.is_none());
            store.close().unwrap();
        });
    }

    struct Fixture {
        store: FacultyStore,
        sources: HealthSources,
        signer: SigningKey,
        _directory: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("health.pile");
            std::fs::File::create(&path).unwrap();
            // Test-only author, never a live transport or pile identity.
            let signer = SigningKey::from_bytes(&[71; 32]);
            let mut store = open_store_as(&path, signer.verifying_key()).unwrap();
            let sources = test_block_on(HealthSources::open(
                &test_storage(),
                &mut store,
                &signer,
                Duration::from_secs(60),
            ))
            .unwrap();
            Self {
                store,
                sources,
                signer,
                _directory: directory,
            }
        }

        fn publish(&mut self, facts: Fragment) {
            self.store
                .commit(self.sources.health.source, &self.signer, facts)
                .unwrap();
        }

        fn observe_at(&mut self, at: Epoch) -> HealthObservation {
            assert!(self
                .sources
                .maintain(&self.store, &self.signer)
                .unwrap()
                .is_none());
            let observation = self.sources.at(self.store.snapshot().unwrap(), at).unwrap();
            // Every scenario also checks that the attention render agrees
            // with the dashboard on everything that can wake someone.
            observation.attention();
            observation
        }

        fn malformed_member(&mut self, source: Collection<SimpleArchive>) -> CollectionCommit {
            // Resident and content-valid as a blob, but not a SimpleArchive.
            // A missing member alone can be skipped by resident source support
            // and would not establish the upkeep-failure boundary.
            let blob = Blob::<SimpleArchive>::new(Bytes::from(vec![0xff]));
            let handle = self.store.put::<SimpleArchive, _>(blob).unwrap();
            let commit = CollectionCommit::sign(
                &self.signer,
                source.handle(),
                inlineencodings::Handle::<SimpleArchive>::to_hash(handle),
                empty_metadata_handle(),
            );
            self.store.insert(CollectionRecord::Commit(commit)).unwrap();
            assert!(self
                .store
                .snapshot()
                .unwrap()
                .contains_blob(handle)
                .unwrap());
            commit
        }
    }

    fn condition(state: State, alert: bool) -> Condition {
        Condition {
            component: Component::Collection,
            collection: None,
            peer: None,
            state,
            alert,
        }
    }

    /// A non-collection alerting fixture.
    ///
    /// Collection-sync conditions no longer reach `attention` (see
    /// `collection_sync_may_wake`), so every test about the *generic* alerting
    /// machinery -- receipts, poll views, private membership, once-only episode
    /// reporting -- uses this instead. `condition` stays on Collection for the
    /// tests that genuinely assert collection-sync wording and grouping.
    fn store_condition(state: State, alert: bool) -> Condition {
        Condition {
            component: Component::Store,
            collection: None,
            peer: None,
            state,
            alert,
        }
    }

    // News-path records as a peer's observer writes them, built here rather
    // than by the writer under test so the reader is checked independently.
    fn path_beat(window: Id, session: Id, created: Epoch) -> Fragment {
        entity! {
            metadata::tag: &KIND_NEWS_PATH_BEAT,
            attrs::node: &window,
            attrs::session: &session,
            metadata::created_at: clock::point(created).unwrap(),
        }
    }

    fn path_exit(window: Id, session: Id, created: Epoch, reason: TextHandle) -> Fragment {
        entity! {
            metadata::tag: &KIND_NEWS_PATH_EXIT,
            attrs::node: &window,
            attrs::session: &session,
            metadata::created_at: clock::point(created).unwrap(),
            metadata::description: reason,
        }
    }

    type TextHandle = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;

    fn news_text(news: News) -> Option<(String, Vec<Id>)> {
        match news {
            News::Report { text, events, .. } => Some((text, events.into_iter().collect())),
            News::Quiet => None,
        }
    }

    #[test]
    fn a_silent_news_path_is_said_once_to_other_windows() {
        let mut f = Fixture::new();
        let window = *fucid();
        let reader = *fucid();
        let beat = path_beat(window, *fucid(), at(0.0));
        let beat_id = beat.root().unwrap();
        f.publish(beat);

        let fresh = f.observe_at(at(60.0));
        let report = fresh.attention();
        assert!(report.attention.is_empty(), "a fresh beat is not news");
        let deadline = report
            .next_change
            .expect("a fresh beat sets the stale deadline");
        let expected = at(NEWS_PATH_MAX_AGE.as_secs_f64());
        assert!((deadline - expected).total_nanoseconds().abs() < 1_000_000);

        let silent = f.observe_at(at(21.0 * 60.0));
        let report = silent.attention();
        let (text, events) =
            news_text(silent.news(reader, &report)).expect("a silent path is news for a peer");
        assert!(text.contains("has not beaten"), "{text}");
        assert_eq!(events.len(), 1, "{text}");
        assert_ne!(events[0], beat_id, "a silence has its own event");
        assert!(
            matches!(silent.news(window, &report), News::Quiet),
            "a window is not told about its own news path"
        );

        f.store
            .commit(
                f.sources.presentations.source,
                &f.signer,
                orient_model::receipt_fragment(events, clock::point_now().unwrap()),
            )
            .unwrap();
        let later = f.observe_at(at(40.0 * 60.0));
        assert!(
            matches!(later.news(reader, &later.attention()), News::Quiet),
            "the same silence is said once"
        );
    }

    #[test]
    fn an_exited_news_path_carries_its_reason() {
        let mut f = Fixture::new();
        let window = *fucid();
        let session = *fucid();
        f.publish(path_beat(window, session, at(0.0)));
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>(
                "deliver complete Orient habit report: Orient delivery callback failed (exit status: 1)"
                    .to_owned(),
            )
            .unwrap();
        let exit = path_exit(window, session, at(10.0), reason);
        let exit_id = exit.root().unwrap();
        f.publish(exit);

        let observed = f.observe_at(at(30.0));
        let report = observed.attention();
        let (text, events) = news_text(observed.news(*fucid(), &report)).expect("exit is news");
        assert!(text.contains("exited"), "{text}");
        assert!(text.contains("callback failed (exit status: 1)"), "{text}");
        assert_eq!(events, vec![exit_id]);
        assert!(matches!(observed.news(window, &report), News::Quiet));
    }

    fn path_delivered(window: Id, session: Id, created: Epoch, resolves: Option<Id>) -> Fragment {
        entity! {
            metadata::tag: &KIND_NEWS_PATH_DELIVERED,
            attrs::node: &window,
            attrs::session: &session,
            metadata::created_at: clock::point(created).unwrap(),
            metadata::supersedes?: resolves,
        }
    }

    fn path_retry(
        window: Id,
        session: Id,
        created: Epoch,
        since: Epoch,
        attempt: u128,
        delay: f64,
        reason: TextHandle,
    ) -> Fragment {
        entity! {
            metadata::tag: &KIND_NEWS_PATH_RETRY,
            attrs::node: &window,
            attrs::session: &session,
            metadata::created_at: clock::point(created).unwrap(),
            metadata::started_at: clock::point(since).unwrap(),
            metadata::expires_at: clock::point(created + hifitime::Duration::from_seconds(delay)).unwrap(),
            metadata::description: reason,
            telemetry::attrs::failed: attempt,
        }
    }

    #[test]
    fn a_restart_reads_as_polling_resumed_until_a_delivery_is_accepted() {
        let mut f = Fixture::new();
        let window = *fucid();
        let first = *fucid();
        let second = *fucid();
        f.publish(path_beat(window, first, at(0.0)));
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("thread status is not idle".to_owned())
            .unwrap();
        let exit = path_exit(window, first, at(10.0), reason);
        let exit_id = exit.root().unwrap();
        f.publish(exit);
        f.publish(entity! {
            metadata::tag*: [KIND_NEWS_PATH_BEAT, schema::KIND_RECOVERED],
            attrs::node: &window,
            attrs::session: &second,
            metadata::created_at: clock::point(at(100.0)).unwrap(),
            metadata::supersedes: &exit_id,
        });

        let observed = f.observe_at(at(120.0));
        let (text, events) =
            news_text(observed.news(*fucid(), &observed.attention())).expect("a restart is news");
        assert!(text.contains("polling resumed"), "{text}");
        assert!(
            text.contains("callback has not accepted a report yet"),
            "{text}"
        );
        assert!(
            !text.contains("accepting again") && !text.contains("recovered"),
            "polling is not acceptance: {text}"
        );
        assert!(text.contains("thread status is not idle"), "{text}");
        assert_eq!(events, vec![second], "one resumption event per session");

        let delivered = path_delivered(window, second, at(150.0), Some(exit_id));
        let delivered_id = delivered.root().unwrap();
        f.publish(delivered);
        let beat = path_beat(window, second, at(200.0));
        let beat_id = beat.root().unwrap();
        f.publish(beat);
        let observed = f.observe_at(at(210.0));
        let (text, events) = news_text(observed.news(*fucid(), &observed.attention()))
            .expect("an accepted delivery is news");
        // Acceptance by the configured callback, never a claim of receipt.
        assert!(text.contains("callback accepting again"), "{text}");
        assert!(!text.contains("recovered"), "{text}");
        assert!(text.contains("thread status is not idle"), "{text}");
        assert_eq!(events, vec![delivered_id]);

        let silent = f.observe_at(at(200.0 + 21.0 * 60.0));
        let (text, events) =
            news_text(silent.news(*fucid(), &silent.attention())).expect("silence again");
        assert!(text.contains("has not beaten"), "{text}");
        assert_eq!(events.len(), 1, "{text}");
        assert_ne!(events[0], beat_id, "a silence has its own event");
    }

    #[test]
    fn a_retry_reads_as_degraded_until_a_later_delivery_or_an_exit() {
        let mut f = Fixture::new();
        let window = *fucid();
        let session = *fucid();
        f.publish(path_beat(window, session, at(0.0)));
        f.publish(path_delivered(window, session, at(5.0), None));
        let quiet = f.observe_at(at(10.0));
        assert!(
            quiet.attention().attention.is_empty(),
            "a first delivery that resolves nothing is not news"
        );

        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("app-server notLoaded".to_owned())
            .unwrap();
        let retry = path_retry(window, session, at(20.0), at(20.0), 1, 5.0, reason);
        let retry_id = retry.root().unwrap();
        f.publish(retry);
        let degraded = f.observe_at(at(22.0));
        let (text, events) =
            news_text(degraded.news(*fucid(), &degraded.attention())).expect("a retry is news");
        assert!(text.contains("degraded since"), "{text}");
        assert!(text.contains("attempt 1"), "{text}");
        assert!(text.contains("app-server notLoaded"), "{text}");
        assert!(text.contains("next try in 5s"), "{text}");
        assert_eq!(events, vec![retry_id]);

        // A later beat does not clear it, and neither does the older DELIVERED.
        f.publish(path_beat(window, session, at(30.0)));
        let still = f.observe_at(at(31.0));
        let (_, events) = news_text(still.news(*fucid(), &still.attention()))
            .expect("degraded holds past a beat");
        assert_eq!(events, vec![retry_id]);

        let delivered = path_delivered(window, session, at(40.0), Some(retry_id));
        let delivered_id = delivered.root().unwrap();
        f.publish(delivered);
        f.publish(path_beat(window, session, at(50.0)));
        let recovered = f.observe_at(at(51.0));
        let (text, events) = news_text(recovered.news(*fucid(), &recovered.attention()))
            .expect("an accepted delivery after a retry is news");
        assert!(text.contains("callback accepting again"), "{text}");
        assert!(!text.contains("recovered"), "{text}");
        // It names the deferral it follows, but is no longer the degraded state.
        assert!(text.contains("after delivery degraded since"), "{text}");
        assert!(!text.contains("next try in"), "{text}");
        assert_eq!(events, vec![delivered_id]);

        let again = path_retry(window, session, at(60.0), at(60.0), 2, 20.0, reason);
        f.publish(again);
        let gone = f
            .store
            .put::<blobencodings::UTF8String, _>("callback failed".to_owned())
            .unwrap();
        let exit = path_exit(window, session, at(70.0), gone);
        let exit_id = exit.root().unwrap();
        f.publish(exit);
        let exited = f.observe_at(at(71.0));
        let (text, events) =
            news_text(exited.news(*fucid(), &exited.attention())).expect("an exit is news");
        assert!(text.contains("exited"), "{text}");
        assert_eq!(events, vec![exit_id], "an exit ends the degraded state");
    }

    #[test]
    fn a_restart_during_a_deferral_carries_the_degraded_evidence_across_sessions() {
        let mut f = Fixture::new();
        let window = *fucid();
        let old = *fucid();
        let input = format!("{window:x}");
        let now = clock::now().unwrap();
        let ago = |seconds: f64| now - hifitime::Duration::from_seconds(seconds);
        // Session A: accepted once, then deferred, then stopped cleanly while
        // its backoff was pending; no exit record and no later DELIVERED.
        f.publish(path_beat(window, old, ago(300.0)));
        f.publish(path_delivered(window, old, ago(290.0), None));
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("callback_exit_75_known_not_submitted".to_owned())
            .unwrap();
        let retry = path_retry(window, old, ago(60.0), ago(60.0), 1, 5.0, reason);
        let retry_id = retry.root().unwrap();
        f.publish(retry);

        // Session B's first beat points at the unresolved retry.
        f.sources.start_beating();
        let mut emit = |_part| Ok(());
        let mut out = crate::out::Out::new(&mut emit);
        f.sources
            .poll_test(&mut f.store, &f.signer, &input, false, &mut out)
            .unwrap();
        let observed = f.sources.observe(&mut f.store, &f.signer).unwrap();
        assert!(
            exists!(pattern!(observed.facts.view(), [{ _?first @
                metadata::tag: &KIND_NEWS_PATH_BEAT,
                metadata::tag: &schema::KIND_RECOVERED,
                attrs::node: &window,
                metadata::supersedes: &retry_id,
            }])),
            "the new session's first beat must reference the unresolved retry"
        );
        let report = observed.attention();
        let (text, _) =
            news_text(observed.news(*fucid(), &report)).expect("the restart is news for a peer");
        assert!(text.contains("polling resumed"), "{text}");
        assert!(text.contains("delivery degraded since"), "{text}");
        assert!(text.contains("attempt 1"), "{text}");
        assert!(
            text.contains("callback_exit_75_known_not_submitted"),
            "{text}"
        );
        assert!(
            text.contains("callback has not accepted a report yet"),
            "{text}"
        );

        // Only an acceptance in the new session clears it.
        let path = f.sources.news_path_target().expect("persona resolved");
        on_delivery_notice(&mut f.store, &f.signer, &path, NewsPathNotice::Accepted).unwrap();
        let observed = f.sources.observe(&mut f.store, &f.signer).unwrap();
        let (text, _) = news_text(observed.news(*fucid(), &observed.attention()))
            .expect("acceptance in the new session is news");
        assert!(text.contains("callback accepting again"), "{text}");
        assert!(text.contains("delivery degraded since"), "{text}");
        assert!(!text.contains("polling resumed"), "{text}");
    }

    #[test]
    fn a_deferral_stays_carried_through_restarts_that_never_accept() {
        let mut f = Fixture::new();
        let window = *fucid();
        let old = *fucid();
        let input = format!("{window:x}");
        let now = clock::now().unwrap();
        let ago = |seconds: f64| now - hifitime::Duration::from_seconds(seconds);
        // Session A: deferred, then stopped cleanly while its backoff was
        // pending; no exit record and no later DELIVERED.
        f.publish(path_beat(window, old, ago(300.0)));
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("callback_exit_75_known_not_submitted".to_owned())
            .unwrap();
        let retry = path_retry(window, old, ago(60.0), ago(60.0), 1, 5.0, reason);
        let retry_id = retry.root().unwrap();
        f.publish(retry);

        // Sessions B, C and D each beat once and stop cleanly, and none has a
        // report accepted: every first beat names A's retry directly.
        let mut emit = |_part| Ok(());
        let mut out = crate::out::Out::new(&mut emit);
        for restart in ["B", "C", "D"] {
            f.sources.start_beating();
            f.sources
                .poll_test(&mut f.store, &f.signer, &input, false, &mut out)
                .unwrap();
            let path = f.sources.news_path_target().expect("persona resolved");
            let observed = f.sources.observe(&mut f.store, &f.signer).unwrap();
            assert!(
                exists!(pattern!(observed.facts.view(), [{ _?first @
                    metadata::tag: &KIND_NEWS_PATH_BEAT,
                    metadata::tag: &schema::KIND_RECOVERED,
                    attrs::node: &window,
                    attrs::session: &path.session,
                    metadata::supersedes: &retry_id,
                }])),
                "session {restart}'s first beat must reference A's unresolved retry"
            );
            let (text, _) = news_text(observed.news(*fucid(), &observed.attention()))
                .expect("the restart is news for a peer");
            assert!(text.contains("polling resumed"), "{restart}: {text}");
            assert!(
                text.contains("delivery degraded since"),
                "{restart}: {text}"
            );
            assert!(
                text.contains("callback_exit_75_known_not_submitted"),
                "{restart}: {text}"
            );
        }

        // An acceptance in D resolves it; the next session follows nothing.
        let path = f.sources.news_path_target().expect("persona resolved");
        on_delivery_notice(&mut f.store, &f.signer, &path, NewsPathNotice::Accepted).unwrap();
        f.sources.start_beating();
        f.sources
            .poll_test(&mut f.store, &f.signer, &input, false, &mut out)
            .unwrap();
        let next = f.sources.news_path_target().expect("persona resolved");
        let observed = f.sources.observe(&mut f.store, &f.signer).unwrap();
        assert!(
            !exists!(pattern!(observed.facts.view(), [{ _?first @
                metadata::tag: &KIND_NEWS_PATH_BEAT,
                attrs::session: &next.session,
                metadata::supersedes: _?prior,
            }])),
            "a session after an acceptance follows nothing"
        );
    }

    #[test]
    fn a_carried_deferral_outranks_a_silent_session() {
        let mut f = Fixture::new();
        let window = *fucid();
        let old = *fucid();
        let silent = *fucid();
        let input = format!("{window:x}");
        let now = clock::now().unwrap();
        let ago = |seconds: f64| now - hifitime::Duration::from_seconds(seconds);
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("callback_exit_75_known_not_submitted".to_owned())
            .unwrap();
        let retry = path_retry(window, old, ago(3600.0), ago(3600.0), 1, 5.0, reason);
        let retry_id = retry.root().unwrap();
        f.publish(retry);
        // Session B followed the retry, never had a report accepted, and then
        // went silent without an exit record.
        f.publish(entity! {
            metadata::tag*: [KIND_NEWS_PATH_BEAT, schema::KIND_RECOVERED],
            attrs::node: &window,
            attrs::session: &silent,
            metadata::created_at: clock::point(ago(1800.0)).unwrap(),
            metadata::supersedes: &retry_id,
        });

        // Polling resuming ends the silence; only an acceptance ends the
        // deferral, so session C still names the retry.
        f.sources.start_beating();
        let mut emit = |_part| Ok(());
        let mut out = crate::out::Out::new(&mut emit);
        f.sources
            .poll_test(&mut f.store, &f.signer, &input, false, &mut out)
            .unwrap();
        let path = f.sources.news_path_target().expect("persona resolved");
        let observed = f.sources.observe(&mut f.store, &f.signer).unwrap();
        assert!(
            exists!(pattern!(observed.facts.view(), [{ _?first @
                metadata::tag: &KIND_NEWS_PATH_BEAT,
                metadata::tag: &schema::KIND_RECOVERED,
                attrs::node: &window,
                attrs::session: &path.session,
                metadata::supersedes: &retry_id,
            }])),
            "session C's first beat must reference the retry, not B's silence"
        );
        let (text, _) = news_text(observed.news(*fucid(), &observed.attention()))
            .expect("the restart is news for a peer");
        assert!(text.contains("polling resumed"), "{text}");
        assert!(text.contains("delivery degraded since"), "{text}");
    }

    /// Record that `events` were shown, as a watcher does after news.
    fn present(f: &mut Fixture, events: Vec<Id>) {
        f.store
            .commit(
                f.sources.presentations.source,
                &f.signer,
                orient_model::receipt_fragment(events, clock::point_now().unwrap()),
            )
            .unwrap();
    }

    #[test]
    fn repeated_attempts_of_one_deferral_wake_peers_once() {
        let mut f = Fixture::new();
        let window = *fucid();
        let session = *fucid();
        let reader = *fucid();
        f.publish(path_beat(window, session, at(0.0)));
        f.publish(path_delivered(window, session, at(5.0), None));
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("callback_exit_75_known_not_submitted".to_owned())
            .unwrap();
        let first = path_retry(window, session, at(20.0), at(20.0), 1, 60.0, reason);
        let first_id = first.root().unwrap();
        f.publish(first);
        let degraded = f.observe_at(at(21.0));
        let (text, events) =
            news_text(degraded.news(reader, &degraded.attention())).expect("a deferral is news");
        assert!(text.contains("attempt 1"), "{text}");
        assert_eq!(events, vec![first_id]);
        present(&mut f, events);

        // Later attempts of the same stretch: the dashboard follows each
        // one, and none is news again.
        let mut last = first_id;
        for attempt in 2..=4u128 {
            let created = 20.0 + 60.0 * (attempt - 1) as f64;
            let retry = path_retry(
                window,
                session,
                at(created),
                at(20.0),
                attempt,
                60.0,
                reason,
            );
            last = retry.root().unwrap();
            f.publish(retry);
            let observed = f.observe_at(at(created + 1.0));
            let full = observed.report().text;
            assert!(
                full.contains(&format!(
                    "degraded since {}: attempt {attempt}, ",
                    utc(at(20.0))
                )),
                "{full}"
            );
            assert!(full.contains("next try in 1m"), "{full}");
            assert!(
                matches!(observed.news(reader, &observed.attention()), News::Quiet),
                "attempt {attempt} of a presented deferral is not news again"
            );
        }

        // An acceptance ends the stretch and is news of its own.
        let delivered = path_delivered(window, session, at(300.0), Some(last));
        let delivered_id = delivered.root().unwrap();
        f.publish(delivered);
        let accepting = f.observe_at(at(301.0));
        let (text, events) = news_text(accepting.news(reader, &accepting.attention()))
            .expect("an acceptance is news");
        assert!(text.contains("callback accepting again"), "{text}");
        assert_eq!(events, vec![delivered_id]);
        present(&mut f, events);

        // The first deferral after it starts a new stretch, and wakes again.
        let next = path_retry(window, session, at(400.0), at(400.0), 1, 60.0, reason);
        let next_id = next.root().unwrap();
        f.publish(next);
        let again = f.observe_at(at(401.0));
        let (text, events) = news_text(again.news(reader, &again.attention()))
            .expect("a new deferral after an acceptance is news");
        assert!(text.contains("attempt 1"), "{text}");
        assert_eq!(events, vec![next_id]);
    }

    #[test]
    fn a_deferral_carried_through_restarts_stays_one_episode_until_accepted() {
        let mut f = Fixture::new();
        let window = *fucid();
        let old = *fucid();
        let reader = *fucid();
        let input = format!("{window:x}");
        let now = clock::now().unwrap();
        let since = now - hifitime::Duration::from_seconds(60.0);
        // Session A deferred, was told to its peers, and stopped cleanly
        // while its backoff was pending.
        f.publish(path_beat(
            window,
            old,
            now - hifitime::Duration::from_seconds(300.0),
        ));
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("callback_exit_75_known_not_submitted".to_owned())
            .unwrap();
        let retry = path_retry(window, old, since, since, 7, 60.0, reason);
        let first_id = retry.root().unwrap();
        f.publish(retry);
        let observed = f.sources.observe(&mut f.store, &f.signer).unwrap();
        let (_, events) =
            news_text(observed.news(reader, &observed.attention())).expect("the deferral is news");
        assert_eq!(events, vec![first_id]);
        present(&mut f, events);

        let mut emit = |_part| Ok(());
        let mut out = crate::out::Out::new(&mut emit);
        let deferred = |attempt| NewsPathNotice::Retry {
            attempt,
            reason: "callback_exit_75_known_not_submitted",
            delay: Duration::from_secs(60),
        };
        // Sessions B and C restart without an acceptance and defer again.
        for restart in ["B", "C"] {
            f.sources.start_beating();
            f.sources
                .poll_test(&mut f.store, &f.signer, &input, false, &mut out)
                .unwrap();
            let observed = f.sources.observe(&mut f.store, &f.signer).unwrap();
            let (text, events) = news_text(observed.news(reader, &observed.attention()))
                .expect("the restart itself is news");
            assert!(text.contains("polling resumed"), "{restart}: {text}");
            present(&mut f, events);
            let path = f.sources.news_path_target().expect("persona resolved");
            for attempt in 1..=2 {
                on_delivery_notice(&mut f.store, &f.signer, &path, deferred(attempt)).unwrap();
                let observed = f.sources.observe(&mut f.store, &f.signer).unwrap();
                assert!(
                    matches!(observed.news(reader, &observed.attention()), News::Quiet),
                    "{restart} attempt {attempt}: a carried deferral is not a new episode"
                );
                let full = observed.report().text;
                assert!(
                    full.contains(&format!(
                        "degraded since {}: attempt {attempt}, ",
                        utc(since)
                    )),
                    "{restart}: {full}"
                );
            }
        }

        // An acceptance in C ends the stretch; the next deferral is new.
        let path = f.sources.news_path_target().expect("persona resolved");
        on_delivery_notice(&mut f.store, &f.signer, &path, NewsPathNotice::Accepted).unwrap();
        let observed = f.sources.observe(&mut f.store, &f.signer).unwrap();
        let (text, events) = news_text(observed.news(reader, &observed.attention()))
            .expect("the acceptance is news");
        assert!(text.contains("callback accepting again"), "{text}");
        present(&mut f, events);
        on_delivery_notice(&mut f.store, &f.signer, &path, deferred(1)).unwrap();
        let observed = f.sources.observe(&mut f.store, &f.signer).unwrap();
        let (text, events) = news_text(observed.news(reader, &observed.attention()))
            .expect("a deferral after an acceptance is a new episode");
        assert!(text.contains("attempt 1"), "{text}");
        assert!(
            !text.contains(&utc(since)),
            "a new stretch has its own start: {text}"
        );
        assert_ne!(events, vec![first_id]);
    }

    /// The one news event a peer has not been shown yet.
    fn only_news(observed: &HealthObservation, reader: Id) -> (String, Id) {
        let (text, events) = news_text(observed.news(reader, &observed.attention()))
            .expect("one new news-path event");
        assert_eq!(events.len(), 1, "{text}");
        (text, events[0])
    }

    fn quiet(observed: &HealthObservation, reader: Id) -> bool {
        matches!(observed.news(reader, &observed.attention()), News::Quiet)
    }

    const MIN: f64 = 60.0;

    #[test]
    fn an_overdue_retry_reads_as_silent_from_its_next_try() {
        let mut f = Fixture::new();
        let window = *fucid();
        let session = *fucid();
        let reader = *fucid();
        f.publish(path_beat(window, session, at(0.0)));
        f.publish(path_delivered(window, session, at(5.0), None));
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("callback_exit_75_known_not_submitted".to_owned())
            .unwrap();
        // Written at 20 s; its next try falls at T = 80 s and never comes.
        let retry = path_retry(window, session, at(20.0), at(20.0), 1, 60.0, reason);
        let retry_id = retry.root().unwrap();
        f.publish(retry);
        let next_try = 80.0;

        let degraded = f.observe_at(at(next_try + 19.0 * MIN));
        let report = degraded.attention();
        assert_eq!(
            report.next_change,
            Some(at(next_try + 20.0 * MIN)),
            "a reader wakes when the retry becomes overdue"
        );
        let (text, event) = only_news(&degraded, reader);
        assert!(text.contains("degraded since"), "{text}");
        assert_eq!(event, retry_id);
        present(&mut f, vec![event]);
        assert!(degraded.report().text.contains("degraded since"));

        let silent = f.observe_at(at(next_try + 21.0 * MIN));
        let full = silent.report().text;
        assert!(
            full.contains(&format!("silent, no retry since {}", utc(at(20.0)))),
            "{full}"
        );
        assert!(!full.contains(": degraded since"), "{full}");
        let (text, event) = only_news(&silent, reader);
        assert!(text.contains("has not beaten"), "{text}");
        assert!(
            text.contains(&format!("no retry since {}", utc(at(20.0)))),
            "{text}"
        );
        assert!(
            !text.contains("dead"),
            "missing evidence, not death: {text}"
        );
        assert!(!text.contains("still"), "not yet an escalation: {text}");
        assert_ne!(
            event, retry_id,
            "silence is news after the degraded stretch was told"
        );
        assert!(quiet(&silent, window), "a window is not told about itself");
        // The stretch's escalation deadline is still ahead.
        assert_eq!(silent.attention().next_change, Some(at(20.0 + 60.0 * MIN)));
        present(&mut f, vec![event]);
        assert!(quiet(&f.observe_at(at(next_try + 30.0 * MIN)), reader));
    }

    #[test]
    fn a_degraded_stretch_escalates_once_after_an_hour() {
        let mut f = Fixture::new();
        let window = *fucid();
        let session = *fucid();
        let reader = *fucid();
        f.publish(path_beat(window, session, at(0.0)));
        f.publish(path_delivered(window, session, at(5.0), None));
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("callback_exit_75_known_not_submitted".to_owned())
            .unwrap();
        let first = path_retry(window, session, at(20.0), at(20.0), 1, 60.0, reason);
        let first_id = first.root().unwrap();
        f.publish(first);
        let (_, event) = only_news(&f.observe_at(at(21.0)), reader);
        assert_eq!(event, first_id);
        present(&mut f, vec![event]);

        // Still retrying a minute before the hour: nothing new, and the
        // reader's next wake is the hour itself.
        f.publish(path_retry(
            window,
            session,
            at(3560.0),
            at(20.0),
            60,
            60.0,
            reason,
        ));
        let before = f.observe_at(at(3561.0));
        assert!(quiet(&before, reader));
        assert_eq!(before.attention().next_change, Some(at(20.0 + 60.0 * MIN)));

        // The hour passes with no new record: one more event.
        let after = f.observe_at(at(3621.0));
        let (text, escalation) = only_news(&after, reader);
        assert!(text.contains("still degraded after 60 min"), "{text}");
        assert!(text.contains(&utc(at(20.0))), "{text}");
        assert_ne!(escalation, first_id);
        assert!(quiet(&after, window), "a window is not told about itself");
        present(&mut f, vec![escalation]);

        // Later attempts and renders add nothing.
        for attempt in 61..=63u128 {
            let created = 3560.0 + 60.0 * (attempt - 60) as f64;
            f.publish(path_retry(
                window,
                session,
                at(created),
                at(20.0),
                attempt,
                60.0,
                reason,
            ));
            let observed = f.observe_at(at(created + 1.0));
            assert!(quiet(&observed, reader), "attempt {attempt}");
            assert!(observed.report().text.contains("degraded since"));
        }
    }

    #[test]
    fn a_stretch_carried_across_restarts_escalates_once() {
        let mut f = Fixture::new();
        let window = *fucid();
        let reader = *fucid();
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("callback_exit_75_known_not_submitted".to_owned())
            .unwrap();
        let first = path_retry(window, *fucid(), at(20.0), at(20.0), 1, 60.0, reason);
        let first_id = first.root().unwrap();
        f.publish(first);
        let (_, event) = only_news(&f.observe_at(at(21.0)), reader);
        assert_eq!(event, first_id);
        present(&mut f, vec![event]);

        // Each restart resumes polling without an acceptance, then defers
        // again under the same stretch start.
        let mut escalations = Vec::new();
        for (resumed, deferred) in [(100.0, 110.0), (3500.0, 3610.0), (3700.0, 3710.0)] {
            let session = *fucid();
            f.publish(entity! {
                metadata::tag*: [KIND_NEWS_PATH_BEAT, schema::KIND_RECOVERED],
                attrs::node: &window,
                attrs::session: &session,
                metadata::created_at: clock::point(at(resumed)).unwrap(),
                metadata::supersedes: &first_id,
            });
            let resumption = f.observe_at(at(resumed + 1.0));
            if let News::Report { events, .. } = resumption.news(reader, &resumption.attention()) {
                present(&mut f, events.into_iter().collect());
            }
            f.publish(path_retry(
                window,
                session,
                at(deferred),
                at(20.0),
                1,
                60.0,
                reason,
            ));
            let observed = f.observe_at(at(deferred + 30.0));
            match observed.news(reader, &observed.attention()) {
                News::Quiet => {}
                News::Report { text, events, .. } => {
                    assert!(text.contains("still degraded after 60 min"), "{text}");
                    escalations.extend(events.iter().copied());
                    present(&mut f, events.into_iter().collect());
                }
            }
        }
        assert_eq!(escalations.len(), 1, "one escalation for the whole stretch");
        assert_ne!(escalations[0], first_id);
    }

    #[test]
    fn silent_and_exited_paths_escalate_an_hour_after_their_record() {
        let mut f = Fixture::new();
        let silent = *fucid();
        let exited = *fucid();
        let reader = *fucid();
        let beat = path_beat(silent, *fucid(), at(0.0));
        let beat_id = beat.root().unwrap();
        f.publish(beat);
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("callback failed".to_owned())
            .unwrap();
        let exit = path_exit(exited, *fucid(), at(10.0), reason);
        let exit_id = exit.root().unwrap();
        f.publish(exit);

        let first = f.observe_at(at(21.0 * MIN));
        let (_, events) = news_text(first.news(reader, &first.attention())).unwrap();
        assert_eq!(events.len(), 2);
        assert!(events.contains(&exit_id));
        assert!(!events.contains(&beat_id), "a silence has its own event");
        assert_eq!(first.attention().next_change, Some(at(60.0 * MIN)));
        present(&mut f, events);

        let before = f.observe_at(at(60.0 * MIN - 1.0));
        assert!(quiet(&before, reader), "not before the hour");

        let silent_hour = f.observe_at(at(60.0 * MIN + 1.0));
        assert_eq!(
            silent_hour.attention().next_change,
            Some(at(10.0 + 60.0 * MIN))
        );
        let (text, event) = only_news(&silent_hour, reader);
        assert!(text.contains("still silent after 60 min"), "{text}");
        assert_ne!(event, beat_id);
        present(&mut f, vec![event]);

        let exit_hour = f.observe_at(at(10.0 + 60.0 * MIN + 1.0));
        let (text, event) = only_news(&exit_hour, reader);
        assert!(text.contains("still exited after 60 min"), "{text}");
        assert!(text.contains("callback failed"), "{text}");
        assert_ne!(event, exit_id);
        present(&mut f, vec![event]);

        assert!(quiet(&f.observe_at(at(120.0 * MIN)), reader), "said once");
    }

    #[test]
    fn a_stopped_deferral_escalates_through_its_deadlines_alone() {
        let mut f = Fixture::new();
        let window = *fucid();
        let reader = *fucid();
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("callback_exit_75_known_not_submitted".to_owned())
            .unwrap();
        let retry = path_retry(window, *fucid(), at(20.0), at(20.0), 1, 60.0, reason);
        f.publish(retry);
        // No record follows: each wake below comes from a deadline.
        let degraded = f.observe_at(at(21.0));
        let mut seen = Vec::new();
        let (_, event) = only_news(&degraded, reader);
        seen.push(event);
        present(&mut f, vec![event]);
        let overdue = degraded
            .attention()
            .next_change
            .expect("an overdue deadline");
        assert_eq!(overdue, at(80.0 + 20.0 * MIN));

        let silent = f.observe_at(overdue);
        let (text, event) = only_news(&silent, reader);
        assert!(text.contains("has not beaten"), "{text}");
        seen.push(event);
        present(&mut f, vec![event]);
        let hour = silent
            .attention()
            .next_change
            .expect("an escalation deadline");
        assert_eq!(hour, at(20.0 + 60.0 * MIN));

        let escalated = f.observe_at(hour);
        let (text, event) = only_news(&escalated, reader);
        assert!(text.contains("still silent after 60 min"), "{text}");
        assert!(!seen.contains(&event));
        present(&mut f, vec![event]);
        assert!(quiet(&f.observe_at(at(5.0 * 60.0 * MIN)), reader));
    }

    #[test]
    fn an_acceptance_before_the_hour_prevents_escalation_and_a_later_stretch_has_its_own() {
        let mut f = Fixture::new();
        let window = *fucid();
        let session = *fucid();
        let reader = *fucid();
        f.publish(path_beat(window, session, at(0.0)));
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("callback_exit_75_known_not_submitted".to_owned())
            .unwrap();
        let retry = path_retry(window, session, at(20.0), at(20.0), 1, 60.0, reason);
        let retry_id = retry.root().unwrap();
        f.publish(retry);
        let (_, event) = only_news(&f.observe_at(at(21.0)), reader);
        present(&mut f, vec![event]);

        let delivered = path_delivered(window, session, at(600.0), Some(retry_id));
        f.publish(delivered);
        let (text, event) = only_news(&f.observe_at(at(601.0)), reader);
        assert!(text.contains("callback accepting again"), "{text}");
        present(&mut f, vec![event]);
        f.publish(path_beat(window, session, at(3500.0)));
        let past_hour = f.observe_at(at(3700.0));
        assert!(
            quiet(&past_hour, reader),
            "an accepted stretch does not escalate"
        );

        // A new stretch after the acceptance escalates on its own clock.
        let next = path_retry(window, session, at(4000.0), at(4000.0), 1, 60.0, reason);
        let next_id = next.root().unwrap();
        f.publish(next);
        let (_, event) = only_news(&f.observe_at(at(4001.0)), reader);
        assert_eq!(event, next_id);
        present(&mut f, vec![event]);
        f.publish(path_retry(
            window,
            session,
            at(7550.0),
            at(4000.0),
            60,
            60.0,
            reason,
        ));
        let before = f.observe_at(at(7551.0));
        assert!(quiet(&before, reader));
        assert_eq!(
            before.attention().next_change,
            Some(at(4000.0 + 60.0 * MIN))
        );
        let (text, _) = only_news(&f.observe_at(at(7601.0)), reader);
        assert!(text.contains("still degraded after 60 min"), "{text}");
        assert!(text.contains(&utc(at(4000.0))), "{text}");
    }

    #[test]
    fn a_fresh_beat_after_an_old_retry_is_never_read_as_overdue() {
        let mut f = Fixture::new();
        let window = *fucid();
        let session = *fucid();
        let reader = *fucid();
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("callback_exit_75_known_not_submitted".to_owned())
            .unwrap();
        // The retry's next try (80 s) is overdue from 1280 s, but the session
        // beats after it: newer polling evidence.
        f.publish(path_retry(
            window,
            session,
            at(20.0),
            at(20.0),
            1,
            60.0,
            reason,
        ));
        f.publish(path_beat(window, session, at(1300.0)));
        let observed = f.observe_at(at(1400.0));
        let full = observed.report().text;
        assert!(full.contains(": degraded since"), "{full}");
        assert!(!full.contains("silent"), "{full}");
        assert!(!full.contains("no retry since"), "{full}");
        let (text, _) = only_news(&observed, reader);
        assert!(text.contains("degraded since"), "{text}");
        assert!(!text.contains("has not beaten"), "{text}");
        assert!(!text.contains("no retry since"), "{text}");
        // The beat's own staleness comes first, then the stretch's hour.
        assert_eq!(
            observed.attention().next_change,
            Some(at(1300.0 + 20.0 * MIN))
        );
    }

    #[test]
    fn an_exit_starts_its_own_hour() {
        let mut f = Fixture::new();
        let window = *fucid();
        let session = *fucid();
        let reader = *fucid();
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("callback_exit_75_known_not_submitted".to_owned())
            .unwrap();
        f.publish(path_retry(
            window,
            session,
            at(20.0),
            at(20.0),
            1,
            60.0,
            reason,
        ));
        let (_, event) = only_news(&f.observe_at(at(21.0)), reader);
        present(&mut f, vec![event]);
        let gone = f
            .store
            .put::<blobencodings::UTF8String, _>("callback failed".to_owned())
            .unwrap();
        let exit = path_exit(window, session, at(3000.0), gone);
        let exit_id = exit.root().unwrap();
        f.publish(exit);
        let (_, event) = only_news(&f.observe_at(at(3001.0)), reader);
        assert_eq!(event, exit_id);
        present(&mut f, vec![event]);

        // Past the degraded stretch's hour: the exit ended that stretch and
        // its own hour runs from the exit.
        let past = f.observe_at(at(3621.0));
        assert!(quiet(&past, reader));
        assert_eq!(past.attention().next_change, Some(at(3000.0 + 60.0 * MIN)));
        let (text, _) = only_news(&f.observe_at(at(3000.0 + 60.0 * MIN)), reader);
        assert!(text.contains("still exited after 60 min"), "{text}");
    }

    #[test]
    fn a_silence_after_an_accepted_delivery_wakes_its_peers() {
        let mut f = Fixture::new();
        let window = *fucid();
        let session = *fucid();
        let reader = *fucid();
        f.publish(path_beat(window, session, at(0.0)));
        f.publish(path_delivered(window, session, at(5.0), None));
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("callback_exit_75_known_not_submitted".to_owned())
            .unwrap();
        let retry = path_retry(window, session, at(20.0), at(20.0), 1, 60.0, reason);
        let retry_id = retry.root().unwrap();
        f.publish(retry);
        let (_, event) = only_news(&f.observe_at(at(21.0)), reader);
        present(&mut f, vec![event]);
        let delivered = path_delivered(window, session, at(60.0), Some(retry_id));
        let delivered_id = delivered.root().unwrap();
        f.publish(delivered);
        let (text, event) = only_news(&f.observe_at(at(61.0)), reader);
        assert!(text.contains("callback accepting again"), "{text}");
        assert_eq!(event, delivered_id);
        present(&mut f, vec![event]);

        // No beat follows the acceptance: the path goes silent.
        let silent = f.observe_at(at(60.0 + 21.0 * MIN));
        let (text, event) = only_news(&silent, reader);
        assert!(text.contains("has not beaten"), "{text}");
        assert_ne!(event, delivered_id, "the silence is not the acceptance");
        assert!(quiet(&silent, window), "a window is not told about itself");
        present(&mut f, vec![event]);
        assert!(quiet(&f.observe_at(at(60.0 + 30.0 * MIN)), reader));
    }

    #[test]
    fn a_silence_after_resumed_polling_wakes_its_peers() {
        let mut f = Fixture::new();
        let window = *fucid();
        let reader = *fucid();
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("callback failed".to_owned())
            .unwrap();
        let exit = path_exit(window, *fucid(), at(10.0), reason);
        let exit_id = exit.root().unwrap();
        f.publish(exit);
        let (_, event) = only_news(&f.observe_at(at(11.0)), reader);
        present(&mut f, vec![event]);
        let resumed = *fucid();
        let beat = entity! {
            metadata::tag*: [KIND_NEWS_PATH_BEAT, schema::KIND_RECOVERED],
            attrs::node: &window,
            attrs::session: &resumed,
            metadata::created_at: clock::point(at(100.0)).unwrap(),
            metadata::supersedes: &exit_id,
        };
        let beat_id = beat.root().unwrap();
        f.publish(beat);
        let (text, event) = only_news(&f.observe_at(at(101.0)), reader);
        assert!(text.contains("polling resumed"), "{text}");
        present(&mut f, vec![event]);

        // The resumed session never beats again.
        let (text, event) = only_news(&f.observe_at(at(100.0 + 21.0 * MIN)), reader);
        assert!(text.contains("has not beaten"), "{text}");
        assert_ne!(event, resumed);
        assert_ne!(event, beat_id);
        present(&mut f, vec![event]);
        assert!(quiet(&f.observe_at(at(100.0 + 30.0 * MIN)), reader));
    }

    #[test]
    fn a_silence_keeps_one_id_across_renders_and_is_said_once() {
        let mut f = Fixture::new();
        let window = *fucid();
        let reader = *fucid();
        f.publish(path_beat(window, *fucid(), at(0.0)));
        let first = f.observe_at(at(21.0 * MIN));
        let (_, event) = only_news(&first, reader);
        let second = f.observe_at(at(25.0 * MIN));
        assert_eq!(
            second.attention().attention.ids().collect::<Vec<_>>(),
            vec![event],
            "every render names the silence the same way"
        );
        present(&mut f, vec![event]);
        for minutes in [26.0, 40.0, 59.0] {
            assert!(
                quiet(&f.observe_at(at(minutes * MIN)), reader),
                "said once ({minutes} min)"
            );
        }
    }

    #[test]
    fn a_window_is_not_told_about_its_own_escalation() {
        let mut f = Fixture::new();
        let window = *fucid();
        f.publish(path_beat(window, *fucid(), at(0.0)));
        let observed = f.observe_at(at(61.0 * MIN));
        let (text, events) = news_text(observed.news(*fucid(), &observed.attention()))
            .expect("a peer hears the silence and its escalation");
        assert_eq!(events.len(), 2, "{text}");
        assert!(text.contains("still silent after 60 min"), "{text}");
        assert!(quiet(&observed, window));
    }

    #[test]
    fn delivered_is_written_on_each_acceptance_transition() {
        let mut f = Fixture::new();
        let window = *fucid();
        let input = format!("{window:x}");
        f.sources.start_beating();
        let mut emit = |_part| Ok(());
        let mut out = crate::out::Out::new(&mut emit);
        f.sources
            .poll_test(&mut f.store, &f.signer, &input, false, &mut out)
            .unwrap();
        let path = f.sources.news_path_target().expect("persona resolved");

        // Report A accepted, then accepted again: one transition.
        on_delivery_notice(&mut f.store, &f.signer, &path, NewsPathNotice::Accepted).unwrap();
        on_delivery_notice(&mut f.store, &f.signer, &path, NewsPathNotice::Accepted).unwrap();
        // Report B deferred once, then accepted: a second transition.
        on_delivery_notice(
            &mut f.store,
            &f.signer,
            &path,
            NewsPathNotice::Retry {
                attempt: 1,
                reason: "notLoaded",
                delay: Duration::from_secs(5),
            },
        )
        .unwrap();
        on_delivery_notice(&mut f.store, &f.signer, &path, NewsPathNotice::Accepted).unwrap();

        let observed = f.sources.observe(&mut f.store, &f.signer).unwrap();
        let facts = observed.facts.view();
        let retries: Vec<(Id, u128, TextHandle)> = find!(
            (retry: Id, attempt: u128, reason: TextHandle),
            pattern!(facts, [{ ?retry @
                metadata::tag: &KIND_NEWS_PATH_RETRY,
                attrs::node: &window,
                telemetry::attrs::failed: ?attempt,
                metadata::description: ?reason,
                metadata::started_at: _?since,
                metadata::expires_at: _?next,
            }])
        )
        .collect();
        assert_eq!(retries.len(), 1);
        let (retry_id, attempt, _) = retries[0];
        assert_eq!(attempt, 1);
        let delivered: Vec<Id> = find!(
            delivered: Id,
            pattern!(facts, [{ ?delivered @
                metadata::tag: &KIND_NEWS_PATH_DELIVERED,
                attrs::node: &window,
            }])
        )
        .collect();
        assert_eq!(
            delivered.len(),
            2,
            "first acceptance and first after a retry"
        );
        assert!(exists!(pattern!(facts, [{ _?resolved @
            metadata::tag: &KIND_NEWS_PATH_DELIVERED,
            metadata::supersedes: &retry_id,
        }])));
    }

    #[test]
    fn a_failed_first_beat_keeps_the_recovery_marker_for_the_next() {
        let mut f = Fixture::new();
        let window = *fucid();
        let input = format!("{window:x}");
        let reason = f
            .store
            .put::<blobencodings::UTF8String, _>("callback failed".to_owned())
            .unwrap();
        let exit = path_exit(window, *fucid(), clock::now().unwrap(), reason);
        let exit_id = exit.root().unwrap();
        f.publish(exit);
        let mut emit = |_part| Ok(());
        let mut out = crate::out::Out::new(&mut emit);
        // Select a view without beating, then start this observer's session.
        f.sources
            .poll_test(&mut f.store, &f.signer, &input, false, &mut out)
            .unwrap();
        f.sources.start_beating();
        let now = clock::now().unwrap();
        // A key the health collection does not admit: this publication fails.
        let stranger = SigningKey::from_bytes(&[74; 32]);
        f.sources
            .beat_if_due(&mut f.store, &stranger, &input, now, &mut out)
            .unwrap();
        let later = now + hifitime::Duration::from_seconds(6.0 * 60.0);
        f.sources
            .beat_if_due(&mut f.store, &f.signer, &input, later, &mut out)
            .unwrap();

        let observed = f.sources.observe(&mut f.store, &f.signer).unwrap();
        let marked: Vec<Id> = find!(
            beat: Id,
            pattern!(observed.facts.view(), [{ ?beat @
                metadata::tag: &KIND_NEWS_PATH_BEAT,
                metadata::tag: &schema::KIND_RECOVERED,
                attrs::node: &window,
                metadata::supersedes: &exit_id,
                metadata::created_at: clock::point(later).unwrap(),
            }])
        )
        .collect();
        assert_eq!(
            marked.len(),
            1,
            "the first PUBLISHED beat carries the marker"
        );
    }

    #[test]
    fn a_beating_observer_writes_one_beat_and_a_best_effort_exit() {
        let mut f = Fixture::new();
        let window = *fucid();
        let input = format!("{window:x}");
        f.sources.start_beating();
        let mut parts = Vec::new();
        let mut emit = |part| {
            parts.push(part);
            Ok(())
        };
        let mut out = crate::out::Out::new(&mut emit);
        f.sources
            .poll_test(&mut f.store, &f.signer, &input, false, &mut out)
            .unwrap();
        f.sources
            .poll_test(&mut f.store, &f.signer, &input, false, &mut out)
            .unwrap();
        let observed = f.sources.observe(&mut f.store, &f.signer).unwrap();
        let beats: Vec<(Id, Id)> = find!(
            (beat: Id, session: Id),
            pattern!(observed.facts.view(), [{ ?beat @
                metadata::tag: &KIND_NEWS_PATH_BEAT,
                attrs::node: &window,
                attrs::session: ?session,
            }])
        )
        .collect();
        assert_eq!(beats.len(), 1, "one beat per interval, not per poll");
        let (_, session) = beats[0];

        f.sources.record_exit(
            &mut f.store,
            &f.signer,
            &anyhow!("Orient delivery callback failed (exit status: 1)"),
            &mut out,
        );
        let observed = f.sources.observe(&mut f.store, &f.signer).unwrap();
        let exits: Vec<(Id, TextHandle)> = find!(
            (exit: Id, reason: TextHandle),
            pattern!(observed.facts.view(), [{ ?exit @
                metadata::tag: &KIND_NEWS_PATH_EXIT,
                attrs::node: &window,
                attrs::session: &session,
                metadata::description: ?reason,
            }])
        )
        .collect();
        assert_eq!(exits.len(), 1);
        let (exit_id, reason) = exits[0];
        let snapshot = f.store.snapshot().unwrap();
        let reason: View<str> =
            BlobStoreGet::get::<View<str>, blobencodings::UTF8String>(&snapshot, reason).unwrap();
        assert!(reason.contains("exit status: 1"), "{}", &*reason);

        // The next process's first beat names what it recovered from.
        let mut next = test_block_on(HealthSources::open(
            &test_storage(),
            &mut f.store,
            &f.signer,
            Duration::from_secs(60),
        ))
        .unwrap();
        next.start_beating();
        next.poll_test(&mut f.store, &f.signer, &input, false, &mut out)
            .unwrap();
        let observed = next.observe(&mut f.store, &f.signer).unwrap();
        let recovered: Vec<Id> = find!(
            beat: Id,
            pattern!(observed.facts.view(), [{ ?beat @
                metadata::tag: &KIND_NEWS_PATH_BEAT,
                metadata::tag: &schema::KIND_RECOVERED,
                attrs::node: &window,
                metadata::supersedes: &exit_id,
            }])
        )
        .collect();
        assert_eq!(recovered.len(), 1);
    }

    fn at(seconds: f64) -> Epoch {
        Epoch::from_unix_seconds(1_700_000_000.0 + seconds)
    }

    #[test]
    fn health_reader_reads_input_nothing_has_attached_yet() {
        let mut f = Fixture::new();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        let first = recorder
            .record(at(0.0), [condition(State::Current, false)])
            .unwrap();
        let first_id = first.root().unwrap();
        f.publish(first);
        f.observe_at(at(1.0));
        let second = recorder
            .record(at(2.0), [condition(State::Stalled, true)])
            .unwrap();
        let second_id = second.root().unwrap();
        f.publish(second);

        let reader_key = SigningKey::from_bytes(&[73; 32]);
        // Keep receipt publication local while health and Relations are
        // external, read-only input for this signing principal.
        f.sources.presentations = {
            let mut local = f.store.store();
            ReceiptSource::register(&mut *local, &reader_key).unwrap()
        };
        let before = f.store.snapshot().unwrap();
        let records: Vec<_> = before.records().unwrap().map(Result::unwrap).collect();
        let observed = f.sources.observe(&mut f.store, &reader_key).unwrap();
        let reports = find!(
            report: Id,
            pattern!(observed.facts.view(), [{ ?report @ metadata::tag: &schema::KIND_REPORT }])
        )
        .collect::<BTreeSet<_>>();
        // The report nothing has attached yet is read from its own bytes.
        assert_eq!(reports, BTreeSet::from([first_id, second_id]));
        assert_eq!(
            f.store
                .snapshot()
                .unwrap()
                .records()
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>(),
            records,
            "health-first reads must not require WRITE on remote inputs",
        );
        assert!(observed.presentations.is_empty());
        assert!(observed.snapshot.wants().unwrap().next().is_none());
    }

    #[test]
    fn health_reads_with_write_authority_maintain_before_observing() {
        let mut f = Fixture::new();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        let first = recorder
            .record(at(0.0), [condition(State::Current, false)])
            .unwrap();
        let first_id = first.root().unwrap();
        f.publish(first);

        let before = f.store.snapshot().unwrap();
        let ready = f.sources.observe(&mut f.store, &f.signer).unwrap();
        assert!(exists!(pattern!(ready.facts.view(), [
            { first_id @ metadata::tag: &schema::KIND_REPORT }
        ])));
        assert!(ready.latest.contains(first_id));
        assert!(f
            .sources
            .at(before.clone(), at(1.0))
            .unwrap()
            .facts
            .collection
            .cover()
            .is_empty());

        let second = recorder
            .record(at(2.0), [condition(State::Stalled, true)])
            .unwrap();
        let second_id = second.root().unwrap();
        f.publish(second);
        let observed = f.sources.observe(&mut f.store, &f.signer).unwrap();
        let reports: BTreeSet<_> = find!(
            report: Id,
            pattern!(observed.facts.view(), [{ ?report @ metadata::tag: &schema::KIND_REPORT }])
        )
        .collect();
        assert_eq!(reports, BTreeSet::from([first_id, second_id]));
        assert!(observed.latest.contains(second_id));
        assert!(!observed.latest.contains(first_id));
        assert!(
            !exists!(pattern!(ready.facts.view(), [
                { second_id @ metadata::tag: &schema::KIND_REPORT }
            ])),
            "foreground upkeep must not replace an earlier selected view"
        );
        assert!(
            observed.presentations.is_empty(),
            "observing is not presenting"
        );
        assert!(observed.snapshot.wants().unwrap().next().is_none());
    }

    #[test]
    fn health_reads_and_polls_carry_existing_private_receipts_before_delivery() {
        let mut f = Fixture::new();
        let persona = *fucid();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        f.publish(
            recorder
                .record(
                    clock::now().unwrap(),
                    [store_condition(State::Stalled, true)],
                )
                .unwrap(),
        );
        let health = f.sources.observe(&mut f.store, &f.signer).unwrap();
        let events: Vec<_> = health.report().attention.ids().collect();
        assert!(!events.is_empty());

        // Receipt ownership is the signing zooid, independent of the selected
        // contact. An earlier source COMMIT must be carried before reporting.
        let reader_key = SigningKey::from_bytes(&[73; 32]);
        f.sources.presentations = {
            let mut local = f.store.store();
            ReceiptSource::register(&mut *local, &reader_key).unwrap()
        };
        f.store
            .commit(
                f.sources.presentations.source,
                &reader_key,
                entity! {
                    metadata::tag: &KIND_PRESENTED,
                    presentation::event*: events.iter().copied(),
                },
            )
            .unwrap();
        let before = f.store.snapshot().unwrap();
        let raw_receipts = before
            .collection(f.sources.presentations.source)
            .unwrap()
            .cover()
            .clone();
        // The unattached receipt is read from its own bytes, so the events
        // are presented before any upkeep attaches it.
        let lagging = f.sources.at(before, clock::now().unwrap()).unwrap();
        assert!(events
            .iter()
            .all(|event| lagging.presentations.contains(*event)));
        assert!(matches!(
            lagging.news(persona, &lagging.report()),
            News::Quiet
        ));
        let ready = f.sources.observe(&mut f.store, &reader_key).unwrap();
        assert!(events
            .iter()
            .all(|event| ready.presentations.contains(*event)));
        assert!(matches!(ready.news(persona, &ready.report()), News::Quiet));
        let mut parts = Vec::new();
        let mut emit = |part| {
            parts.push(part);
            Ok(())
        };
        let (fired, _) = f
            .sources
            .poll_test(
                &mut f.store,
                &reader_key,
                &fmt_id(persona),
                false,
                &mut Out::new(&mut emit),
            )
            .unwrap();
        assert!(
            !fired,
            "foreground receipt upkeep suppresses an already presented report"
        );
        assert!(parts.is_empty());
        let after = f.store.snapshot().unwrap();
        assert_eq!(
            after
                .collection(f.sources.presentations.source)
                .unwrap()
                .cover(),
            &raw_receipts,
            "catching up a receipt projection must not author another receipt",
        );
        assert!(after.wants().unwrap().next().is_none());
    }

    #[test]
    fn foreground_dashboard_needs_no_historical_receipt_shards() {
        let mut f = Fixture::new();
        test_block_on(async {
            let observation = f
                .sources
                .observe_acquiring(&mut f.store, &f.signer)
                .await
                .unwrap();
            let report = observation.report_acquiring().await.unwrap();
            assert!(report.text.contains("not observed / not configured"));
            assert!(report.attention.is_empty());
        });
    }

    #[test]
    fn malformed_receipt_upkeep_is_reported_without_blocking_resident_health() {
        let mut f = Fixture::new();
        let persona = *fucid();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        f.publish(
            recorder
                .record(
                    clock::now().unwrap(),
                    [store_condition(State::Stalled, true)],
                )
                .unwrap(),
        );
        let receipts = f.sources.presentations.source;
        f.malformed_member(receipts);
        let before = f.store.snapshot().unwrap();
        assert!(receipts
            .writer_is_admitted(&before, f.signer.verifying_key())
            .unwrap());
        let raw_receipts = before.collection(receipts).unwrap().cover().clone();
        let failure = {
            let mut local = f.store.store();
            // The malformed member is in the SOURCE, so the hop that reads the
            // source is where it surfaces. Maintaining the Rank9 tip alone
            // would derive from the Succinct intermediate and never look.
            pollster::block_on(local.maintain_attached(f.sources.presentations.succinct, &f.signer))
                .err()
                .expect("resident malformed admitted receipt must actually fail upkeep")
        };
        assert!(format!("{failure:#}").contains("malformed"));

        // The baseline/show observation is still useful even though its
        // optional private presentation projection could not catch up.
        let observed = f.sources.observe(&mut f.store, &f.signer).unwrap();
        assert!(!observed.report().attention.is_empty());
        assert!(observed.presentations.is_empty());
        let mut text = String::new();
        let mut emit = |part| {
            let crate::out::Part::Text { text: part } = part else {
                bail!("expected health text");
            };
            text.push_str(&part);
            Ok(())
        };
        let (fired, _) = f
            .sources
            .poll_test(
                &mut f.store,
                &f.signer,
                &fmt_id(persona),
                true,
                &mut Out::new(&mut emit),
            )
            .unwrap();
        assert!(fired);
        assert!(text.contains("note: Orient receipt membership not refreshed"));
        assert!(text.contains("malformed"));
        assert!(text.contains("News:"));
        let after = f.store.snapshot().unwrap();
        assert_eq!(after.collection(receipts).unwrap().cover(), &raw_receipts);
        assert!(after.wants().unwrap().next().is_none());
    }

    #[test]
    fn failed_receipt_attempt_remains_visible_on_unchanged_quiet_polls() {
        let mut f = Fixture::new();
        let receipts = f.sources.presentations.source;
        f.malformed_member(receipts);
        let before = f.store.snapshot().unwrap();
        let records = before
            .records()
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>();
        for _ in 0..2 {
            let mut text = String::new();
            let mut emit = |part| {
                let crate::out::Part::Text { text: part } = part else {
                    bail!("expected health text");
                };
                text.push_str(&part);
                Ok(())
            };
            let (fired, _) = f
                .sources
                .poll_test(
                    &mut f.store,
                    &f.signer,
                    "unused-quiet-persona",
                    true,
                    &mut Out::new(&mut emit),
                )
                .unwrap();
            assert!(!fired);
            assert!(text.contains("note: Orient receipt membership not refreshed"));
            assert!(text.contains("malformed"));
            assert_eq!(f.sources.maintenance_passes.get(), 1);
        }
        let after = f.store.snapshot().unwrap();
        assert!(after.changes_since(&before).is_empty());
        assert_eq!(
            after
                .records()
                .unwrap()
                .map(Result::unwrap)
                .collect::<Vec<_>>(),
            records
        );
        assert!(after.wants().unwrap().next().is_none());
    }

    #[test]
    fn malformed_health_input_is_not_downgraded_to_a_receipt_warning() {
        let mut f = Fixture::new();
        let source = f.sources.health.source;
        f.malformed_member(source);
        let error = f
            .sources
            .observe(&mut f.store, &f.signer)
            .err()
            .expect("malformed health data remains a real observation error");
        assert!(format!("{error:#}").contains("malformed"));
        let mut parts = Vec::new();
        let mut emit = |part| {
            parts.push(part);
            Ok(())
        };
        assert!(f
            .sources
            .poll_test(
                &mut f.store,
                &f.signer,
                "unused-error-persona",
                true,
                &mut Out::new(&mut emit),
            )
            .is_err());
        assert!(parts.is_empty());
        assert!(f.sources.maintained_view.is_none());
        assert!(f
            .store
            .snapshot()
            .unwrap()
            .wants()
            .unwrap()
            .next()
            .is_none());
    }

    #[test]
    fn health_poll_maintains_new_input_without_turning_peek_into_a_receipt() {
        let mut f = Fixture::new();
        let persona = *fucid();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        f.publish(
            recorder
                .record(
                    clock::now().unwrap(),
                    [store_condition(State::Stalled, true)],
                )
                .unwrap(),
        );
        let mut parts = Vec::new();
        let mut emit = |part| {
            parts.push(part);
            Ok(())
        };
        assert!(
            f.sources
                .poll_test(
                    &mut f.store,
                    &f.signer,
                    &fmt_id(persona),
                    true,
                    &mut Out::new(&mut emit),
                )
                .unwrap()
                .0
        );
        assert!(!parts.is_empty());
        let peeked = f.store.snapshot().unwrap();
        assert!(peeked
            .collection(f.sources.presentations.source)
            .unwrap()
            .cover()
            .is_empty());
        assert!(peeked.wants().unwrap().next().is_none());

        parts.clear();
        let mut emit = |part| {
            parts.push(part);
            Ok(())
        };
        assert!(
            f.sources
                .poll_test(
                    &mut f.store,
                    &f.signer,
                    &fmt_id(persona),
                    false,
                    &mut Out::new(&mut emit),
                )
                .unwrap()
                .0
        );
        assert!(!parts.is_empty());

        // A new watcher has no process-local receipt state to lean on. Its
        // first poll carries the committed receipt through the ordinary set.
        let mut rearmed = test_block_on(HealthSources::open(
            &test_storage(),
            &mut f.store,
            &f.signer,
            Duration::from_secs(60),
        ))
        .unwrap();
        parts.clear();
        let mut emit = |part| {
            parts.push(part);
            Ok(())
        };
        assert!(
            !rearmed
                .poll_test(
                    &mut f.store,
                    &f.signer,
                    &fmt_id(persona),
                    false,
                    &mut Out::new(&mut emit),
                )
                .unwrap()
                .0
        );
        assert!(parts.is_empty());
    }

    /// An own historical receipt whose payload is nowhere is the receipt
    /// projection's lag on the health path, as on the ordinary path: no poll
    /// calls the refresh failed, and the receipt a delivered report records
    /// still reaches Rank9 around it, so a re-armed watcher does not repeat
    /// the report.
    #[test]
    fn own_receipt_payload_that_is_nowhere_is_lag_and_does_not_repeat_a_report() {
        let mut f = Fixture::new();
        let persona = *fucid();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        f.publish(
            recorder
                .record(
                    clock::now().unwrap(),
                    [store_condition(State::Stalled, true)],
                )
                .unwrap(),
        );
        // A receipt of this signer's arrives without its archive.
        let cold = orient_model::receipt_fragment([*fucid()], clock::point_now().unwrap());
        let cold = IntoBlob::<SimpleArchive>::to_blob(cold.facts().clone());
        f.store
            .insert(CollectionRecord::Commit(CollectionCommit::sign(
                &f.signer,
                f.sources.presentations.source.handle(),
                inlineencodings::Handle::<SimpleArchive>::to_hash(cold.get_handle()),
                empty_metadata_handle(),
            )))
            .unwrap();

        let poll = |sources: &mut HealthSources, store: &mut FacultyStore| {
            let mut text = String::new();
            let mut emit = |part| {
                let crate::out::Part::Text { text: part } = part else {
                    bail!("expected health text");
                };
                text.push_str(&part);
                Ok(())
            };
            let (fired, _) = sources
                .poll_test(
                    store,
                    &f.signer,
                    &fmt_id(persona),
                    false,
                    &mut Out::new(&mut emit),
                )
                .unwrap();
            (fired, text)
        };
        let (fired, text) = poll(&mut f.sources, &mut f.store);
        assert!(fired);
        assert!(!text.contains("not refreshed"), "{text}");

        let mut rearmed = test_block_on(HealthSources::open(
            &test_storage(),
            &mut f.store,
            &f.signer,
            Duration::from_secs(60),
        ))
        .unwrap();
        let (fired, text) = poll(&mut rearmed, &mut f.store);
        assert!(!fired, "a delivered report must not repeat: {text}");
        assert!(text.is_empty(), "{text}");
        let after = f.store.snapshot().unwrap();
        assert!(!after.contains_blob(cold.get_handle()).unwrap());
        assert!(after.wants().unwrap().next().is_none());
    }

    #[test]
    fn unchanged_health_poll_prefix_skips_upkeep_after_own_writes_settle() {
        let mut f = Fixture::new();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        f.publish(
            recorder
                .record(at(0.0), [condition(State::Current, false)])
                .unwrap(),
        );
        assert!(f
            .sources
            .maintain_if_changed(&mut f.store, &f.signer)
            .unwrap());
        assert_eq!(f.sources.maintenance_passes.get(), 1);
        assert!(f
            .sources
            .maintain_if_changed(&mut f.store, &f.signer)
            .unwrap());
        assert_eq!(f.sources.maintenance_passes.get(), 2);
        let before = f.store.snapshot().unwrap();
        for instant in [1.0, 60.0, -1.0, 2.0] {
            assert!(!f
                .sources
                .maintain_if_changed(&mut f.store, &f.signer)
                .unwrap());
            f.sources
                .refresh_poll_view(f.store.snapshot().unwrap(), at(instant))
                .unwrap();
        }
        assert_eq!(f.sources.maintenance_passes.get(), 2);
        assert_eq!(f.sources.poll_observations, 1);
        assert!(f
            .store
            .snapshot()
            .unwrap()
            .changes_since(&before)
            .is_empty());
    }

    #[test]
    fn health_upkeep_does_not_cover_a_second_writer_after_its_attempt() {
        let mut f = Fixture::new();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        f.publish(
            recorder
                .record(at(0.0), [condition(State::Current, false)])
                .unwrap(),
        );
        f.sources
            .maintain_if_changed(&mut f.store, &f.signer)
            .unwrap();
        let earlier = f.sources.at(f.store.snapshot().unwrap(), at(1.0)).unwrap();

        // Deterministic interleaving: a separately opened writer appends after
        // upkeep, but before this reader's final selected observation. No
        // concurrent-thread timing is needed to place the append at the seam.
        let second = recorder
            .record(at(2.0), [condition(State::Stalled, true)])
            .unwrap();
        let second_id = second.root().unwrap();
        let mut writer = open_store(&f._directory.path().join("health.pile")).unwrap();
        writer
            .commit(f.sources.health.source, &f.signer, second)
            .unwrap();
        // The raced report is read from its own bytes before any upkeep
        // attaches it, so the refreshed view already has it.
        let raced = f.store.snapshot().unwrap();
        f.sources.refresh_poll_view(raced, at(3.0)).unwrap();
        assert!(exists!(
            pattern!(f.sources.poll_view.as_ref().unwrap().1.facts.view(), [
                { second_id @ metadata::tag: &schema::KIND_REPORT }
            ])
        ));

        assert!(f
            .sources
            .maintain_if_changed(&mut f.store, &f.signer)
            .unwrap());
        f.sources
            .refresh_poll_view(f.store.snapshot().unwrap(), at(3.0))
            .unwrap();
        assert!(exists!(
            pattern!(f.sources.poll_view.as_ref().unwrap().1.facts.view(), [
                { second_id @ metadata::tag: &schema::KIND_REPORT }
            ])
        ));
        assert!(!exists!(pattern!(earlier.facts.view(), [
            { second_id @ metadata::tag: &schema::KIND_REPORT }
        ])));
    }

    #[test]
    fn reader_age_deadline_changes_attention_without_new_records_and_never_repeats() {
        let mut f = Fixture::new();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        let heartbeat = recorder
            .record(at(0.0), [condition(State::Current, false)])
            .unwrap();
        let report_id = heartbeat.root().unwrap();
        f.publish(heartbeat);
        let fresh = f.observe_at(at(59.0));
        let before = fresh.report();
        assert!(before.attention.is_empty());
        assert!(before.text.contains("converged at observed pairwise roots"));
        assert_eq!(before.next_change, Some(at(60.0)));
        assert_eq!(
            until(before.next_change, at(59.0)),
            Some(Duration::from_secs(1))
        );

        // Same stored records and resident targets: time is the only difference.
        let expired = f.sources.at(f.store.snapshot().unwrap(), at(60.0)).unwrap();
        assert!(expired.snapshot.changes_since(&fresh.snapshot).is_empty());
        let after = expired.report();
        assert_eq!(after.attention.ids().collect::<Vec<_>>(), vec![report_id]);
        assert!(after.text.contains("unknown (report too old)"));
        assert!(after.text.contains("last sample only"));
        assert_eq!(after.next_change, None);
        let persona = *fucid();
        save_test_presentations(&mut f.store, &f.signer, [report_id]).unwrap();
        let next = f.observe_at(at(100.0));
        assert!(matches!(next.news(persona, &next.report()), News::Quiet));
        assert!(next.snapshot.wants().unwrap().next().is_none());
    }

    #[test]
    fn health_poll_reuses_views_through_expiry_and_clock_rollback() {
        let mut f = Fixture::new();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        let report = recorder
            .record(at(0.0), [condition(State::Current, false)])
            .unwrap();
        let report_id = report.root().unwrap();
        f.publish(report);
        // Complete local production before the counted polling observation.
        assert!(f.sources.maintain(&f.store, &f.signer).unwrap().is_none());
        let watermark = f.store.snapshot().unwrap();
        for (instant, stale, next) in [
            (59.0, false, Some(60.0)),
            (59.5, false, Some(60.0)),
            (60.0, true, None),
            (61.0, true, None),
            (-1.0, false, Some(0.0)),
            (0.0, false, Some(60.0)),
        ] {
            let sampled = f.store.snapshot().unwrap();
            assert!(sampled.changes_since(&watermark).is_empty());
            f.sources.refresh_poll_view(sampled, at(instant)).unwrap();
            let report = f.sources.poll_view.as_ref().unwrap().1.report();
            assert_eq!(report.attention.ids().any(|id| id == report_id), stale);
            assert_eq!(report.next_change, next.map(at));
            assert_eq!(f.sources.poll_observations, 1);
        }
    }

    #[test]
    fn health_poll_reuses_views_after_unrelated_blob_and_collection_appends() {
        let mut f = Fixture::new();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        f.publish(
            recorder
                .record(at(0.0), [condition(State::Current, false)])
                .unwrap(),
        );
        assert!(f.sources.maintain(&f.store, &f.signer).unwrap().is_none());
        let before = f.store.snapshot().unwrap();
        f.sources
            .refresh_poll_view(before.clone(), at(1.0))
            .unwrap();
        assert_eq!(f.sources.poll_observations, 1);

        f.store
            .put::<blobencodings::UTF8String, _>("unrelated hydrated payload".to_owned())
            .unwrap();
        let unrelated =
            open_configured(&mut f.store, MESSAGE_SCOPE_ID, f.signer.verifying_key()).unwrap();
        f.store
            .commit(
                unrelated,
                &f.signer,
                entity! { metadata::tag: &metadata::KIND_MULTI },
            )
            .unwrap();
        let sampled = f.store.snapshot().unwrap();
        let changes = sampled.changes_since(&before);
        assert!(changes.contains(StoreChanges::BLOBS));
        assert!(changes.contains(StoreChanges::COLLECTION_RECORDS));
        assert!(f.sources.poll_view.as_ref().unwrap().1.is_current(&sampled));

        f.sources
            .refresh_poll_view(sampled.clone(), at(2.0))
            .unwrap();
        assert_eq!(f.sources.poll_observations, 1);
        let observed = &f.sources.poll_view.as_ref().unwrap().1;
        assert_eq!(observed.evaluated_at, at(2.0));
        assert!(observed.report().attention.is_empty());
        assert!(f
            .store
            .snapshot()
            .unwrap()
            .changes_since(&sampled)
            .is_empty());
    }

    #[test]
    fn health_poll_refreshes_when_only_latest_target_advances() {
        let mut f = Fixture::new();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        f.publish(
            recorder
                .record(at(0.0), [store_condition(State::Stalled, true)])
                .unwrap(),
        );
        assert!(f.sources.maintain(&f.store, &f.signer).unwrap().is_none());
        f.sources
            .refresh_poll_view(f.store.snapshot().unwrap(), at(1.0))
            .unwrap();
        let original_facts = f
            .sources
            .poll_view
            .as_ref()
            .unwrap()
            .1
            .facts
            .collection
            .cover()
            .clone();
        assert!(!f
            .sources
            .poll_view
            .as_ref()
            .unwrap()
            .1
            .report()
            .attention
            .is_empty());

        f.publish(
            recorder
                .record(at(2.0), [store_condition(State::Current, false)])
                .unwrap(),
        );
        {
            let mut local = f.store.store();
            drop(pollster::block_on(local.maintain_attached(f.sources.latest, &f.signer)).unwrap());
        }
        let sampled = f.store.snapshot().unwrap();
        let prior = &f.sources.poll_view.as_ref().unwrap().1;
        // The new report is a new node of the parent both reads stand on.
        assert!(!prior.facts.is_current(&sampled));
        assert!(!prior.latest_collection.is_current(&sampled));
        f.sources
            .refresh_poll_view(sampled.clone(), at(3.0))
            .unwrap();
        assert_eq!(f.sources.poll_observations, 2);
        // Nothing attached the new report to the facts; they read it from its
        // bytes, so the recovery it records is what the refreshed view shows.
        let observed = &f.sources.poll_view.as_ref().unwrap().1;
        assert_eq!(observed.facts.collection.cover(), &original_facts);
        let report = observed.report();
        assert!(!report.attention.is_empty());
        assert!(report.text.contains("fresh"));
        assert!(!report.text.contains("not observed / not configured"));
        assert!(f
            .store
            .snapshot()
            .unwrap()
            .changes_since(&sampled)
            .is_empty());
    }

    #[test]
    fn health_poll_refreshes_private_membership_only_when_its_receipts_move() {
        let mut f = Fixture::new();
        let persona = *fucid();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        f.publish(
            recorder
                .record(at(0.0), [store_condition(State::Stalled, true)])
                .unwrap(),
        );
        assert!(f.sources.maintain(&f.store, &f.signer).unwrap().is_none());
        f.sources
            .refresh_poll_view(f.store.snapshot().unwrap(), at(1.0))
            .unwrap();
        let events: Vec<_> = f
            .sources
            .poll_view
            .as_ref()
            .unwrap()
            .1
            .report()
            .attention
            .ids()
            .collect();
        assert!(!events.is_empty());
        f.store
            .commit(
                f.sources.presentations.source,
                &f.signer,
                entity! {
                    metadata::tag: &KIND_PRESENTED,
                    presentation::event*: events.iter().copied(),
                },
            )
            .unwrap();
        // The receipt is read from its own bytes before any upkeep attaches
        // it: the poll view is no longer current, and the refreshed one
        // presents the events at once.
        let committed = f.store.snapshot().unwrap();
        assert!(!f
            .sources
            .poll_view
            .as_ref()
            .unwrap()
            .1
            .is_current(&committed));
        f.sources
            .refresh_poll_view(committed.clone(), at(2.0))
            .unwrap();
        assert_eq!(f.sources.poll_observations, 2);
        let observed = &f.sources.poll_view.as_ref().unwrap().1;
        assert!(events
            .iter()
            .all(|event| observed.presentations.contains(*event)));
        assert!(matches!(
            observed.news(persona, &observed.report()),
            News::Quiet
        ));
        assert!(f
            .store
            .snapshot()
            .unwrap()
            .changes_since(&committed)
            .is_empty());

        // Attaching the receipt moves the receipt read and nothing else.
        {
            let mut local = f.store.store();
            drop(
                pollster::block_on(
                    local.maintain_attached(f.sources.presentations.succinct, &f.signer),
                )
                .unwrap(),
            );
            drop(
                pollster::block_on(
                    local.maintain_attached(f.sources.presentations.rank9, &f.signer),
                )
                .unwrap(),
            );
        }
        let attached = f.store.snapshot().unwrap();
        let prior = &f.sources.poll_view.as_ref().unwrap().1;
        assert!(prior.facts.is_current(&attached));
        assert!(prior.latest_collection.is_current(&attached));
        assert!(!prior.presentations.is_current(&attached));
        f.sources
            .refresh_poll_view(attached.clone(), at(3.0))
            .unwrap();
        assert_eq!(f.sources.poll_observations, 3);
        let observed = &f.sources.poll_view.as_ref().unwrap().1;
        assert!(events
            .iter()
            .all(|event| observed.presentations.contains(*event)));
        assert!(matches!(
            observed.news(persona, &observed.report()),
            News::Quiet
        ));
    }

    #[test]
    fn latest_report_reordering_and_healthy_heartbeats_are_quiet() {
        let mut f = Fixture::new();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        let old = recorder
            .record(at(0.0), [condition(State::Unknown, false)])
            .unwrap();
        let first = recorder
            .record(at(10.0), [condition(State::Current, false)])
            .unwrap();
        let next = recorder
            .record(at(20.0), [condition(State::Current, false)])
            .unwrap();
        f.publish(next);
        f.publish(old);
        f.publish(first);
        let report = f.observe_at(at(70.0)).report();
        assert!(report.attention.is_empty());
        assert_eq!(report.next_change, Some(at(80.0)));
        assert_eq!(report.text.matches("- observer").count(), 1);
        assert!(report.text.contains("converged at observed pairwise roots"));
        assert!(!report.text.contains("report too old"));
    }

    /// The collection-sync hotfix, pinned in both directions.
    ///
    /// A collection-sync condition still renders in the report `show` prints --
    /// nothing is hidden -- but it no longer becomes attention, so it wakes
    /// nobody. Every other component must still wake, which is the half that
    /// keeps a peer cut off from the swarm able to notice and reconnect.
    #[test]
    fn collection_sync_renders_but_does_not_wake_while_other_components_do() {
        let mut f = Fixture::new();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        f.publish(
            recorder
                .record(at(0.0), [condition(State::Stalled, true)])
                .unwrap(),
        );
        let collection = f.observe_at(at(1.0)).report();
        assert!(
            collection.attention.is_empty(),
            "collection sync must not wake anyone, got {:?}",
            collection.attention.ids().collect::<Vec<_>>()
        );
        assert!(
            collection.text.contains("collection sync"),
            "the condition must still be visible to `show`: {}",
            collection.text
        );

        let mut other = Fixture::new();
        let mut other_recorder = Recorder::new(other.signer.verifying_key());
        other.publish(
            other_recorder
                .record(at(0.0), [store_condition(State::Stalled, true)])
                .unwrap(),
        );
        assert_eq!(
            other.observe_at(at(1.0)).report().attention.ids().len(),
            1,
            "a non-collection component must still wake"
        );
    }

    #[test]
    fn failure_and_recovery_are_each_reported_once() {
        let mut f = Fixture::new();
        let persona = *fucid();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        f.publish(
            recorder
                .record(at(0.0), [store_condition(State::Stalled, true)])
                .unwrap(),
        );
        let failure = f.observe_at(at(1.0)).report();
        assert_eq!(failure.attention.ids().len(), 1);
        save_test_presentations(&mut f.store, &f.signer, failure.attention.ids()).unwrap();
        f.publish(
            recorder
                .record(at(10.0), [store_condition(State::Stalled, true)])
                .unwrap(),
        );
        let heartbeat = f.observe_at(at(11.0));
        assert_eq!(failure.attention, heartbeat.report().attention);
        assert!(matches!(
            heartbeat.news(persona, &heartbeat.report()),
            News::Quiet
        ));

        f.publish(
            recorder
                .record(at(20.0), [store_condition(State::Current, false)])
                .unwrap(),
        );
        let recovered = f.observe_at(at(21.0));
        let recovered_report = recovered.report();
        assert_eq!(recovered_report.attention.ids().len(), 1);
        let recovery_ids: Vec<_> = recovered_report.attention.ids().collect();
        let recovery_news = recovered.news(persona, &recovered_report);
        assert!(
            matches!(&recovery_news, News::Report { text, .. } if text.contains("recovered; current"))
        );
        save_test_presentations(&mut f.store, &f.signer, recovery_ids).unwrap();
        f.publish(
            recorder
                .record(at(30.0), [store_condition(State::Current, false)])
                .unwrap(),
        );
        let heartbeat = f.observe_at(at(31.0));
        assert!(matches!(
            heartbeat.news(persona, &heartbeat.report()),
            News::Quiet
        ));

        let mut restarted = Recorder::new(f.signer.verifying_key());
        f.publish(
            restarted
                .record(at(40.0), [store_condition(State::Current, false)])
                .unwrap(),
        );
        assert!(f.observe_at(at(41.0)).report().attention.is_empty());
    }

    #[test]
    fn counter_changes_do_not_repeat_an_episode_and_a_later_failure_is_new() {
        let mut f = Fixture::new();
        let persona = *fucid();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        let measurement = |state, alert, resident_blobs| Measurement {
            condition: Condition {
                component: Component::Store,
                collection: None,
                peer: None,
                state,
                alert,
            },
            evidence: Evidence {
                resident_blobs: Some(resident_blobs),
                ..Evidence::default()
            },
        };

        f.publish(
            recorder
                .record_measurements(at(0.0), [measurement(State::Stalled, true, 17)])
                .unwrap(),
        );
        let first = f.observe_at(at(1.0));
        let first_report = first.report();
        let first_ids: Vec<_> = first_report.attention.ids().collect();
        assert_eq!(first_ids.len(), 1);
        save_test_presentations(&mut f.store, &f.signer, first_ids.clone()).unwrap();

        f.publish(
            recorder
                .record_measurements(at(10.0), [measurement(State::Stalled, true, 18)])
                .unwrap(),
        );
        let changed = f.observe_at(at(11.0));
        let changed_report = changed.report();
        let changed_ids: Vec<_> = changed_report.attention.ids().collect();
        assert_eq!(changed_ids.len(), 1);
        assert_ne!(
            changed_ids, first_ids,
            "fresh counters replace the condition entity"
        );
        assert!(matches!(
            changed.news(persona, &changed_report),
            News::Quiet
        ));

        f.publish(
            recorder
                .record_measurements(at(20.0), [measurement(State::Current, false, 19)])
                .unwrap(),
        );
        let recovered = f.observe_at(at(21.0));
        let recovered_report = recovered.report();
        let recovered_ids: Vec<_> = recovered_report.attention.ids().collect();
        assert_eq!(recovered_ids.len(), 1);
        assert!(matches!(
            recovered.news(persona, &recovered_report),
            News::Report { .. }
        ));
        save_test_presentations(&mut f.store, &f.signer, recovered_ids).unwrap();

        f.publish(
            recorder
                .record_measurements(at(30.0), [measurement(State::Current, false, 20)])
                .unwrap(),
        );
        let changed_recovery = f.observe_at(at(31.0));
        assert!(matches!(
            changed_recovery.news(persona, &changed_recovery.report()),
            News::Quiet
        ));

        f.publish(
            recorder
                .record_measurements(at(40.0), [measurement(State::Stalled, true, 21)])
                .unwrap(),
        );
        let next_failure = f.observe_at(at(41.0));
        assert!(matches!(
            next_failure.news(persona, &next_failure.report()),
            News::Report { .. }
        ));
    }

    #[test]
    fn news_groups_collection_sync_alerts_with_the_same_observer_and_peer() {
        let mut f = Fixture::new();
        let persona = *fucid();
        let first = *fucid();
        let second = *fucid();
        let group = CollectionSyncGroup {
            observer: "010203040506".to_owned(),
            peer_scope: " via [111213141516]".to_owned(),
            issue: CollectionSyncIssue::ComparisonUnavailable,
        };
        let mut attention = AttentionView::default();
        attention.insert(AttentionEvent::Health {
            event: first,
            detail: "observer [010203040506] collection sync alpha [aaaaaaaaaaaa] via [111213141516]: pairwise-root comparison unavailable beyond progress grace".to_owned(),
            collection_group: Some(group.clone()),
        });
        attention.insert(AttentionEvent::Health {
            event: second,
            detail: "observer [010203040506] collection sync beta [bbbbbbbbbbbb] via [111213141516]: pairwise-root comparison unavailable beyond progress grace".to_owned(),
            collection_group: Some(group),
        });
        let report = HealthReport {
            text: "complete snapshot".to_owned(),
            attention,
            next_change: None,
        };
        let observation = f.observe_at(at(1.0));
        let (text, events) = match observation.news(persona, &report) {
            News::Report { text, events, .. } => (text, events),

            News::Quiet => panic!("grouped alerts must remain actionable"),
        };
        assert_eq!(text.lines().count(), 1);
        assert!(text.contains(
            "observer [010203040506] collection sync via [111213141516]: 2 collection comparisons unavailable beyond progress grace"
        ));
        assert!(!text.contains("complete snapshot"));
        assert!(!text.contains("alpha"));
        assert!(!text.contains("beta"));
        assert_eq!(
            events.into_iter().collect::<BTreeSet<_>>(),
            BTreeSet::from([first, second])
        );
    }

    #[test]
    fn opaque_ids_extra_facts_partial_rows_and_source_isolation_remain_readable() {
        let mut f = Fixture::new();
        let report = fucid();
        let node = fucid();
        let session = fucid();
        let issue = fucid();
        let unfamiliar = fucid();
        let facts = entity! { &report @
            metadata::tag: &schema::KIND_REPORT,
            metadata::created_at: clock::point(at(0.0)).unwrap(),
            metadata::expires_at: clock::point(at(60.0)).unwrap(),
            attrs::node: &node,
            attrs::session: &session,
            attrs::condition: &issue,
        } + entity! { &node @ attrs::endpoint: f.signer.verifying_key() }
            + entity! { &issue @
                metadata::tag*: [schema::KIND_CONDITION, schema::DHT, schema::KIND_ALERT, *unfamiliar],
                attrs::node: &node,
                attrs::session: &session,
                attrs::state: &schema::STALLED,
            }
            + entity! { metadata::tag: &schema::KIND_REPORT };
        let unrelated =
            open_configured(&mut f.store, MESSAGE_SCOPE_ID, f.signer.verifying_key()).unwrap();
        f.store.commit(unrelated, &f.signer, facts.clone()).unwrap();
        assert!(f
            .observe_at(at(1.0))
            .report()
            .text
            .contains("not observed / not configured"));
        f.publish(facts);
        let selected = f.observe_at(at(1.0)).report();
        assert_eq!(selected.attention.ids().collect::<Vec<_>>(), vec![*issue]);
        assert!(selected.text.contains("DHT publication: stalled"));
        assert!(selected.text.contains("do not prove blob availability"));
    }

    #[test]
    fn maintained_winners_and_facts_need_not_have_identical_support() {
        let mut f = Fixture::new();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        f.publish(
            recorder
                .record(at(0.0), [condition(State::Stalled, true)])
                .unwrap(),
        );
        let first = f.observe_at(at(1.0)).report();
        f.publish(
            recorder
                .record(at(10.0), [condition(State::Current, false)])
                .unwrap(),
        );
        {
            let mut local = f.store.store();
            pollster::block_on(async {
                drop(
                    local
                        .maintain_attached(f.sources.health.succinct, &f.signer)
                        .await
                        .unwrap(),
                );
                drop(
                    local
                        .maintain_attached(f.sources.health.rank9, &f.signer)
                        .await
                        .unwrap(),
                );
            });
        }
        let snapshot = f.store.snapshot().unwrap();
        let lagging = f.sources.at(snapshot.clone(), at(11.0)).unwrap();
        // The facts attached the new report; the winners register lags the
        // source by it and is read as it stands.
        assert!(lagging.facts.collection.residual().is_empty());
        assert_eq!(lagging.latest_collection.residual().len(), 1);
        assert_eq!(lagging.report().attention, first.attention);
        assert!(f
            .observe_at(at(11.0))
            .report()
            .text
            .contains("converged at observed pairwise roots"));
    }

    #[test]
    fn reader_policy_controls_freshness_and_ignores_legacy_expiry_annotations() {
        let mut f = Fixture::new();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        let mut report = recorder
            .record(at(0.0), [condition(State::Current, false)])
            .unwrap();
        let id = report.root().unwrap();
        assert!(!exists!(
            pattern!(report.facts(), [{ id @ metadata::expires_at: _?expiry }])
        ));
        // Historical annotations are ordinary extra facts, not reader policy.
        // Neither an already-past nor a far-future expiry can alter the deadline.
        report += entity! { ExclusiveId::force_ref(&id) @ metadata::expires_at*: [
            clock::point(at(-100.0)).unwrap(),
            clock::point(at(1_000_000.0)).unwrap(),
        ] };
        f.publish(report);
        let short = f.observe_at(at(90.0));
        assert_eq!(short.report().attention.ids().collect::<Vec<_>>(), vec![id]);

        f.sources.max_age = Duration::from_secs(120);
        let long = f
            .sources
            .at(short.snapshot.clone(), short.evaluated_at)
            .unwrap();
        assert!(long.report().attention.is_empty());
        assert_eq!(long.report().next_change, Some(at(120.0)));
        assert!(long.snapshot.changes_since(&short.snapshot).is_empty());
        assert_eq!(long.report().text.matches("- observer").count(), 1);
    }

    #[test]
    fn future_observation_waits_for_its_timestamp_before_becoming_current() {
        let mut f = Fixture::new();
        let mut recorder = Recorder::new(f.signer.verifying_key());
        f.publish(
            recorder
                .record(at(10.0), [condition(State::Current, false)])
                .unwrap(),
        );
        let future = f.observe_at(at(0.0)).report();
        assert!(future.text.contains("sample is in the future"));
        assert!(future.attention.is_empty());
        assert_eq!(future.next_change, Some(at(10.0)));
        assert_eq!(f.observe_at(at(10.0)).report().next_change, Some(at(70.0)));
    }

    #[test]
    fn an_elapsed_health_deadline_interrupts_a_pending_ordinary_read() {
        runtime().unwrap().block_on(async {
            let deadline_at = clock::now().unwrap() - hifitime::Duration::from_seconds(1.0);
            let was_deadline = tokio::select! {
                result = deadline(Some(deadline_at)) => { result.unwrap(); true }
                _ = std::future::pending::<()>() => false,
            };
            assert!(was_deadline);
            assert_eq!(until(Some(at(1.0)), at(2.0)), Some(Duration::ZERO));
            assert_eq!(until(None, at(2.0)), None);
        });
    }
}
