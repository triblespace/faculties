//! Direct Teams operations over maintained collection views and Graph.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt::Write as _;
#[cfg(test)]
use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::Duration as StdDuration;

#[cfg(test)]
use crate::storage::initialize_signer;
#[cfg(test)]
use crate::storage::open_secrets_collection;
#[cfg(test)]
use crate::storage::{load_signer, open_pile_strict};
use crate::storage::{AcquiringReader, FactArchive, FacultySnapshot, FacultyStore, Storage};
use anyhow::{bail, Context, Result};
use base64::Engine as _;
use hifitime::{Epoch, TimeScale};
use reqwest::blocking::Client;
use reqwest::header::CONTENT_TYPE;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value as JsonValue};
use triblespace::core::blob::encodings::simplearchive::SimpleArchive;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::blob::Bytes;
use triblespace::core::collection::{
    Collection, CollectionCommit, CollectionSnapshotExt, CollectionStoreExt, Support,
};
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::BlobStoreGet;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::{Handle, NsTAIInterval, U256BE};
use triblespace::prelude::*;

use crate::clock;
#[cfg(test)]
use crate::collection_names::open_configured;
use crate::files as file_capability;
use crate::schemas::archive::{archive, RawBytes};
use crate::schemas::files::{file, KIND_MEDIA_TYPE};
use crate::schemas::teams::{teams, DEFAULT_DELTA_URL, DEFAULT_SCOPE_ID};
use crate::secrets::{storage as secret_storage, SecretsSnapshot};
use crate::teams as teams_core;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArchiveAccess {
    /// Query the frozen archive without Graph or credential access. Missing
    /// referenced bytes may be acquired from peers.
    Resident,
    /// Complete one finite delta round before observing the archive.
    Synchronize,
}

/// A completed operation and the professional context in which it ran.
#[derive(Clone, Debug)]
pub struct Activity<T> {
    pub context: PresentationContext,
    pub value: T,
    /// Durable credential publication/recovery and delta-reset notices.
    pub notices: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct PullReceipt {
    pub pages: Vec<Id>,
    pub observations: usize,
}
#[derive(Clone, Debug)]
pub struct SentMessage {
    pub chat_id: String,
    pub message_id: Option<String>,
}
#[derive(Clone, Debug)]
pub struct InvitedMember {
    pub chat_id: String,
    pub user_id: String,
    pub owner: bool,
}
#[derive(Clone, Debug)]
pub struct PresenceSet {
    pub user_id: String,
    pub availability: String,
    pub activity: String,
    pub session_id: String,
    pub duration_mins: u32,
}
#[derive(Clone, Debug)]
pub struct DirectoryUser {
    pub id: String,
    pub name: String,
    pub contact: String,
}
#[derive(Clone, Debug)]
pub struct Presence {
    pub id: String,
    pub availability: String,
    pub activity: String,
}
#[derive(Clone, Debug)]
pub struct ContextSet {
    pub source: Id,
    pub context: Id,
    pub value: PresentationContext,
}
#[derive(Clone, Debug)]
pub struct AuthSet {
    pub source: Id,
    pub profile: Id,
    pub changed: bool,
}
#[derive(Clone, Debug)]
pub struct AuthProfileInput {
    pub tenant: String,
    pub client_id: String,
    pub user_id: String,
    pub scopes: String,
    pub client_secret_version: Option<Id>,
    pub delegated_token_version: Option<Id>,
}
/// Host-side interactive login input. Credentials are resident, never path selectors.
pub struct LoginInput<'a> {
    pub tenant: &'a str,
    pub client_id: &'a str,
    pub scopes: &'a str,
    pub client_secret: Option<&'a str>,
    pub client_secret_version: Option<Id>,
}
#[derive(Clone, Debug)]
pub struct LoginReceipt {
    pub source: Id,
    pub profile: Id,
    pub client_secret_version: Option<Id>,
    pub delegated_token_version: Id,
    pub notices: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct ArchivedMessage {
    pub id: Id,
    pub chat: String,
    pub author: String,
    /// Absent where this version says the message was deleted.
    pub content: Option<String>,
    pub created_at: Option<Inline<NsTAIInterval>>,
    pub modified_at: Option<Inline<NsTAIInterval>>,
    /// Several unordered current versions of one message follow each other;
    /// none of them is the message's one state.
    pub version_index: usize,
    pub version_count: usize,
}
#[derive(Clone, Debug)]
pub struct AttachmentInfo {
    pub id: Id,
    pub chat: String,
    pub message: String,
    pub reference: String,
    pub created_at: Inline<NsTAIInterval>,
    pub name: Option<String>,
    /// Names other writers gave beside `name`; ordinarily empty.
    pub other_names: Vec<String>,
    pub media_type: Option<String>,
    /// Media types other writers gave beside `media_type`; ordinarily empty.
    pub other_media_types: Vec<String>,
    pub size: Option<u128>,
    pub source_pointers: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct AttachmentMatch {
    pub chat: String,
    pub message: String,
    pub reference: String,
}
#[derive(Clone, Debug)]
pub struct AttachmentData {
    pub id: Id,
    pub bytes: Bytes,
    pub name: String,
    pub media_type: Option<String>,
    pub reference: String,
}
#[derive(Clone, Debug)]
pub enum AttachmentLookup {
    Missing { reference: String },
    Ambiguous(Vec<AttachmentMatch>),
    Found(AttachmentData),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresenceAvailability {
    Available,
    Busy,
    Away,
    DoNotDisturb,
}
impl PresenceAvailability {
    pub fn as_graph(self) -> &'static str {
        match self {
            Self::Available => "Available",
            Self::Busy => "Busy",
            Self::Away => "Away",
            Self::DoNotDisturb => "DoNotDisturb",
        }
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PresenceActivity {
    Available,
    InACall,
    InAConferenceCall,
    Away,
    Presenting,
}
impl PresenceActivity {
    pub fn as_graph(self) -> &'static str {
        match self {
            Self::Available => "Available",
            Self::InACall => "InACall",
            Self::InAConferenceCall => "InAConferenceCall",
            Self::Away => "Away",
            Self::Presenting => "Presenting",
        }
    }
}

impl Default for ReadOptions {
    fn default() -> Self {
        Self {
            chat_id: None,
            since: None,
            limit: 20,
            descending: false,
        }
    }
}
impl Default for AttachmentListOptions {
    fn default() -> Self {
        Self {
            chat_id: None,
            message_id: None,
            limit: 20,
            descending: false,
        }
    }
}

/// Configured direct Teams operations. No argv, environment lookup, or host-file expansion.
/// Each operation opens one on-demand maintained session; payload reads retain that snapshot.
#[derive(Clone, Debug)]
pub struct Teams {
    config: TeamsCommandConfig,
}
impl Teams {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(Storage::new(pile, key))
    }

    pub fn with_storage(storage: Storage) -> Self {
        Self {
            config: TeamsCommandConfig {
                storage,
                tenant_selector: None,
                delta_url: DEFAULT_DELTA_URL.to_owned(),
            },
        }
    }

    pub fn with_tenant(mut self, tenant: Option<String>) -> Self {
        self.config.tenant_selector = tenant;
        self
    }

    /// Trusted host configuration, never a per-call MCP URL.
    pub fn with_delta_url(mut self, delta_url: String) -> Self {
        self.config.delta_url = delta_url;
        self
    }

    fn archive<T>(
        &self,
        access: ArchiveAccess,
        operation: impl FnOnce(Id, &mut TeamsSession) -> Result<T>,
    ) -> Result<Activity<T>> {
        if access == ArchiveAccess::Synchronize {
            return with_teams_context(&self.config, None, false, |runtime, session| {
                pull_once_with_cache(runtime, &mut None, session)?;
                operation(runtime.source_id, session)
            });
        }
        storage(&self.config).with_session(|session| {
            let source = match self.config.tenant_selector.as_deref() {
                Some(tenant) => source_id_for_tenant(tenant)?,
                None => sole(
                    teams_core::source_ids(&session.facts),
                    "Teams archive source (select a tenant when more than one exists)",
                )?,
            };
            let context = load_context(&session.reader, &session.facts, source)?;
            let value = operation(source, session)?;
            Ok(Activity {
                context,
                value,
                notices: std::mem::take(&mut session.notices),
            })
        })
    }

