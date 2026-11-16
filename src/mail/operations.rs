//! `mail` — collection-native RFC 5322 evidence, intent, and receipt faculty.

#[cfg(test)]
use crate::storage::FactRead;
use std::cell::RefCell;
use std::collections::BTreeSet;
#[cfg(test)]
use std::path::Path;
use std::path::PathBuf;

use crate::clock;
#[cfg(test)]
use crate::collection_names::open_configured;
use crate::files;
use crate::mail::{self, AccountConfigInput, DraftInput, Head, SendAttemptInput};
use crate::mail_imap;
use crate::relations;
use crate::schemas::{
    decide as decide_schema, files as files_schema, mail as mail_schema,
    relations as relations_schema,
};
use crate::secrets::{storage as secret_storage, SecretsSnapshot};
#[cfg(test)]
use crate::storage::{load_signer, open_pile_strict, open_pile_strict_as};
use crate::storage::{AcquiringReader, FactArchive, FacultySnapshot};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use ed25519_dalek::SigningKey;
use lettre::address::{Address as SmtpAddress, Envelope as LettreEnvelope};
use lettre::transport::smtp::authentication::Credentials;
use lettre::{SmtpTransport, Transport};
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::CollectionStoreExt;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
type PileSnapshot = AcquiringReader<FacultySnapshot>;
use triblespace::prelude::*;

#[derive(Clone, Debug)]
pub struct AccountOptions {
    pub account: Option<String>,
    pub address: String,
    pub display_name: String,
    /// Implicit-TLS IMAP endpoint as `host:port` (normally port 993).
    pub imap_endpoint: String,
    pub smtp_endpoint: String,
    pub username: Option<String>,
    pub credential_version: Option<Id>,
    pub disabled: bool,
    /// Store this mailbox's inbound mail without making it attention.
    pub quiet: bool,
}
#[derive(Clone, Debug)]
pub struct AccountSetReceipt {
    pub account: Id,
    pub config: Id,
    pub credential: Id,
    pub changed: bool,
    pub notices: Vec<String>,
}
#[derive(Clone, Debug)]
pub enum AccountState {
    Missing,
    Forked(Vec<Id>),
    Configured {
        config: Id,
        address: String,
        enabled: bool,
        quiet: bool,
    },
}
#[derive(Clone, Debug)]
pub struct AccountSummary {
    pub account: Id,
    pub state: AccountState,
}
#[derive(Clone, Debug)]
pub enum DraftAttachment {
    File(Id),
    Resident(mail::AttachmentData),
}
#[derive(Clone, Debug)]
pub struct DraftRequest {
    pub account: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub attachments: Vec<DraftAttachment>,
}
#[derive(Clone, Debug)]
pub struct ReplyRequest {
    pub message: String,
    pub account: String,
    pub body: String,
}
#[derive(Clone, Debug)]
pub struct DraftReceipt {
    pub draft: Id,
    pub decision: Id,
}
#[derive(Clone, Debug)]
pub enum SendStatus {
    AlreadyAccepted,
    Accepted(mail::AcceptedReply),
}
#[derive(Clone, Debug)]
pub struct SendReceipt {
    pub draft: Id,
    pub attempt: Id,
    pub status: SendStatus,
}
#[derive(Clone, Debug)]
pub enum DraftDelivery {
    Pending,
    Uncertain(Id),
    Accepted(Id),
    MultipleAttempts,
}
#[derive(Clone, Debug)]
pub struct DraftStatus {
    pub draft: Id,
    pub subject: String,
    pub delivery: DraftDelivery,
}
#[derive(Clone, Debug)]
pub struct InboxMessage {
    pub projection: mail::ProjectionView,
    pub unread: bool,
}
#[derive(Clone, Debug)]
pub struct ReadReceipt {
    pub wire: Id,
    pub observation: Id,
    pub reader: Id,
}
#[derive(Clone, Debug)]
pub struct AccountFetched {
    pub account: Id,
    pub config: Id,
    pub address: String,
    /// Messages newly observed, summed over `mailboxes`.
    pub fetched: usize,
    pub mailboxes: Vec<mail::MailboxFetched>,
}
/// What one read-only fetch looks at. The default is every unseen-by-us
/// message in `INBOX` of every enabled account.
#[derive(Clone, Debug, Default)]
pub struct FetchRequest {
    pub mailboxes: mail::MailboxSelection,
    pub criteria: mail::SearchCriteria,
}

/// Configured direct Mail operations. Each call observes maintained Mail, Files,
/// Decide, Relations and Secrets through one shared store snapshot.
/// Text and attachments are resident values; no argv, host-path expansion, or ambient persona.
#[derive(Clone, Debug)]
pub struct Mail {
    storage: crate::storage::Storage,
}
impl Mail {
    pub fn new(pile: PathBuf, key: Option<PathBuf>) -> Self {
        Self::with_storage(crate::storage::Storage::new(pile, key))
    }
    pub fn with_storage(storage: crate::storage::Storage) -> Self {
        Self { storage }
    }
    fn with_operation<T>(&self, execute: impl FnOnce(&Storage) -> Result<T>) -> Result<T> {
        self.storage.scope(|storage| {
            let context = Storage::from_storage(storage.clone(), Scopes::FIXED)?;
            let result = execute(&context);
            let result = match result {
                Ok(value) => Ok(value),
                Err(error) if context.published.get() => Err(error.context(
                    "Mail action failed after facts were committed; publication was not rolled back",
                )),
                Err(error) => Err(error),
            };
            let notices = context.notices.take();
            match result {
                Err(error) if !notices.is_empty() => Err(error.context(notices.join("\n"))),
                other => other,
            }
        })
    }
    pub fn account_set(&self, input: AccountOptions) -> Result<AccountSetReceipt> {
        self.update_account(input, None)
    }
    /// Host credential-provisioning interface. MCP accepts exact Secrets versions instead.
    pub fn account_set_with_password(
        &self,
        input: AccountOptions,
        password: String,
    ) -> Result<AccountSetReceipt> {
        self.update_account(input, Some(password))
    }
    fn update_account(
        &self,
        input: AccountOptions,
        password: Option<String>,
    ) -> Result<AccountSetReceipt> {
        self.with_operation(|storage| {
            account_set(
                storage,
                input.account,
                input.address,
                input.display_name,
                input.imap_endpoint,
                input.smtp_endpoint,
                input.username,
                password,
                input.credential_version,
                input.disabled,
                input.quiet,
            )
        })
    }
    pub fn account_list(&self) -> Result<Vec<AccountSummary>> {
        self.with_operation(account_list)
    }
    /// Observe every enabled account's selected mailboxes read-only over IMAP:
    /// EXAMINE, UID SEARCH, and BODY.PEEK[] for each UID not yet observed under
    /// the mailbox's current UIDVALIDITY. Nothing on the server is changed.
    pub fn fetch(&self, request: FetchRequest) -> Result<Vec<AccountFetched>> {
        self.with_operation(|storage| cmd_fetch(storage, request))
    }
    pub fn draft(&self, input: DraftRequest) -> Result<DraftReceipt> {
        self.with_operation(|storage| cmd_draft(storage, input))
    }
    pub fn reply(&self, input: ReplyRequest) -> Result<DraftReceipt> {
        self.with_operation(|storage| cmd_reply(storage, input))
    }
    /// Submit one authorized immutable draft. Deployments MUST externally serialize
    /// SMTP execution per account, including other processes/replicas. An uncertain
    /// durable attempt is never retried automatically.
    pub fn send(&self, draft: &str) -> Result<SendReceipt> {
        self.with_operation(|storage| cmd_send(storage, draft))
    }
    pub fn outbox(&self) -> Result<Vec<DraftStatus>> {
        self.with_operation(cmd_outbox)
    }
    pub fn list(&self, persona: &str, unread: bool, spam: bool) -> Result<Vec<InboxMessage>> {
        self.with_operation(|storage| cmd_list(storage, persona, unread, spam))
    }
    /// Add intrinsic seen evidence; this does not display the message.
    pub fn read(&self, persona: &str, message: &str) -> Result<ReadReceipt> {
        self.with_operation(|storage| cmd_read(storage, message, persona))
    }
    pub fn show(&self, message: &str) -> Result<Vec<mail::ProjectionView>> {
        self.with_operation(|storage| cmd_show(storage, message))
    }
    pub fn search(&self, query: &str) -> Result<Vec<mail::ProjectionView>> {
        self.with_operation(|storage| cmd_search(storage, query))
    }
}

#[derive(Clone, Copy)]
struct Scopes {
    mail: Id,
    files: Id,
    decide: Id,
    relations: Id,
}

impl Scopes {
    const FIXED: Self = Self {
        mail: mail_schema::DEFAULT_SCOPE_ID,
        files: files_schema::DEFAULT_SCOPE_ID,
        decide: decide_schema::DEFAULT_SCOPE_ID,
        relations: relations_schema::DEFAULT_SCOPE_ID,
    };
}

struct CollectionView {
    facts: FactArchive,
    reader: PileSnapshot,
}

struct Views {
    mail: CollectionView,
    files: CollectionView,
    decide: CollectionView,
    relations: CollectionView,
    secrets: SecretsSnapshot<PileSnapshot>,
}