    pub fn read(
        &self,
        options: ReadOptions,
        access: ArchiveAccess,
    ) -> Result<Activity<Vec<ArchivedMessage>>> {
        // Reject malformed time input before credential refresh or delta publication.
        parse_since_key(options.since.as_deref())?;
        self.archive(access, |source, session| {
            read_messages(source, session, options)
        })
    }
    pub fn pull(&self) -> Result<Activity<PullReceipt>> {
        with_teams_context(&self.config, None, false, |runtime, session| {
            pull_once_with_cache(runtime, &mut None, session)
        })
    }
    pub fn send(
        &self,
        present_as: &str,
        chat_id: &str,
        text: &str,
    ) -> Result<Activity<SentMessage>> {
        with_teams_context(&self.config, Some(present_as), true, |runtime, session| {
            send_message(runtime, session, chat_id, text)
        })
    }
    pub fn users_list(
        &self,
        prefix: Option<&str>,
        limit: usize,
    ) -> Result<Activity<Vec<DirectoryUser>>> {
        with_teams_context(&self.config, None, false, |runtime, session| {
            list_users(runtime, session, prefix, limit)
        })
    }
    pub fn presence_set(
        &self,
        present_as: &str,
        availability: PresenceAvailability,
        activity: Option<PresenceActivity>,
        duration_mins: u32,
        session_id: Option<String>,
    ) -> Result<Activity<PresenceSet>> {
        validate_presence(availability, activity, duration_mins)?;
        with_teams_context(&self.config, Some(present_as), true, |runtime, session| {
            set_presence_status(
                runtime,
                session,
                availability,
                activity,
                duration_mins,
                session_id,
            )
        })
    }
    pub fn presence_get(&self, user_ids: Vec<String>) -> Result<Activity<Vec<Presence>>> {
        if user_ids.is_empty() {
            bail!("presence-get requires at least one user id");
        }
        with_teams_context(&self.config, None, false, |runtime, session| {
            get_presence(runtime, session, user_ids)
        })
    }
    pub fn chat_invite(
        &self,
        present_as: &str,
        chat_id: &str,
        user_id: &str,
        owner: bool,
    ) -> Result<Activity<InvitedMember>> {
        with_teams_context(&self.config, Some(present_as), true, |runtime, session| {
            invite_to_chat(runtime, session, chat_id, user_id, owner)
        })
    }
    pub fn chat_create(
        &self,
        present_as: &str,
        user_ids: Vec<String>,
        group: bool,
        topic: Option<String>,
    ) -> Result<Activity<String>> {
        if user_ids.is_empty() {
            bail!("chat-create requires at least one user id");
        }
        with_teams_context(&self.config, Some(present_as), true, |runtime, session| {
            create_chat(runtime, session, user_ids, group, topic)
        })
    }
    pub fn attachments_list(
        &self,
        options: AttachmentListOptions,
        access: ArchiveAccess,
    ) -> Result<Activity<Vec<AttachmentInfo>>> {
        self.archive(access, |source, session| {
            list_attachments(source, session, options)
        })
    }
    pub fn attachment_get(
        &self,
        options: AttachmentGetOptions,
        access: ArchiveAccess,
    ) -> Result<Activity<AttachmentLookup>> {
        if parse_attachment_reference(options.source_id.trim())
            .1
            .is_empty()
        {
            bail!("attachment source id is empty");
        }
        self.archive(access, |source, session| {
            get_attachment(source, session, options)
        })
    }
    pub fn context_set(
        &self,
        tenant: &str,
        present_as: &str,
        boundary: &str,
    ) -> Result<ContextSet> {
        let source = source_id_for_tenant(tenant)?;
        storage(&self.config)
            .with_session(|session| store_context(session, source, tenant, present_as, boundary))
    }
    pub fn context_show(&self) -> Result<PresentationContext> {
        storage(&self.config).with_session(|session| {
            let source = match self.config.tenant_selector.as_deref() {
                Some(tenant) => source_id_for_tenant(tenant)?,
                None => sole(
                    teams_core::source_ids(&session.facts),
                    "Teams source (select a tenant when more than one exists)",
                )?,
            };
            load_context(&session.reader, &session.facts, source)
        })
    }
    /// Safe metadata and exact encrypted version IDs, never credential plaintext.
    pub fn auth_status(&self) -> Result<Activity<String>> {
        storage(&self.config).with_session(|session| {
            let sources = teams_core::auth_profile_sources(&session.facts);
            let source = match self.config.tenant_selector.as_deref() {
                Some(tenant) => Some(source_id_for_tenant(tenant)?),
                None => (sources.len() == 1).then(|| *sources.first().unwrap()),
            };
            let context = source
                .map(|source| load_context(&session.reader, &session.facts, source))
                .transpose()?
                .unwrap_or_default();
            let value = show_auth_status(session, self.config.tenant_selector.as_deref())?;
            Ok(Activity {
                context,
                value,
                notices: std::mem::take(&mut session.notices),
            })
        })
    }
    pub fn auth_set(&self, input: AuthProfileInput) -> Result<AuthSet> {
        storage(&self.config).with_session(|session| {
            set_auth_profile(
                session,
                &input.tenant,
                &input.client_id,
                &input.user_id,
                &input.scopes,
                input.client_secret_version,
                input.delegated_token_version,
            )
        })
    }
    /// Interactive host interface. The callback delivers the device-code instructions.
    /// Not exposed as MCP: it can wait for a person and accepts configured credentials.
    pub fn login(
        &self,
        input: LoginInput<'_>,
        prompt: &mut dyn FnMut(&str) -> Result<()>,
    ) -> Result<LoginReceipt> {
        if input.client_secret.is_some() && input.client_secret_version.is_some() {
            bail!("provide client-secret bytes or one exact version, not both");
        }
        storage(&self.config).with_session(|session| {
            login_device_code_collection(
                session,
                input.tenant,
                input.client_id,
                input.client_secret,
                input.client_secret_version,
                input.scopes,
                prompt,
            )
        })
    }
}

pub fn validate_presence(
    availability: PresenceAvailability,
    activity: Option<PresenceActivity>,
    duration_mins: u32,
) -> Result<()> {
    let availability = availability.as_graph();
    ensure_presence_combo(
        availability,
        activity
            .map(PresenceActivity::as_graph)
            .unwrap_or_else(|| default_activity_for(availability)),
    )?;
    if !(5..=240).contains(&duration_mins) {
        bail!("duration-mins must be between 5 and 240");
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct TeamsCommandConfig {
    storage: Storage,
    tenant_selector: Option<String>,
    delta_url: String,
}

#[derive(Clone, Debug)]
struct TeamsBridgeConfig {
    source_id: Id,
    tenant: String,
    client_id: String,
    user_id: String,
    scopes: String,
    profile: Id,
    client_secret_version: Option<Id>,
    delegated_token_version: Option<Id>,
    delta_url: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct DelegatedTokenBundle {
    access_token: String,
    refresh_token: Option<String>,
    expires_at_unix: i64,
    token_type: Option<String>,
    scope: Option<String>,
}

#[derive(Clone)]
struct TeamsStorage {
    storage: Storage,
}

#[derive(Clone)]
struct CollectionView {
    facts: FactArchive,
    reader: AcquiringReader<FacultySnapshot>,
}

struct TeamsSession {
    storage: Storage,
    collection: Collection<SimpleArchive>,
    rank9: Collection<Rank9AcceleratedSuccinctArchiveBlob>,
    support: Support<SimpleArchive>,
    facts: FactArchive,
    reader: AcquiringReader<FacultySnapshot>,
    signer: ed25519_dalek::SigningKey,
    secret_collection: secret_storage::SecretsCollection,
    secrets: SecretsSnapshot<AcquiringReader<FacultySnapshot>>,
    notices: Vec<String>,
}

impl TeamsSession {
    fn view(&self) -> CollectionView {
        CollectionView {
            facts: self.facts.clone(),
            reader: self.reader.clone(),
        }
    }

    fn commit(
        &mut self,
        mut fragment: Fragment,
        description: &'static str,
    ) -> Result<Option<CollectionCommit>> {
        // A page is one independently signed transaction: it may refer to
        // earlier receipts, but it must carry its own message identity
        // closure. This is a publication-boundary check over the staged
        // member, not a validation pass over the maintained collection.
        teams_core::validate_commit_fragment(fragment.facts())?;

        // Evaluate state-dependent invariants over a shallow logical union of
        // the frozen Rank9 view and this one staged member. No current facts
        // are flattened back into a TribleSet.
        let candidate = self.facts.with_segments([fragment.facts().into()]);
        let auth_sources = teams_core::auth_profile_sources(fragment.facts());
        teams_core::validate_auth_secret_references_for_sources(
            &candidate,
            &self.secrets,
            auth_sources,
        )?;

        // Unordered versions and concurrent receipts are observations, which
        // readers show; only what this writer itself would fork is refused.
        let context_sources = find!(
            source: Id,
            pattern!(fragment.facts(), [{
                _?context @
                metadata::tag: teams::kind_context,
                teams::source: ?source,
            }])
        )
        .collect::<BTreeSet<_>>();
        for source in context_sources {
            let context_heads = teams_core::context_head_ids(&candidate, source);
            if context_heads.len() > 1 {
                bail!(
                    "Teams context head has {} values; refusing arbitrary selection",
                    context_heads.len()
                );
            }
        }

        fragment.describe_with(entity! { metadata::description: description });
        let storage = self.storage.clone();
        storage.with_store(|pile, _, runtime| {
            crate::collection_names::require_command_write_admission_acquiring(
                pile,
                self.collection,
                &self.signer,
                "Teams",
                "teams read",
                runtime,
            )?;
            let commit = pile
                .commit(self.collection, &self.signer, fragment)
                .context("commit Teams fragment")?;
            runtime.block_on(async {
                // A write derives its own leaves into every view of its
                // collection, so the commit is readable through them at once.
                drop(
                    crate::storage::ensure_downstream(pile, self.collection, &self.signer)
                        .await
                        .context("Teams fragment was committed, but ensuring its views failed")?,
                );
                Ok::<_, anyhow::Error>(())
            })?;
            self.refresh_secrets_for(pile, None, runtime)?;
            Ok(Some(commit))
        })
    }

    fn refresh_secrets(&mut self) -> Result<()> {
        // A credential publication does not reselect the session's Teams
        // support. Only Secrets gets a new observation here.
        let support = self.support.clone();
        let storage = self.storage.clone();
        storage
            .with_store(|pile, _, runtime| self.refresh_secrets_for(pile, Some(support), runtime))
    }

    fn refresh_secrets_for(
        &mut self,
        pile: &mut FacultyStore,
        support: Option<Support<SimpleArchive>>,
        runtime: &std::sync::Arc<tokio::runtime::Runtime>,
    ) -> Result<()> {
        let snapshot = runtime
            .block_on(self.secret_collection.ensure(pile, &self.signer))
            .context("refresh configured Secrets collection for Teams")?;
        let reader = AcquiringReader::new(snapshot, std::sync::Arc::clone(runtime));
        let secrets = secret_storage::snapshot_acquiring(reader.clone(), self.secret_collection)?;
        // A credential-only refresh keeps the session's Teams facts and
        // support as they were selected; only Secrets and the reader move.
        // A Teams commit observes the ordinary view at this final snapshot.
        if support.is_none() {
            let observed = reader
                .attached_acquiring(self.rank9)
                .context("attach Teams through Secrets snapshot")?;
            let support = observed.support().clone();
            let facts = crate::storage::acquire_attached_facts(&observed)
                .context("read Teams through Secrets snapshot")?;
            drop(observed);
            self.support = support;
            self.facts = facts;
        }
        self.reader = reader;
        self.secrets = secrets;
        Ok(())
    }

    fn add_secret(
        &mut self,
        name: &str,
        plaintext: &[u8],
        observed_at: Inline<NsTAIInterval>,
    ) -> Result<Id> {
        let secret = self
            .storage
            .with_store(|pile, _, runtime| {
                self.storage.open_secrets_collection(
                    pile,
                    self.signer.verifying_key(),
                    runtime,
                    true,
                )?;
                secret_storage::add_secret(
                    pile,
                    &self.signer,
                    self.secret_collection,
                    name,
                    plaintext,
                    observed_at,
                )
            })
            .context("publish Teams credential in configured Secrets collection")?;
        self.refresh_secrets()?;
        if !self.secrets.contains(secret) {
            bail!("published Teams Secrets version {secret} was not rediscovered");
        }
        Ok(secret)
    }
}

impl TeamsStorage {
    fn with_session<T>(&self, operation: impl FnOnce(&mut TeamsSession) -> Result<T>) -> Result<T> {
        self.storage.scope(|storage| {
            let mut session = storage.with_store(|pile, signer, runtime| {
                let collection = self.storage.open_collection_read(
                    pile,
                    DEFAULT_SCOPE_ID,
                    signer.verifying_key(),
                    runtime,
                )?;
                let maintained_succinct = pile.attach::<SuccinctArchiveBlob>(collection, ())?;
                let maintained_rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(
                    collection,
                    maintained_succinct,
                )?;
                let secret_collection = self.storage.open_secrets_collection(
                    pile,
                    signer.verifying_key(),
                    runtime,
                    false,
                )?;
                // The session carries the source and attaches the frontier
                // the carry leaves; a commit neither attachment reaches yet
                // is read from its own bytes, and what this key cannot
                // attach is lag, not a failure.
                let snapshot = runtime.block_on(async {
                    crate::storage::tolerate_own_lag(
                        pile.maintain_attached(maintained_succinct, signer).await,
                    )
                    .context("maintain Teams fact collection")?;
                    crate::storage::tolerate_own_lag(
                        pile.maintain_attached(maintained_rank9, signer).await,
                    )
                    .context("maintain Teams fact collection")?;
                    let snapshot = secret_collection
                        .ensure(pile, signer)
                        .await
                        .context("observe configured Secrets collection for Teams")?;
                    Ok::<_, anyhow::Error>(snapshot)
                })?;
                let reader = AcquiringReader::new(snapshot, std::sync::Arc::clone(runtime));
                let secrets =
                    secret_storage::snapshot_acquiring(reader.clone(), secret_collection)?;
                let observed = reader
                    .attached_acquiring(maintained_rank9)
                    .context("observe Teams through Secrets snapshot")?;
                let support = observed.support().clone();
                let facts = crate::storage::acquire_attached_facts(&observed)
                    .context("read Teams through Secrets snapshot")?;
                drop(observed);
                Ok(TeamsSession {
                    storage: storage.clone(),
                    collection,
                    rank9: maintained_rank9,
                    support,
                    facts,
                    reader,
                    signer: signer.clone(),
                    secret_collection,
                    secrets,
                    notices: Vec::new(),
                })
            })?;
            // Graph/OAuth requests run with frozen observations and no
            // borrowed backend. Publication reacquires the same store only
            // for its append/maintenance step.
            match operation(&mut session) {
                Ok(value) => Ok(value),
                Err(error) if session.notices.is_empty() => Err(error),
                Err(error) => Err(error.context(session.notices.join("\n"))),
            }
        })
    }

    #[cfg(test)]
    fn view(&self) -> Result<CollectionView> {
        self.with_session(|session| Ok(session.view()))
    }

    #[cfg(test)]
    fn publish(
        &self,
        fragment: Fragment,
        message: &'static str,
    ) -> Result<Option<CollectionCommit>> {
        self.with_session(|session| session.commit(fragment, message))
    }
}

fn source_fragment(tenant: &str) -> Fragment {
    teams_core::source_fragment(tenant)
}

fn source_id_for_tenant(tenant: &str) -> Result<Id> {
    let tenant = tenant.trim();
    if tenant.is_empty() || is_generic_tenant(tenant) {
        bail!("Teams collection state requires one concrete tenant, got {tenant:?}");
    }
    Ok(source_fragment(tenant)
        .root()
        .expect("Teams source fragment has one root"))
}

fn selected_source(session: &TeamsSession, tenant_selector: Option<&str>) -> Result<Id> {
    if let Some(tenant) = tenant_selector {
        return source_id_for_tenant(tenant);
    }
    sole(
        teams_core::auth_profile_sources(&session.facts),
        "Teams auth-profile source (set --tenant when more than one exists)",
    )
}

fn resolve_auth_config(
    session: &TeamsSession,
    config: &TeamsCommandConfig,
    source_id: Id,
) -> Result<TeamsBridgeConfig> {
    let profile = match teams_core::auth_profile_head(&session.facts, source_id) {
        teams_core::AuthProfileHead::Missing => {
            bail!("Teams source {source_id:x} has no auth profile; run `teams login`")
        }
        teams_core::AuthProfileHead::Unique(profile) => profile,
        teams_core::AuthProfileHead::Forked(heads) => bail!(
            "Teams source {source_id:x} has forked auth-profile heads {heads:?}; reconcile with `teams auth set`"
        ),
    };
    let record = teams_core::auth_profile(&session.facts, profile)?;
    let tenant = teams_core::source_label(&session.reader, &session.facts, source_id)?;
    Ok(TeamsBridgeConfig {
        source_id,
        tenant,
        client_id: read_utf8string(&session.reader, record.client_id, "Teams auth client id")?,
        user_id: read_utf8string(&session.reader, record.user_id, "Teams auth user id")?,
        scopes: read_utf8string(&session.reader, record.scopes, "Teams auth scopes")?,
        profile,
        client_secret_version: record.client_secret_version,
        delegated_token_version: record.delegated_token_version,
        delta_url: config.delta_url.clone(),
    })
}

fn require_exact_secret(session: &TeamsSession, id: Id, label: &str) -> Result<Id> {
    if !session.secrets.contains(id) {
        bail!("unknown {label} {id:x}");
    }
    Ok(id)
}

fn open_exact_secret(session: &TeamsSession, secret: Id) -> Result<Vec<u8>> {
    session
        .secrets
        .open(secret, &session.signer)
        .with_context(|| format!("open exact Teams Secrets version {secret:x}"))
}

fn teams_secret_name(source: Id, kind: &str) -> String {
    format!(
        "teams/{kind}/{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(source.raw())
    )
}

fn storage(config: &TeamsCommandConfig) -> TeamsStorage {
    TeamsStorage {
        storage: config.storage.clone(),
    }
}

fn with_teams_context<T>(
    config: &TeamsCommandConfig,
    requested_as: Option<&str>,
    require_explicit_identity: bool,
    operation: impl FnOnce(&TeamsBridgeConfig, &mut TeamsSession) -> Result<T>,
) -> Result<Activity<T>> {
    storage(config).with_session(|session| {
        let source_id = selected_source(session, config.tenant_selector.as_deref())?;
        let context = load_context(&session.reader, &session.facts, source_id)?;
        let context = prepare_teams_context(&context, requested_as, require_explicit_identity)?;
        // The presentation gate deliberately precedes auth-profile resolution.
        // Outward mutations fail before inspecting credentials or making requests.
        let runtime = resolve_auth_config(session, config, source_id)?;
        let value = operation(&runtime, session)?;
        Ok(Activity {
            context,
            value,
            notices: std::mem::take(&mut session.notices),
        })
    })
}

pub fn default_scopes() -> String {
    [
        "openid",
        "offline_access",
        "User.Read.All",
        "Presence.ReadWrite",
        "Presence.Read.All",
        "Chat.ReadWrite",
        "ChatMessage.Send",
        "Chat.Create",
        "ChatMember.ReadWrite",
    ]
    .join(" ")
}

fn is_generic_tenant(tenant: &str) -> bool {
    teams_core::is_generic_tenant(tenant)
}

fn canonical_tenant(tenant: &str) -> String {
    teams_core::canonical_tenant(tenant)
}

fn jwt_tenant(token: &str) -> Option<String> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload)
        .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(payload))
        .ok()?;
    let claims: JsonValue = serde_json::from_slice(&bytes).ok()?;
    claims
        .get("tid")
        .and_then(JsonValue::as_str)
        .map(str::trim)
        .filter(|tenant| !tenant.is_empty() && !is_generic_tenant(tenant))
        .map(str::to_owned)
}

fn resolve_source_tenant(
    requested_authority: &str,
    id_token: Option<&str>,
    access_token: Option<&str>,
) -> Result<String> {
    if let Some(tenant) = id_token
        .and_then(jwt_tenant)
        .or_else(|| access_token.and_then(jwt_tenant))
    {
        return Ok(canonical_tenant(&tenant));
    }
    let requested = requested_authority.trim();
    if !requested.is_empty() && !is_generic_tenant(requested) {
        return Ok(canonical_tenant(requested));
    }
    bail!(
        "Microsoft did not return a concrete tenant identity for authority {requested_authority:?}; include `openid` in login scopes or login against the actual tenant id"
    )
}

fn pull_once_with_cache(
    config: &TeamsBridgeConfig,
    app_token_cache: &mut Option<AppTokenCache>,
    session: &mut TeamsSession,
) -> Result<PullReceipt> {
    let mut receipt = PullReceipt::default();
    let (token, app_config) = get_app_token(config, app_token_cache, session)?;
    let mut known_messages =
        load_known_messages(&session.reader, &session.facts, config.source_id)?;
    let mut coverage = coverage_frontier(&session.reader, &session.facts, config.source_id)?;
    let base_url = resolve_delta_url(&config.delta_url, &app_config.user_id)?;
    let mut request_url = coverage
        .as_ref()
        .and_then(|coverage| coverage.cursor.clone())
        .unwrap_or_else(|| base_url.clone());
    let mut reset_expired = coverage
        .as_ref()
        .is_some_and(|coverage| coverage.cursor.is_some());
    loop {
        let page = match fetch_delta_page(&Client::new(), &token, &request_url) {
            Ok(page) => page,
            Err(error) if reset_expired && error.downcast_ref::<DeltaCursorExpired>().is_some() => {
                session.notices.push(
                    "Teams delta cursor expired; beginning a new covered round from the base endpoint."
                        .to_owned(),
                );
                request_url = base_url.clone();
                reset_expired = false;
                continue;
            }
            Err(error) => return Err(error),
        };
        reset_expired = false;

        let (cursor_kind, cursor) = match (page.next_link, page.delta_link) {
            (Some(next), None) => ("next", next),
            (None, Some(delta)) => ("delta", delta),
            _ => bail!("Teams delta page must contain exactly one nextLink or deltaLink"),
        };
        let incoming = parse_messages(page.messages)?;
        // A round which brings nothing writes nothing: the same cursor asks
        // again. A cursor which expired, a round under way, and heads which
        // await a join each still get their receipt.
        if incoming.is_empty()
            && cursor_kind == "delta"
            && coverage.as_ref().is_some_and(|coverage| {
                coverage.settled && coverage.cursor.as_deref() == Some(request_url.as_str())
            })
        {
            return Ok(receipt);
        }
        let generation = coverage
            .as_ref()
            .map(|coverage| coverage.generation + 1)
            .unwrap_or(1);
        let (mut fragment, observations, next_known_messages) = build_page_fragment(
            &app_config.tenant,
            config.source_id,
            incoming,
            &token,
            &known_messages,
        )?;
        let coverage_record = coverage_fragment(
            config.source_id,
            generation,
            coverage
                .iter()
                .flat_map(|coverage| coverage.heads.iter().copied()),
            &request_url,
            &cursor,
            cursor_kind,
            observations.iter().copied(),
        )?;
        let receipt_id = coverage_record
            .root()
            .expect("coverage receipt has one root");
        fragment += coverage_record;
        session.commit(fragment, "teams delta page")?;
        receipt.pages.push(receipt_id);
        receipt.observations += observations.len();
        known_messages = next_known_messages;

        coverage = Some(CoverageFrontier {
            heads: BTreeSet::from([receipt_id]),
            generation,
            cursor: Some(cursor.clone()),
            settled: cursor_kind == "delta",
        });
        if cursor_kind == "delta" {
            return Ok(receipt);
        }
        request_url = cursor;
    }
}

#[derive(Debug, Clone)]
struct AppTokenCache {
    access_token: String,
    expires_at_key: i128,
}

#[derive(Debug, Clone)]
struct AppConfig {
    tenant: String,
    client_id: String,
    client_secret: String,
    user_id: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PresentationContext {
    pub name: Option<String>,
    pub boundary: Option<String>,
}

fn get_app_token(
    config: &TeamsBridgeConfig,
    app_token_cache: &mut Option<AppTokenCache>,
    session: &TeamsSession,
) -> Result<(String, AppConfig)> {
    let app_config = app_config(config, session)?;
    let now = clock::now()?;
    let now_key = interval_key(clock::point(now)?);

    if let Some(cache) = app_token_cache {
        if cache.expires_at_key > now_key + 30 * 1_000_000_000 {
            return Ok((cache.access_token.clone(), app_config));
        }
    }

    let token = request_client_credentials_token(
        &app_config.tenant,
        &app_config.client_id,
        &app_config.client_secret,
    )?;
    let expires_at = clock::point(epoch_after_seconds(clock::now()?, token.expires_in))?;
    let expires_at_key = interval_key(expires_at);
    let access_token = token.access_token;
    *app_token_cache = Some(AppTokenCache {
        access_token: access_token.clone(),
        expires_at_key,
    });
    Ok((access_token, app_config))
}

fn app_config(config: &TeamsBridgeConfig, session: &TeamsSession) -> Result<AppConfig> {
    let secret = config.client_secret_version.ok_or_else(|| {
        anyhow::anyhow!(
            "Teams auth profile {} has no app client-secret version; rotate with `teams login --client-secret ...` or `teams auth set`",
            config.profile
        )
    })?;
    let client_secret = String::from_utf8(open_exact_secret(session, secret)?)
        .context("Teams client secret is not UTF-8")?;

    Ok(AppConfig {
        tenant: config.tenant.clone(),
        client_id: config.client_id.clone(),
        client_secret,
        user_id: config.user_id.clone(),
    })
}

fn resolve_delta_url(template: &str, user_id: &str) -> Result<String> {
    if template.contains("{user_id}") {
        return Ok(template.replace("{user_id}", user_id));
    }
    if template.contains("/me/") {
        bail!("delta url uses /me; configure /users/{{user_id}}/chats/getAllMessages/delta");
    }
    Ok(template.to_owned())
}

fn get_delegated_token(config: &TeamsBridgeConfig, session: &mut TeamsSession) -> Result<String> {
    let secret = config.delegated_token_version.ok_or_else(|| {
        anyhow::anyhow!(
            "Teams auth profile {} has no delegated-token version; run `teams login`",
            config.profile
        )
    })?;
    let plaintext = open_exact_secret(session, secret)?;
    let bundle: DelegatedTokenBundle =
        serde_json::from_slice(&plaintext).context("decode Teams delegated-token bundle")?;
    if bundle.expires_at_unix > now_epoch_secs()? + 30 {
        return Ok(bundle.access_token);
    }
    let refresh = bundle.refresh_token.as_deref().ok_or_else(|| {
        anyhow::anyhow!("delegated token expired without a refresh token; run `teams login`")
    })?;
    let refreshed = refresh_token(
        &config.tenant,
        &config.client_id,
        refresh,
        bundle.scope.as_deref().or(Some(&config.scopes)),
    )?;
    let next_bundle = DelegatedTokenBundle {
        access_token: refreshed.access_token.clone(),
        refresh_token: refreshed
            .refresh_token
            .or_else(|| bundle.refresh_token.clone()),
        expires_at_unix: now_epoch_secs()? + refreshed.expires_in,
        token_type: refreshed.token_type.or(bundle.token_type),
        scope: refreshed
            .scope
            .or(bundle.scope)
            .or_else(|| Some(config.scopes.clone())),
    };
    if !session.secrets.contains(secret) {
        bail!("exact delegated-token Secrets version disappeared");
    }
    let name = teams_secret_name(config.source_id, "delegated-token");
    let encoded =
        serde_json::to_vec(&next_bundle).context("encode refreshed Teams token bundle")?;
    let next_secret = session.add_secret(&name, &encoded, clock::point_now()?)?;
    let client_repair = config
        .client_secret_version
        .map(|id| format!(" --client-secret-version {id:x}"))
        .unwrap_or_default();
    session.notices.push(format!(
        "Published refreshed delegated-token version {next_secret:x}; if auth-profile publication is interrupted, repair with `teams auth set --tenant {} --client-id {} --user-id {} --scopes @SCOPES{client_repair} --delegated-token-version {next_secret:x}`.",
        config.tenant, config.client_id, config.user_id,
    ));
    let mut fragment = source_fragment(&config.tenant);
    let (profile, _) = teams_core::auth_profile_fragment(
        config.source_id,
        &config.client_id,
        &config.user_id,
        next_bundle.scope.as_deref().unwrap_or(&config.scopes),
        config.client_secret_version,
        Some(next_secret),
        [config.profile],
    )?;
    fragment += profile;
    session.commit(fragment, "teams auth profile after token refresh")?;
    Ok(refreshed.access_token)
}

#[derive(Debug, Clone, Deserialize)]
struct DeviceCodeResponse {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: i64,
    interval: Option<i64>,
    message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    id_token: Option<String>,
    expires_in: i64,
    scope: Option<String>,
    token_type: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct ErrorResponse {
    error: String,
}

fn oauth_error_kind(body: &str) -> String {
    serde_json::from_str::<ErrorResponse>(body)
        .ok()
        .map(|error| error.error)
        .filter(|kind| {
            !kind.is_empty()
                && kind.len() <= 64
                && kind
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

fn now_epoch_secs() -> Result<i64> {
    Ok(clock::now()?.to_unix_seconds() as i64)
}

fn load_context<P>(
    reader: &impl BlobStoreGet,
    catalog: &P,
    source_id: Id,
) -> Result<PresentationContext>
where
    P: TriblePattern,
{
    // Concurrent context versions are resolved to the newest by the creation
    // time each carries, then by the smallest id. Name and boundary are the
    // version's identity; should a writer ever reuse an id, the smallest
    // handle is shown.
    let Some(context_id) = current_context_head_ids(catalog, source_id)
        .into_iter()
        .max_by_key(|head| {
            let created = find!(
                created: Inline<NsTAIInterval>,
                pattern!(catalog, [{ *head @ metadata::created_at: ?created }])
            )
            .map(interval_key)
            .max();
            (created, std::cmp::Reverse(*head))
        })
    else {
        return Ok(PresentationContext::default());
    };
    let name = find!(
        name: Inline<Handle<UTF8String>>,
        pattern!(catalog, [{ context_id @ metadata::name: ?name }])
    )
    .min()
    .map(|handle| read_utf8string(reader, handle, "Teams presentation name"))
    .transpose()?;
    let boundary = find!(
        boundary: Inline<Handle<UTF8String>>,
        pattern!(catalog, [{ context_id @ metadata::description: ?boundary }])
    )
    .min()
    .map(|handle| read_utf8string(reader, handle, "Teams presentation boundary"))
    .transpose()?;
    Ok(PresentationContext { name, boundary })
}

fn store_context(
    session: &mut TeamsSession,
    source_id: Id,
    tenant: &str,
    presentation_name: &str,
    presentation_boundary: &str,
) -> Result<ContextSet> {
    let presentation_name = presentation_name.trim();
    if presentation_name.is_empty() {
        bail!("Teams presentation name must not be empty");
    }
    let presentation_boundary = presentation_boundary.trim();
    if presentation_boundary.is_empty() {
        bail!("Teams presentation boundary must not be empty");
    }

    let supersedes = current_context_head_ids(&session.facts, source_id);
    let mut fragment = source_fragment(tenant);
    if fragment.root() != Some(source_id) {
        bail!("Teams context tenant/source identity mismatch");
    }
    let context = teams_core::context_fragment(
        source_id,
        clock::point_now()?,
        supersedes,
        presentation_name,
        presentation_boundary,
    )?;
    let context_id = context.root().expect("context fragment has one root");
    fragment += context;
    session.commit(fragment, "teams professional context")?;
    Ok(ContextSet {
        source: source_id,
        context: context_id,
        value: PresentationContext {
            name: Some(presentation_name.to_owned()),
            boundary: Some(presentation_boundary.to_owned()),
        },
    })
}

fn prepare_teams_context(
    context: &PresentationContext,
    requested_as: Option<&str>,
    require_explicit_identity: bool,
) -> Result<PresentationContext> {
    let context = context.clone();
    if !require_explicit_identity {
        return Ok(context);
    }

    let Some(configured_name) = context
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    else {
        bail!("outward Teams mutations require a configured context; run `teams context set`");
    };
    if context
        .boundary
        .as_deref()
        .map(str::trim)
        .filter(|boundary| !boundary.is_empty())
        .is_none()
    {
        bail!("outward Teams mutations require a configured work boundary");
    }
    let Some(requested_as) = requested_as.map(str::trim).filter(|name| !name.is_empty()) else {
        bail!("outward Teams mutations require `--as {configured_name}`");
    };
    if requested_as != configured_name {
        bail!(
            "Teams presentation mismatch: configured as {configured_name}, requested --as {requested_as}"
        );
    }
    Ok(context)
}

fn show_auth_status(session: &TeamsSession, tenant: Option<&str>) -> Result<String> {
    let mut output = String::new();
    let sources = match tenant {
        Some(tenant) => BTreeSet::from([source_id_for_tenant(tenant)?]),
        None => teams_core::auth_profile_sources(&session.facts),
    };
    if sources.is_empty() {
        writeln!(output, "auth_profile: (unset)")?;
        return Ok(output);
    }
    for source in sources {
        let tenant = teams_core::source_label(&session.reader, &session.facts, source)
            .unwrap_or_else(|_| format!("unknown-source-{source:x}"));
        writeln!(output, "tenant: {tenant}")?;
        match teams_core::auth_profile_head(&session.facts, source) {
            teams_core::AuthProfileHead::Missing => writeln!(output, "auth_profile: (unset)")?,
            teams_core::AuthProfileHead::Forked(heads) => {
                writeln!(output, "auth_profile: FORKED {heads:?}")?
            }
            teams_core::AuthProfileHead::Unique(profile) => {
                let record = teams_core::auth_profile(&session.facts, profile)?;
                writeln!(output, "auth_profile: {profile:x}")?;
                writeln!(
                    output,
                    "client_id: {}",
                    read_utf8string(&session.reader, record.client_id, "Teams client id")?
                )?;
                writeln!(
                    output,
                    "user_id: {}",
                    read_utf8string(&session.reader, record.user_id, "Teams user id")?
                )?;
                writeln!(
                    output,
                    "scopes: {}",
                    read_utf8string(&session.reader, record.scopes, "Teams scopes")?
                )?;
                writeln!(
                    output,
                    "client_secret_version: {}",
                    record
                        .client_secret_version
                        .map(|id| format!("{id:x}"))
                        .unwrap_or_else(|| "(unset)".to_owned())
                )?;
                writeln!(
                    output,
                    "delegated_token_version: {}",
                    record
                        .delegated_token_version
                        .map(|id| format!("{id:x}"))
                        .unwrap_or_else(|| "(unset)".to_owned())
                )?;
            }
        }
    }
    Ok(output)
}

#[allow(clippy::too_many_arguments)]
fn set_auth_profile(
    session: &mut TeamsSession,
    tenant: &str,
    client_id: &str,
    user_id: &str,
    scopes: &str,
    client_secret_version: Option<Id>,
    delegated_token_version: Option<Id>,
) -> Result<AuthSet> {
    let source = source_id_for_tenant(tenant)?;
    let client_secret_version = client_secret_version
        .map(|id| require_exact_secret(session, id, "client-secret version"))
        .transpose()?;
    let delegated_token_version = delegated_token_version
        .map(|id| require_exact_secret(session, id, "delegated-token version"))
        .transpose()?;
    let predecessors = teams_core::auth_profile_head_ids(&session.facts, source);
    let canonical_scopes = teams_core::canonical_auth_scopes(scopes)?;
    if let teams_core::AuthProfileHead::Unique(current) =
        teams_core::auth_profile_head(&session.facts, source)
    {
        let record = teams_core::auth_profile(&session.facts, current)?;
        let same_public_state = read_utf8string(
            &session.reader,
            record.client_id,
            "current Teams auth client id",
        )? == client_id.trim()
            && read_utf8string(
                &session.reader,
                record.user_id,
                "current Teams auth user id",
            )? == user_id.trim()
            && read_utf8string(&session.reader, record.scopes, "current Teams auth scopes")?
                == canonical_scopes;
        if same_public_state
            && record.client_secret_version == client_secret_version
            && record.delegated_token_version == delegated_token_version
        {
            return Ok(AuthSet {
                source,
                profile: current,
                changed: false,
            });
        }
    }
    let mut fragment = source_fragment(tenant);
    let (profile, profile_id) = teams_core::auth_profile_fragment(
        source,
        client_id,
        user_id,
        &canonical_scopes,
        client_secret_version,
        delegated_token_version,
        predecessors,
    )?;
    fragment += profile;
    session.commit(fragment, "teams auth profile")?;
    Ok(AuthSet {
        source,
        profile: profile_id,
        changed: true,
    })
}

fn current_context_head_ids<P>(catalog: &P, source_id: Id) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    teams_core::context_head_ids(catalog, source_id)
}

fn load_chat_map<P>(
    reader: &impl BlobStoreGet,
    catalog: &P,
    source_id: Id,
) -> Result<HashMap<Id, String>>
where
    P: TriblePattern,
{
    let mut map = HashMap::new();
    for (chat_id, handle) in find!(
        (chat: Id, chat_id: Inline<Handle<UTF8String>>),
        pattern!(catalog, [{
            ?chat @
            metadata::tag: teams::kind_chat,
            teams::source: source_id,
            teams::chat_id: ?chat_id,
        }])
    ) {
        let value = read_utf8string(reader, handle, "Teams chat id")?;
        map.insert(chat_id, value);
    }
    Ok(map)
}

fn load_message_external_map<P>(
    reader: &impl BlobStoreGet,
    catalog: &P,
    source_id: Id,
) -> Result<HashMap<Id, String>>
where
    P: TriblePattern,
{
    let mut map = HashMap::new();
    for (message_id, handle) in find!(
        (message: Id, external: Inline<Handle<UTF8String>>),
        pattern!(catalog, [
            {
                ?message @
                metadata::tag: archive::kind_message,
                teams::chat: _?chat,
                teams::message_id: ?external,
            },
            {
                _?chat @
                metadata::tag: teams::kind_chat,
                teams::source: source_id,
            }
        ])
    ) {
        let value = read_utf8string(reader, handle, "Teams message id")?;
        map.insert(message_id, value);
    }
    Ok(map)
}

fn load_known_messages<P>(
    reader: &impl BlobStoreGet,
    catalog: &P,
    source_id: Id,
) -> Result<Vec<KnownMessage>>
where
    P: TriblePattern,
{
    let mut known = BTreeSet::new();
    for (message_id, message_external, chat_id, chat_external) in find!(
        (
            message: Id,
            message_external: Inline<Handle<UTF8String>>,
            chat: Id,
            chat_external: Inline<Handle<UTF8String>>
        ),
        pattern!(catalog, [
            {
                ?message @
                metadata::tag: archive::kind_message,
                teams::chat: ?chat,
                teams::message_id: ?message_external,
            },
            {
                ?chat @
                metadata::tag: teams::kind_chat,
                teams::source: source_id,
                teams::chat_id: ?chat_external,
            }
        ])
    ) {
        known.insert(KnownMessage {
            message_id,
            message_external_id: read_utf8string(reader, message_external, "Teams message id")?,
            chat_id,
            chat_external_id: read_utf8string(reader, chat_external, "Teams chat id")?,
        });
    }
    Ok(known.into_iter().collect())
}

fn read_utf8string(
    reader: &impl BlobStoreGet,
    handle: Inline<Handle<UTF8String>>,
    field: &str,
) -> Result<String> {
    let view: anybytes::View<str> = reader
        .get(handle)
        .with_context(|| format!("read {field} payload {}", hex::encode_upper(handle.raw)))?;
    Ok(view.as_ref().to_owned())
}

fn epoch_after_seconds(base: Epoch, seconds: i64) -> Epoch {
    use hifitime::Duration as HifiDuration;
    base + HifiDuration::from_seconds(seconds as f64)
}

fn login_device_code_collection(
    session: &mut TeamsSession,
    tenant: &str,
    client_id: &str,
    client_secret: Option<&str>,
    client_secret_version: Option<Id>,
    scopes: &str,
    prompt: &mut dyn FnMut(&str) -> Result<()>,
) -> Result<LoginReceipt> {
    let tenant = tenant.trim();
    if tenant.is_empty() {
        bail!("Teams login tenant must not be empty");
    }
    let client_id = client_id.trim();
    if client_id.is_empty() {
        bail!("Teams login client id must not be empty");
    }
    if client_secret.is_some_and(str::is_empty) {
        bail!("Teams client secret must not be empty");
    }
    let scopes = teams_core::canonical_auth_scopes(scopes)?;
    // Validate every caller-supplied collection coordinate before starting an
    // interactive OAuth flow. Source-derived inheritance still has to wait
    // until Microsoft identifies the concrete tenant.
    let explicit_client_version = client_secret_version
        .map(|id| require_exact_secret(session, id, "client-secret version"))
        .transpose()?;
    let device = request_device_code(tenant, client_id, &scopes)?;
    if let Some(message) = &device.message {
        prompt(message)?;
    } else if let Some(url) = &device.verification_uri_complete {
        prompt(&format!("Open {} to authenticate.", url))?;
    } else {
        prompt(&format!(
            "Visit {} and enter code {} to authenticate.",
            device.verification_uri, device.user_code
        ))?;
    }

    let interval = device.interval.unwrap_or(5).max(1) as u64;
    let deadline = now_epoch_secs()? + device.expires_in;
    let token = poll_device_token(tenant, client_id, &device.device_code, interval, deadline)?;
    let user_id = fetch_me_id(&token.access_token)?;
    let source_tenant =
        resolve_source_tenant(tenant, token.id_token.as_deref(), Some(&token.access_token))?;
    let source = source_id_for_tenant(&source_tenant)?;
    let predecessors = teams_core::auth_profile_head_ids(&session.facts, source);
    let previous = match teams_core::auth_profile_head(&session.facts, source) {
        teams_core::AuthProfileHead::Unique(profile) => {
            Some(teams_core::auth_profile(&session.facts, profile)?)
        }
        teams_core::AuthProfileHead::Missing | teams_core::AuthProfileHead::Forked(_) => None,
    };
    let inherited_client_version = match previous.as_ref() {
        Some(profile)
            if read_utf8string(
                &session.reader,
                profile.client_id,
                "predecessor Teams auth client id",
            )? == client_id =>
        {
            profile.client_secret_version
        }
        Some(_) | None => None,
    };
    let canonical_scopes =
        teams_core::canonical_auth_scopes(token.scope.as_deref().unwrap_or(&scopes))?;
    let bundle = DelegatedTokenBundle {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at_unix: now_epoch_secs()? + token.expires_in,
        token_type: token.token_type,
        scope: Some(canonical_scopes.clone()),
    };
    let encoded_bundle =
        serde_json::to_vec(&bundle).context("encode Teams delegated-token bundle")?;
    // Read chronology before the first durable append so a clock failure
    // cannot leave a partially published login.
    let observed_at = clock::point_now()?;
    let client_version = if let Some(client_secret) = client_secret {
        let client_version = session.add_secret(
            &teams_secret_name(source, "client-secret"),
            client_secret.as_bytes(),
            observed_at,
        )?;
        session.notices.push(format!(
            "Published client-secret version {client_version:x}; if later login publication is interrupted, repair with `teams auth set --tenant {source_tenant} --client-id {client_id} --user-id {user_id} --scopes @SCOPES --client-secret-version {client_version:x}`."
    ));
        Some(client_version)
    } else {
        explicit_client_version.or(inherited_client_version)
    };

    let token_version = session.add_secret(
        &teams_secret_name(source, "delegated-token"),
        &encoded_bundle,
        observed_at,
    )?;
    let client_repair = client_version
        .map(|id| format!(" --client-secret-version {id:x}"))
        .unwrap_or_default();
    session.notices.push(format!(
        "Published delegated-token version {token_version:x}; if Teams profile publication is interrupted, repair with `teams auth set --tenant {source_tenant} --client-id {client_id} --user-id {user_id} --scopes @SCOPES{client_repair} --delegated-token-version {token_version:x}`."
    ));
    let mut fragment = source_fragment(&source_tenant);
    let (profile, profile_id) = teams_core::auth_profile_fragment(
        source,
        client_id,
        &user_id,
        &canonical_scopes,
        client_version,
        Some(token_version),
        predecessors,
    )?;
    fragment += profile;
    session.commit(fragment, "teams login auth profile")?;
    Ok(LoginReceipt {
        source,
        profile: profile_id,
        delegated_token_version: token_version,
        client_secret_version: client_version,
        notices: std::mem::take(&mut session.notices),
    })
}

fn request_device_code(tenant: &str, client_id: &str, scopes: &str) -> Result<DeviceCodeResponse> {
    let url = format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/devicecode");
    let params = [("client_id", client_id), ("scope", scopes)];
    let client = Client::new();
    let resp = client
        .post(url)
        .form(&params)
        .send()
        .context("request device code")?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!(
            "device code request failed: status={status} error={}",
            oauth_error_kind(&body)
        );
    }
    let parsed: DeviceCodeResponse =
        serde_json::from_str(&body).context("parse device code response")?;
    Ok(parsed)
}

fn fetch_me_id(access_token: &str) -> Result<String> {
    let client = Client::new();
    let resp = client
        .get("https://graph.microsoft.com/v1.0/me")
        .bearer_auth(access_token)
        .send()
        .context("GET /me")?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!("GET /me failed: status={status} body={body}");
    }
    let json: JsonValue = serde_json::from_str(&body).context("parse /me response")?;
    let Some(id) = json.get("id").and_then(JsonValue::as_str) else {
        bail!("GET /me response missing id");
    };
    Ok(id.to_owned())
}

fn poll_device_token(
    tenant: &str,
    client_id: &str,
    device_code: &str,
    interval_secs: u64,
    deadline: i64,
) -> Result<TokenResponse> {
    let url = format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token");
    let client = Client::new();
    let mut interval = interval_secs;

    loop {
        if now_epoch_secs()? >= deadline {
            bail!("device code expired before authorization completed");
        }

        let params = [
            ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ("client_id", client_id),
            ("device_code", device_code),
        ];
        let resp = client
            .post(&url)
            .form(&params)
            .send()
            .context("poll device token")?;
        let status = resp.status();
        let body = resp.text().unwrap_or_default();
        if status.is_success() {
            let token: TokenResponse =
                serde_json::from_str(&body).context("parse token response")?;
            return Ok(token);
        }

        let error = oauth_error_kind(&body);
        match error.as_str() {
            "authorization_pending" => {
                thread::sleep(StdDuration::from_secs(interval));
            }
            "slow_down" => {
                interval += 5;
                thread::sleep(StdDuration::from_secs(interval));
            }
            "expired_token" => bail!("device code expired"),
            other => {
                bail!("device code authorization failed: status={status} error={other}")
            }
        }
    }
}

fn refresh_token(
    tenant: &str,
    client_id: &str,
    refresh_token: &str,
    scope: Option<&str>,
) -> Result<TokenResponse> {
    let url = format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token");
    let mut params = vec![
        ("grant_type", "refresh_token"),
        ("client_id", client_id),
        ("refresh_token", refresh_token),
    ];
    if let Some(scope) = scope {
        params.push(("scope", scope));
    }
    let client = Client::new();
    let resp = client
        .post(url)
        .form(&params)
        .send()
        .context("refresh token")?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!(
            "refresh token failed: status={status} error={}",
            oauth_error_kind(&body)
        );
    }
    let token: TokenResponse = serde_json::from_str(&body).context("parse refresh response")?;
    Ok(token)
}

fn request_client_credentials_token(
    tenant: &str,
    client_id: &str,
    client_secret: &str,
) -> Result<TokenResponse> {
    let url = format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token");
    let params = [
        ("grant_type", "client_credentials"),
        ("client_id", client_id),
        ("client_secret", client_secret),
        ("scope", "https://graph.microsoft.com/.default"),
    ];
    let client = Client::new();
    let resp = client
        .post(url)
        .form(&params)
        .send()
        .context("request client credentials token")?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!(
            "client credentials token failed: status={status} error={}",
            oauth_error_kind(&body)
        );
    }
    let token: TokenResponse =
        serde_json::from_str(&body).context("parse client credentials response")?;
    Ok(token)
}

struct DeltaPage {
    messages: Vec<JsonValue>,
    next_link: Option<String>,
    delta_link: Option<String>,
}

#[derive(Debug)]
struct DeltaCursorExpired;

impl std::fmt::Display for DeltaCursorExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Teams delta cursor expired")
    }
}