struct Storage {
    storage: crate::storage::Storage,
    signer: SigningKey,
    scopes: Scopes,
    notices: RefCell<Vec<String>>,
    // Whether this action already committed, so a later failure reports that
    // its publication was not rolled back.
    published: std::cell::Cell<bool>,
}

impl Storage {
    fn from_storage(storage: crate::storage::Storage, scopes: Scopes) -> Result<Self> {
        let signer = storage.with_store(|_, signer, _| Ok(signer.clone()))?;
        Ok(Self {
            storage,
            signer,
            scopes,
            notices: RefCell::new(Vec::new()),
            published: std::cell::Cell::new(false),
        })
    }

    #[cfg(test)]
    fn open(pile: &Path, key: Option<&Path>, scopes: Scopes) -> Result<Self> {
        Self::from_storage(
            crate::storage::Storage::shared(pile.to_owned(), key.map(Path::to_owned)),
            scopes,
        )
    }

    fn views(&self) -> Result<Views> {
        self.storage.with_store(|store, _, runtime| {
            for scope in [self.scopes.mail, self.scopes.files, self.scopes.decide, self.scopes.relations] {
                self.storage.open_collection_read(
                    store, scope, self.signer.verifying_key(), runtime,
                )?;
            }
            let secrets_collection = self.storage.open_secrets_collection(store, self.signer.verifying_key(), runtime, false)?;
            let pile = store;
            let (mail_facts, files_facts, decide_facts, relations_facts, store_snapshot, secrets) = {
                let mail_collection =
                    self.storage.open_collection_local(pile, self.scopes.mail, self.signer.verifying_key())?;
                let files_collection =
                    self.storage.open_collection_local(pile, self.scopes.files, self.signer.verifying_key())?;
                let decide_collection =
                    self.storage.open_collection_local(pile, self.scopes.decide, self.signer.verifying_key())?;
                let relations_collection =
                    self.storage.open_collection_local(pile, self.scopes.relations, self.signer.verifying_key())?;
                let mail_succinct =
                    pile.attach::<SuccinctArchiveBlob>(mail_collection, ())?;
                let mail_rank9 =
                    pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(mail_collection, mail_succinct)?;
                let files_succinct =
                    pile.attach::<SuccinctArchiveBlob>(files_collection, ())?;
                let files_rank9 =
                    pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(files_collection, files_succinct)?;
                let decide_succinct =
                    pile.attach::<SuccinctArchiveBlob>(decide_collection, ())?;
                let decide_rank9 =
                    pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(decide_collection, decide_succinct)?;
                let relations_succinct =
                    pile.attach::<SuccinctArchiveBlob>(relations_collection, ())?;
                let relations_rank9 =
                    pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(relations_collection, relations_succinct)?;
                // Attach the views as they stand through the same final
                // target snapshot as the configured Secrets view. A read
                // never maintains and never waits for a source commit nothing
                // has attached yet, which it reads from its own bytes; a write
                // attaches the source's frontier after its commit.
                let secrets = runtime.block_on(async {
                    let secrets = secret_storage::ensure_and_snapshot(
                        pile,
                        secrets_collection,
                        &self.signer,
                    )
                    .await
                    .context("observe configured Secrets collection")?;
                    Ok::<_, anyhow::Error>(secrets)
                })?;
                // Secrets attachment owns the final immutable pile snapshot. Attach
                // every maintained view through that same world so Mail
                // facts, file payloads, decisions, relations, and credentials can
                // never be assembled from different store prefixes.
                let store_snapshot = AcquiringReader::new(secrets.store_snapshot().clone(), runtime.clone());
                let secrets = secret_storage::snapshot_acquiring(store_snapshot.clone(), secrets_collection)?;
                let mail_facts = crate::storage::acquire_facts(&store_snapshot, mail_rank9)
                    .context("read maintained Mail fact collection")?;
                let files_facts = crate::storage::acquire_facts(&store_snapshot, files_rank9)
                    .context("read maintained Files fact collection")?;
                let decide_facts = crate::storage::acquire_facts(&store_snapshot, decide_rank9)
                    .context("read maintained Decide fact collection")?;
                let relations_facts = crate::storage::acquire_facts(&store_snapshot, relations_rank9)
                    .context("read maintained Relations fact collection")?;
                (
                    mail_facts,
                    files_facts,
                    decide_facts,
                    relations_facts,
                    store_snapshot,
                    secrets,
                )
            };
            Ok(Views {
                mail: CollectionView {
                    facts: mail_facts,
                    reader: store_snapshot.clone(),
                },
                files: CollectionView {
                    facts: files_facts,
                    reader: store_snapshot.clone(),
                },
                decide: CollectionView {
                    facts: decide_facts,
                    reader: store_snapshot.clone(),
                },
                relations: CollectionView {
                    facts: relations_facts,
                    reader: store_snapshot,
                },
                secrets,
            })
        })
    }

    fn add_secret(&self, name: &str, plaintext: &[u8]) -> Result<Id> {
        self.storage.with_store(|store, _, runtime| {
            let collection = self.storage.open_secrets_collection(
                store,
                self.signer.verifying_key(),
                runtime,
                true,
            )?;
            let pile = store;
            secret_storage::add_secret(
                &mut *pile,
                &self.signer,
                collection,
                name,
                plaintext,
                point_now()?,
            )
            .context("publish mailbox credential to configured Secrets collection")
        })
    }

    fn publish(&self, scope: Id, fragment: Fragment, description: &str) -> Result<()> {
        self.storage.with_store(|store, _, runtime| {
            let collection = self.storage.open_collection_write(
                store,
                scope,
                self.signer.verifying_key(),
                runtime,
            )?;
            let pile = store;
            let mut fragment = fragment;
            fragment.describe_with(entity! { metadata::description: description.to_owned() });
            pile.commit(collection, &self.signer, fragment)
                .with_context(|| format!("commit collection {scope:x}"))?;
            self.published.set(true);
            drop(
                runtime
                    .block_on(crate::storage::ensure_downstream(
                        pile,
                        collection,
                        &self.signer,
                    ))
                    .context(
                        "Mail facts were committed, but ensuring their derived views failed",
                    )?,
            );
            Ok(())
        })
    }

    #[cfg(test)]
    fn close(self) -> Result<()> {
        self.storage.close()
    }
}

fn fmt_id(id: Id) -> String {
    format!("{id:x}")
}

fn point_now() -> Result<mail::IntervalValue> {
    clock::point_now()
}

fn relation_persona(views: &Views, raw: &str) -> Result<Id> {
    relations::resolve_person(&views.relations.reader, &views.relations.facts, raw, false)?
        .require_unique("active Relations person", &raw)
}

fn mailbox_secret_name(account: Id) -> String {
    format!("mail/{}", URL_SAFE_NO_PAD.encode(account.raw()))
}

fn resolve_account(views: &Views, input: &str) -> Result<Id> {
    let anchors = mail::account_anchors(&views.mail.facts);
    if let Some(id) = Id::from_hex(input.trim()) {
        if anchors.contains(&id) {
            return Ok(id);
        }
        bail!("unknown mail account {id:x}");
    }
    let lowered = input.trim().to_ascii_lowercase();
    let mut matches = BTreeSet::new();
    for anchor in anchors {
        if fmt_id(anchor).starts_with(&lowered) {
            matches.insert(anchor);
            continue;
        }
        let config = match mail::account_head(&views.mail.facts, anchor)? {
            Head::Unique(id) => mail::account_config(&views.mail.facts, id)?,
            Head::Missing | Head::Forked(_) => continue,
        };
        if mail::read_text(&views.mail.reader, config.address)?.eq_ignore_ascii_case(input.trim()) {
            matches.insert(anchor);
        }
    }
    match matches.len() {
        0 => bail!("no mail account matches {input:?}"),
        1 => Ok(matches.pop_first().unwrap()),
        count => bail!("{count} mail accounts match {input:?}"),
    }
}

fn resolve_draft<P>(facts: &P, input: &str) -> Result<Id>
where
    P: TriblePattern,
{
    let candidates: BTreeSet<Id> = find!(
        id: Id,
        pattern!(facts, [{ ?id @ metadata::tag: &mail_schema::KIND_DRAFT_INTENT }])
    )
    .collect();
    crate::resolve_id_prefix(input, candidates)
}

fn wire_candidates<P>(facts: &P) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    find!(id: Id, pattern!(facts, [{ ?id @ metadata::tag: &mail_schema::KIND_WIRE_MESSAGE }]))
        .collect()
}

fn resolve_wire<P>(facts: &P, input: &str) -> Result<Id>
where
    P: TriblePattern,
{
    crate::resolve_id_prefix(input, wire_candidates(facts))
}