impl std::error::Error for DeltaCursorExpired {}

fn fetch_delta_page(client: &Client, token: &str, url: &str) -> Result<DeltaPage> {
    let safe_url = url_without_query(url);
    let resp = client
        .get(url)
        .bearer_auth(token)
        .send()
        .map_err(|err| anyhow::anyhow!("GET {safe_url}: {}", err.without_url()))?;
    let status = resp.status();
    let body = resp.text().map_err(|err| {
        anyhow::anyhow!("read response body for {safe_url}: {}", err.without_url())
    })?;
    let graph_error_code = serde_json::from_str::<JsonValue>(&body)
        .ok()
        .and_then(|json| {
            json.pointer("/error/code")
                .and_then(JsonValue::as_str)
                .map(str::to_owned)
        });
    if status.as_u16() == 410
        || (status.is_client_error() && graph_error_code.as_deref() == Some("syncStateNotFound"))
    {
        return Err(DeltaCursorExpired.into());
    }
    if !status.is_success() {
        bail!(
            "GET {} failed: status={status} graph_error={}",
            url_without_query(url),
            graph_error_code.as_deref().unwrap_or("unknown"),
        );
    }

    let json: JsonValue = serde_json::from_str(&body).context("parse delta json")?;
    let messages = json
        .get("value")
        .and_then(JsonValue::as_array)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("Teams delta response is missing its value array"))?;
    let next_link = json
        .get("@odata.nextLink")
        .and_then(JsonValue::as_str)
        .map(str::to_owned);
    let delta_link = json
        .get("@odata.deltaLink")
        .and_then(JsonValue::as_str)
        .map(str::to_owned);

    Ok(DeltaPage {
        messages,
        next_link,
        delta_link,
    })
}

fn url_without_query(url: &str) -> &str {
    url.split_once('?').map_or(url, |(base, _)| base)
}

fn send_message(
    config: &TeamsBridgeConfig,
    session: &mut TeamsSession,
    chat_id: &str,
    text: &str,
) -> Result<SentMessage> {
    let token = get_delegated_token(config, session)?;
    let url = format!("https://graph.microsoft.com/v1.0/chats/{chat_id}/messages");
    let body = json!({
        "body": {
            "contentType": "text",
            "content": text
        }
    });

    let client = Client::new();
    let resp = client
        .post(url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .context("POST chat message")?;
    let status = resp.status();
    let response_body = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!("send message failed: status={status} body={response_body}");
    }
    let message_id = serde_json::from_str::<JsonValue>(&response_body)
        .ok()
        .and_then(|value| {
            value
                .get("id")
                .and_then(JsonValue::as_str)
                .map(str::to_owned)
        });
    Ok(SentMessage {
        chat_id: chat_id.to_owned(),
        message_id,
    })
}

fn list_users(
    config: &TeamsBridgeConfig,
    session: &mut TeamsSession,
    prefix: Option<&str>,
    limit: usize,
) -> Result<Vec<DirectoryUser>> {
    let token = get_delegated_token(config, session)?;
    let mut url =
        reqwest::Url::parse("https://graph.microsoft.com/v1.0/users").context("parse users url")?;
    {
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("$select", "id,displayName,mail,userPrincipalName");
        if let Some(prefix) = prefix.map(str::trim).filter(|value| !value.is_empty()) {
            let escaped = escape_odata_literal(prefix);
            let filter = format!(
                "startswith(displayName,'{escaped}') or startswith(userPrincipalName,'{escaped}') or startswith(mail,'{escaped}')"
            );
            pairs.append_pair("$filter", &filter);
        }
        if limit > 0 {
            pairs.append_pair("$top", &limit.to_string());
        }
    }

    let client = Client::new();
    let resp = client
        .get(url)
        .bearer_auth(token)
        .send()
        .context("GET /users")?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!("list users failed: status={status} body={body}");
    }
    let json_body: JsonValue = serde_json::from_str(&body).context("parse users json")?;
    let users = json_body
        .get("value")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();

    let mut result = Vec::new();
    for user in users {
        let id = user
            .get("id")
            .and_then(JsonValue::as_str)
            .unwrap_or("unknown");
        let name = user
            .get("displayName")
            .and_then(JsonValue::as_str)
            .unwrap_or("unknown");
        let mail = user.get("mail").and_then(JsonValue::as_str);
        let upn = user.get("userPrincipalName").and_then(JsonValue::as_str);
        let contact = mail.or(upn).unwrap_or("-");
        result.push(DirectoryUser {
            id: id.to_owned(),
            name: name.to_owned(),
            contact: contact.to_owned(),
        });
    }
    Ok(result)
}

fn set_presence_status(
    config: &TeamsBridgeConfig,
    session: &mut TeamsSession,
    availability: PresenceAvailability,
    activity: Option<PresenceActivity>,
    duration_mins: u32,
    session_id: Option<String>,
) -> Result<PresenceSet> {
    let availability = availability.as_graph();
    let activity = activity
        .map(|value| value.as_graph().to_string())
        .unwrap_or_else(|| default_activity_for(availability).to_string());
    ensure_presence_combo(availability, &activity)?;
    if !(5..=240).contains(&duration_mins) {
        bail!("duration-mins must be between 5 and 240");
    }
    let user_id = config.user_id.clone();
    let default_session = config.client_id.clone();
    let session_id = session_id.unwrap_or(default_session);

    let token = get_delegated_token(config, session)?;
    let url = format!("https://graph.microsoft.com/v1.0/users/{user_id}/presence/setPresence");
    let expiration = format!("PT{}M", duration_mins);
    let body = json!({
        "sessionId": session_id,
        "availability": availability,
        "activity": activity,
        "expirationDuration": expiration,
    });

    let client = Client::new();
    let resp = client
        .post(url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .context("POST setPresence")?;
    let status = resp.status();
    let response_body = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!("set presence failed: status={status} body={response_body}");
    }
    Ok(PresenceSet {
        user_id,
        availability: availability.to_owned(),
        activity,
        session_id,
        duration_mins,
    })
}