fn account_set(
    storage: &Storage,
    account_selector: Option<String>,
    address: String,
    display_name: String,
    imap_endpoint: String,
    smtp_endpoint: String,
    username: Option<String>,
    password: Option<String>,
    credential_version: Option<Id>,
    disabled: bool,
    quiet: bool,
) -> Result<AccountSetReceipt> {
    let mut views = storage.views()?;
    let (anchor, predecessors, old_credential, replacing_fork) =
        if let Some(selector) = account_selector {
            let anchor = resolve_account(&views, &selector)?;
            match mail::account_head(&views.mail.facts, anchor)? {
                Head::Unique(head) => {
                    let config = mail::account_config(&views.mail.facts, head)?;
                    (anchor, vec![head], Some(config.credential), false)
                }
                Head::Missing => bail!("account {anchor:x} has no configuration"),
                // A complete new snapshot can reconcile every observed branch,
                // but no branch may be selected as the credential donor.
                Head::Forked(heads) => (anchor, heads, None, true),
            }
        } else {
            (genid().id, Vec::new(), None, false)
        };

    if let Some(id) = credential_version {
        if !views.secrets.contains(id) {
            bail!("unknown Secrets credential version {id:x}");
        }
    }

    // Canonicalize every account field before a supplied password can cause
    // an immutable secret publication. The temporary credential is replaced
    // below; it does not escape this in-memory validation step.
    let username = username.unwrap_or_else(|| address.clone());
    let mut input = AccountConfigInput {
        address,
        display_name,
        imap_endpoint,
        smtp_endpoint,
        username,
        credential: credential_version.or(old_credential).unwrap_or(anchor),
        enabled: !disabled,
        quiet,
        predecessors,
    }
    .canonicalized()?;

    let credential_id = match (password, credential_version, old_credential) {
        (None, None, Some(id)) => id,
        (None, None, None) if replacing_fork => {
            bail!("--credential-version or MAIL_PASS/--password is required to reconcile a forked account")
        }
        (None, None, None) => {
            bail!("--credential-version or MAIL_PASS/--password is required for a new account")
        }
        (None, Some(id), _) => id,
        (Some(value), None, _) => {
            let id = storage.add_secret(&mailbox_secret_name(anchor), value.as_bytes())?;
            storage.notices.borrow_mut().push(format!(
                "Published mailbox credential {id:x}; if Mail publication is interrupted, retry with --credential-version {id:x}"
            ));
            views = storage.views()?;
            if !views.secrets.contains(id) {
                bail!("published mailbox secret {id:x} did not materialize");
            }
            id
        }
        (Some(_), Some(_), _) => {
            bail!("--password cannot be combined with --credential-version")
        }
    };
    input.credential = credential_id;
    if let [predecessor] = input.predecessors.as_slice() {
        let previous = mail::account_config(&views.mail.facts, *predecessor)?;
        let same = mail::read_text(&views.mail.reader, previous.address)? == input.address
            && mail::read_text(&views.mail.reader, previous.display_name)? == input.display_name
            && previous.pop_endpoint.is_none()
            && previous
                .imap_endpoint
                .map(|handle| mail::read_text(&views.mail.reader, handle))
                .transpose()?
                .as_deref()
                == Some(input.imap_endpoint.as_str())
            && mail::read_text(&views.mail.reader, previous.smtp_endpoint)? == input.smtp_endpoint
            && mail::read_text(&views.mail.reader, previous.username)? == input.username
            && previous.credential == input.credential
            && previous.enabled == input.enabled
            && previous.quiet == input.quiet;
        if same {
            return Ok(AccountSetReceipt {
                account: anchor,
                config: *predecessor,
                changed: false,
                credential: credential_id,
                notices: storage.notices.borrow().clone(),
            });
        }
    }

    let mut fragment = Fragment::empty();
    let (config_fragment, config_id) = mail::account_config_fragment(anchor, input)?;
    fragment += config_fragment;
    storage.publish(
        storage.scopes.mail,
        fragment,
        "mail: account full-state config",
    )?;
    Ok(AccountSetReceipt {
        account: anchor,
        config: config_id,
        changed: true,
        credential: credential_id,
        notices: storage.notices.borrow().clone(),
    })
}

fn account_list(storage: &Storage) -> Result<Vec<AccountSummary>> {
    let views = storage.views()?;
    let mut result = Vec::new();
    for account in mail::account_anchors(&views.mail.facts) {
        let state = match mail::account_head(&views.mail.facts, account)? {
            Head::Missing => AccountState::Missing,
            Head::Forked(ids) => AccountState::Forked(ids),
            Head::Unique(id) => {
                let config = mail::account_config(&views.mail.facts, id)?;
                AccountState::Configured {
                    config: id,
                    address: mail::read_text(&views.mail.reader, config.address)?,
                    enabled: config.enabled,
                    quiet: config.quiet,
                }
            }
        };
        result.push(AccountSummary { account, state });
    }
    Ok(result)
}

fn stage_attachments(
    views: &Views,
    attachments: Vec<DraftAttachment>,
) -> Result<(Fragment, Vec<Id>)> {
    let mut fragment = Fragment::empty();
    let mut ids = Vec::new();
    for attachment in attachments {
        match attachment {
            DraftAttachment::File(id) => {
                if !exists!(
                    pattern!(&views.files.facts, [{ id @ metadata::tag: &files_schema::KIND_FILE }])
                ) {
                    bail!("unknown Files attachment {id:x}");
                }
                // An attachment needs bytes, a name and a media type to be
                // sent; a file observed with several of any is still one.
                if !exists!(
                    (
                        content: files::ContentHandle,
                        name: files::NameHandle,
                        media: files::NameHandle
                    ),
                    pattern!(&views.files.facts, [
                        { id @
                            files_schema::file::content: ?content,
                            files_schema::file::name: ?name,
                            files_schema::file::media_type: _?media_type,
                        },
                        { _?media_type @
                            metadata::tag: &files_schema::KIND_MEDIA_TYPE,
                            metadata::name: ?media,
                        }
                    ])
                ) {
                    bail!("incomplete Files attachment {id:x}");
                }
                ids.push(id);
            }
            DraftAttachment::Resident(value) => {
                let file = files::stage(value.bytes, value.filename, &value.media_type)?;
                ids.push(file.root().expect("canonical file root"));
                fragment += file;
            }
        }
    }
    Ok((fragment, ids))
}

#[allow(clippy::too_many_arguments)]
fn create_draft(
    storage: &Storage,
    views: &Views,
    account_selector: &str,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: String,
    body: String,
    attachments: Vec<DraftAttachment>,
    in_reply_to: Vec<Id>,
    references: Vec<Id>,
) -> Result<DraftReceipt> {
    let account_id = resolve_account(views, account_selector)?;
    let account = match mail::account_head(&views.mail.facts, account_id)? {
        Head::Unique(id) => mail::account_config(&views.mail.facts, id)?,
        Head::Missing => bail!("account {account_id:x} has no configuration"),
        Head::Forked(ids) => bail!("account {account_id:x} has forked configurations {ids:?}"),
    };
    if !account.enabled {
        bail!("account {account_id:x} is disabled");
    }
    let envelope_from = mail::read_text(&views.mail.reader, account.address)?;
    let (files_fragment, attachment_ids) = stage_attachments(views, attachments)?;
    let draft = mail::draft_publication(DraftInput {
        nonce: genid().id,
        account: account_id,
        envelope_from,
        to,
        cc,
        bcc,
        subject,
        body,
        attachments: attachment_ids,
        in_reply_to,
        references,
        created_at: point_now()?,
    })?;
    if !files_fragment.facts().is_empty() {
        storage.publish(
            storage.scopes.files,
            files_fragment,
            "mail: draft attachments",
        )?;
    }
    storage.publish(
        storage.scopes.decide,
        draft.decide,
        "mail: draft send decision",
    )?;
    storage.publish(
        storage.scopes.mail,
        draft.mail,
        "mail: immutable draft intent",
    )?;
    Ok(DraftReceipt {
        draft: draft.draft,
        decision: draft.decision,
    })
}

fn cmd_draft(storage: &Storage, args: DraftRequest) -> Result<DraftReceipt> {
    let views = storage.views()?;
    create_draft(
        storage,
        &views,
        &args.account,
        args.to,
        args.cc,
        args.bcc,
        args.subject,
        args.body,
        args.attachments,
        Vec::new(),
        Vec::new(),
    )
}

fn projection_for_wire(views: &Views, wire_id: Id) -> Result<mail::ProjectionView> {
    let ids: BTreeSet<Id> = find!(
        projection_id: Id,
        pattern!(&views.mail.facts, [
            { _?source @ observation::wire: &wire_id },
            { ?projection_id @ projection::source: _?source, projection::recipe: &mail_schema::RECIPE_RFC5322_V1 }
        ])
    )
    .collect();
    let candidates = ids
        .into_iter()
        .map(|id| mail::projection_view(&views.mail.reader, &views.mail.facts, id))
        .collect::<Result<Vec<_>>>()?;
    let Some(chosen) = candidates.first().cloned() else {
        bail!("wire message {wire_id:x} has no parser projection");
    };
    // Re-observing byte-identical mail creates another source/projection pair
    // but must not make the WireMessage unusable.  The source-local attachment
    // occurrence ids legitimately differ, so reply arbitration compares the
    // semantic fields a reply consumes and rejects only a real conflict.
    let agrees = |other: &mail::ProjectionView| {
        chosen.wire == other.wire
            && chosen.message_id == other.message_id
            && chosen.from == other.from
            && chosen.to == other.to
            && chosen.cc == other.cc
            && chosen.bcc == other.bcc
            && chosen.subject == other.subject
            && chosen.body == other.body
            && chosen.claimed_date == other.claimed_date
            && chosen.in_reply_to == other.in_reply_to
            && chosen.references == other.references
            && chosen.spam == other.spam
    };
    if candidates.iter().skip(1).all(agrees) {
        Ok(chosen)
    } else {
        bail!(
            "wire message {wire_id:x} has conflicting parser projections; choose an exact source before replying"
        )
    }
}