fn get_presence(
    config: &TeamsBridgeConfig,
    session: &mut TeamsSession,
    user_ids: Vec<String>,
) -> Result<Vec<Presence>> {
    if user_ids.is_empty() {
        bail!("presence-get requires at least one user id");
    }
    let token = get_delegated_token(config, session)?;
    let url = "https://graph.microsoft.com/v1.0/communications/getPresencesByUserId";
    let body = json!({
        "ids": user_ids,
    });

    let client = Client::new();
    let resp = client
        .post(url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .context("POST getPresencesByUserId")?;
    let status = resp.status();
    let response_body = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!("get presence failed: status={status} body={response_body}");
    }
    let json_body: JsonValue =
        serde_json::from_str(&response_body).context("parse presence json")?;
    let presences = json_body
        .get("value")
        .and_then(JsonValue::as_array)
        .cloned()
        .unwrap_or_default();

    let mut result = Vec::new();
    for presence in presences {
        let id = presence
            .get("id")
            .and_then(JsonValue::as_str)
            .unwrap_or("unknown");
        let availability = presence
            .get("availability")
            .and_then(JsonValue::as_str)
            .unwrap_or("unknown");
        let activity = presence
            .get("activity")
            .and_then(JsonValue::as_str)
            .unwrap_or("unknown");
        result.push(Presence {
            id: id.to_owned(),
            availability: availability.to_owned(),
            activity: activity.to_owned(),
        });
    }
    Ok(result)
}

fn default_activity_for(availability: &str) -> &'static str {
    match availability {
        "Available" => "Available",
        "Away" => "Away",
        "Busy" => "InACall",
        "DoNotDisturb" => "Presenting",
        _ => "Available",
    }
}

fn ensure_presence_combo(availability: &str, activity: &str) -> Result<()> {
    let ok = match (availability, activity) {
        ("Available", "Available") => true,
        ("Busy", "InACall") => true,
        ("Busy", "InAConferenceCall") => true,
        ("Away", "Away") => true,
        ("DoNotDisturb", "Presenting") => true,
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        bail!(
            "unsupported availability/activity combo: {availability}/{activity} (allowed: Available/Available, Busy/InACall, Busy/InAConferenceCall, Away/Away, DoNotDisturb/Presenting)"
        )
    }
}

fn invite_to_chat(
    config: &TeamsBridgeConfig,
    session: &mut TeamsSession,
    chat_id: &str,
    user_id: &str,
    owner: bool,
) -> Result<InvitedMember> {
    let token = get_delegated_token(config, session)?;
    let url = format!("https://graph.microsoft.com/v1.0/chats/{chat_id}/members");
    let roles = if owner { vec!["owner"] } else { Vec::new() };
    let body = json!({
        "@odata.type": "#microsoft.graph.aadUserConversationMember",
        "roles": roles,
        "user@odata.bind": format!("https://graph.microsoft.com/v1.0/users('{user_id}')"),
    });

    let client = Client::new();
    let resp = client
        .post(url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .context("POST chat member")?;
    let status = resp.status();
    let response_body = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!("chat invite failed: status={status} body={response_body}");
    }
    Ok(InvitedMember {
        chat_id: chat_id.to_owned(),
        user_id: user_id.to_owned(),
        owner,
    })
}

fn create_chat(
    config: &TeamsBridgeConfig,
    session: &mut TeamsSession,
    mut user_ids: Vec<String>,
    force_group: bool,
    topic: Option<String>,
) -> Result<String> {
    if user_ids.is_empty() {
        bail!("chat-create requires at least one user id");
    }
    let self_id = config.user_id.clone();
    if !user_ids.iter().any(|id| id == &self_id) {
        user_ids.push(self_id.clone());
    }
    user_ids.sort();
    user_ids.dedup();
    let chat_type = if user_ids.len() == 2 && !force_group {
        "oneOnOne"
    } else {
        "group"
    };

    let members: Vec<JsonValue> = user_ids
        .iter()
        .map(|id| {
            let mut member = serde_json::Map::new();
            member.insert(
                "@odata.type".to_string(),
                json!("#microsoft.graph.aadUserConversationMember"),
            );
            member.insert(
                "user@odata.bind".to_string(),
                json!(format!("https://graph.microsoft.com/v1.0/users('{id}')")),
            );
            // Graph requires every member to have an explicit role (owner or guest).
            // Use owner for in-tenant users by default.
            member.insert("roles".to_string(), json!(["owner"]));
            JsonValue::Object(member)
        })
        .collect();

    let mut body = serde_json::Map::new();
    body.insert("chatType".to_string(), json!(chat_type));
    body.insert("members".to_string(), JsonValue::Array(members));
    if chat_type == "group" {
        if let Some(topic) = topic {
            let trimmed = topic.trim();
            if !trimmed.is_empty() {
                body.insert("topic".to_string(), json!(trimmed));
            }
        }
    }
    let token = get_delegated_token(config, session)?;
    let client = Client::new();
    let resp = client
        .post("https://graph.microsoft.com/v1.0/chats")
        .bearer_auth(token)
        .json(&body)
        .send()
        .context("POST create chat")?;
    let status = resp.status();
    let response_body = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!("chat create failed: status={status} body={response_body}");
    }
    let json_body: JsonValue =
        serde_json::from_str(&response_body).context("parse create chat response")?;
    let chat_id = json_body
        .get("id")
        .and_then(JsonValue::as_str)
        .unwrap_or("unknown");
    Ok(chat_id.to_owned())
}

fn escape_odata_literal(value: &str) -> String {
    value.replace('\'', "''")
}

#[derive(Debug, Clone)]
pub struct ReadOptions {
    pub chat_id: Option<String>,
    pub since: Option<String>,
    pub limit: usize,
    pub descending: bool,
}

/// One logical message and its current versions, several where they are
/// unordered. A bare source tombstone has no observation to read.
#[derive(Debug, Clone)]
struct ReadMessage {
    message_id: Id,
    chat_id: Id,
    versions: Vec<Option<ReadVersion>>,
}

impl ReadMessage {
    /// The versions which still carry the message.
    fn live(&self) -> impl Iterator<Item = &ReadVersion> {
        self.versions
            .iter()
            .flatten()
            .filter(|version| !version.deleted)
    }

    /// When the message was sent, the earliest its versions say; absent where
    /// every version is a deletion.
    fn live_since(&self) -> Option<i128> {
        self.live().map(|version| version.created_at_key).min()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadVersion {
    author_names: BTreeSet<Inline<Handle<UTF8String>>>,
    deleted: bool,
    created_at: Inline<NsTAIInterval>,
    modified_at: Inline<NsTAIInterval>,
    created_at_key: i128,
    content: Option<Inline<Handle<UTF8String>>>,
    attachments: BTreeSet<Id>,
}

#[derive(Debug, Clone)]
pub struct AttachmentListOptions {
    pub chat_id: Option<String>,
    pub message_id: Option<String>,
    pub limit: usize,
    pub descending: bool,
}

#[derive(Debug, Clone)]
pub struct AttachmentGetOptions {
    pub source_id: String,
    pub chat_id: Option<String>,
    pub message_id: Option<String>,
}

#[derive(Debug, Clone)]
struct AttachmentExportCandidate {
    attachment_id: Id,
    message_id: Id,
    chat_id: Id,
    source_id: String,
    source_kind: Option<String>,
    data_handle: Inline<Handle<RawBytes>>,
    name: Option<Inline<Handle<UTF8String>>>,
    media_type: Option<Inline<Handle<UTF8String>>>,
}

#[derive(Debug, Clone)]
struct AttachmentRow {
    attachment_id: Id,
    message_id: Id,
    chat_id: Id,
    created_at: Inline<NsTAIInterval>,
    created_at_key: i128,
    source_id: Option<Inline<Handle<UTF8String>>>,
    source_kind: Option<Inline<Handle<UTF8String>>>,
    source_pointers: BTreeSet<Inline<Handle<UTF8String>>>,
    /// The smallest name, and every other one a writer gave.
    name: Option<Inline<Handle<UTF8String>>>,
    other_names: BTreeSet<Inline<Handle<UTF8String>>>,
    /// The smallest media type, and every other one.
    media_type: Option<Inline<Handle<UTF8String>>>,
    other_media_types: BTreeSet<Inline<Handle<UTF8String>>>,
    size: Option<Inline<U256BE>>,
}

fn attachment_reference(source_kind: Option<&str>, source_id: &str) -> String {
    match source_kind {
        Some(kind @ ("attachment" | "hosted-content")) => format!("{kind}:{source_id}"),
        _ => source_id.to_owned(),
    }
}

fn parse_attachment_reference(reference: &str) -> (Option<&str>, &str) {
    for kind in ["attachment", "hosted-content"] {
        if let Some(source_id) = reference
            .strip_prefix(kind)
            .and_then(|rest| rest.strip_prefix(':'))
        {
            return (Some(kind), source_id);
        }
    }
    (None, reference)
}

fn current_messages<P>(catalog: &P, source_id: Id) -> Result<Vec<ReadMessage>>
where
    P: TriblePattern,
{
    let message_chats = find!(
        (message: Id, chat: Id),
        pattern!(catalog, [
            {
                ?message @
                metadata::tag: archive::kind_message,
                teams::chat: ?chat,
                teams::message_id: _?external,
            },
            {
                ?chat @
                metadata::tag: teams::kind_chat,
                teams::source: source_id,
            }
        ])
    )
    .collect::<BTreeMap<_, _>>();
    let mut result = Vec::new();
    for (message, versions) in teams_core::current_message_states(catalog, source_id) {
        let chat = message_chats.get(&message).copied().ok_or_else(|| {
            anyhow::anyhow!(
                "Teams causal state names message {message:x} outside source {source_id:x}"
            )
        })?;
        let mut distinct = Vec::new();
        for version in versions {
            let version = match version {
                teams_core::MessageVersion::Present(observation) => {
                    read_message_observation(catalog, observation, false).map(Some)
                }
                teams_core::MessageVersion::Deleted(observation) => {
                    read_message_observation(catalog, observation, true).map(Some)
                }
                teams_core::MessageVersion::Removed => Ok(None),
            }?;
            // Versions which differ only in the source's etag read the same,
            // which is not a conflict.
            if !distinct.contains(&version) {
                distinct.push(version);
            }
        }
        result.push(ReadMessage {
            message_id: message,
            chat_id: chat,
            versions: distinct,
        });
    }
    Ok(result)
}

fn read_message_observation<P>(
    catalog: &P,
    observation_id: Id,
    deleted: bool,
) -> Result<ReadVersion>
where
    P: TriblePattern,
{
    // Creation time and content annotate the observation: should two writers
    // disagree, the earliest time and the smallest handle are shown.
    let created_at = find!(
        created: Inline<NsTAIInterval>,
        pattern!(catalog, [{ observation_id @ metadata::created_at: ?created }])
    )
    .min_by_key(|created| (interval_key(*created), *created));
    let content = find!(
        content: Inline<Handle<UTF8String>>,
        pattern!(catalog, [{ observation_id @ archive::content: ?content }])
    )
    .min();
    let author_names = find!(
        name: Inline<Handle<UTF8String>>,
        pattern!(catalog, [{ observation_id @ teams::author_name: ?name }])
    )
    .collect::<BTreeSet<_>>();
    if !deleted && (created_at.is_none() || content.is_none()) {
        bail!("present Teams observation {observation_id:x} lacks created time or content");
    }
    // The modification time is the observation's identity.
    let modified_at = find!(
        modified: Inline<NsTAIInterval>,
        pattern!(catalog, [{ observation_id @ teams::modified_at: ?modified }])
    )
    .min_by_key(|modified| (interval_key(*modified), *modified))
    .ok_or_else(|| anyhow::anyhow!("Teams message modified time is missing"))?;
    let created_at = created_at.unwrap_or(modified_at);
    let attachments = find!(
        attachment: Id,
        pattern!(catalog, [{ observation_id @ archive::attachment: ?attachment }])
    )
    .collect::<BTreeSet<_>>();
    Ok(ReadVersion {
        author_names,
        deleted,
        created_at,
        modified_at,
        created_at_key: interval_key(created_at),
        content,
        attachments,
    })
}

fn read_messages(
    source_id: Id,
    session: &mut TeamsSession,
    options: ReadOptions,
) -> Result<Vec<ArchivedMessage>> {
    let view = session.view();
    let chat_map = load_chat_map(&view.reader, &view.facts, source_id)?;
    let chat_filter_ids = filter_external_ids(options.chat_id.as_deref(), &chat_map, "chat")?;
    let since_key = parse_since_key(options.since.as_deref())?;
    let mut messages = current_messages(&view.facts, source_id)?
        .into_iter()
        .filter_map(|message| message.live_since().map(|since| (since, message)))
        .filter(|(_, message)| {
            chat_filter_ids
                .as_ref()
                .is_none_or(|filter| filter.contains(&message.chat_id))
        })
        .filter(|(sent, _)| since_key.is_none_or(|since| *sent >= since))
        .collect::<Vec<_>>();
    messages.sort_by_key(|(sent, message)| (*sent, message.message_id));
    if options.limit > 0 && messages.len() > options.limit {
        messages = messages.split_off(messages.len() - options.limit);
    }
    if options.descending {
        messages.reverse();
    }
    let mut result = Vec::new();
    for (_, message) in messages {
        let chat = chat_map
            .get(&message.chat_id)
            .cloned()
            .unwrap_or_else(|| format!("{}", message.chat_id));
        let version_count = message.versions.len();
        for (version_index, version) in message.versions.into_iter().enumerate() {
            let content = version
                .as_ref()
                .filter(|version| !version.deleted)
                .map(|version| {
                    read_utf8string(
                        &view.reader,
                        version.content.expect("present observation has content"),
                        "Teams message content",
                    )
                })
                .transpose()?;
            let mut author_names = version
                .iter()
                .flat_map(|version| &version.author_names)
                .map(|handle| read_utf8string(&view.reader, *handle, "Teams author display name"))
                .collect::<Result<Vec<_>>>()?;
            author_names.sort();
            author_names.dedup();
            let author = if author_names.is_empty() {
                "unknown".to_owned()
            } else {
                author_names.join(" / ")
            };
            result.push(ArchivedMessage {
                id: message.message_id,
                created_at: version.as_ref().map(|version| version.created_at),
                modified_at: version.as_ref().map(|version| version.modified_at),
                chat: chat.clone(),
                author,
                content,
                version_index,
                version_count,
            });
        }
    }
    Ok(result)
}

fn filter_external_ids(
    requested: Option<&str>,
    map: &HashMap<Id, String>,
    kind: &str,
) -> Result<Option<HashSet<Id>>> {
    let Some(requested) = requested.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let ids = map
        .iter()
        .filter_map(|(id, external)| (external == requested).then_some(*id))
        .collect::<HashSet<_>>();
    if ids.is_empty() {
        bail!("No {kind} found for id {requested}");
    }
    Ok(Some(ids))
}

#[derive(Debug, Clone)]
struct IncomingMessage {
    chat_external_id: Option<String>,
    message_external_id: String,
    raw_json: String,
    author_external_id: Option<String>,
    author_display_name: Option<String>,
    content: Option<String>,
    created_at: Option<Inline<NsTAIInterval>>,
    modified_at: Option<Inline<NsTAIInterval>>,
    source_removed: bool,
    deleted: bool,
    deleted_at: Option<Inline<NsTAIInterval>>,
    etag: Option<String>,
    attachments: Vec<AttachmentSource>,
}

/// A source-local logical message identity already established by a previous
/// admitted page (or by a fully identified message in the page being built).
/// Graph's minimal `@removed` records contain only the source-local message id,
/// so they may be resolved only when that id names exactly one such record.
#[derive(Debug, Clone, Eq, Ord, PartialEq, PartialOrd)]
struct KnownMessage {
    message_id: Id,
    message_external_id: String,
    chat_id: Id,
    chat_external_id: String,
}

#[derive(Debug, Clone)]
struct AttachmentSource {
    source_kind: &'static str,
    source_id: String,
    source_url: Option<String>,
    fetch_required: bool,
    name: Option<String>,
    content_type: Option<String>,
    content_bytes: Option<Vec<u8>>,
}

fn list_attachments(
    source_id: Id,
    session: &mut TeamsSession,
    options: AttachmentListOptions,
) -> Result<Vec<AttachmentInfo>> {
    let view = session.view();
    let chat_map = load_chat_map(&view.reader, &view.facts, source_id)?;
    let message_map = load_message_external_map(&view.reader, &view.facts, source_id)?;
    let chat_filter = filter_external_ids(options.chat_id.as_deref(), &chat_map, "chat")?;
    let message_filter =
        filter_external_ids(options.message_id.as_deref(), &message_map, "message")?;
    let mut rows = attachment_rows(
        &view.reader,
        &view.facts,
        source_id,
        chat_filter.as_ref(),
        message_filter.as_ref(),
    )?;
    rows.sort_by(|left, right| {
        left.created_at_key
            .cmp(&right.created_at_key)
            .then_with(|| left.attachment_id.cmp(&right.attachment_id))
    });
    if options.limit > 0 && rows.len() > options.limit {
        rows = rows.split_off(rows.len() - options.limit);
    }
    if options.descending {
        rows.reverse();
    }
    let mut result = Vec::new();
    for row in rows {
        let chat = chat_map
            .get(&row.chat_id)
            .cloned()
            .unwrap_or_else(|| format!("{}", row.chat_id));
        let message = message_map
            .get(&row.message_id)
            .cloned()
            .unwrap_or_else(|| format!("{}", row.message_id));
        result.push(attachment_info(&view.reader, chat, message, row)?);
    }
    Ok(result)
}

/// One listed attachment, its payloads read.
fn attachment_info(
    reader: &impl BlobStoreGet,
    chat: String,
    message: String,
    row: AttachmentRow,
) -> Result<AttachmentInfo> {
    let source_id = row
        .source_id
        .map(|handle| read_utf8string(reader, handle, "Teams attachment source id"))
        .transpose()?
        .unwrap_or_default();
    let source_kind = row
        .source_kind
        .map(|handle| read_utf8string(reader, handle, "Teams attachment kind"))
        .transpose()?;
    let mut source_pointers = row
        .source_pointers
        .into_iter()
        .map(|handle| read_utf8string(reader, handle, "Teams attachment pointer"))
        .collect::<Result<Vec<_>>>()?;
    source_pointers.sort();
    source_pointers.dedup();
    let name = row
        .name
        .map(|handle| read_utf8string(reader, handle, "Teams attachment name"))
        .transpose()?;
    let media_type = row
        .media_type
        .map(|handle| read_utf8string(reader, handle, "Teams attachment media type"))
        .transpose()?;
    let other_names = row
        .other_names
        .into_iter()
        .map(|handle| read_utf8string(reader, handle, "Teams attachment name"))
        .collect::<Result<Vec<_>>>()?;
    let other_media_types = row
        .other_media_types
        .into_iter()
        .map(|handle| read_utf8string(reader, handle, "Teams attachment media type"))
        .collect::<Result<Vec<_>>>()?;
    let size = row.size.map(inline_u256_to_u128).transpose()?;
    Ok(AttachmentInfo {
        id: row.attachment_id,
        created_at: row.created_at,
        chat,
        message,
        reference: attachment_reference(source_kind.as_deref(), &source_id),
        name,
        other_names,
        media_type,
        other_media_types,
        size,
        source_pointers,
    })
}

fn attachment_rows<P>(
    _reader: &impl BlobStoreGet,
    catalog: &P,
    source_id: Id,
    chat_filter: Option<&HashSet<Id>>,
    message_filter: Option<&HashSet<Id>>,
) -> Result<Vec<AttachmentRow>>
where
    P: TriblePattern,
{
    let mut rows = Vec::new();
    for message in current_messages(catalog, source_id)? {
        if chat_filter.is_some_and(|filter| !filter.contains(&message.chat_id))
            || message_filter.is_some_and(|filter| !filter.contains(&message.message_id))
        {
            continue;
        }
        // An occurrence several unordered versions carry is one occurrence,
        // listed with the version sent earliest.
        let mut occurrences = BTreeMap::<Id, (i128, Inline<NsTAIInterval>)>::new();
        for version in message.live() {
            for attachment_id in &version.attachments {
                let sent = (version.created_at_key, version.created_at);
                occurrences
                    .entry(*attachment_id)
                    .and_modify(|kept| *kept = (*kept).min(sent))
                    .or_insert(sent);
            }
        }
        for (attachment_id, (created_at_key, created_at)) in occurrences {
            // Source id, kind, and name are the occurrence's identity; should
            // a writer ever reuse an id, the smallest handle is shown. An
            // occurrence without a readable source id cannot be referenced.
            let Some(source_id) = find!(
                value: Inline<Handle<UTF8String>>,
                pattern!(catalog, [{ attachment_id @ archive::attachment_source_id: ?value }])
            )
            .min() else {
                continue;
            };
            // Absent on occurrences written before the kind became text,
            // until a backfill gives them its twin.
            let source_kind = find!(
                value: Inline<Handle<UTF8String>>,
                pattern!(catalog, [{ attachment_id @ teams::attachment_kind: ?value }])
            )
            .min();
            let source_pointers = find!(
                value: Inline<Handle<UTF8String>>,
                pattern!(catalog, [{ attachment_id @ archive::attachment_source_pointer: ?value }])
            )
            .collect::<BTreeSet<_>>();
            // File and size annotate the occurrence once it is materialized,
            // and the file carries its name and media type. Should two
            // writers disagree on file or size, the smallest is shown. Names
            // and media types are shown smallest first with the others kept
            // beside it, so the listing says that alternatives exist; one
            // annotated file never refuses it.
            let file_id = attachment_file(catalog, attachment_id);
            let occurrence_names = find!(
                value: Inline<Handle<UTF8String>>,
                pattern!(catalog, [{ attachment_id @ archive::attachment_name: ?value }])
            )
            .collect::<BTreeSet<_>>();
            // The occurrence's own name wins over its file's, as before.
            let mut names = if occurrence_names.is_empty() {
                file_id
                    .map(|file_id| {
                        find!(
                            value: Inline<Handle<UTF8String>>,
                            pattern!(catalog, [{ file_id @ file::name: ?value }])
                        )
                        .collect::<BTreeSet<_>>()
                    })
                    .unwrap_or_default()
            } else {
                occurrence_names
            };
            let mut media_types = file_id
                .map(|file_id| {
                    find!(
                        value: Inline<Handle<UTF8String>>,
                        pattern!(catalog, [
                            { file_id @ file::media_type: _?media_type },
                            { _?media_type @
                                metadata::tag: &KIND_MEDIA_TYPE,
                                metadata::name: ?value,
                            }
                        ])
                    )
                    .collect::<BTreeSet<_>>()
                })
                .unwrap_or_default();
            let size = find!(
                value: Inline<U256BE>,
                pattern!(catalog, [{ attachment_id @ archive::attachment_size_bytes: ?value }])
            )
            .min();
            rows.push(AttachmentRow {
                attachment_id,
                message_id: message.message_id,
                chat_id: message.chat_id,
                created_at,
                created_at_key,
                source_id: Some(source_id),
                source_kind,
                source_pointers,
                name: names.pop_first(),
                other_names: names,
                media_type: media_types.pop_first(),
                other_media_types: media_types,
                size,
            });
        }
    }
    Ok(rows)
}

fn get_attachment(
    source_id: Id,
    session: &mut TeamsSession,
    options: AttachmentGetOptions,
) -> Result<AttachmentLookup> {
    let view = session.view();
    let chat_map = load_chat_map(&view.reader, &view.facts, source_id)?;
    let message_map = load_message_external_map(&view.reader, &view.facts, source_id)?;
    let chat_filter = filter_external_ids(options.chat_id.as_deref(), &chat_map, "chat")?;
    let message_filter =
        filter_external_ids(options.message_id.as_deref(), &message_map, "message")?;
    let wanted = options.source_id.trim();
    let (wanted_kind, wanted_source) = parse_attachment_reference(wanted);
    if wanted_source.is_empty() {
        bail!("attachment source id is empty");
    }
    let rows = attachment_rows(
        &view.reader,
        &view.facts,
        source_id,
        chat_filter.as_ref(),
        message_filter.as_ref(),
    )?;
    let mut candidates = Vec::new();
    for row in rows {
        let source = read_utf8string(
            &view.reader,
            row.source_id.expect("attachment row has source id"),
            "Teams attachment source id",
        )?;
        let kind = row
            .source_kind
            .map(|handle| read_utf8string(&view.reader, handle, "Teams attachment kind"))
            .transpose()?;
        if source != wanted_source
            || wanted_kind.is_some_and(|wanted| Some(wanted) != kind.as_deref())
        {
            continue;
        }
        let Some(file_id) = attachment_file(&view.facts, row.attachment_id) else {
            continue;
        };
        // A file observed with several contents is a candidate for each, so
        // the lookup below reports it as ambiguous instead of choosing.
        let data_handles: Vec<file_capability::ContentHandle> = find!(
            content: file_capability::ContentHandle,
            pattern!(&view.facts, [{ file_id @ file::content: ?content }])
        )
        .collect();
        anyhow::ensure!(
            !data_handles.is_empty(),
            "attachment file {file_id:x} has no content"
        );
        for data_handle in data_handles {
            candidates.push(AttachmentExportCandidate {
                attachment_id: row.attachment_id,
                message_id: row.message_id,
                chat_id: row.chat_id,
                source_id: source.clone(),
                source_kind: kind.clone(),
                data_handle,
                name: row.name.clone(),
                media_type: row.media_type.clone(),
            });
        }
    }
    if candidates.is_empty() {
        return Ok(AttachmentLookup::Missing {
            reference: wanted.to_owned(),
        });
    }
    if candidates.len() > 1 {
        return Ok(AttachmentLookup::Ambiguous(
            candidates
                .iter()
                .map(|candidate| AttachmentMatch {
                    chat: chat_map
                        .get(&candidate.chat_id)
                        .cloned()
                        .unwrap_or_else(|| "?".to_owned()),
                    message: message_map
                        .get(&candidate.message_id)
                        .cloned()
                        .unwrap_or_else(|| "?".to_owned()),
                    reference: attachment_reference(
                        candidate.source_kind.as_deref(),
                        &candidate.source_id,
                    ),
                })
                .collect(),
        ));
    }
    let candidate = candidates.remove(0);
    let media_type = candidate
        .media_type
        .map(|handle| read_utf8string(&view.reader, handle, "attachment media type"))
        .transpose()?;
    // An unavailable descriptive filename remains optional, as in CLI export.
    let name = candidate
        .name
        .map(|handle| read_utf8string(&view.reader, handle, "attachment name"))
        .transpose()
        .ok()
        .flatten()
        .unwrap_or_else(|| candidate.source_id.clone());
    let bytes: Bytes = view
        .reader
        .get(candidate.data_handle)
        .context("load attachment bytes")?;
    Ok(AttachmentLookup::Found(AttachmentData {
        id: candidate.attachment_id,
        bytes,
        name,
        media_type,
        reference: attachment_reference(candidate.source_kind.as_deref(), &candidate.source_id),
    }))
}

fn parse_messages(messages: Vec<JsonValue>) -> Result<Vec<IncomingMessage>> {
    // Preserve every distinct immutable source version. Full versions are
    // ordered only by Graph's lastModifiedDateTime and etag; neither field is
    // synthesized from another timestamp. Minimal @removed records carry no
    // usable version clock and are ordered causally by their page receipt.
    let mut parsed = BTreeMap::new();
    for message in messages {
        let message_external_id = message
            .get("id")
            .and_then(JsonValue::as_str)
            .filter(|value| !value.trim().is_empty())
            .ok_or_else(|| anyhow::anyhow!("Teams delta message is missing id"))?;
        let chat_external_id = message
            .get("chatId")
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        let source_removed = message.get("@removed").is_some();
        let raw_json = serde_json::to_string(&message).context("serialize teams message json")?;

        if source_removed {
            let incoming = IncomingMessage {
                chat_external_id,
                message_external_id: message_external_id.to_owned(),
                raw_json: raw_json.clone(),
                author_external_id: None,
                author_display_name: None,
                content: None,
                created_at: None,
                modified_at: None,
                source_removed: true,
                deleted: true,
                deleted_at: None,
                etag: None,
                attachments: Vec::new(),
            };
            parsed.insert(
                (
                    incoming.chat_external_id.clone(),
                    incoming.message_external_id.clone(),
                    None,
                    None,
                    raw_json,
                ),
                incoming,
            );
            continue;
        }

        let chat_external_id = chat_external_id.ok_or_else(|| {
            anyhow::anyhow!("Teams delta message {message_external_id} is missing chatId")
        })?;
        let content = message
            .get("body")
            .and_then(|body| body.get("content"))
            .and_then(JsonValue::as_str)
            .map(str::to_owned);

        let created_at = message
            .get("createdDateTime")
            .and_then(JsonValue::as_str)
            .map(|value| {
                teams_core::parse_graph_datetime(value)
                    .map(epoch_interval)
                    .ok_or_else(|| anyhow::anyhow!("invalid Teams createdDateTime {value:?}"))
            })
            .transpose()?;
        let deleted_at = message
            .get("deletedDateTime")
            .and_then(JsonValue::as_str)
            .map(|value| {
                teams_core::parse_graph_datetime(value)
                    .map(epoch_interval)
                    .ok_or_else(|| anyhow::anyhow!("invalid Teams deletedDateTime {value:?}"))
            })
            .transpose()?;
        let deleted = deleted_at.is_some();
        if !deleted && content.is_none() {
            bail!("Teams delta message {message_external_id} is missing body.content");
        }
        let modified_at = message
            .get("lastModifiedDateTime")
            .and_then(JsonValue::as_str)
            .map(|value| {
                teams_core::parse_graph_datetime(value)
                    .map(epoch_interval)
                    .ok_or_else(|| anyhow::anyhow!("invalid Teams lastModifiedDateTime {value:?}"))
            })
            .transpose()?
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Teams delta message {message_external_id} has no Graph lastModifiedDateTime; refusing to manufacture a source version"
                )
            })?;
        let modified_at_key = interval_key(modified_at);
        let etag = message
            .get("etag")
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|etag| !etag.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Teams delta message {message_external_id} has no Graph etag; refusing to manufacture a source version"
                )
            })?;

        let from = message.get("from");
        let author_external_id = from
            .and_then(|from| from.get("user"))
            .and_then(|user| user.get("id"))
            .and_then(JsonValue::as_str)
            .map(str::to_owned);
        let author_display_name = from
            .and_then(|from| from.get("user"))
            .and_then(|user| user.get("displayName"))
            .and_then(JsonValue::as_str)
            .map(str::to_owned);

        let mut attachments = Vec::new();
        let mut seen_sources = HashSet::new();
        attachments.extend(parse_json_attachments(
            &message,
            &chat_external_id,
            message_external_id,
            &mut seen_sources,
        )?);
        if let Some(content) = content.as_deref() {
            attachments.extend(parse_hosted_content_attachments(
                content,
                &chat_external_id,
                message_external_id,
                &mut seen_sources,
            ));
        }

        let incoming = IncomingMessage {
            chat_external_id: Some(chat_external_id.clone()),
            message_external_id: message_external_id.to_owned(),
            raw_json: raw_json.clone(),
            author_external_id,
            author_display_name,
            content,
            created_at,
            modified_at: Some(modified_at),
            source_removed: false,
            deleted,
            deleted_at,
            etag: Some(etag.clone()),
            attachments,
        };
        parsed.insert(
            (
                incoming.chat_external_id.clone(),
                incoming.message_external_id.clone(),
                Some(modified_at_key),
                Some(etag),
                raw_json,
            ),
            incoming,
        );
    }

    Ok(parsed.into_values().collect())
}

fn parse_json_attachments(
    message: &JsonValue,
    chat_external_id: &str,
    message_external_id: &str,
    seen: &mut HashSet<String>,
) -> Result<Vec<AttachmentSource>> {
    let mut attachments = Vec::new();
    let Some(value) = message.get("attachments") else {
        return Ok(attachments);
    };
    if value.is_null() {
        return Ok(attachments);
    }
    let list = value.as_array().ok_or_else(|| {
        anyhow::anyhow!(
            "Teams delta message {message_external_id} has a non-array attachments field"
        )
    })?;
    for attachment in list {
        let source_id = attachment
            .get("id")
            .and_then(JsonValue::as_str)
            .map(str::trim)
            .filter(|source_id| !source_id.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Teams delta message {message_external_id} has an attachment without an id"
                )
            })?;
        if !seen.insert(format!("attachment:{source_id}")) {
            bail!("Teams delta message {message_external_id} repeats attachment id {source_id:?}");
        }

        let source_url = attachment
            .get("contentUrl")
            .and_then(JsonValue::as_str)
            .map(str::to_owned);
        let name = attachment
            .get("name")
            .and_then(JsonValue::as_str)
            .map(str::to_owned);
        let content_type = attachment
            .get("contentType")
            .and_then(JsonValue::as_str)
            .map(str::to_owned);
        let content_bytes = attachment
            .get("contentBytes")
            .and_then(JsonValue::as_str)
            .map(decode_base64)
            .transpose()?;

        attachments.push(AttachmentSource {
            source_kind: "attachment",
            source_id: source_id.to_owned(),
            source_url,
            fetch_required: false,
            name,
            content_type,
            content_bytes,
        });
    }

    let _ = (chat_external_id, message_external_id);
    Ok(attachments)
}

fn parse_hosted_content_attachments(
    content: &str,
    chat_external_id: &str,
    message_external_id: &str,
    seen: &mut HashSet<String>,
) -> Vec<AttachmentSource> {
    let mut attachments = Vec::new();
    for hosted_id in teams_core::extract_hosted_content_ids(content) {
        if !seen.insert(format!("hosted-content:{hosted_id}")) {
            continue;
        }
        let url = format!(
            "https://graph.microsoft.com/v1.0/chats/{chat_external_id}/messages/{message_external_id}/hostedContents/{hosted_id}/$value"
        );
        attachments.push(AttachmentSource {
            source_kind: "hosted-content",
            source_id: hosted_id,
            source_url: Some(url),
            fetch_required: true,
            name: None,
            content_type: None,
            content_bytes: None,
        });
    }
    attachments
}

fn decode_base64(value: &str) -> Result<Vec<u8>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(Vec::new());
    }
    base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|err| anyhow::anyhow!("base64 decode failed: {err:?}"))
}

type CoverageFrontier = teams_core::CoverageFrontier;

/// The one candidate among distinct entities, such as the sole source when no
/// tenant is selected or the logical message a minimal record names. Several
/// candidates are an ambiguity for the caller to resolve, never a choice made
/// here. This is not a check on how many values an attribute holds.
fn sole<T: Ord>(candidates: BTreeSet<T>, what: &str) -> Result<T> {
    let mut candidates = candidates.into_iter();
    match (candidates.next(), candidates.len()) {
        (None, _) => bail!("{what} is missing"),
        (Some(candidate), 0) => Ok(candidate),
        (Some(_), rest) => bail!(
            "{what} has {} values; refusing arbitrary selection",
            rest + 1
        ),
    }
}

/// The materialized file of an attachment occurrence, the smallest should
/// two writers disagree.
fn attachment_file<P>(catalog: &P, attachment: Id) -> Option<Id>
where
    P: TriblePattern,
{
    find!(
        file: Id,
        pattern!(catalog, [{ attachment @ archive::attachment_file: ?file }])
    )
    .min()
}

#[cfg(test)]
fn one_optional<T: Ord>(values: BTreeSet<T>, field: &str) -> Result<Option<T>> {
    match values.len() {
        0 => Ok(None),
        1 => Ok(values.into_iter().next()),
        count => bail!("{field} has {count} values; refusing arbitrary selection"),
    }
}

#[cfg(test)]
fn one_required<T: Ord>(values: BTreeSet<T>, field: &str) -> Result<T> {
    one_optional(values, field)?.ok_or_else(|| anyhow::anyhow!("{field} is missing"))
}

fn inline_u256_to_u128(value: Inline<U256BE>) -> Result<u128> {
    let raw = value.raw;
    if raw[..16].iter().any(|byte| *byte != 0) {
        bail!("Teams coverage generation exceeds u128");
    }
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&raw[16..]);
    Ok(u128::from_be_bytes(bytes))
}

fn coverage_frontier<P>(
    reader: &impl BlobStoreGet,
    catalog: &P,
    source_id: Id,
) -> Result<Option<CoverageFrontier>>
where
    P: TriblePattern,
{
    teams_core::coverage_frontier(reader, catalog, source_id)
}

fn coverage_fragment(
    source_id: Id,
    generation: u128,
    predecessors: impl IntoIterator<Item = Id>,
    request: &str,
    cursor: &str,
    kind: &str,
    observations: impl IntoIterator<Item = Id>,
) -> Result<Fragment> {
    teams_core::coverage_fragment(
        source_id,
        generation,
        predecessors,
        request,
        cursor,
        kind,
        observations,
    )
}

fn build_page_fragment(
    tenant: &str,
    source_id: Id,
    incoming: Vec<IncomingMessage>,
    token: &str,
    known: &[KnownMessage],
) -> Result<(Fragment, BTreeSet<Id>, Vec<KnownMessage>)> {
    let mut fragment = source_fragment(tenant);
    if fragment.root() != Some(source_id) {
        bail!("Teams tenant/source identity changed during one sync");
    }
    let mut events = BTreeSet::new();
    let mut identities = known.iter().cloned().collect::<BTreeSet<_>>();

    // Establish every fully identified logical message first. This lets a
    // minimal tombstone later in the same page resolve without depending on
    // Graph's response order.
    for message in &incoming {
        let Some(chat_external_id) = message.chat_external_id.as_deref() else {
            continue;
        };
        identities.insert(stage_message_identity(
            &mut fragment,
            source_id,
            chat_external_id,
            &message.message_external_id,
        ));
    }

    let mut by_external = BTreeMap::<String, BTreeSet<KnownMessage>>::new();
    for identity in &identities {
        by_external
            .entry(identity.message_external_id.clone())
            .or_default()
            .insert(identity.clone());
    }

    for message in incoming {
        let identity = resolve_message_identity(&message, &identities, &by_external)?;
        // Every page COMMIT is independently inspectable: even a minimal
        // tombstone repeats the complete source/chat/message identity closure.
        let staged = stage_message_identity(
            &mut fragment,
            source_id,
            &identity.chat_external_id,
            &identity.message_external_id,
        );
        if staged.message_id != identity.message_id || staged.chat_id != identity.chat_id {
            bail!("Teams logical message identity changed while staging a page");
        }
        let message_id = identity.message_id;

        if message.source_removed {
            let tombstone = entity! {
                metadata::tag: teams::kind_message_tombstone,
                teams::message: message_id,
            };
            let tombstone_id = tombstone
                .root()
                .expect("Teams message tombstone has one root");
            fragment += tombstone;
            let raw = fragment.put::<UTF8String, _>(message.raw_json);
            fragment += entity! { ExclusiveId::force_ref(&tombstone_id) @
                teams::message_state: "deleted",
                teams::message_raw: raw,
            };
            events.insert(tombstone_id);
            continue;
        }

        let author_id = message
            .author_external_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|external| {
                let external = fragment.put::<UTF8String, _>(external.to_owned());
                let author = entity! {
                    metadata::tag: archive::kind_author,
                    teams::source: source_id,
                    teams::user_id: external,
                };
                let id = author.root().expect("Teams user fragment has one root");
                fragment += author;
                id
            });

        let mut attachment_ids = BTreeSet::new();
        for attachment in &message.attachments {
            let attachment = build_attachment_fragment(message_id, attachment, token)?;
            let id = attachment
                .root()
                .expect("Teams attachment fragment has one root");
            attachment_ids.insert(id);
            fragment += attachment;
        }

        let content = message
            .content
            .as_ref()
            .map(|content| fragment.put::<UTF8String, _>(content.to_owned()));
        let etag = fragment.put::<UTF8String, _>(
            message
                .etag
                .as_ref()
                .expect("full Teams source version has an etag")
                .to_owned(),
        );
        let author_name = message
            .author_display_name
            .as_ref()
            .map(|name| fragment.put::<UTF8String, _>(name.to_owned()));
        let state = if message.deleted {
            "deleted"
        } else {
            "present"
        };
        let observation = entity! {
            metadata::tag: teams::kind_message_observation,
            teams::message: message_id,
            teams::modified_at: message.modified_at.expect("full Teams source version has a timestamp"),
            teams::etag: etag,
        };
        let observation_id = observation
            .root()
            .expect("Teams message observation has one root");
        fragment += observation;
        let raw = fragment.put::<UTF8String, _>(message.raw_json);
        fragment += entity! { ExclusiveId::force_ref(&observation_id) @
            teams::message_state: state,
            metadata::created_at?: message.created_at,
            teams::deleted_at?: message.deleted_at,
            archive::author?: author_id,
            teams::author_name?: author_name,
            archive::content?: content,
            archive::attachment*: attachment_ids,
            teams::message_raw: raw,
        };
        events.insert(observation_id);
    }

    Ok((fragment, events, identities.into_iter().collect()))
}