fn cmd_reply(storage: &Storage, args: ReplyRequest) -> Result<DraftReceipt> {
    let views = storage.views()?;
    let wire_id = resolve_wire(&views.mail.facts, &args.message)?;
    let parent = projection_for_wire(&views, wire_id)?;
    let recipient = parent
        .from
        .ok_or_else(|| anyhow!("parent message has no From mailbox claim"))?;
    let subject = if parent.subject.to_ascii_lowercase().starts_with("re:") {
        parent.subject
    } else {
        format!("Re: {}", parent.subject)
    };
    let mut references = parent.references;
    let in_reply_to = if parent.message_id.is_some() {
        references.push(wire_id);
        vec![wire_id]
    } else {
        // A digest-only parent did not claim an RFC Message-ID. It is a valid
        // local WireMessage identity, but cannot honestly appear in a remote
        // In-Reply-To or References header.
        Vec::new()
    };
    create_draft(
        storage,
        &views,
        &args.account,
        vec![recipient],
        Vec::new(),
        Vec::new(),
        subject,
        args.body,
        Vec::new(),
        in_reply_to,
        references,
    )
}

struct LettreSubmit {
    transport: SmtpTransport,
}

impl mail::SmtpSubmit for LettreSubmit {
    fn submit(&mut self, envelope: &mail::SmtpEnvelope, raw: &[u8]) -> Result<mail::AcceptedReply> {
        let from: SmtpAddress = envelope.from.parse().context("parse SMTP reverse path")?;
        let recipients = envelope
            .recipients
            .iter()
            .map(|value| {
                value
                    .parse()
                    .with_context(|| format!("parse SMTP recipient {value:?}"))
            })
            .collect::<Result<Vec<SmtpAddress>>>()?;
        let envelope =
            LettreEnvelope::new(Some(from), recipients).context("construct SMTP envelope")?;
        let response = self
            .transport
            .send_raw(&envelope, raw)
            .context("SMTP submission is uncertain; the durable SendAttempt must not be retried")?;
        let code: u16 = response.code().into();
        let message = response.message().collect::<Vec<_>>().join(" ");
        Ok(mail::AcceptedReply {
            code,
            message: if message.is_empty() {
                code.to_string()
            } else {
                message
            },
        })
    }
}

fn endpoint<'a>(value: &'a str, label: &str) -> Result<(&'a str, u16)> {
    let (host, port) = value
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("{label} endpoint must be host:port"))?;
    if host.is_empty() {
        bail!("{label} endpoint has an empty host");
    }
    Ok((
        host,
        port.parse()
            .with_context(|| format!("parse {label} port"))?,
    ))
}

fn cmd_send(storage: &Storage, selector: &str) -> Result<SendReceipt> {
    cmd_send_with(
        storage,
        selector,
        mail::MAX_DRAFT_ATTACHMENT_BYTES,
        |account| {
            let (host, port) = endpoint(&account.smtp_endpoint, "SMTP")?;
            let creds = Credentials::new(account.username.clone(), account.password.clone());
            let transport = SmtpTransport::relay(host)
                .with_context(|| format!("configure SMTP relay {host}"))?
                .port(port)
                .credentials(creds)
                .build();
            Ok(LettreSubmit { transport })
        },
    )
}

/// [`cmd_send`] with its attachment budget and its SMTP connection supplied.
/// `connect` is called only once the attempt is prepared and its Files
/// evidence published, immediately before the attempt is published and
/// submitted.
fn cmd_send_with<S: mail::SmtpSubmit>(
    storage: &Storage,
    selector: &str,
    attachment_limit: u64,
    connect: impl FnOnce(&mail::OpenAccount) -> Result<S>,
) -> Result<SendReceipt> {
    let views = storage.views()?;
    let draft_id = resolve_draft(&views.mail.facts, selector)?;
    let existing = mail::attempts_for_draft(&views.mail.facts, draft_id);
    if let Some(&attempt) = existing.first() {
        if mail::acceptances_for_attempt(&views.mail.facts, attempt).is_empty() {
            bail!("draft {draft_id:x} already has uncertain attempt {attempt:x}; never retry automatically");
        }
        return Ok(SendReceipt {
            draft: draft_id,
            attempt,
            status: SendStatus::AlreadyAccepted,
        });
    }
    let record = mail::draft_value(&views.mail.facts, draft_id)?;
    let current_config = match mail::account_head(&views.mail.facts, record.account)? {
        Head::Unique(id) => mail::account_config(&views.mail.facts, id)?,
        Head::Missing => bail!("draft account {:x} has no configuration", record.account),
        Head::Forked(ids) => bail!(
            "draft account {:x} has forked configuration heads: {ids:?}",
            record.account
        ),
    };
    if !current_config.enabled {
        bail!("draft account {} is disabled", fmt_id(record.account));
    }
    // An over-budget draft is refused here: before its credential is opened,
    // before it is rendered and before Decide is consulted.
    let draft = mail::materialize_draft(
        &views.mail.reader,
        &views.mail.facts,
        &views.files.facts,
        draft_id,
        attachment_limit,
    )?;
    let account = mail::open_account(
        &views.mail.reader,
        &views.mail.facts,
        &views.secrets,
        record.account,
        &storage.signer,
    )?;
    let envelope_from = draft.envelope_from.clone();
    let (to, cc, bcc) = (draft.to.clone(), draft.cc.clone(), draft.bcc.clone());
    let rendered = mail::render_draft(draft, &account)?;
    let (decision, heads) =
        mail::authorized_send(&views.decide.reader, &views.decide.facts, draft_id)?;
    let prepared = mail::prepare_send(
        &views.mail.reader,
        &views.decide.reader,
        &views.mail.facts,
        &views.files.facts,
        &views.decide.facts,
        SendAttemptInput {
            draft: draft_id,
            config: account.config,
            decision,
            decision_heads: heads,
            raw: rendered.raw,
            envelope_from,
            to,
            cc,
            bcc,
        },
        attachment_limit,
    )?;
    let attempt_id = prepared.attempt_id();
    // Any Files evidence needed by the post-effect outgoing projection is
    // durable before SMTP. Most drafts reuse already-published file values.
    if !prepared.outgoing_files().facts().is_empty() {
        storage.publish(
            storage.scopes.files,
            prepared.outgoing_files().clone(),
            "mail: outgoing attachment evidence",
        )?;
    }
    let mut submitter = connect(&account)?;
    let response = mail::submit_once(
        &mut submitter,
        &prepared,
        |fragment| {
            storage.publish(
                storage.scopes.mail,
                fragment.clone(),
                "mail: send attempt before SMTP",
            )
        },
        |fragment| {
            storage.publish(
                storage.scopes.mail,
                fragment.clone(),
                "mail: SMTP acceptance and outgoing evidence",
            )
        },
    )?;
    Ok(SendReceipt {
        draft: draft_id,
        attempt: attempt_id,
        status: SendStatus::Accepted(response),
    })
}

fn cmd_outbox(storage: &Storage) -> Result<Vec<DraftStatus>> {
    let views = storage.views()?;
    let drafts: BTreeSet<Id> = find!(
        id: Id,
        pattern!(&views.mail.facts, [{ ?id @ metadata::tag: &mail_schema::KIND_DRAFT_INTENT }])
    )
    .collect();
    let mut result = Vec::new();
    for id in drafts {
        // The outbox shows each draft's subject, so it reads the subject
        // alone and none of the draft's attachments.
        let draft = mail::draft_value(&views.mail.facts, id)?;
        let attempts = mail::attempts_for_draft(&views.mail.facts, id);
        let state = match attempts.as_slice() {
            [] => DraftDelivery::Pending,
            [attempt] if mail::acceptances_for_attempt(&views.mail.facts, *attempt).is_empty() => {
                DraftDelivery::Uncertain(*attempt)
            }
            [attempt] => DraftDelivery::Accepted(*attempt),
            _ => DraftDelivery::MultipleAttempts,
        };
        result.push(DraftStatus {
            draft: id,
            subject: mail::read_text(&views.mail.reader, draft.subject)?,
            delivery: state,
        });
    }
    Ok(result)
}

fn cmd_list(
    storage: &Storage,
    persona: &str,
    unread_only: bool,
    spam_only: bool,
) -> Result<Vec<InboxMessage>> {
    let views = storage.views()?;
    let persona = relation_persona(&views, persona)?;
    let mut result = Vec::new();
    for row in mail::inbox_projection(&views.mail.facts, &views.relations.facts, persona)? {
        if unread_only && !row.unread {
            continue;
        }
        let view = mail::projection_view(&views.mail.reader, &views.mail.facts, row.projection)?;
        if spam_only && !view.spam {
            continue;
        }
        result.push(InboxMessage {
            projection: view,
            unread: row.unread,
        });
    }
    Ok(result)
}