fn stage_message_identity(
    fragment: &mut Fragment,
    source_id: Id,
    chat_external_id: &str,
    message_external_id: &str,
) -> KnownMessage {
    let chat_external = fragment.put::<UTF8String, _>(chat_external_id.to_owned());
    let chat = entity! {
        metadata::tag: teams::kind_chat,
        teams::source: source_id,
        teams::chat_id: chat_external,
    };
    let chat_id = chat.root().expect("Teams chat fragment has one root");
    *fragment += chat;

    let message_external = fragment.put::<UTF8String, _>(message_external_id.to_owned());
    let logical = entity! {
        metadata::tag: archive::kind_message,
        teams::chat: chat_id,
        teams::message_id: message_external,
    };
    let message_id = logical.root().expect("Teams message fragment has one root");
    *fragment += logical;

    KnownMessage {
        message_id,
        message_external_id: message_external_id.to_owned(),
        chat_id,
        chat_external_id: chat_external_id.to_owned(),
    }
}

fn resolve_message_identity(
    message: &IncomingMessage,
    identities: &BTreeSet<KnownMessage>,
    by_external: &BTreeMap<String, BTreeSet<KnownMessage>>,
) -> Result<KnownMessage> {
    if let Some(chat_external_id) = message.chat_external_id.as_deref() {
        return sole(
            identities
                .iter()
                .filter(|known| {
                    known.chat_external_id == chat_external_id
                        && known.message_external_id == message.message_external_id
                })
                .cloned()
                .collect(),
            &format!(
                "Teams logical message identity for chat {chat_external_id:?}, message {:?}",
                message.message_external_id
            ),
        );
    }

    sole(
        by_external
            .get(&message.message_external_id)
            .cloned()
            .unwrap_or_default(),
        &format!(
            "source-local Teams message id {:?} needed by minimal @removed record",
            message.message_external_id
        ),
    )
}

fn build_attachment_fragment(
    message_id: Id,
    source: &AttachmentSource,
    token: &str,
) -> Result<Fragment> {
    let source_id = source.source_id.trim();
    if source_id.is_empty() {
        bail!("Teams attachment has an empty source id");
    }
    let mut fragment = Fragment::empty();
    let source_handle = fragment.put::<UTF8String, _>(source_id.to_owned());
    let name = source
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(|name| fragment.put::<UTF8String, _>(name.to_owned()));
    let source_pointer = source
        .source_url
        .as_ref()
        .map(|url| fragment.put::<UTF8String, _>(url.to_owned()));

    let mut content_type = source.content_type.clone();
    let bytes = match source.content_bytes.clone() {
        Some(bytes) => Some(bytes),
        None if source.fetch_required => {
            let url = source.source_url.as_deref().ok_or_else(|| {
                anyhow::anyhow!("required Teams attachment {source_id} has no fetch URL")
            })?;
            let (bytes, fetched_type) = fetch_attachment_bytes(token, url)?;
            if content_type.is_none() {
                content_type = fetched_type;
            }
            Some(bytes)
        }
        None => None,
    };
    let (file_id, size) = if let Some(bytes) = bytes {
        let size: Inline<U256BE> = (bytes.len() as u128).to_inline();
        let file_name = source
            .name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(source_id);
        let media_type = file_capability::normalize_media_type_or_default(
            content_type
                .as_deref()
                .unwrap_or("application/octet-stream"),
        );
        let file = file_capability::stage(bytes, file_name, &media_type)?;
        let file_id = file.root().expect("canonical file fragment has one root");
        let (_, file_facts, file_metafacts, file_blobs) = file.into_parts();
        fragment += Fragment::from_parts(file_facts, file_metafacts, file_blobs);
        (Some(file_id), Some(size))
    } else {
        (None, None)
    };

    // The source occurrence is stable evidence. Materialized bytes are an
    // additive DERIVE of that evidence, not part of its identity: ordinary
    // Graph attachments often arrive pointer-only and may be fetched by a
    // separate process later without minting a second occurrence entity.
    let attachment = entity! {
        metadata::tag: archive::kind_attachment,
        archive::attachment_source_id: source_handle,
        teams::attachment_message: message_id,
        teams::attachment_kind: source.source_kind,
        archive::attachment_name?: name,
    };
    let attachment_id = attachment
        .root()
        .expect("Teams attachment fragment has one root");
    fragment += attachment;
    fragment += entity! { ExclusiveId::force_ref(&attachment_id) @
        archive::attachment_source_pointer?: source_pointer,
        archive::attachment_file?: file_id,
        archive::attachment_size_bytes?: size,
    };
    Ok(fragment)
}

fn fetch_attachment_bytes(token: &str, url: &str) -> Result<(Vec<u8>, Option<String>)> {
    let client = Client::new();
    let safe_url = url_without_query(url);
    let resp = client
        .get(url)
        .bearer_auth(token)
        .send()
        .map_err(|err| anyhow::anyhow!("GET {safe_url}: {}", err.without_url()))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("GET {} failed: status={status}", url_without_query(url));
    }
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| value.to_string());
    let bytes = resp
        .bytes()
        .map_err(|err| anyhow::anyhow!("read attachment bytes: {}", err.without_url()))?;
    Ok((bytes.to_vec(), content_type))
}

fn epoch_interval(epoch: Epoch) -> Inline<NsTAIInterval> {
    (epoch, epoch).try_to_inline().unwrap()
}

fn interval_key(interval: Inline<NsTAIInterval>) -> i128 {
    let (lower, _): (Epoch, Epoch) = interval.try_from_inline().unwrap();
    lower.to_tai_duration().total_nanoseconds()
}

pub(super) fn format_interval(interval: Inline<NsTAIInterval>) -> String {
    let (lower, _): (Epoch, Epoch) = interval.try_from_inline().unwrap();
    lower.to_gregorian_str(TimeScale::UTC)
}

pub(super) fn parse_since_key(value: Option<&str>) -> Result<Option<i128>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    let epoch = Epoch::from_gregorian_str(value)
        .ok()
        .or_else(|| teams_core::parse_graph_datetime(value))
        .ok_or_else(|| anyhow::anyhow!("invalid timestamp: {}", value))?;
    Ok(Some(interval_key(epoch_interval(epoch))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use triblespace::core::repo::StoreSnapshot;

    static NEXT_FIXTURE: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn oauth_error_kind_exposes_only_a_bounded_machine_kind() {
        let described = r#"{"error":"invalid_grant","error_description":"contains-secret"}"#;
        assert_eq!(oauth_error_kind(described), "invalid_grant");
        assert!(!oauth_error_kind(described).contains("contains-secret"));
        assert_eq!(
            oauth_error_kind(r#"{"error":"secret value with spaces"}"#),
            "unknown"
        );
        assert_eq!(oauth_error_kind("contains-secret"), "unknown");
    }

    struct Fixture {
        dir: PathBuf,
        pile: PathBuf,
        key: PathBuf,
    }

    impl Fixture {
        fn new() -> Self {
            let sequence = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "faculties-teams-collection-{}-{sequence}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            let pile = dir.join("test.pile");
            fs::File::create(&pile).unwrap();
            let key = dir.join("test.key");
            initialize_signer(&pile, Some(&key)).unwrap();
            Self { dir, pile, key }
        }

        fn storage(&self) -> TeamsStorage {
            TeamsStorage {
                storage: Storage::new(self.pile.clone(), Some(self.key.clone())),
            }
        }

        fn publish(&self, fragment: Fragment) {
            self.storage().publish(fragment, "test Teams page").unwrap();
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn graph_message(
        chat: &str,
        message: &str,
        created: &str,
        modified: &str,
        content: &str,
    ) -> JsonValue {
        json!({
            "chatId": chat,
            "id": message,
            "createdDateTime": created,
            "lastModifiedDateTime": modified,
            "etag": format!("{message}:{modified}:{content}"),
            "from": { "user": { "id": "user-1", "displayName": "Tester" } },
            "body": { "content": content },
            "attachments": [],
        })
    }

    fn page_fragment(
        tenant: &str,
        messages: Vec<JsonValue>,
        generation: u128,
        predecessors: impl IntoIterator<Item = Id>,
        cursor: &str,
    ) -> (Fragment, Id) {
        page_fragment_with_known(tenant, messages, generation, predecessors, cursor, &[])
    }

    fn page_fragment_with_known(
        tenant: &str,
        messages: Vec<JsonValue>,
        generation: u128,
        predecessors: impl IntoIterator<Item = Id>,
        cursor: &str,
        known: &[KnownMessage],
    ) -> (Fragment, Id) {
        let source = source_fragment(tenant);
        let source_id = source.root().unwrap();
        let incoming = parse_messages(messages).unwrap();
        let (mut fragment, observations, _) =
            build_page_fragment(tenant, source_id, incoming, "test-token", known).unwrap();
        let receipt = coverage_fragment(
            source_id,
            generation,
            predecessors,
            "https://graph.example/request",
            cursor,
            "delta",
            observations,
        )
        .unwrap();
        let receipt_id = receipt.root().unwrap();
        fragment += receipt;
        (fragment, receipt_id)
    }

    fn load_view(fixture: &Fixture) -> CollectionView {
        fixture.storage().view().unwrap()
    }

    fn assert_session_has_one_store_watermark(session: &TeamsSession) {
        let secrets_reader = session.secrets.store_snapshot();
        assert!(session.reader.changes_since(secrets_reader).is_empty());
        assert!(secrets_reader.changes_since(&session.reader).is_empty());
    }

    fn initialize_test_secrets(fixture: &Fixture) -> (Id, Id) {
        let signer = load_signer(&fixture.pile, Some(&fixture.key)).unwrap();
        let mut pile = open_pile_strict(&fixture.pile).unwrap();
        let collection = open_secrets_collection(&mut pile, signer.verifying_key()).unwrap();
        let client_id = secret_storage::add_secret(
            &mut pile,
            &signer,
            collection,
            "teams/client-secret/test",
            b"distinct-test-client-secret",
            clock::point_now().unwrap(),
        )
        .unwrap();
        let token_bundle = DelegatedTokenBundle {
            access_token: "distinct-test-access-token".to_owned(),
            refresh_token: Some("distinct-test-refresh-token".to_owned()),
            expires_at_unix: now_epoch_secs().unwrap() + 3600,
            token_type: Some("Bearer".to_owned()),
            scope: Some("Chat.ReadWrite offline_access".to_owned()),
        };
        let token_id = secret_storage::add_secret(
            &mut pile,
            &signer,
            collection,
            "teams/delegated-token/test",
            &serde_json::to_vec(&token_bundle).unwrap(),
            clock::point_now().unwrap(),
        )
        .unwrap();
        pile.close().unwrap();
        (client_id, token_id)
    }

    #[test]
    fn auth_profile_persists_only_exact_encrypted_secrets_references() {
        let fixture = Fixture::new();
        let (client_secret, delegated_token) = initialize_test_secrets(&fixture);
        let tenant = "tenant.example";
        let source = source_fragment(tenant).root().unwrap();
        fixture
            .storage()
            .with_session(|session| {
                let mut fragment = source_fragment(tenant);
                let (profile, profile_id) = teams_core::auth_profile_fragment(
                    source,
                    "client-id",
                    "user-id",
                    "offline_access Chat.ReadWrite",
                    Some(client_secret),
                    Some(delegated_token),
                    [],
                )?;
                fragment += profile;
                session.commit(fragment, "test Teams auth profile")?;
                assert_session_has_one_store_watermark(session);
                assert_eq!(
                    teams_core::auth_profile_head(&session.facts, source),
                    teams_core::AuthProfileHead::Unique(profile_id)
                );
                assert!(session.secrets.contains(client_secret));
                let opened_client = session.secrets.open(client_secret, &session.signer)?;
                assert_eq!(opened_client, b"distinct-test-client-secret");
                Ok(())
            })
            .unwrap();

        let bytes = fs::read(&fixture.pile).unwrap();
        for plaintext in [
            b"distinct-test-client-secret".as_slice(),
            b"distinct-test-access-token".as_slice(),
            b"distinct-test-refresh-token".as_slice(),
        ] {
            assert!(!bytes
                .windows(plaintext.len())
                .any(|window| window == plaintext));
        }
    }

    #[test]
    fn session_secret_creation_targets_the_configured_collection_and_refreshes() {
        let fixture = Fixture::new();
        initialize_test_secrets(&fixture);
        fixture
            .storage()
            .with_session(|session| {
                assert_session_has_one_store_watermark(session);
                let support = session.support.clone();
                let later = session.storage.with_pile(|pile, signer| {
                    pile.commit(
                        session.collection,
                        signer,
                        source_fragment("newer-session-input.example"),
                    )?;
                    crate::storage::carry_facts(pile, session.collection, signer);
                    pile.snapshot().map_err(Into::into)
                })?;
                assert_ne!(later.attached(session.rank9)?.support(), &support);
                drop(later);
                let observed_at = clock::point_now()?;
                let secret =
                    session.add_secret("teams/session-test", b"configured", observed_at)?;
                assert_session_has_one_store_watermark(session);
                assert_eq!(session.support, support);
                assert_eq!(
                    session.secrets.open(secret, &session.signer)?,
                    b"configured"
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn unknown_auth_secret_reference_is_rejected_before_collection_commit() {
        let fixture = Fixture::new();
        let source_identity = source_fragment("tenant.example");
        let source = source_identity.root().unwrap();
        let (profile, _) = teams_core::auth_profile_fragment(
            source,
            "client-id",
            "user-id",
            "offline_access",
            None,
            Some(Id::new([0xD7; 16]).unwrap()),
            [],
        )
        .unwrap();
        let mut fragment = source_identity;
        fragment += profile;
        let error = fixture
            .storage()
            .publish(fragment, "dangling auth ref")
            .unwrap_err();
        assert!(error.to_string().contains("unknown delegated token bundle"));

        // Registering a collection may append its descriptor closure, but a
        // rejected publication must not make any collection commit visible.
        let mut pile = open_pile_strict(&fixture.pile).unwrap();
        let store_snapshot = pile.snapshot().unwrap();
        let records = crate::storage::discovered_records(&store_snapshot).unwrap();
        assert!(records.commits().is_empty());
    }

    #[test]
    fn auth_set_can_repair_a_migrated_dangling_profile() {
        let fixture = Fixture::new();
        let (client_secret, delegated_token) = initialize_test_secrets(&fixture);
        let tenant = "tenant.example";
        let source_identity = source_fragment(tenant);
        let source = source_identity.root().unwrap();
        let missing = Id::new([0xD8; 16]).unwrap();
        let (dangling, dangling_id) = teams_core::auth_profile_fragment(
            source,
            "client-id",
            "user-id",
            "offline_access",
            Some(missing),
            Some(missing),
            [],
        )
        .unwrap();
        let mut historical = source_identity;
        historical += dangling;

        // Model an additive cutover that retained Teams provenance while the
        // old secret representation itself was deliberately left behind.
        let signer = load_signer(&fixture.pile, Some(&fixture.key)).unwrap();
        let mut pile = open_pile_strict(&fixture.pile).unwrap();
        let collection =
            open_configured(&mut pile, DEFAULT_SCOPE_ID, signer.verifying_key()).unwrap();
        pile.commit(collection, &signer, historical).unwrap();
        pile.close().unwrap();

        fixture
            .storage()
            .with_session(|session| {
                set_auth_profile(
                    session,
                    tenant,
                    "client-id",
                    "user-id",
                    "offline_access",
                    Some(client_secret),
                    Some(delegated_token),
                )?;
                let head = match teams_core::auth_profile_head(&session.facts, source) {
                    teams_core::AuthProfileHead::Unique(head) => head,
                    other => panic!("expected one repaired auth head, got {other:?}"),
                };
                assert_ne!(head, dangling_id);
                let repaired = teams_core::auth_profile(&session.facts, head)?;
                assert_eq!(repaired.client_secret_version, Some(client_secret));
                assert_eq!(repaired.delegated_token_version, Some(delegated_token));
                teams_core::validate_auth_secret_references(&session.facts, &session.secrets)?;

                set_auth_profile(
                    session,
                    tenant,
                    "client-id",
                    "user-id",
                    "offline_access",
                    Some(client_secret),
                    Some(delegated_token),
                )?;
                assert_eq!(
                    teams_core::auth_profile_head(&session.facts, source),
                    teams_core::AuthProfileHead::Unique(head)
                );
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn source_chat_user_and_message_ids_are_tenant_scoped() {
        let a_source = source_fragment("tenant-a").root().unwrap();
        let b_source = source_fragment("tenant-b").root().unwrap();
        assert_eq!(a_source, source_fragment(" TENANT-A ").root().unwrap());
        assert_ne!(a_source, b_source);

        let a = build_page_fragment(
            "tenant-a",
            a_source,
            parse_messages(vec![graph_message(
                "chat",
                "message",
                "2026-08-01T10:00:00Z",
                "2026-08-01T10:00:00Z",
                "A",
            )])
            .unwrap(),
            "token",
            &[],
        )
        .unwrap()
        .0;
        let b = build_page_fragment(
            "tenant-b",
            b_source,
            parse_messages(vec![graph_message(
                "chat",
                "message",
                "2026-08-01T10:00:00Z",
                "2026-08-01T10:00:00Z",
                "A",
            )])
            .unwrap(),
            "token",
            &[],
        )
        .unwrap()
        .0;
        let a_chat = find!(
            chat: Id,
            pattern!(&a, [{ ?chat @ metadata::tag: teams::kind_chat }])
        )
        .collect::<BTreeSet<_>>();
        let b_chat = find!(
            chat: Id,
            pattern!(&b, [{ ?chat @ metadata::tag: teams::kind_chat }])
        )
        .collect::<BTreeSet<_>>();
        assert!(a_chat.is_disjoint(&b_chat));
        let a_messages = find!(
            message: Id,
            pattern!(&a, [{ ?message @ metadata::tag: archive::kind_message }])
        )
        .collect::<BTreeSet<_>>();
        let b_messages = find!(
            message: Id,
            pattern!(&b, [{ ?message @ metadata::tag: archive::kind_message }])
        )
        .collect::<BTreeSet<_>>();
        assert!(a_messages.is_disjoint(&b_messages));
    }

    #[test]
    fn edits_and_deletes_are_immutable_max_time_observations() {
        let fixture = Fixture::new();
        let (first, first_receipt) = page_fragment(
            "tenant-a",
            vec![graph_message(
                "chat",
                "message",
                "2026-08-01T10:00:00Z",
                "2026-08-01T10:00:00Z",
                "first",
            )],
            1,
            [],
            "https://graph.example/delta-1",
        );
        fixture.publish(first);
        let (edited, edited_receipt) = page_fragment(
            "tenant-a",
            vec![graph_message(
                "chat",
                "message",
                "2026-08-01T10:00:00Z",
                "2026-08-01T11:00:00Z",
                "edited",
            )],
            2,
            [first_receipt],
            "https://graph.example/delta-2",
        );
        fixture.publish(edited);
        let view = load_view(&fixture);
        let source = source_fragment("tenant-a").root().unwrap();
        assert_eq!(read_all(&fixture.storage()), vec![one("edited")]);
        assert_eq!(
            coverage_frontier(&view.reader, &view.facts, source)
                .unwrap()
                .unwrap()
                .heads,
            BTreeSet::from([edited_receipt])
        );

        let mut deleted = graph_message(
            "chat",
            "message",
            "2026-08-01T10:00:00Z",
            "2026-08-01T12:00:00Z",
            "",
        );
        deleted["deletedDateTime"] = json!("2026-08-01T12:00:00Z");
        deleted["body"] = JsonValue::Null;
        let (tombstone, _) = page_fragment(
            "tenant-a",
            vec![deleted],
            3,
            [edited_receipt],
            "https://graph.example/delta-3",
        );
        fixture.publish(tombstone);
        let view = load_view(&fixture);
        let current = current_messages(&view.facts, source).unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].live_since(), None);
        assert!(current[0]
            .versions
            .iter()
            .flatten()
            .all(|version| version.deleted));
        assert!(read_all(&fixture.storage()).is_empty());
    }

    #[test]
    fn minimal_removed_is_causal_reversible_and_old_replay_cannot_restore() {
        let fixture = Fixture::new();
        let original = graph_message(
            "chat",
            "message",
            "2026-08-01T10:00:00Z",
            "2026-08-01T10:00:00Z",
            "original",
        );
        let (first, first_receipt) = page_fragment(
            "tenant-a",
            vec![original.clone()],
            1,
            [],
            "https://graph.example/delta-1",
        );
        fixture.publish(first);
        let first_view = load_view(&fixture);
        let source = source_fragment("tenant-a").root().unwrap();
        let known = load_known_messages(&first_view.reader, &first_view.facts, source).unwrap();

        let removed = json!({
            "id": "message",
            "@removed": { "reason": "deleted" }
        });
        let (tombstone, tombstone_receipt) = page_fragment_with_known(
            "tenant-a",
            vec![removed],
            2,
            [first_receipt],
            "https://graph.example/delta-2",
            &known,
        );
        fixture.publish(tombstone);
        assert!(read_all(&fixture.storage()).is_empty());

        // A cursor reset can replay the exact pre-deletion source version in
        // a descendant receipt. Receipt recency alone must not resurrect it.
        let (replay, replay_receipt) = page_fragment(
            "tenant-a",
            vec![original],
            3,
            [tombstone_receipt],
            "https://graph.example/delta-3",
        );
        fixture.publish(replay);
        assert!(read_all(&fixture.storage()).is_empty());

        let (restore, _) = page_fragment(
            "tenant-a",
            vec![graph_message(
                "chat",
                "message",
                "2026-08-01T10:00:00Z",
                "2026-08-01T11:00:00Z",
                "restored",
            )],
            4,
            [replay_receipt],
            "https://graph.example/delta-4",
        );
        fixture.publish(restore);
        assert_eq!(read_all(&fixture.storage()), vec![one("restored")]);
    }

    #[test]
    fn minimal_removed_requires_unique_source_local_message_resolution() {
        let source = source_fragment("tenant-a").root().unwrap();
        let removed = parse_messages(vec![json!({
            "id": "message",
            "@removed": { "reason": "deleted" }
        })])
        .unwrap();
        assert!(
            build_page_fragment("tenant-a", source, removed.clone(), "token", &[])
                .unwrap_err()
                .to_string()
                .contains("is missing")
        );

        let mut scratch = source_fragment("tenant-a");
        let left = stage_message_identity(&mut scratch, source, "chat-left", "message");
        let right = stage_message_identity(&mut scratch, source, "chat-right", "message");
        let error =
            build_page_fragment("tenant-a", source, removed, "token", &[left, right]).unwrap_err();
        assert!(error.to_string().contains("has 2 values"));
    }

    #[test]
    fn graph_source_versions_require_modified_time_and_etag() {
        let mut missing_modified = graph_message(
            "chat",
            "message",
            "2026-08-01T10:00:00Z",
            "2026-08-01T10:00:00Z",
            "content",
        );
        missing_modified
            .as_object_mut()
            .unwrap()
            .remove("lastModifiedDateTime");
        assert!(parse_messages(vec![missing_modified])
            .unwrap_err()
            .to_string()
            .contains("lastModifiedDateTime"));

        let mut missing_etag = graph_message(
            "chat",
            "message",
            "2026-08-01T10:00:00Z",
            "2026-08-01T10:00:00Z",
            "content",
        );
        missing_etag.as_object_mut().unwrap().remove("etag");
        assert!(parse_messages(vec![missing_etag])
            .unwrap_err()
            .to_string()
            .contains("etag"));
    }
    #[test]
    fn repeated_partial_payload_for_one_source_version_converges() {
        let fixture = Fixture::new();
        let mut first = graph_message(
            "chat",
            "message",
            "2026-08-01T10:00:00Z",
            "2026-08-01T10:00:00Z",
            "content",
        );
        first["etag"] = json!("stable-etag");
        first["attachments"] = json!([{
            "id": "attachment-1",
            "name": "note.txt",
            "contentUrl": "https://graph.example/first",
        }]);
        let (first_page, first_receipt) = page_fragment(
            "tenant-a",
            vec![first],
            1,
            [],
            "https://graph.example/delta-1",
        );
        fixture.publish(first_page);

        let mut second = graph_message(
            "chat",
            "message",
            "2026-08-01T10:00:00Z",
            "2026-08-01T10:00:00Z",
            "content",
        );
        second["etag"] = json!("stable-etag");
        second["from"]["user"]["displayName"] = json!("Renamed Tester");
        second["attachments"] = json!([{
            "id": "attachment-1",
            "name": "note.txt",
            "contentUrl": "https://graph.example/second",
            "contentType": "text/plain",
            "contentBytes": base64::engine::general_purpose::STANDARD.encode(b"hello"),
        }]);
        let (second_page, _) = page_fragment(
            "tenant-a",
            vec![second],
            2,
            [first_receipt],
            "https://graph.example/delta-2",
        );
        fixture.publish(second_page);

        let view = load_view(&fixture);
        let observations = find!(
            value: Id,
            pattern!(&view.facts, [{
                ?value @ metadata::tag: teams::kind_message_observation
            }])
        )
        .collect::<BTreeSet<_>>();
        assert_eq!(observations.len(), 1);
        let observation = *observations.first().unwrap();
        assert_eq!(
            find!(
                value: Inline<Handle<UTF8String>>,
                pattern!(&view.facts, [{ observation @ teams::author_name: ?value }])
            )
            .collect::<BTreeSet<_>>()
            .len(),
            2
        );
        let attachment = one_required(
            find!(
                value: Id,
                pattern!(&view.facts, [{ observation @ archive::attachment: ?value }])
            )
            .collect(),
            "test attachment",
        )
        .unwrap();
        assert_eq!(
            find!(
                value: Inline<Handle<UTF8String>>,
                pattern!(&view.facts, [{ attachment @ archive::attachment_source_pointer: ?value }])
            )
            .collect::<BTreeSet<_>>()
            .len(),
            2
        );
        assert_eq!(
            find!(
                value: Id,
                pattern!(&view.facts, [{ attachment @ archive::attachment_file: ?value }])
            )
            .collect::<BTreeSet<_>>()
            .len(),
            1
        );
    }

    #[test]
    fn pointer_only_attachment_accepts_later_file_materialization() {
        let fixture = Fixture::new();
        let mut message = graph_message(
            "chat",
            "message",
            "2026-08-01T10:00:00Z",
            "2026-08-01T10:00:00Z",
            "content",
        );
        message["attachments"] = json!([{
            "id": "attachment-1",
            "name": "note.txt",
            "contentUrl": "https://graph.example/content",
        }]);
        let (page, _) = page_fragment(
            "tenant-a",
            vec![message],
            1,
            [],
            "https://graph.example/delta",
        );
        fixture.publish(page);
        let view = load_view(&fixture);
        let attachment = one_required(
            find!(
                value: Id,
                pattern!(&view.facts, [{
                    ?value @ metadata::tag: archive::kind_attachment
                }])
            )
            .collect(),
            "test attachment",
        )
        .unwrap();

        let mut derived =
            file_capability::stage(b"hello".to_vec(), "note.txt", "text/plain").unwrap();
        let file_id = derived.root().unwrap();
        let size: Inline<U256BE> = 5_u128.to_inline();
        derived += entity! { ExclusiveId::force_ref(&attachment) @
            archive::attachment_file: file_id,
            archive::attachment_size_bytes: size,
        };
        fixture
            .storage()
            .publish(derived, "materialize attachment")
            .unwrap();

        let materialized = load_view(&fixture);
        assert_eq!(
            one_required(
                find!(
                    value: Id,
                    pattern!(&materialized.facts, [{ attachment @ archive::attachment_file: ?value }])
                )
                .collect(),
                "materialized attachment file",
            )
            .unwrap(),
            file_id
        );
    }

    /// A file that two writers named differently is still listed: the
    /// smallest name is shown, exactly as the smallest file and size are,
    /// and the listing never refuses because of one annotated file.
    #[test]
    fn an_attachment_file_with_two_names_is_still_listed() {
        let fixture = Fixture::new();
        let mut message = graph_message(
            "chat",
            "message",
            "2026-08-01T10:00:00Z",
            "2026-08-01T10:00:00Z",
            "content",
        );
        message["attachments"] = json!([{
            "id": "attachment-1",
            "contentUrl": "https://graph.example/content",
        }]);
        let (page, _) = page_fragment(
            "tenant-a",
            vec![message],
            1,
            [],
            "https://graph.example/delta",
        );
        fixture.publish(page);
        let view = load_view(&fixture);
        let attachment = one_required(
            find!(
                value: Id,
                pattern!(&view.facts, [{
                    ?value @ metadata::tag: archive::kind_attachment
                }])
            )
            .collect(),
            "test attachment",
        )
        .unwrap();
        let mut derived =
            file_capability::stage(b"hello".to_vec(), "note.txt", "text/plain").unwrap();
        let file_id = derived.root().unwrap();
        derived += entity! { ExclusiveId::force_ref(&attachment) @
            archive::attachment_file: file_id,
        };
        // A second writer's name for the same file entity.
        derived += entity! { ExclusiveId::force_ref(&file_id) @
            crate::schemas::files::file::name: "a-renamed-note.txt",
        };
        fixture
            .storage()
            .publish(derived, "materialize attachment twice named")
            .unwrap();

        let view = load_view(&fixture);
        let source = source_fragment("tenant-a").root().unwrap();
        let rows = attachment_rows(&view.reader, &view.facts, source, None, None).unwrap();
        assert_eq!(rows.len(), 1);
        let names: BTreeSet<String> = find!(
            handle: Inline<Handle<UTF8String>>,
            pattern!(&view.facts, [{ file_id @ crate::schemas::files::file::name: ?handle }])
        )
        .map(|handle| read_utf8string(&view.reader, handle, "test name").unwrap())
        .collect();
        assert_eq!(names.len(), 2);
        let shown = read_utf8string(&view.reader, rows[0].name.unwrap(), "shown name").unwrap();
        let smallest = find!(
            handle: Inline<Handle<UTF8String>>,
            pattern!(&view.facts, [{ file_id @ crate::schemas::files::file::name: ?handle }])
        )
        .min()
        .unwrap();
        assert_eq!(rows[0].name, Some(smallest));
        assert!(names.contains(&shown));

        // The listing says that the shown name has an alternative, and which.
        let other = names.iter().find(|name| **name != shown).unwrap().clone();
        let info = attachment_info(
            &view.reader,
            "chat".to_owned(),
            "message".to_owned(),
            rows.into_iter().next().unwrap(),
        )
        .unwrap();
        let mut rendered = String::new();
        crate::teams::render::attachments(
            &[info],
            &mut crate::out::Out::new(&mut |part| {
                if let crate::out::Part::Text { text } = part {
                    rendered.push_str(&text);
                }
                Ok(())
            }),
        )
        .unwrap();
        assert!(
            rendered.contains(&format!("name={shown} (+1 other name: {other})")),
            "{rendered}"
        );
    }

    /// The whole archive as a reader sees it: the text, or `None` where a
    /// version is a deletion, and which of how many versions it is.
    fn read_all(storage: &TeamsStorage) -> Vec<(Option<String>, usize, usize)> {
        archived(storage)
            .into_iter()
            .map(|row| (row.content, row.version_index, row.version_count))
            .collect()
    }

    fn archived(storage: &TeamsStorage) -> Vec<ArchivedMessage> {
        let source = source_fragment("tenant-a").root().unwrap();
        storage
            .with_session(|session| {
                read_messages(
                    source,
                    session,
                    ReadOptions {
                        chat_id: None,
                        since: None,
                        limit: 0,
                        descending: false,
                    },
                )
            })
            .unwrap()
    }

    /// The shared presentation records, in the same terms.
    fn presented(storage: &TeamsStorage) -> Vec<(Option<String>, usize, usize)> {
        let source = source_fragment("tenant-a").root().unwrap();
        let view = storage.view().unwrap();
        teams_core::current_messages(&view.facts, source)
            .unwrap()
            .into_iter()
            .map(|record| {
                let content = record
                    .content
                    .filter(|_| !record.deleted)
                    .map(|handle| read_utf8string(&view.reader, handle, "test content").unwrap());
                (content, record.version_index, record.version_count)
            })
            .collect()
    }

    fn one(content: &str) -> (Option<String>, usize, usize) {
        (Some(content.to_owned()), 0, 1)
    }

    fn sorted(
        mut rows: Vec<(Option<String>, usize, usize)>,
    ) -> Vec<(Option<String>, usize, usize)> {
        rows.sort();
        rows
    }

    /// The texts of several versions of one message, whichever comes first.
    fn texts(rows: &[(Option<String>, usize, usize)]) -> BTreeSet<Option<String>> {
        rows.iter().map(|(content, _, _)| content.clone()).collect()
    }

    fn some(text: &str) -> Option<String> {
        Some(text.to_owned())
    }

    #[test]
    fn two_versions_of_one_time_are_both_stored_and_shown() {
        let fixture = Fixture::new();
        let (first, first_receipt) = page_fragment(
            "tenant-a",
            vec![graph_message(
                "chat",
                "message",
                "2026-08-01T10:00:00Z",
                "2026-08-01T11:00:00Z",
                "left",
            )],
            1,
            [],
            "https://graph.example/delta-1",
        );
        fixture.publish(first);
        let (tie, tie_receipt) = page_fragment(
            "tenant-a",
            vec![
                graph_message(
                    "chat",
                    "message",
                    "2026-08-01T10:00:00Z",
                    "2026-08-01T11:00:00Z",
                    "right",
                ),
                graph_message(
                    "chat",
                    "other",
                    "2026-08-01T12:00:00Z",
                    "2026-08-01T12:00:00Z",
                    "other",
                ),
            ],
            2,
            [first_receipt],
            "https://graph.example/delta-2",
        );
        fixture.publish(tie);

        let rows = read_all(&fixture.storage());
        assert_eq!(rows.len(), 3);
        assert_eq!(
            texts(&rows[..2]),
            BTreeSet::from([some("left"), some("right")])
        );
        assert_eq!((rows[0].1, rows[0].2), (0, 2));
        assert_eq!((rows[1].1, rows[1].2), (1, 2));
        assert_eq!(rows[2], one("other"));

        assert_eq!(presented(&fixture.storage()), rows);

        // Both are said to conflict, with their times and authors.
        let mut rendered = String::new();
        crate::teams::render::messages(
            &archived(&fixture.storage()),
            &mut crate::out::Out::new(&mut |part| {
                if let crate::out::Part::Text { text } = part {
                    rendered.push_str(&text);
                }
                Ok(())
            }),
        )
        .unwrap();
        let lines = rendered.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 3, "{rendered}");
        for (line, index) in lines[..2].iter().zip(1..) {
            assert!(
                line.contains(&format!("[CONFLICTING {index}/2 of message ")),
                "{line}"
            );
            assert!(line.contains("modified 2026-08-01T11:00:00"), "{line}");
            assert!(line.contains("Tester: "), "{line}");
        }
        assert!(!lines[2].contains("CONFLICTING"), "{rendered}");

        // A conflict is one message to a limit, never half of one.
        let source = source_fragment("tenant-a").root().unwrap();
        let limited = fixture
            .storage()
            .with_session(|session| {
                read_messages(
                    source,
                    session,
                    ReadOptions {
                        chat_id: None,
                        since: None,
                        limit: 2,
                        descending: true,
                    },
                )
            })
            .unwrap();
        assert_eq!(limited.len(), 3);

        // The source's next version settles it.
        let (later, _) = page_fragment(
            "tenant-a",
            vec![graph_message(
                "chat",
                "message",
                "2026-08-01T10:00:00Z",
                "2026-08-01T13:00:00Z",
                "settled",
            )],
            3,
            [tie_receipt],
            "https://graph.example/delta-3",
        );
        fixture.publish(later);
        assert_eq!(
            read_all(&fixture.storage()),
            vec![one("settled"), one("other")]
        );
    }

    /// The source may name one version anew; what it says is unchanged.
    #[test]
    fn versions_differing_only_in_their_etag_read_as_one() {
        let fixture = Fixture::new();
        let version = |etag: &str| {
            let mut message = graph_message(
                "chat",
                "message",
                "2026-08-01T10:00:00Z",
                "2026-08-01T11:00:00Z",
                "same",
            );
            message["etag"] = json!(etag);
            message
        };
        let (first, receipt) = page_fragment(
            "tenant-a",
            vec![version("one")],
            1,
            [],
            "https://graph.example/delta-1",
        );
        fixture.publish(first);
        let (second, _) = page_fragment(
            "tenant-a",
            vec![version("two")],
            2,
            [receipt],
            "https://graph.example/delta-2",
        );
        fixture.publish(second);
        assert_eq!(read_all(&fixture.storage()), vec![one("same")]);
        let view = load_view(&fixture);
        let source = source_fragment("tenant-a").root().unwrap();
        assert_eq!(
            teams_core::current_message_states(&view.facts, source)
                .into_values()
                .map(|versions| versions.len())
                .collect::<Vec<_>>(),
            vec![2]
        );
        drop(view);
        assert_eq!(presented(&fixture.storage()), vec![one("same")]);
    }

    #[test]
    fn one_page_may_bring_unordered_versions_and_a_removal() {
        let fixture = Fixture::new();
        let version = |content| {
            graph_message(
                "chat",
                "message",
                "2026-08-01T10:00:00Z",
                "2026-08-01T11:00:00Z",
                content,
            )
        };
        let (page, receipt) = page_fragment(
            "tenant-a",
            vec![version("left"), version("right")],
            1,
            [],
            "https://graph.example/delta-1",
        );
        fixture.publish(page);
        assert_eq!(
            texts(&read_all(&fixture.storage())),
            BTreeSet::from([some("left"), some("right")])
        );

        let mut newer = version("newer");
        newer["lastModifiedDateTime"] = json!("2026-08-01T12:00:00Z");
        let (page, _) = page_fragment(
            "tenant-a",
            vec![
                newer,
                json!({ "id": "message", "@removed": { "reason": "deleted" } }),
            ],
            2,
            [receipt],
            "https://graph.example/delta-2",
        );
        fixture.publish(page);
        let versions = vec![(some("newer"), 0, 2), (None, 1, 2)];
        assert_eq!(read_all(&fixture.storage()), versions);
        assert_eq!(
            presented(&fixture.storage())
                .into_iter()
                .collect::<BTreeSet<_>>(),
            versions.into_iter().collect()
        );
    }

    /// An occurrence both versions of a message carry is one occurrence.
    #[test]
    fn a_conflicting_message_lists_its_attachment_once_and_exports_it() {
        let fixture = Fixture::new();
        let version = |content| {
            let mut message = graph_message(
                "chat",
                "message",
                "2026-08-01T10:00:00Z",
                "2026-08-01T11:00:00Z",
                content,
            );
            message["attachments"] = json!([{
                "id": "attachment-1",
                "name": "note.txt",
                "contentType": "text/plain",
                "contentBytes":
                    base64::engine::general_purpose::STANDARD.encode(b"hello"),
            }]);
            message
        };
        let (page, _) = page_fragment(
            "tenant-a",
            vec![version("left"), version("right")],
            1,
            [],
            "https://graph.example/delta-1",
        );
        fixture.publish(page);
        let source = source_fragment("tenant-a").root().unwrap();
        fixture
            .storage()
            .with_session(|session| {
                let view = session.view();
                let rows = attachment_rows(&view.reader, &view.facts, source, None, None)?;
                assert_eq!(rows.len(), 1);
                let found = get_attachment(
                    source,
                    session,
                    AttachmentGetOptions {
                        source_id: "attachment-1".to_owned(),
                        chat_id: None,
                        message_id: None,
                    },
                )?;
                let AttachmentLookup::Found(found) = found else {
                    panic!("one occurrence: {found:?}")
                };
                assert_eq!(found.bytes.as_ref(), b"hello");
                Ok(())
            })
            .unwrap();
    }

    #[test]
    fn concurrent_receipts_are_both_current_whatever_their_generation() {
        let fixture = Fixture::new();
        let page = |content: &str, generation, predecessors: Vec<Id>| {
            page_fragment(
                "tenant-a",
                vec![graph_message(
                    "chat",
                    content,
                    "2026-08-01T10:00:00Z",
                    "2026-08-01T10:00:00Z",
                    content,
                )],
                generation,
                predecessors,
                &format!("https://graph.example/{content}"),
            )
        };
        let (root, root_id) = page("root", 1, vec![]);
        fixture.publish(root);
        let (main, main_id) = page("main", 2, vec![root_id]);
        fixture.publish(main);
        let (advanced, advanced_id) = page("advanced", 3, vec![main_id]);
        fixture.publish(advanced);
        let (stale, stale_id) = page("stale", 2, vec![root_id]);
        fixture.publish(stale);

        assert_eq!(
            sorted(read_all(&fixture.storage())),
            vec![one("advanced"), one("main"), one("root"), one("stale")]
        );
        let view = load_view(&fixture);
        let source = source_fragment("tenant-a").root().unwrap();
        assert_eq!(
            coverage_frontier(&view.reader, &view.facts, source).unwrap(),
            Some(CoverageFrontier {
                heads: BTreeSet::from([advanced_id, stale_id]),
                generation: 3,
                cursor: some("https://graph.example/advanced"),
                settled: false,
            })
        );
    }

    /// Commits arrive one by one, so a page may precede the page it follows.
    #[test]
    fn a_page_whose_predecessor_has_not_arrived_is_readable() {
        let fixture = Fixture::new();
        let (first, first_receipt) = page_fragment(
            "tenant-a",
            vec![
                graph_message(
                    "chat",
                    "message",
                    "2026-08-01T10:00:00Z",
                    "2026-08-01T10:00:00Z",
                    "first",
                ),
                graph_message(
                    "chat",
                    "earlier",
                    "2026-08-01T09:00:00Z",
                    "2026-08-01T09:00:00Z",
                    "earlier",
                ),
            ],
            1,
            [],
            "https://graph.example/delta-1",
        );
        let (second, _) = page_fragment(
            "tenant-a",
            vec![graph_message(
                "chat",
                "message",
                "2026-08-01T10:00:00Z",
                "2026-08-01T11:00:00Z",
                "second",
            )],
            2,
            [first_receipt],
            "https://graph.example/delta-2",
        );
        fixture.publish(second);
        assert_eq!(read_all(&fixture.storage()), vec![one("second")]);
        fixture.publish(first);
        assert_eq!(
            read_all(&fixture.storage()),
            vec![one("earlier"), one("second")]
        );
    }

    /// A Graph which gives these answers in turn, and records what was asked.
    struct Source {
        address: std::net::SocketAddr,
        answers: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<(u16, String)>>>,
        server: std::thread::JoinHandle<Vec<String>>,
    }

    impl Source {
        fn start() -> Self {
            use std::io::{BufRead, BufReader, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let answers = std::sync::Arc::<
                std::sync::Mutex<std::collections::VecDeque<(u16, String)>>,
            >::default();
            let queue = std::sync::Arc::clone(&answers);
            let server = std::thread::spawn(move || {
                let mut requested = Vec::new();
                loop {
                    let (mut stream, _) = listener.accept().unwrap();
                    let mut lines = BufReader::new(stream.try_clone().unwrap()).lines();
                    let request = lines.next().unwrap().unwrap();
                    for line in lines {
                        if line.unwrap().is_empty() {
                            break;
                        }
                    }
                    let path = request.split(' ').nth(1).unwrap().to_owned();
                    if path == "/stop" {
                        return requested;
                    }
                    requested.push(path);
                    let (status, body) = queue
                        .lock()
                        .unwrap()
                        .pop_front()
                        .unwrap_or((500, "{}".to_owned()));
                    write!(
                        stream,
                        "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                }
            });
            Self {
                address,
                answers,
                server,
            }
        }

        fn url(&self, name: &str) -> String {
            format!("http://{}/{name}", self.address)
        }

        fn answer(&self, status: u16, body: JsonValue) {
            self.answers
                .lock()
                .unwrap()
                .push_back((status, body.to_string()));
        }

        /// A terminal page.
        fn delta(&self, messages: Vec<JsonValue>, cursor: &str) {
            self.answer(
                200,
                json!({ "value": messages, "@odata.deltaLink": self.url(cursor) }),
            );
        }

        fn pull(&self, fixture: &Fixture, client_secret: Id) -> Result<PullReceipt> {
            let source = source_fragment("tenant-a").root().unwrap();
            let config = TeamsBridgeConfig {
                source_id: source,
                tenant: "tenant-a".to_owned(),
                client_id: "client".to_owned(),
                user_id: "user".to_owned(),
                scopes: "offline_access".to_owned(),
                profile: source,
                client_secret_version: Some(client_secret),
                delegated_token_version: None,
                delta_url: self.url("base"),
            };
            let mut token = Some(AppTokenCache {
                access_token: "test-token".to_owned(),
                expires_at_key: i128::MAX,
            });
            fixture
                .storage()
                .with_session(|session| pull_once_with_cache(&config, &mut token, session))
        }

        /// The paths asked for, in order.
        fn requested(self) -> Vec<String> {
            let _ = Client::new().get(self.url("stop")).send();
            self.server.join().unwrap()
        }
    }

    fn commits(fixture: &Fixture) -> usize {
        let signer = load_signer(&fixture.pile, Some(&fixture.key)).unwrap();
        let mut pile = open_pile_strict(&fixture.pile).unwrap();
        let found =
            crate::storage::discover_target(&mut pile, DEFAULT_SCOPE_ID, signer.verifying_key())
                .unwrap();
        pile.close().unwrap();
        found.commits().len()
    }

    #[test]
    fn a_round_which_brings_nothing_writes_nothing() {
        let fixture = Fixture::new();
        let (client_secret, _) = initialize_test_secrets(&fixture);
        let source = Source::start();
        let message = |id: &str, content: &str| {
            graph_message(
                "chat",
                id,
                "2026-08-01T10:00:00Z",
                "2026-08-01T10:00:00Z",
                content,
            )
        };

        // The first page ever is recorded, empty or not.
        source.delta(vec![], "one");
        assert_eq!(source.pull(&fixture, client_secret).unwrap().pages.len(), 1);
        let after_first = commits(&fixture);

        let mut length = None;
        for cursor in ["two", "three", "four"] {
            source.delta(vec![], cursor);
            let receipt = source.pull(&fixture, client_secret).unwrap();
            assert!(receipt.pages.is_empty());
            assert_eq!(receipt.observations, 0);
            assert_eq!(commits(&fixture), after_first);
            let now = fs::metadata(&fixture.pile).unwrap().len();
            assert_eq!(*length.get_or_insert(now), now);
        }

        // What comes after quiet rounds lands once, and moves the cursor.
        source.delta(vec![message("message", "after")], "five");
        assert_eq!(source.pull(&fixture, client_secret).unwrap().pages.len(), 1);
        assert_eq!(commits(&fixture), after_first + 1);
        source.delta(vec![], "six");
        assert!(source
            .pull(&fixture, client_secret)
            .unwrap()
            .pages
            .is_empty());
        assert_eq!(read_all(&fixture.storage()), vec![one("after")]);
        assert_eq!(
            source.requested(),
            ["/base", "/one", "/one", "/one", "/one", "/five"]
        );
    }

    /// A cursor kept too long expires; its replacement is recorded even where
    /// the source has nothing to say, and a re-delivery adds nothing.
    #[test]
    fn an_expired_cursor_is_replaced_and_its_redelivery_adds_nothing() {
        let fixture = Fixture::new();
        let (client_secret, _) = initialize_test_secrets(&fixture);
        let source = Source::start();
        let message = graph_message(
            "chat",
            "message",
            "2026-08-01T10:00:00Z",
            "2026-08-01T10:00:00Z",
            "kept",
        );
        source.delta(vec![message.clone()], "one");
        source.pull(&fixture, client_secret).unwrap();

        source.answer(410, json!({}));
        source.delta(vec![message], "two");
        assert_eq!(source.pull(&fixture, client_secret).unwrap().pages.len(), 1);
        assert_eq!(read_all(&fixture.storage()), vec![one("kept")]);
        let view = load_view(&fixture);
        assert_eq!(
            find!(
                observation: Id,
                pattern!(&view.facts, [{
                    ?observation @ metadata::tag: teams::kind_message_observation
                }])
            )
            .collect::<BTreeSet<_>>()
            .len(),
            1
        );
        drop(view);

        source.answer(410, json!({}));
        source.delta(vec![], "three");
        assert_eq!(source.pull(&fixture, client_secret).unwrap().pages.len(), 1);
        source.delta(vec![], "four");
        assert!(source
            .pull(&fixture, client_secret)
            .unwrap()
            .pages
            .is_empty());
        assert_eq!(read_all(&fixture.storage()), vec![one("kept")]);
        assert_eq!(
            source.requested(),
            ["/base", "/one", "/base", "/two", "/base", "/three"]
        );
    }

    /// A round left at an intermediate page is completed by its terminal page,
    /// empty or not.
    #[test]
    fn an_empty_terminal_page_still_completes_a_round_under_way() {
        let fixture = Fixture::new();
        let (client_secret, _) = initialize_test_secrets(&fixture);
        let source = Source::start();
        source.answer(
            200,
            json!({ "value": [], "@odata.nextLink": source.url("next") }),
        );
        source.answer(500, json!({}));
        source.pull(&fixture, client_secret).unwrap_err();
        source.delta(vec![], "done");
        assert_eq!(source.pull(&fixture, client_secret).unwrap().pages.len(), 1);
        source.delta(vec![], "later");
        assert!(source
            .pull(&fixture, client_secret)
            .unwrap()
            .pages
            .is_empty());
        assert_eq!(source.requested(), ["/base", "/next", "/next", "/done"]);
    }

    #[test]
    fn an_empty_page_still_joins_several_heads() {
        let fixture = Fixture::new();
        let (client_secret, _) = initialize_test_secrets(&fixture);
        let source = Source::start();
        let page = |content: &str| {
            page_fragment(
                "tenant-a",
                vec![graph_message(
                    "chat",
                    content,
                    "2026-08-01T10:00:00Z",
                    "2026-08-01T10:00:00Z",
                    content,
                )],
                1,
                [],
                &source.url(content),
            )
        };
        let (left, left_id) = page("left");
        fixture.publish(left);
        let (right, right_id) = page("right");
        fixture.publish(right);
        let id = source_fragment("tenant-a").root().unwrap();
        let heads = || {
            let view = load_view(&fixture);
            coverage_frontier(&view.reader, &view.facts, id)
                .unwrap()
                .unwrap()
                .heads
        };
        assert_eq!(heads(), BTreeSet::from([left_id, right_id]));

        source.delta(vec![], "joined");
        let receipt = source.pull(&fixture, client_secret).unwrap();
        assert_eq!(heads(), receipt.pages.iter().copied().collect());
        assert_eq!(receipt.pages.len(), 1);
        source.delta(vec![], "later");
        assert!(source
            .pull(&fixture, client_secret)
            .unwrap()
            .pages
            .is_empty());
        assert_eq!(
            sorted(read_all(&fixture.storage())),
            vec![one("left"), one("right")]
        );
        assert_eq!(source.requested().last().unwrap(), "/joined");
    }

    /// Two hosts pull independently from one common page, their piles are
    /// concatenated, and the next pull continues from one of their cursors.
    #[test]
    fn concatenated_pulls_show_both_hosts_and_the_next_pull_joins_them() {
        let graph = Source::start();
        let cursor = |name: &str| graph.url(name);
        let message = |id: &str, modified: &str, content: &str| {
            graph_message("chat", id, "2026-08-01T10:00:00Z", modified, content)
        };

        let here = Fixture::new();
        let (client_secret, _) = initialize_test_secrets(&here);
        let there = Fixture::new();
        let (root, root_id) = page_fragment(
            "tenant-a",
            vec![
                message("tied", "2026-08-01T10:00:00Z", "tied"),
                message("kept", "2026-08-01T10:00:00Z", "kept"),
                message("gone", "2026-08-01T10:00:00Z", "gone"),
                message("edited", "2026-08-01T10:00:00Z", "edited"),
            ],
            1,
            [],
            &cursor("root"),
        );
        here.publish(root);
        fs::copy(&here.pile, &there.pile).unwrap();
        fs::copy(&here.key, &there.key).unwrap();

        // This host: two pages, one version of `tied`, and it sees `gone` and
        // `edited` removed.
        let tied_here = message("tied", "2026-08-01T11:00:00Z", "tied here");
        let only_here = message("here", "2026-08-01T11:00:00Z", "only here");
        let view = load_view(&here);
        let source = source_fragment("tenant-a").root().unwrap();
        let known = load_known_messages(&view.reader, &view.facts, source).unwrap();
        drop(view);
        let (first, first_id) = page_fragment_with_known(
            "tenant-a",
            vec![
                tied_here.clone(),
                json!({ "id": "gone", "@removed": { "reason": "deleted" } }),
                json!({ "id": "edited", "@removed": { "reason": "deleted" } }),
            ],
            2,
            [root_id],
            &cursor("here-1"),
            &known,
        );
        here.publish(first);
        let (second, second_id) = page_fragment(
            "tenant-a",
            vec![only_here.clone()],
            3,
            [first_id],
            &cursor("here-2"),
        );
        here.publish(second);

        // The other host: one page, another version of `tied` at that time,
        // and an edit of `edited`.
        let tied_there = message("tied", "2026-08-01T11:00:00Z", "tied there");
        let only_there = message("there", "2026-08-01T11:00:00Z", "only there");
        let edited_there = message("edited", "2026-08-01T11:00:00Z", "edited there");
        let (other, other_id) = page_fragment(
            "tenant-a",
            vec![tied_there.clone(), only_there.clone(), edited_there.clone()],
            2,
            [root_id],
            &cursor("there-1"),
        );
        there.publish(other);

        // Concatenation is the merge.
        let mut merged = fs::OpenOptions::new()
            .append(true)
            .open(&here.pile)
            .unwrap();
        std::io::Write::write_all(&mut merged, &fs::read(&there.pile).unwrap()).unwrap();
        drop(merged);

        let conflicts = BTreeSet::from([
            some("tied here"),
            some("tied there"),
            some("edited there"),
            None,
        ]);
        let whole = |extra: &[&str]| {
            let mut plain = vec!["kept", "only here", "only there"];
            plain.extend(extra);
            sorted(plain.into_iter().map(one).collect())
        };
        let check = |extra: &[&str]| {
            let rows = read_all(&here.storage());
            let (conflicting, plain): (Vec<_>, Vec<_>) =
                rows.into_iter().partition(|(_, _, count)| *count > 1);
            assert_eq!(conflicting.len(), 4);
            assert_eq!(texts(&conflicting), conflicts);
            assert_eq!(sorted(plain), whole(extra));
        };
        check(&[]);
        let view = load_view(&here);
        let frontier = coverage_frontier(&view.reader, &view.facts, source)
            .unwrap()
            .unwrap();
        assert_eq!(frontier.heads, BTreeSet::from([second_id, other_id]));
        assert_eq!(frontier.cursor, Some(cursor("here-2")));
        drop(view);

        // The continued cursor never covered the other host's pages, so the
        // source delivers them again beside what is new.
        graph.delta(
            vec![
                tied_here,
                only_here,
                tied_there,
                only_there,
                edited_there,
                message("new", "2026-08-01T12:00:00Z", "new"),
            ],
            "joined",
        );
        let receipt = graph.pull(&here, client_secret).unwrap();
        assert_eq!(graph.requested(), ["/here-2"]);
        assert_eq!(receipt.pages.len(), 1);

        // Nothing is lost, nothing doubled, and no new conflict: what was
        // unordered before the replay still is.
        check(&["new"]);
        let view = load_view(&here);
        assert_eq!(
            coverage_frontier(&view.reader, &view.facts, source)
                .unwrap()
                .unwrap()
                .heads,
            BTreeSet::from([receipt.pages[0]])
        );
        assert_eq!(
            find!(
                observation: Id,
                pattern!(&view.facts, [{
                    ?observation @ metadata::tag: teams::kind_message_observation
                }])
            )
            .collect::<BTreeSet<_>>()
            .len(),
            10
        );
    }

    #[test]
    fn inline_attachment_bytes_live_in_the_same_page_fragment() {
        let source = source_fragment("tenant-a").root().unwrap();
        let mut message = graph_message(
            "chat",
            "message",
            "2026-08-01T10:00:00Z",
            "2026-08-01T10:00:00Z",
            "file",
        );
        message["attachments"] = json!([{
            "id": "attachment-1",
            "name": "note.txt",
            "contentType": "text/plain; charset=utf-8",
            "contentBytes": base64::engine::general_purpose::STANDARD.encode(b"hello"),
        }]);
        let (fragment, _, _) = build_page_fragment(
            "tenant-a",
            source,
            parse_messages(vec![message]).unwrap(),
            "token",
            &[],
        )
        .unwrap();
        assert_eq!(
            find!(
                file: Id,
                pattern!(&fragment, [{ ?file @ metadata::tag: crate::schemas::files::KIND_FILE }])
            )
            .count(),
            1
        );
        assert_eq!(
            find!(
                attachment: Id,
                pattern!(&fragment, [{
                    ?attachment @
                    metadata::tag: archive::kind_attachment,
                    archive::attachment_file: _?file,
                }])
            )
            .count(),
            1
        );
    }

    #[test]
    fn observations_without_their_page_receipt_are_rejected() {
        let source = source_fragment("tenant-a").root().unwrap();
        let (mut fragment, observations, _) = build_page_fragment(
            "tenant-a",
            source,
            parse_messages(vec![graph_message(
                "chat",
                "message",
                "2026-08-01T10:00:00Z",
                "2026-08-01T10:00:00Z",
                "atomic",
            )])
            .unwrap(),
            "token",
            &[],
        )
        .unwrap();
        assert!(teams_core::validate_commit_fragment(fragment.facts()).is_err());

        fragment += coverage_fragment(
            source,
            1,
            [],
            "https://graph.example/request",
            "https://graph.example/delta",
            "delta",
            observations,
        )
        .unwrap();
        teams_core::validate_commit_fragment(fragment.facts()).unwrap();
    }

    #[test]
    fn page_commit_cannot_borrow_identity_facts_from_the_catalog() {
        let fixture = Fixture::new();
        let (first, first_receipt) = page_fragment(
            "tenant-a",
            vec![graph_message(
                "chat",
                "message",
                "2026-08-01T10:00:00Z",
                "2026-08-01T10:00:00Z",
                "first",
            )],
            1,
            [],
            "https://graph.example/delta-1",
        );
        fixture.publish(first);

        let (second, _) = page_fragment(
            "tenant-a",
            vec![graph_message(
                "chat",
                "message",
                "2026-08-01T10:00:00Z",
                "2026-08-01T11:00:00Z",
                "second",
            )],
            2,
            [first_receipt],
            "https://graph.example/delta-2",
        );
        let chat = one_required(
            find!(
                value: Id,
                pattern!(&second, [{ ?value @ metadata::tag: teams::kind_chat }])
            )
            .collect(),
            "test chat",
        )
        .unwrap();
        let (_, facts, metafacts, blobs) = second.into_parts();
        let incomplete = facts
            .iter()
            .filter(|fact| fact.e() != &chat)
            .copied()
            .collect::<TribleSet>();
        let incomplete = Fragment::from_parts(incomplete, metafacts, blobs);
        let before = fs::read(&fixture.pile).unwrap();
        let error = fixture
            .storage()
            .publish(incomplete, "incomplete page")
            .unwrap_err();
        assert!(error.to_string().contains("names an unknown chat"));
        assert_eq!(fs::read(&fixture.pile).unwrap(), before);
    }

    #[test]
    fn malformed_inline_attachment_blocks_page_construction() {
        let mut message = graph_message(
            "chat",
            "message",
            "2026-08-01T10:00:00Z",
            "2026-08-01T10:00:00Z",
            "file",
        );
        message["attachments"] = json!([{
            "id": "attachment-1",
            "contentBytes": "not base64!",
        }]);
        assert!(parse_messages(vec![message]).is_err());

        let mut missing_id = graph_message(
            "chat",
            "message",
            "2026-08-01T10:00:00Z",
            "2026-08-01T10:00:00Z",
            "file",
        );
        missing_id["attachments"] = json!([{ "name": "orphan.bin" }]);
        assert!(parse_messages(vec![missing_id]).is_err());
    }

    #[test]
    fn exact_page_replay_is_idempotent() {
        let fixture = Fixture::new();
        let (page, _) = page_fragment(
            "tenant-a",
            vec![graph_message(
                "chat",
                "message",
                "2026-08-01T10:00:00Z",
                "2026-08-01T10:00:00Z",
                "same",
            )],
            1,
            [],
            "https://graph.example/delta",
        );
        fixture.publish(page.clone());
        let after_first = fs::metadata(&fixture.pile).unwrap().len();
        fixture.publish(page);
        assert_eq!(fs::metadata(&fixture.pile).unwrap().len(), after_first);
    }

    #[test]
    fn login_resolves_generic_authority_to_token_tenant() {
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"tid":"tenant-guid"}"#);
        let token = format!("e30.{payload}.signature");
        assert_eq!(
            resolve_source_tenant("common", Some(&token), None).unwrap(),
            "tenant-guid"
        );
        assert!(resolve_source_tenant("common", None, None).is_err());
        assert_eq!(
            resolve_source_tenant("tenant.example", None, None).unwrap(),
            "tenant.example"
        );
    }

    #[test]
    fn attachment_references_preserve_collection_scope() {
        assert_eq!(
            attachment_reference(Some("hosted-content"), "42"),
            "hosted-content:42"
        );
        assert_eq!(
            parse_attachment_reference("attachment:42"),
            (Some("attachment"), "42")
        );
        assert_eq!(parse_attachment_reference("42"), (None, "42"));
    }
}