fn cmd_read(storage: &Storage, selector: &str, persona: &str) -> Result<ReadReceipt> {
    let views = storage.views()?;
    let wire = resolve_wire(&views.mail.facts, selector)?;
    let reader = relation_persona(&views, persona)?;
    let (fragment, id) = mail::read_observation_fragment(wire, reader);
    storage.publish(storage.scopes.mail, fragment, "mail: read observation")?;
    Ok(ReadReceipt {
        wire,
        observation: id,
        reader,
    })
}

fn cmd_show(storage: &Storage, selector: &str) -> Result<Vec<mail::ProjectionView>> {
    let views = storage.views()?;
    let wire = resolve_wire(&views.mail.facts, selector)?;
    let projections: BTreeSet<Id> = find!(
        id: Id,
        pattern!(&views.mail.facts, [
            { _?source @ observation::wire: &wire },
            { ?id @ projection::source: _?source, projection::recipe: &mail_schema::RECIPE_RFC5322_V1 }
        ])
    )
    .collect();
    if projections.is_empty() {
        bail!("wire message {wire:x} has no parser projection");
    }
    projections
        .into_iter()
        .map(|id| mail::projection_view(&views.mail.reader, &views.mail.facts, id))
        .collect()
}

fn cmd_search(storage: &Storage, query: &str) -> Result<Vec<mail::ProjectionView>> {
    let views = storage.views()?;
    let needle = query.to_lowercase();
    let projections: BTreeSet<Id> = find!(
        id: Id,
        pattern!(&views.mail.facts, [{ ?id @ metadata::tag: &mail_schema::KIND_PARSED_PROJECTION }])
    )
    .collect();
    let mut result = Vec::new();
    for id in projections {
        let view = mail::projection_view(&views.mail.reader, &views.mail.facts, id)?;
        if view.subject.to_lowercase().contains(&needle)
            || view.body.to_lowercase().contains(&needle)
        {
            result.push(view);
        }
    }
    Ok(result)
}

#[cfg(test)]
fn fragment_is_materialized(facts: &FactArchive, fragment: &Fragment) -> bool {
    fragment
        .facts()
        .iter()
        .all(|expected| facts.iter().any(|actual| &actual == expected))
}

fn publish_source_publication_with<P>(
    publication: &mail::SourcePublication,
    scopes: Scopes,
    mut publish: P,
) -> Result<()>
where
    P: FnMut(Id, Fragment, &str) -> Result<()>,
{
    // Both fragments were constructed locally by typed APIs. Publish Files
    // before Mail so every referenced attachment blob is durable before the
    // source observation that names it. Replaying either intrinsic fragment
    // is harmless and needs no derived-id lookup against the current union.
    if !publication.files.facts().is_empty() {
        publish(
            scopes.files,
            publication.files.clone(),
            "mail: IMAP attachment evidence",
        )?;
    }

    publish(
        scopes.mail,
        publication.mail.clone(),
        "mail: IMAP source evidence and parser projection",
    )?;

    Ok(())
}

fn cmd_fetch(storage: &Storage, request: FetchRequest) -> Result<Vec<AccountFetched>> {
    let views = storage.views()?;
    let mut accounts = Vec::new();
    for anchor in mail::account_anchors(&views.mail.facts) {
        let config_id = match mail::account_head(&views.mail.facts, anchor)? {
            Head::Unique(id) => id,
            Head::Missing => bail!("mail account {anchor:x} has no configuration"),
            Head::Forked(ids) => {
                bail!("mail account {anchor:x} has forked configuration heads: {ids:?}")
            }
        };
        let config = mail::account_config(&views.mail.facts, config_id)?;
        if !config.enabled {
            continue;
        }
        let Some(endpoint_handle) = config.imap_endpoint else {
            let address = mail::read_text(&views.mail.reader, config.address)?;
            bail!(
                "mail account {address} ({anchor:x}) has a POP-era configuration {config_id:x} with no IMAP endpoint and cannot fetch. POP was removed so that no mailbox is ever drained; re-set the account with `mail account set --account {anchor:x} --imap-endpoint <host>:993 ...` (MCP: mail_account_set with imap_endpoint)"
            );
        };
        let account = mail::open_account(
            &views.mail.reader,
            &views.mail.facts,
            &views.secrets,
            anchor,
            &storage.signer,
        )
        .with_context(|| format!("open IMAP account {anchor:x}"))?;
        accounts.push((account, endpoint_handle, config.username));
    }
    drop(views);

    let mut result = Vec::new();
    for (account, endpoint_handle, username_handle) in accounts {
        let endpoint_text = account
            .imap_endpoint
            .clone()
            .expect("an account with an IMAP endpoint handle opens with its text");
        let (host, port) = endpoint(&endpoint_text, "IMAP")?;
        let session =
            mail_imap::connect_implicit_tls(host, port, &account.username, &account.password)
                .with_context(|| format!("connect IMAP account {}", account.address))?;
        let mailboxes = mail::fetch_imap(
            session,
            account.anchor,
            account.config,
            &request.mailboxes,
            &request.criteria,
            |mailbox, uidvalidity| {
                // Asked once per mailbox at the point of use, so it also sees
                // what this fetch published in earlier mailboxes.
                let views = storage.views()?;
                Ok(mail::imap_held_uids(
                    &views.mail.facts,
                    account.anchor,
                    endpoint_handle,
                    username_handle,
                    mailbox,
                    uidvalidity,
                ))
            },
            |publication| {
                publish_source_publication_with(
                    publication,
                    storage.scopes,
                    |scope, fragment, description| storage.publish(scope, fragment, description),
                )
            },
        )
        .with_context(|| format!("fetch IMAP account {}", account.address))?;
        let fetched = mailboxes.iter().map(|mailbox| mailbox.fetched).sum();
        {
            let mut notices = storage.notices.borrow_mut();
            for mailbox in &mailboxes {
                for (uid, size) in &mailbox.oversized {
                    notices.push(format!(
                        "{} {:?}: UID {uid} is {size} bytes, over the {}-byte cap; left unread on the server",
                        account.address,
                        mailbox.mailbox,
                        mail::MAX_MESSAGE_BYTES
                    ));
                }
                for (uid, reason) in &mailbox.unparsed {
                    notices.push(format!(
                        "{} {:?}: UID {uid} was not recorded: {reason}",
                        account.address, mailbox.mailbox
                    ));
                }
            }
        }
        result.push(AccountFetched {
            account: account.anchor,
            config: account.config,
            address: account.address,
            fetched,
            mailboxes,
        });
    }
    Ok(result)
}

use crate::schemas::mail::{observation, projection};

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs::File;
    use std::rc::Rc;

    use crate::secrets::secret_rows;
    use crate::storage::open_secrets_collection;
    use crate::storage::{initialize_signer, publish_fragment};
    use triblespace::core::repo::StoreSnapshot;

    fn id(byte: u8) -> Id {
        Id::new([byte; 16]).unwrap()
    }

    fn scopes() -> Scopes {
        Scopes {
            mail: mail_schema::DEFAULT_SCOPE_ID,
            files: files_schema::DEFAULT_SCOPE_ID,
            decide: decide_schema::DEFAULT_SCOPE_ID,
            relations: relations_schema::DEFAULT_SCOPE_ID,
        }
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        pile: PathBuf,
        key: PathBuf,
        account: Id,
        config: Id,
        credential: Id,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let pile = directory.path().join("mail-cli.pile");
            let key = directory.path().join("mail-cli.key");
            File::create(&pile).unwrap();
            initialize_signer(&pile, Some(&key)).unwrap();

            let account = id(70);
            let signer = load_signer(&pile, Some(&key)).unwrap();
            let mut store = open_pile_strict(&pile).unwrap();
            let collection = open_secrets_collection(&mut store, signer.verifying_key()).unwrap();
            let credential_id = secret_storage::add_secret(
                &mut store,
                &signer,
                collection,
                &mailbox_secret_name(account),
                b"mailbox password",
                point_now().unwrap(),
            )
            .unwrap();
            store.close().unwrap();
            let mut fragment = Fragment::empty();
            let (config_fragment, config) = mail::account_config_fragment(
                account,
                AccountConfigInput {
                    address: "me@example.test".into(),
                    display_name: "Me".into(),
                    imap_endpoint: "imap.example.test:993".into(),
                    smtp_endpoint: "smtp.example.test:465".into(),
                    username: "me@example.test".into(),
                    credential: credential_id,
                    enabled: true,
                    quiet: false,
                    predecessors: Vec::new(),
                },
            )
            .unwrap();
            fragment += config_fragment;
            publish_fragment(&pile, Some(&key), mail_schema::DEFAULT_SCOPE_ID, fragment).unwrap();

            let fixture = Self {
                _directory: directory,
                pile,
                key,
                account,
                config,
                credential: credential_id,
            };
            let storage = fixture.storage();
            storage.views().unwrap();
            storage.close().unwrap();
            fixture
        }

        fn storage(&self) -> Storage {
            Storage::open(&self.pile, Some(&self.key), scopes()).unwrap()
        }
    }

    fn raw(message_id: &str) -> Vec<u8> {
        format!(
            "From: Sender <sender@example.test>\r\nTo: me@example.test\r\nMessage-ID: <{message_id}>\r\nDate: Sat, 8 Aug 2026 00:00:01 +0000\r\nSubject: Hello\r\nContent-Type: multipart/mixed; boundary=test\r\n\r\n--test\r\nContent-Type: text/plain\r\n\r\nbody\r\n--test\r\nContent-Type: application/octet-stream; name=note.bin\r\nContent-Disposition: attachment; filename=note.bin\r\nContent-Transfer-Encoding: base64\r\n\r\nAQID\r\n--test--\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn a_mail_action_commits_every_written_scope_for_a_preparing_reader() {
        let fixture = Fixture::new();
        let marker = *fucid();
        let facade = Mail::new(fixture.pile.clone(), Some(fixture.key.clone()));
        facade
            .with_operation(|storage| {
                storage.publish(
                    storage.scopes.mail,
                    entity! { metadata::tag: &marker },
                    "first",
                )?;
                storage.publish(
                    storage.scopes.files,
                    entity! { metadata::tag: &marker },
                    "attachment",
                )?;
                Ok(())
            })
            .unwrap();
        let signer = load_signer(&fixture.pile, Some(&fixture.key)).unwrap();
        let mut pile = open_pile_strict_as(&fixture.pile, signer.verifying_key()).unwrap();
        for scope in [scopes().mail, scopes().files] {
            let source = open_configured(&mut pile, scope, signer.verifying_key()).unwrap();
            let succinct = pile.attach::<SuccinctArchiveBlob>(source, ()).unwrap();
            let rank9 = pile
                .attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)
                .unwrap();
            let facts = pollster::block_on(async {
                drop(pile.maintain_attached(succinct, &signer).await.unwrap());
                pile.maintain_attached(rank9, &signer).await
            })
            .unwrap()
            .read_facts(rank9)
            .unwrap();
            assert!(exists!(
                pattern!(&facts, [{ _?event @ metadata::tag: &marker }])
            ));
        }
        pile.close().unwrap();
    }

    #[test]
    fn failed_mail_action_preserves_and_carries_its_committed_prefix() {
        let fixture = Fixture::new();
        let marker = *fucid();
        let facade = Mail::new(fixture.pile.clone(), Some(fixture.key.clone()));
        let error = facade
            .with_operation(|storage| {
                storage.publish(
                    storage.scopes.mail,
                    entity! { metadata::tag: &marker },
                    "committed prefix",
                )?;
                Err::<(), _>(anyhow!("injected later action failure"))
            })
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("facts were committed"));
        assert!(text.contains("injected later action failure"));
        let signer = load_signer(&fixture.pile, Some(&fixture.key)).unwrap();
        let mut pile = open_pile_strict_as(&fixture.pile, signer.verifying_key()).unwrap();
        let source = open_configured(&mut pile, scopes().mail, signer.verifying_key()).unwrap();
        let succinct = pile.attach::<SuccinctArchiveBlob>(source, ()).unwrap();
        let rank9 = pile
            .attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)
            .unwrap();
        let facts = pollster::block_on(async {
            drop(pile.maintain_attached(succinct, &signer).await.unwrap());
            pile.maintain_attached(rank9, &signer).await
        })
        .unwrap()
        .read_facts(rank9)
        .unwrap();
        assert!(exists!(
            pattern!(&facts, [{ _?event @ metadata::tag: &marker }])
        ));
        pile.close().unwrap();
    }

    #[test]
    fn views_share_the_final_secrets_snapshot() {
        let fixture = Fixture::new();
        let storage = fixture.storage();

        // Leave one secret commit without its maintained representations. This
        // makes Secrets attachment advance the pile after the ordinary Mail
        // collections have been maintained and catches cross-watermark views.
        storage
            .add_secret("snapshot-regression", b"new secret")
            .unwrap();

        let views = storage.views().unwrap();
        let secrets_snapshot = views.secrets.store_snapshot();
        for reader in [
            &views.mail.reader,
            &views.files.reader,
            &views.decide.reader,
            &views.relations.reader,
        ] {
            assert!(reader.changes_since(secrets_snapshot).is_empty());
            assert!(secrets_snapshot.changes_since(reader).is_empty());
        }
    }

    #[test]
    fn account_config_update_reuses_exact_secret_without_opening_it() {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        let before = storage.views().unwrap();
        let secrets_before = secret_rows(before.secrets.facts().unwrap());

        account_set(
            &storage,
            Some(format!("{:x}", fixture.account)),
            "me@example.test".into(),
            "Renamed".into(),
            "imap.example.test:993".into(),
            "smtp.example.test:465".into(),
            None,
            None,
            None,
            false,
            false,
        )
        .unwrap();

        let after = storage.views().unwrap();
        assert_eq!(secret_rows(after.secrets.facts().unwrap()), secrets_before);
        let head = match mail::account_head(&after.mail.facts, fixture.account).unwrap() {
            Head::Unique(id) => id,
            other => panic!("expected unique account head, got {other:?}"),
        };
        assert_ne!(head, fixture.config);
        assert_eq!(
            mail::account_config(&after.mail.facts, head)
                .unwrap()
                .credential,
            fixture.credential
        );
    }

    #[test]
    fn supplied_password_always_seals_a_fresh_version_before_mail() {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        let before = storage.views().unwrap();
        let versions_before = secret_rows(before.secrets.facts().unwrap()).len();

        account_set(
            &storage,
            Some(format!("{:x}", fixture.account)),
            "me@example.test".into(),
            "Me".into(),
            "imap.example.test:993".into(),
            "smtp.example.test:465".into(),
            None,
            Some("mailbox password".into()),
            None,
            false,
            false,
        )
        .unwrap();

        let after = storage.views().unwrap();
        assert_eq!(
            secret_rows(after.secrets.facts().unwrap()).len(),
            versions_before + 1
        );
        let head = match mail::account_head(&after.mail.facts, fixture.account).unwrap() {
            Head::Unique(id) => id,
            other => panic!("expected unique account head, got {other:?}"),
        };
        let credential = mail::account_config(&after.mail.facts, head)
            .unwrap()
            .credential;
        assert_ne!(credential, fixture.credential);
        assert!(after.secrets.contains(credential));
    }

    #[test]
    fn interrupted_secrets_first_update_has_an_exact_repair_path() {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        let credential = storage
            .add_secret(
                &mailbox_secret_name(fixture.account),
                b"replacement password",
            )
            .unwrap();
        let views = storage.views().unwrap();
        let versions = secret_rows(views.secrets.facts().unwrap()).len();
        drop(views);

        account_set(
            &storage,
            Some(format!("{:x}", fixture.account)),
            "  me@example.test  ".into(),
            "  Me  ".into(),
            "  imap.example.test:993  ".into(),
            "  smtp.example.test:465  ".into(),
            None,
            None,
            Some(credential),
            false,
            false,
        )
        .unwrap();

        let after = storage.views().unwrap();
        assert_eq!(secret_rows(after.secrets.facts().unwrap()).len(), versions);
        let head = match mail::account_head(&after.mail.facts, fixture.account).unwrap() {
            Head::Unique(id) => id,
            other => panic!("expected unique account head, got {other:?}"),
        };
        assert_eq!(
            mail::account_config(&after.mail.facts, head)
                .unwrap()
                .credential,
            credential
        );
        let mail_after_first_repair = after.mail.facts.iter().collect::<Vec<_>>();
        drop(after);
        account_set(
            &storage,
            Some(format!("{:x}", fixture.account)),
            "  me@example.test  ".into(),
            "  Me  ".into(),
            "  imap.example.test:993  ".into(),
            "  smtp.example.test:465  ".into(),
            None,
            None,
            Some(credential),
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            storage
                .views()
                .unwrap()
                .mail
                .facts
                .iter()
                .collect::<Vec<_>>(),
            mail_after_first_repair
        );
    }

    #[test]
    fn invalid_account_input_does_not_publish_the_staged_secret() {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        let views = storage.views().unwrap();
        let secrets_before = secret_rows(views.secrets.facts().unwrap());
        drop(views);

        let error = account_set(
            &storage,
            Some(format!("{:x}", fixture.account)),
            "   ".into(),
            "Me".into(),
            "imap.example.test:993".into(),
            "smtp.example.test:465".into(),
            None,
            Some("replacement password".into()),
            None,
            false,
            false,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("account address"));
        let after = storage.views().unwrap();
        assert_eq!(secret_rows(after.secrets.facts().unwrap()), secrets_before);
    }

    #[test]
    fn new_account_uses_the_configured_secrets_collection() {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        account_set(
            &storage,
            None,
            "other@example.test".into(),
            "Other".into(),
            "imap.example.test:993".into(),
            "smtp.example.test:465".into(),
            None,
            Some("new password".into()),
            None,
            false,
            false,
        )
        .unwrap();
        assert_eq!(
            mail::account_anchors(&storage.views().unwrap().mail.facts).len(),
            2
        );
        let views = storage.views().unwrap();
        assert_eq!(
            resolve_account(&views, "me@example.test").unwrap(),
            fixture.account
        );
        assert_ne!(
            resolve_account(&views, "other@example.test").unwrap(),
            fixture.account
        );
    }

    #[test]
    fn fetch_skips_disabled_accounts_before_credential_open() {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        account_set(
            &storage,
            Some(format!("{:x}", fixture.account)),
            "me@example.test".into(),
            "Me".into(),
            "imap.example.test:993".into(),
            "smtp.example.test:465".into(),
            None,
            None,
            None,
            true,
            false,
        )
        .unwrap();

        // Disabled accounts do not participate in the all-enabled credential
        // opening preflight.
        cmd_fetch(&storage, FetchRequest::default()).unwrap();
    }

    #[test]
    fn disabled_send_refuses_before_credential_access() {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        cmd_draft(
            &storage,
            DraftRequest {
                account: format!("{:x}", fixture.account),
                to: vec!["recipient@example.test".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Disabled account".into(),
                body: "must not send".into(),
                attachments: Vec::new(),
            },
        )
        .unwrap();
        let views = storage.views().unwrap();
        let drafts: Vec<Id> = find!(
            draft: Id,
            pattern!(&views.mail.facts, [{ ?draft @ metadata::tag: &mail_schema::KIND_DRAFT_INTENT }])
        )
        .collect();
        assert_eq!(drafts.len(), 1);
        drop(views);

        account_set(
            &storage,
            Some(format!("{:x}", fixture.account)),
            "me@example.test".into(),
            "Me".into(),
            "imap.example.test:993".into(),
            "smtp.example.test:465".into(),
            None,
            None,
            None,
            true,
            false,
        )
        .unwrap();
        let error = cmd_send(&storage, &format!("{:x}", drafts[0])).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("disabled"), "{message}");
        assert!(!message.contains("open secret"), "{message}");
    }

    /// A test transport that records what it is given and accepts it. It
    /// never opens a connection.
    struct RecordingSmtp {
        submitted: Rc<RefCell<Vec<Vec<u8>>>>,
    }

    impl mail::SmtpSubmit for RecordingSmtp {
        fn submit(
            &mut self,
            _envelope: &mail::SmtpEnvelope,
            raw: &[u8],
        ) -> Result<mail::AcceptedReply> {
            self.submitted.borrow_mut().push(raw.to_vec());
            Ok(mail::AcceptedReply {
                code: 250,
                message: "accepted by the test transport".into(),
            })
        }
    }

    fn never_connect(_: &mail::OpenAccount) -> Result<RecordingSmtp> {
        panic!("an over-budget draft must not reach the SMTP transport")
    }

    fn draft_with_attachment(storage: &Storage, fixture: &Fixture, bytes: usize) -> Id {
        cmd_draft(
            storage,
            DraftRequest {
                account: format!("{:x}", fixture.account),
                to: vec!["recipient@example.test".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Large attachment".into(),
                body: "attached".into(),
                attachments: vec![DraftAttachment::Resident(mail::AttachmentData {
                    filename: "payload.bin".into(),
                    media_type: "application/octet-stream".into(),
                    bytes: vec![42; bytes],
                })],
            },
        )
        .unwrap()
        .draft
    }

    fn pile_len(fixture: &Fixture) -> u64 {
        std::fs::metadata(&fixture.pile).unwrap().len()
    }

    fn assert_over_budget(error: &anyhow::Error, draft: Id, total: u64, limit: u64) {
        let refused = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<mail::DraftOverBudget>())
            .unwrap_or_else(|| panic!("not an attachment budget refusal: {error:#}"));
        assert_eq!(
            refused,
            &mail::DraftOverBudget {
                draft,
                attachments: 1,
                total,
                complete: true,
                limit,
            }
        );
    }

    #[test]
    fn an_over_budget_draft_is_refused_before_decide_rendering_and_transport() {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        let draft = draft_with_attachment(&storage, &fixture, 1000);
        let selector = format!("{draft:x}");

        // No send resolution exists yet, so consulting Decide would fail
        // with "has no resolution": the budget refusal comes before it.
        storage.views().unwrap();
        let before = pile_len(&fixture);
        let error = cmd_send_with(&storage, &selector, 999, never_connect).unwrap_err();
        assert_over_budget(&error, draft, 1000, 999);
        assert!(!format!("{error:#}").contains("resolution"));
        assert_eq!(pile_len(&fixture), before, "a refused send published");

        // Authorized, the draft is still refused over budget: nothing is
        // published and the transport is never created.
        let (resolution, _) = crate::decide::resolution_fragment(
            mail::draft_decision_id(draft),
            "send",
            None,
            true,
            &[],
            &[],
            point_now().unwrap(),
        )
        .unwrap();
        storage
            .publish(storage.scopes.decide, resolution, "test: authorize send")
            .unwrap();
        storage.views().unwrap();
        let before = pile_len(&fixture);
        let error = cmd_send_with(&storage, &selector, 999, never_connect).unwrap_err();
        assert_over_budget(&error, draft, 1000, 999);
        assert_eq!(pile_len(&fixture), before, "a refused send published");
        let views = storage.views().unwrap();
        assert!(mail::attempts_for_draft(&views.mail.facts, draft).is_empty());
        drop(views);

        // Within the budget the same draft reaches the test transport once.
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let receipt = cmd_send_with(&storage, &selector, 1000, |_| {
            Ok(RecordingSmtp {
                submitted: submitted.clone(),
            })
        })
        .unwrap();
        assert!(matches!(receipt.status, SendStatus::Accepted(_)));
        assert_eq!(submitted.borrow().len(), 1);
        let views = storage.views().unwrap();
        assert_eq!(
            mail::attempts_for_draft(&views.mail.facts, draft),
            vec![receipt.attempt]
        );
    }

    #[test]
    fn the_outbox_reads_no_attachment_bytes() {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        // A Files record whose blobs this pile does not hold: the outbox
        // can list a draft naming it only if it reads none of its bytes.
        let file = files::stage(vec![9; 64], "absent.bin", "application/octet-stream").unwrap();
        let file_id = file.root().unwrap();
        storage
            .publish(
                storage.scopes.files,
                Fragment::new(file.exports(), file.facts().clone()),
                "test: a file without its blobs",
            )
            .unwrap();
        let draft = cmd_draft(
            &storage,
            DraftRequest {
                account: format!("{:x}", fixture.account),
                to: vec!["recipient@example.test".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Absent attachment".into(),
                body: "attached".into(),
                attachments: vec![DraftAttachment::File(file_id)],
            },
        )
        .unwrap()
        .draft;

        let outbox = cmd_outbox(&storage).unwrap();
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].draft, draft);
        assert_eq!(outbox[0].subject, "Absent attachment");
        assert!(matches!(outbox[0].delivery, DraftDelivery::Pending));
    }

    #[test]
    fn reply_to_digest_only_wire_omits_remote_thread_headers() {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        let publication = mail::imap_publication(
            fixture.account,
            fixture.config,
            "INBOX",
            1,
            1,
            b"From: Sender <sender@example.test>\r\nTo: me@example.test\r\nSubject: No remote identity\r\n\r\nbody",
        )
        .unwrap();
        let wire = publication.wire;
        publish_source_publication_with(
            &publication,
            storage.scopes,
            |scope, fragment, description| storage.publish(scope, fragment, description),
        )
        .unwrap();

        cmd_reply(
            &storage,
            ReplyRequest {
                message: format!("{wire:x}"),
                account: format!("{:x}", fixture.account),
                body: "reply without invented Message-ID".into(),
            },
        )
        .unwrap();

        let views = storage.views().unwrap();
        let drafts: Vec<Id> = find!(
            draft: Id,
            pattern!(&views.mail.facts, [{ ?draft @ metadata::tag: &mail_schema::KIND_DRAFT_INTENT }])
        )
        .collect();
        assert_eq!(drafts.len(), 1);
        let draft = mail::draft_value(&views.mail.facts, drafts[0]).unwrap();
        assert!(draft.in_reply_to.is_empty());
        assert!(draft.references.is_empty());
    }

    /// A scripted read-only IMAP server: mailboxes of (UIDVALIDITY, messages).
    /// It records every call so tests can see exactly what a fetch asked for.
    struct FakeImap {
        events: Rc<RefCell<Vec<String>>>,
        mailboxes: Vec<(String, u32, Vec<(u32, Vec<u8>)>)>,
        examined: Option<usize>,
    }

    impl mail::ImapSource for FakeImap {
        fn mailboxes(&mut self) -> Result<Vec<String>> {
            self.events.borrow_mut().push("list".into());
            Ok(self
                .mailboxes
                .iter()
                .map(|(name, ..)| name.clone())
                .collect())
        }

        fn examine(&mut self, mailbox: &str) -> Result<u32> {
            self.events.borrow_mut().push(format!("examine:{mailbox}"));
            let index = self
                .mailboxes
                .iter()
                .position(|(name, ..)| name == mailbox)
                .ok_or_else(|| anyhow!("scripted NO for {mailbox}"))?;
            self.examined = Some(index);
            Ok(self.mailboxes[index].1)
        }

        fn search(&mut self, _criteria: &mail::SearchCriteria) -> Result<Vec<u32>> {
            let index = self.examined.expect("search after examine");
            Ok(self.mailboxes[index]
                .2
                .iter()
                .map(|(uid, _)| *uid)
                .collect())
        }

        fn sizes(&mut self, uids: &[u32]) -> Result<Vec<(u32, u64)>> {
            self.events.borrow_mut().push(format!("sizes:{uids:?}"));
            let index = self.examined.expect("sizes after examine");
            Ok(self.mailboxes[index]
                .2
                .iter()
                .filter(|(uid, _)| uids.contains(uid))
                .map(|(uid, raw)| (*uid, raw.len() as u64))
                .collect())
        }

        fn fetch(&mut self, uid: u32) -> Result<Option<Vec<u8>>> {
            self.events.borrow_mut().push(format!("fetch:{uid}"));
            let index = self.examined.expect("fetch after examine");
            Ok(self.mailboxes[index]
                .2
                .iter()
                .find(|(candidate, _)| *candidate == uid)
                .map(|(_, raw)| raw.clone()))
        }

        fn logout(self) -> Result<()> {
            self.events.borrow_mut().push("logout".into());
            Ok(())
        }
    }

    /// One fetch through the real pile-backed `held` query and publisher,
    /// exactly as `cmd_fetch` composes them, minus the socket.
    fn fetch_through(
        storage: &Storage,
        fixture: &Fixture,
        mailboxes: Vec<(&str, u32, Vec<(u32, Vec<u8>)>)>,
        selection: mail::MailboxSelection,
    ) -> (Vec<mail::MailboxFetched>, Vec<String>) {
        let events = Rc::new(RefCell::new(Vec::new()));
        let source = FakeImap {
            events: events.clone(),
            mailboxes: mailboxes
                .into_iter()
                .map(|(name, validity, messages)| (name.to_owned(), validity, messages))
                .collect(),
            examined: None,
        };
        let views = storage.views().unwrap();
        let config = mail::account_config(&views.mail.facts, fixture.config).unwrap();
        drop(views);
        let reports = mail::fetch_imap(
            source,
            fixture.account,
            fixture.config,
            &selection,
            &mail::SearchCriteria::default(),
            |mailbox, uidvalidity| {
                let views = storage.views()?;
                Ok(mail::imap_held_uids(
                    &views.mail.facts,
                    fixture.account,
                    config.imap_endpoint.unwrap(),
                    config.username,
                    mailbox,
                    uidvalidity,
                ))
            },
            |publication| {
                publish_source_publication_with(
                    publication,
                    storage.scopes,
                    |scope, fragment, description| storage.publish(scope, fragment, description),
                )
            },
        )
        .unwrap();
        let events = events.borrow().clone();
        (reports, events)
    }

    #[test]
    fn a_second_fetch_finds_everything_held_and_fetches_nothing() {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        let inbox = || {
            vec![(
                "INBOX",
                11,
                vec![(1, raw("one@example.test")), (2, raw("two@example.test"))],
            )]
        };
        let (reports, events) = fetch_through(&storage, &fixture, inbox(), Default::default());
        assert_eq!(reports[0].fetched, 2);
        assert_eq!(
            events,
            [
                "examine:INBOX",
                "sizes:[1, 2]",
                "fetch:1",
                "fetch:2",
                "logout"
            ]
        );
        let expected = mail::imap_publication(
            fixture.account,
            fixture.config,
            "INBOX",
            11,
            2,
            &raw("two@example.test"),
        )
        .unwrap();
        let views = storage.views().unwrap();
        assert!(fragment_is_materialized(&views.mail.facts, &expected.mail));
        assert!(fragment_is_materialized(
            &views.files.facts,
            &expected.files
        ));
        drop(views);

        let (reports, events) = fetch_through(&storage, &fixture, inbox(), Default::default());
        assert_eq!(
            (reports[0].matched, reports[0].held, reports[0].fetched),
            (2, 2, 0)
        );
        assert_eq!(events, ["examine:INBOX", "logout"]);

        // A new message arriving is the only thing the next fetch asks for.
        let mut grown = inbox();
        grown[0].2.push((3, raw("three@example.test")));
        let (reports, events) = fetch_through(&storage, &fixture, grown, Default::default());
        assert_eq!((reports[0].held, reports[0].fetched), (2, 1));
        assert_eq!(events, ["examine:INBOX", "sizes:[3]", "fetch:3", "logout"]);
    }

    #[test]
    fn a_changed_uidvalidity_refetches_into_a_new_namespace_and_keeps_the_old() {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        let message = raw("renumbered@example.test");
        let before =
            mail::imap_publication(fixture.account, fixture.config, "INBOX", 5, 9, &message)
                .unwrap();
        fetch_through(
            &storage,
            &fixture,
            vec![("INBOX", 5, vec![(9, message.clone())])],
            Default::default(),
        );

        let (reports, events) = fetch_through(
            &storage,
            &fixture,
            vec![("INBOX", 6, vec![(1, message.clone())])],
            Default::default(),
        );
        assert_eq!((reports[0].uidvalidity, reports[0].fetched), (6, 1));
        assert_eq!(events, ["examine:INBOX", "sizes:[1]", "fetch:1", "logout"]);
        let after =
            mail::imap_publication(fixture.account, fixture.config, "INBOX", 6, 1, &message)
                .unwrap();
        assert_eq!(after.wire, before.wire);
        let views = storage.views().unwrap();
        assert!(fragment_is_materialized(&views.mail.facts, &before.mail));
        assert!(fragment_is_materialized(&views.mail.facts, &after.mail));
        // One wire value, two observations of it: the inbox shows it once
        // per observation source, both resolving to the same message.
        let wires: BTreeSet<Id> = find!(
            wire: Id,
            pattern!(&views.mail.facts, [{ _?source @
                metadata::tag: &mail_schema::KIND_IMAP_OBSERVATION,
                observation::wire: ?wire,
            }])
        )
        .collect();
        assert_eq!(wires, BTreeSet::from([before.wire]));
    }

    #[test]
    fn all_mailboxes_are_listed_and_each_keeps_its_own_namespace() {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        let mailboxes = || {
            vec![
                ("INBOX", 1, vec![(1, raw("inbox@example.test"))]),
                ("Archive", 1, vec![(1, raw("archive@example.test"))]),
            ]
        };
        let (reports, events) =
            fetch_through(&storage, &fixture, mailboxes(), mail::MailboxSelection::All);
        assert_eq!(reports.iter().map(|r| r.fetched).sum::<usize>(), 2);
        assert_eq!(
            events,
            [
                "list",
                "examine:INBOX",
                "sizes:[1]",
                "fetch:1",
                "examine:Archive",
                "sizes:[1]",
                "fetch:1",
                "logout"
            ]
        );
        let (reports, _) = fetch_through(
            &storage,
            &fixture,
            mailboxes(),
            mail::MailboxSelection::Named(vec!["Archive".into()]),
        );
        assert_eq!(
            (reports.len(), reports[0].held, reports[0].fetched),
            (1, 1, 0)
        );
    }

    #[test]
    fn a_failed_mail_publication_is_retried_by_the_next_fetch() {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        let message = raw("retry@example.test");
        let source = FakeImap {
            events: Rc::new(RefCell::new(Vec::new())),
            mailboxes: vec![("INBOX".into(), 3, vec![(4, message.clone())])],
            examined: None,
        };
        let error = mail::fetch_imap(
            source,
            fixture.account,
            fixture.config,
            &Default::default(),
            &mail::SearchCriteria::default(),
            |_, _| Ok(BTreeSet::new()),
            |publication| {
                publish_source_publication_with(
                    publication,
                    storage.scopes,
                    |scope, fragment, description| {
                        if scope == storage.scopes.mail {
                            bail!("scripted mail publication failure");
                        }
                        storage.publish(scope, fragment, description)
                    },
                )
            },
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("scripted mail publication failure"));

        let (reports, _) = fetch_through(
            &storage,
            &fixture,
            vec![("INBOX", 3, vec![(4, message)])],
            Default::default(),
        );
        assert_eq!((reports[0].held, reports[0].fetched), (0, 1));
    }

    #[test]
    fn a_pop_era_account_refuses_to_fetch_with_a_repair_hint() {
        let fixture = Fixture::new();
        let storage = fixture.storage();
        let views = storage.views().unwrap();
        let current = mail::account_config(&views.mail.facts, fixture.config).unwrap();
        drop(views);
        // A legacy successor: the same snapshot with POP in place of IMAP.
        let mut fragment = Fragment::empty();
        let legacy = entity! {
            metadata::tag: &mail_schema::KIND_ACCOUNT_CONFIG,
            mail_schema::account::of: &fixture.account,
            mail_schema::account::address: current.address,
            mail_schema::account::display_name: current.display_name,
            mail_schema::account::pop_endpoint: fragment.put("pop.example.test:995".to_owned()),
            mail_schema::account::smtp_endpoint: current.smtp_endpoint,
            mail_schema::account::username: current.username,
            mail_schema::account::credential: &current.credential,
            mail_schema::account::enabled: true,
            metadata::supersedes: &fixture.config,
        };
        fragment += legacy;
        storage
            .publish(storage.scopes.mail, fragment, "legacy config")
            .unwrap();
        let error = cmd_fetch(&storage, FetchRequest::default()).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("POP-era configuration"), "{message}");
        assert!(message.contains("--imap-endpoint"), "{message}");
    }
}
