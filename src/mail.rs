//! Collection-native Mail values, projections, validation, and protocol seams.
//!
//! This module deliberately separates facts from effects.  Constructors
//! produce self-contained immutable fragments; callers validate the exact
//! collection union and durably publish those fragments before asking the
//! SMTP transport to perform an irreversible action. Receiving is read-only:
//! the IMAP seam can examine, search and peek, and nothing else.

pub mod cli;
pub mod mcp;
pub mod operations;
pub mod render;

pub use operations::{
    AccountFetched, AccountOptions, AccountSetReceipt, AccountState, AccountSummary,
    AttachmentListing, AttachmentState, DraftAttachment, DraftDelivery, DraftReceipt, DraftRequest,
    DraftStatus, FetchRequest, InboxMessage, Mail, ReadReceipt, ReplyRequest, SendReceipt,
    SendStatus,
};

use std::collections::{BTreeMap, BTreeSet};

use anybytes::View;
use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use lettre::message::{header, Mailbox, MultiPart, SinglePart};
use lettre::Message;
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::pile::PileSnapshot;
use triblespace::core::repo::{BlobStoreGet, BlobStoreMeta};
use triblespace::prelude::*;

use crate::decisions::{self, Resolution};
use crate::files;
use crate::relations::IdentityComponents;
use crate::schemas::files::{file as file_schema, KIND_FILE, KIND_MEDIA_TYPE};
use crate::schemas::mail::{
    acceptance, account, attachment_occurrence, attempt, draft, imported, imported_legacy,
    observation, projection, read, wire, IMPORT_DRAFT, IMPORT_RECEIVED, IMPORT_SENT,
    KIND_ACCOUNT_CONFIG, KIND_ATTACHMENT_OCCURRENCE, KIND_DRAFT_INTENT, KIND_IMAP_OBSERVATION,
    KIND_IMPORTED_OBSERVATION, KIND_MAIL_ACCOUNT, KIND_OUTGOING_OBSERVATION,
    KIND_PARSED_PROJECTION, KIND_POP_OBSERVATION, KIND_READ_OBSERVATION, KIND_SEND_ATTEMPT,
    KIND_SMTP_ACCEPTANCE, KIND_WIRE_MESSAGE, LEGACY_KIND_MESSAGE, LEGACY_KIND_SPAM,
    RECIPE_RFC5322_V1,
};
use crate::secrets::SecretsSnapshot;

pub type TextHandle = Inline<inlineencodings::Handle<blobencodings::UTF8String>>;
pub type BytesHandle = Inline<inlineencodings::Handle<blobencodings::RawBytes>>;
pub type DigestValue = Inline<inlineencodings::Hash<inlineencodings::Blake3>>;
pub type IntervalValue = Inline<inlineencodings::NsTAIInterval>;
pub type CountValue = Inline<inlineencodings::U256BE>;
pub type ArchiveHandle = Inline<inlineencodings::Handle<blobencodings::SimpleArchive>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountConfigInput {
    pub address: String,
    pub display_name: String,
    pub imap_endpoint: String,
    pub smtp_endpoint: String,
    pub username: String,
    pub credential: Id,
    pub enabled: bool,
    /// Inbound mail is stored but never becomes attention.
    pub quiet: bool,
    pub predecessors: Vec<Id>,
}

impl AccountConfigInput {
    /// Canonicalize the complete immutable account snapshot before comparing
    /// it with a predecessor or deriving its entity identity.
    pub fn canonicalized(mut self) -> Result<Self> {
        self.address = canonical_nonempty(self.address, "account address")?;
        self.display_name = canonical_nonempty(self.display_name, "display name")?;
        self.imap_endpoint = canonical_nonempty(self.imap_endpoint, "IMAP endpoint")?;
        self.smtp_endpoint = canonical_nonempty(self.smtp_endpoint, "SMTP endpoint")?;
        self.username = canonical_nonempty(self.username, "account username")?;
        self.predecessors = sorted_ids(self.predecessors);
        Ok(self)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountConfigRecord {
    pub id: Id,
    pub account: Id,
    pub address: TextHandle,
    pub display_name: TextHandle,
    /// Present only on configurations written before IMAP replaced POP.
    /// Kept so those snapshots stay readable; nothing fetches through it.
    pub pop_endpoint: Option<TextHandle>,
    /// Absent on legacy POP-era configurations, which cannot fetch until
    /// a successor configuration names an IMAP endpoint.
    pub imap_endpoint: Option<TextHandle>,
    pub smtp_endpoint: TextHandle,
    pub username: TextHandle,
    pub credential: Id,
    pub enabled: bool,
    /// False on every configuration without `account::quiet: true`.
    pub quiet: bool,
    pub predecessors: Vec<Id>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OpenAccount {
    pub anchor: Id,
    pub config: Id,
    pub address: String,
    pub display_name: String,
    pub imap_endpoint: Option<String>,
    pub smtp_endpoint: String,
    pub username: String,
    pub password: String,
    pub enabled: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Head {
    Missing,
    Unique(Id),
    Forked(Vec<Id>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttachmentData {
    pub filename: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

/// The most attachment bytes one materialization of a draft copies, summed
/// over every attachment it names. Sending materializes a draft twice, once
/// to render it and once to check the frozen attempt against it. `cmd_send`
/// materializes before it reads the send resolution. So when every
/// attachment's length is known locally, a draft over this budget is refused
/// there before any attachment is copied, before it is rendered and before
/// Decide is consulted. When a length is only known after fetching, earlier
/// in-budget attachments may already have been copied, and a peer fetch may
/// already have cached verified bytes, before the refusal. A direct call of
/// `prepare_send` consults Decide before its own budget check.
///
/// This bounds the attachment copies, not the process's memory: rendering
/// base64-encodes the copies, and [`materialize_draft`] says when a blob is
/// fetched before it can be charged.
///
/// It is the input budget of one MCP call,
/// [`crate::mcp::input::MAX_CALL_INPUT_BYTES`]. Mail has no message size
/// limit of its own; a provider's limit is the provider's to enforce at
/// submission.
pub const MAX_DRAFT_ATTACHMENT_BYTES: u64 = crate::mcp::input::MAX_CALL_INPUT_BYTES;

/// A draft whose attachments exceed the attachment budget of one
/// materialization. `total` sums every attachment whose length was known
/// when the draft was refused; `complete` says whether that was all of them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DraftOverBudget {
    pub draft: Id,
    pub attachments: usize,
    pub total: u64,
    pub complete: bool,
    pub limit: u64,
}

impl std::fmt::Display for DraftOverBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "draft {:x}: its {} attachments total {}{} bytes, over the {}-byte attachment budget of one draft; it was not rendered or sent",
            self.draft,
            self.attachments,
            if self.complete { "" } else { "at least " },
            self.total,
            self.limit,
        )
    }
}

impl std::error::Error for DraftOverBudget {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WireIdentity {
    /// An opaque identity explicitly claimed in exactly one Message-ID field.
    Claimed(String),
    /// A digest of the exact raw bytes, used only when Message-ID is absent.
    RawDigest(DigestValue),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedMessage {
    pub identity: WireIdentity,
    pub from: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub claimed_date: Option<IntervalValue>,
    pub in_reply_to: Vec<String>,
    pub references: Vec<String>,
    pub spam: bool,
    pub attachments: Vec<AttachmentData>,
}

#[derive(Debug)]
pub struct SourcePublication {
    pub mail: Fragment,
    pub files: Fragment,
    pub wire: Id,
    pub observation: Id,
    pub projection: Id,
}

/// Canonical publication recovered from one exact historical Mail record.
/// A draft without raw wire bytes intentionally has no parser projection.
#[derive(Debug)]
pub struct ImportedPublication {
    pub mail: Fragment,
    pub files: Fragment,
    pub wire: Id,
    pub observation: Id,
    pub projection: Option<Id>,
}

/// Strict structural view of one embedded historical Mail record.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImportedPayloadRecord {
    pub legacy_entity: Id,
    pub direction: Id,
    pub message_id: TextHandle,
    pub from: Option<Id>,
    pub to: Vec<Id>,
    pub cc: Vec<Id>,
    pub bcc: Vec<Id>,
    pub subject: TextHandle,
    pub body: TextHandle,
    pub in_reply_to: Vec<Id>,
    pub references: Vec<Id>,
    pub sent_at: Option<IntervalValue>,
    pub raw: Option<BytesHandle>,
    pub attachments: Vec<Id>,
    pub created_at: IntervalValue,
    pub spam: bool,
    pub tags: Vec<Id>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DraftInput {
    pub nonce: Id,
    pub account: Id,
    pub envelope_from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub attachments: Vec<Id>,
    pub in_reply_to: Vec<Id>,
    pub references: Vec<Id>,
    pub created_at: IntervalValue,
}

#[derive(Debug)]
pub struct DraftPublication {
    pub mail: Fragment,
    pub decisions: Fragment,
    pub draft: Id,
    pub decision: Id,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DraftRecord {
    pub id: Id,
    pub nonce: Id,
    pub account: Id,
    pub envelope_from: TextHandle,
    pub to: Vec<TextHandle>,
    pub cc: Vec<TextHandle>,
    pub bcc: Vec<TextHandle>,
    pub subject: TextHandle,
    pub body: TextHandle,
    pub attachments: Vec<Id>,
    pub in_reply_to: Vec<Id>,
    pub references: Vec<Id>,
    pub created_at: IntervalValue,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SendAttemptInput {
    pub draft: Id,
    pub config: Id,
    pub decision: Id,
    pub decision_heads: Vec<Id>,
    pub raw: Vec<u8>,
    pub envelope_from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SendAttemptRecord {
    pub id: Id,
    pub draft: Id,
    pub config: Id,
    pub decision: Id,
    pub decision_heads: Vec<Id>,
    pub raw: BytesHandle,
    pub envelope_from: TextHandle,
    pub to: Vec<TextHandle>,
    pub cc: Vec<TextHandle>,
    pub bcc: Vec<TextHandle>,
}

/// A locally validated, immutable SMTP effect plan.
///
/// Its fields are private so the exact attempt, envelope, wire bytes, and
/// outgoing evidence cannot be mixed with pieces from another plan between
/// validation and the irreversible transport call.
#[derive(Debug)]
pub struct PreparedSend {
    attempt: Fragment,
    attempt_id: Id,
    outgoing: SourcePublication,
    envelope: SmtpEnvelope,
    raw: Vec<u8>,
}

impl PreparedSend {
    pub fn attempt_fragment(&self) -> &Fragment {
        &self.attempt
    }

    pub fn attempt_id(&self) -> Id {
        self.attempt_id
    }

    pub fn outgoing_files(&self) -> &Fragment {
        &self.outgoing.files
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InboxProjection {
    pub wire: Id,
    pub projection: Id,
    pub source: Id,
    pub unread: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MaterializedDraft {
    pub id: Id,
    pub account: Id,
    pub envelope_from: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub attachments: Vec<AttachmentData>,
    pub in_reply_to: Vec<String>,
    pub references: Vec<String>,
    pub created_at: IntervalValue,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedMail {
    pub raw: Vec<u8>,
    pub envelope: SmtpEnvelope,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectionView {
    pub id: Id,
    pub source: Id,
    pub wire: Id,
    pub message_id: Option<String>,
    pub from: Option<String>,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub bcc: Vec<String>,
    pub subject: String,
    pub body: String,
    pub claimed_date: Option<IntervalValue>,
    pub in_reply_to: Vec<Id>,
    pub references: Vec<Id>,
    pub spam: bool,
    pub attachments: Vec<Id>,
}

/// The exact structural fields needed to decide and summarize inbox
/// attention, without reading a body, envelope address set, attachment, or
/// other parser payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProjectionSummaryRecord {
    pub id: Id,
    pub source: Id,
    pub wire: Id,
    pub from: Option<TextHandle>,
    pub subject: TextHandle,
    pub claimed_date: Option<IntervalValue>,
    pub spam: bool,
}

/// Transport meaning of the immutable source observation behind a parser
/// projection. This is source evidence, not mutable mailbox state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectionDirection {
    Received,
    Sent,
    Draft,
}

fn sorted_ids(values: impl IntoIterator<Item = Id>) -> Vec<Id> {
    let mut values: Vec<_> = values.into_iter().collect();
    values.sort_unstable();
    values.dedup();
    values
}

fn canonical_nonempty(value: impl Into<String>, field: &str) -> Result<String> {
    let value = value.into();
    let value = value.trim();
    if value.is_empty() || value.bytes().any(|byte| byte == 0) {
        bail!("{field} is empty or contains a NUL byte");
    }
    Ok(value.to_owned())
}

fn exact_text(value: impl Into<String>, field: &str) -> Result<String> {
    let value = value.into();
    if value.bytes().any(|byte| byte == 0) {
        bail!("{field} contains a NUL byte");
    }
    Ok(value)
}

fn point_interval(value: IntervalValue, field: &str) -> Result<()> {
    let (low, high): (i128, i128) = value
        .try_from_inline()
        .map_err(|error| anyhow!("decode {field}: {error:?}"))?;
    if low != high {
        bail!("{field} must be a point interval");
    }
    Ok(())
}

fn one<T: Ord>(mut values: BTreeSet<T>, field: &str) -> Result<Option<T>> {
    match values.len() {
        0 => Ok(None),
        1 => Ok(values.pop_first()),
        count => bail!("{field} is ambiguous ({count} distinct values)"),
    }
}

fn required<T: Ord>(values: BTreeSet<T>, field: &str) -> Result<T> {
    one(values, field)?.ok_or_else(|| anyhow!("missing {field}"))
}

fn ids_of_kind<P>(facts: &P, kind: Id) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    find!(id: Id, pattern!(facts, [{ ?id @ metadata::tag: &kind }])).collect()
}

/// Every canonical RFC-5322 parser projection in stable id order.
pub fn projection_ids<P>(facts: &P) -> Vec<Id>
where
    P: TriblePattern,
{
    ids_of_kind(facts, KIND_PARSED_PROJECTION)
        .into_iter()
        .collect()
}

/// Every immutable native draft intent in stable id order.
pub fn draft_ids<P>(facts: &P) -> Vec<Id>
where
    P: TriblePattern,
{
    ids_of_kind(facts, KIND_DRAFT_INTENT).into_iter().collect()
}

fn imported_payload_record(record: &ImportedPayloadRecord) -> Fragment {
    entity! { ExclusiveId::force_ref(&record.legacy_entity) @
        metadata::tag*: record.tags.iter(),
        metadata::created_at: record.created_at,
        imported_legacy::from?: record.from.as_ref(),
        imported_legacy::to*: record.to.iter(),
        imported_legacy::cc*: record.cc.iter(),
        imported_legacy::bcc*: record.bcc.iter(),
        imported_legacy::subject: record.subject,
        imported_legacy::body: record.body,
        imported_legacy::message_id: record.message_id,
        imported_legacy::in_reply_to*: record.in_reply_to.iter(),
        imported_legacy::reference*: record.references.iter(),
        imported_legacy::sent_at?: record.sent_at.as_ref(),
        imported_legacy::raw?: record.raw.as_ref(),
        imported_legacy::attachment*: record.attachments.iter(),
    }
}

/// Apply the historical one-shot direction rule exactly once at the import
/// boundary. Explicit direction evidence wins; conflicting evidence fails.
pub fn legacy_import_direction(facts: &TribleSet, legacy_entity: Id) -> Result<Id> {
    let tags: BTreeSet<Id> =
        find!(tag: Id, pattern!(facts, [{ legacy_entity @ metadata::tag: ?tag }])).collect();
    if tags.contains(&IMPORT_RECEIVED) && tags.contains(&IMPORT_SENT) {
        bail!("legacy Mail entity {legacy_entity:x} has conflicting direction tags");
    }
    if tags.contains(&IMPORT_RECEIVED) {
        return Ok(IMPORT_RECEIVED);
    }
    if tags.contains(&IMPORT_SENT) {
        return Ok(IMPORT_SENT);
    }
    let message = tags.contains(&LEGACY_KIND_MESSAGE);
    let draft = tags.contains(&IMPORT_DRAFT);
    match (message, draft) {
        (true, true) => Ok(IMPORT_SENT),
        (true, false) => Ok(IMPORT_RECEIVED),
        (false, true) => Ok(IMPORT_DRAFT),
        (false, false) => {
            bail!("legacy Mail entity {legacy_entity:x} is neither a message nor a draft")
        }
    }
}

fn imported_payload_union<Overlay: BlobStoreGet>(
    reader: &impl BlobStoreGet,
    overlay: Option<&Overlay>,
    facts: &TribleSet,
    legacy_entity: Id,
) -> Result<ImportedPayloadRecord> {
    let entities: BTreeSet<Id> = facts.iter().map(|fact| *fact.e()).collect();
    if entities != BTreeSet::from([legacy_entity]) {
        bail!(
            "imported Mail payload for {legacy_entity:x} contains {} entity subjects",
            entities.len()
        );
    }
    let direction = legacy_import_direction(facts, legacy_entity)?;
    let tags =
        sorted_ids(find!(tag: Id, pattern!(facts, [{ legacy_entity @ metadata::tag: ?tag }])));
    for tag in &tags {
        if ![
            LEGACY_KIND_MESSAGE,
            LEGACY_KIND_SPAM,
            IMPORT_DRAFT,
            IMPORT_RECEIVED,
            IMPORT_SENT,
        ]
        .contains(tag)
        {
            bail!("legacy Mail entity {legacy_entity:x} has unknown tag {tag:x}");
        }
    }

    let message_id = required(
        find!(v: TextHandle, pattern!(facts, [{ legacy_entity @ imported_legacy::message_id: ?v }]))
            .collect(),
        "legacy Message-ID",
    )?;
    let subject = required(
        find!(v: TextHandle, pattern!(facts, [{ legacy_entity @ imported_legacy::subject: ?v }]))
            .collect(),
        "legacy mail subject",
    )?;
    let body = required(
        find!(v: TextHandle, pattern!(facts, [{ legacy_entity @ imported_legacy::body: ?v }]))
            .collect(),
        "legacy mail body",
    )?;
    let created_at = required(
        find!(v: IntervalValue, pattern!(facts, [{ legacy_entity @ metadata::created_at: ?v }]))
            .collect(),
        "legacy mail creation time",
    )?;
    let sent_at = one(
        find!(v: IntervalValue, pattern!(facts, [{ legacy_entity @ imported_legacy::sent_at: ?v }]))
            .collect(),
        "legacy claimed send time",
    )?;
    let raw = one(
        find!(v: BytesHandle, pattern!(facts, [{ legacy_entity @ imported_legacy::raw: ?v }]))
            .collect(),
        "legacy raw message",
    )?;
    let record = ImportedPayloadRecord {
        legacy_entity,
        direction,
        message_id,
        from: one(
            find!(v: Id, pattern!(facts, [{ legacy_entity @ imported_legacy::from: ?v }]))
                .collect(),
            "legacy From relation",
        )?,
        to: sorted_ids(
            find!(v: Id, pattern!(facts, [{ legacy_entity @ imported_legacy::to: ?v }])),
        ),
        cc: sorted_ids(
            find!(v: Id, pattern!(facts, [{ legacy_entity @ imported_legacy::cc: ?v }])),
        ),
        bcc: sorted_ids(
            find!(v: Id, pattern!(facts, [{ legacy_entity @ imported_legacy::bcc: ?v }])),
        ),
        subject,
        body,
        in_reply_to: sorted_ids(
            find!(v: Id, pattern!(facts, [{ legacy_entity @ imported_legacy::in_reply_to: ?v }])),
        ),
        references: sorted_ids(
            find!(v: Id, pattern!(facts, [{ legacy_entity @ imported_legacy::reference: ?v }])),
        ),
        sent_at,
        raw,
        attachments: sorted_ids(
            find!(v: Id, pattern!(facts, [{ legacy_entity @ imported_legacy::attachment: ?v }])),
        ),
        created_at,
        spam: tags.contains(&LEGACY_KIND_SPAM),
        tags,
    };

    point_interval(record.created_at, "legacy mail creation time")?;
    if let Some(sent_at) = record.sent_at {
        point_interval(sent_at, "legacy claimed send time")?;
    }
    match record.direction {
        IMPORT_DRAFT if record.raw.is_some() || record.sent_at.is_some() => {
            bail!("legacy unsent draft {legacy_entity:x} carries sent/raw evidence")
        }
        IMPORT_RECEIVED | IMPORT_SENT if record.raw.is_none() || record.sent_at.is_none() => {
            bail!("legacy transmitted mail {legacy_entity:x} lacks sent/raw evidence")
        }
        _ => {}
    }

    let message_id_text = text_union(reader, overlay, record.message_id)?;
    canonical_message_id_value(&message_id_text)
        .with_context(|| format!("validate legacy Message-ID on {legacy_entity:x}"))?;
    exact_text(
        text_union(reader, overlay, record.subject)?,
        "legacy mail subject",
    )?;
    exact_text(
        text_union(reader, overlay, record.body)?,
        "legacy mail body",
    )?;
    if let Some(raw) = record.raw {
        bytes_union(reader, overlay, raw)?;
    }

    let current_identity = entity! { imported_legacy::message_id: record.message_id }
        .root()
        .expect("legacy Message-ID identity has one root");
    let historical_identity = triblespace::core::trible::intrinsic_entity_id_v1(vec![(
        imported_legacy::message_id.id(),
        record.message_id.raw,
    )]);
    // The first Mail implementation predated intrinsic entity ids and named a
    // message with the first 128 bits of BLAKE3(trim(Message-ID)).  This is an
    // imported identity epoch, not a current-catalog tolerance: prove the
    // exact historical algorithm over the resident Message-ID text, then
    // rebuild the observation under today's intrinsic identities.
    let historical_v0_digest = blake3::hash(message_id_text.trim().as_bytes());
    let historical_v0_identity = Id::new(
        historical_v0_digest.as_bytes()[..16]
            .try_into()
            .expect("BLAKE3 prefix is 16 bytes"),
    )
    .expect("BLAKE3 Message-ID prefix is non-nil");
    if legacy_entity != current_identity
        && legacy_entity != historical_identity
        && legacy_entity != historical_v0_identity
    {
        bail!(
            "legacy Mail entity {legacy_entity:x} is not derived from its exact Message-ID under the current ({current_identity:x}), historical-v1 ({historical_identity:x}), or historical-v0 ({historical_v0_identity:x}) rule"
        );
    }

    if imported_payload_record(&record).facts() != facts {
        bail!("legacy Mail entity {legacy_entity:x} is not an exact supported record");
    }
    Ok(record)
}

/// Strictly decode one exact historical payload from a resident pile.
pub fn imported_payload(
    reader: &impl BlobStoreGet,
    facts: &TribleSet,
    legacy_entity: Id,
) -> Result<ImportedPayloadRecord> {
    imported_payload_union(reader, None::<&PileSnapshot>, facts, legacy_entity)
}

// ── accounts ──────────────────────────────────────────────────────────────

fn account_anchor_record(anchor: Id) -> Fragment {
    entity! { ExclusiveId::force_ref(&anchor) @ metadata::tag: &KIND_MAIL_ACCOUNT }
}

fn account_config_record(record: &AccountConfigRecord) -> Fragment {
    entity! {
        metadata::tag: &KIND_ACCOUNT_CONFIG,
        account::of: &record.account,
        account::address: record.address,
        account::display_name: record.display_name,
        account::pop_endpoint?: record.pop_endpoint,
        account::imap_endpoint?: record.imap_endpoint,
        account::smtp_endpoint: record.smtp_endpoint,
        account::username: record.username,
        account::credential: &record.credential,
        account::enabled: record.enabled,
        account::quiet?: record.quiet.then_some(true),
        metadata::supersedes*: record.predecessors.iter(),
    }
}

/// Build one account anchor plus one intrinsic full-state configuration.
pub fn account_config_fragment(anchor: Id, input: AccountConfigInput) -> Result<(Fragment, Id)> {
    let input = input.canonicalized()?;
    let mut fragment = Fragment::empty();
    let record = AccountConfigRecord {
        id: anchor,
        account: anchor,
        address: fragment.put(input.address),
        display_name: fragment.put(input.display_name),
        pop_endpoint: None,
        imap_endpoint: Some(fragment.put(input.imap_endpoint)),
        smtp_endpoint: fragment.put(input.smtp_endpoint),
        username: fragment.put(input.username),
        credential: input.credential,
        enabled: input.enabled,
        quiet: input.quiet,
        predecessors: input.predecessors,
    };
    let config = account_config_record(&record);
    let id = config
        .root()
        .expect("account config has one intrinsic root");
    fragment += account_anchor_record(anchor);
    fragment += config;
    Ok((fragment, id))
}

pub fn account_anchors<P>(facts: &P) -> BTreeSet<Id>
where
    P: TriblePattern,
{
    ids_of_kind(facts, KIND_MAIL_ACCOUNT)
}

pub fn account_config<P>(facts: &P, id: Id) -> Result<AccountConfigRecord>
where
    P: TriblePattern,
{
    Ok(AccountConfigRecord {
        id,
        account: required(
            find!(v: Id, pattern!(facts, [{ id @ account::of: ?v }])).collect(),
            "account config owner",
        )?,
        address: required(
            find!(v: TextHandle, pattern!(facts, [{ id @ account::address: ?v }])).collect(),
            "account address",
        )?,
        display_name: required(
            find!(v: TextHandle, pattern!(facts, [{ id @ account::display_name: ?v }])).collect(),
            "account display name",
        )?,
        pop_endpoint: one(
            find!(v: TextHandle, pattern!(facts, [{ id @ account::pop_endpoint: ?v }])).collect(),
            "POP endpoint",
        )?,
        imap_endpoint: one(
            find!(v: TextHandle, pattern!(facts, [{ id @ account::imap_endpoint: ?v }])).collect(),
            "IMAP endpoint",
        )?,
        smtp_endpoint: required(
            find!(v: TextHandle, pattern!(facts, [{ id @ account::smtp_endpoint: ?v }])).collect(),
            "SMTP endpoint",
        )?,
        username: required(
            find!(v: TextHandle, pattern!(facts, [{ id @ account::username: ?v }])).collect(),
            "account username",
        )?,
        credential: required(
            find!(v: Id, pattern!(facts, [{ id @ account::credential: ?v }])).collect(),
            "account credential",
        )?,
        enabled: required(
            find!(v: bool, pattern!(facts, [{ id @ account::enabled: ?v }])).collect(),
            "account enabled flag",
        )?,
        quiet: exists!(pattern!(facts, [{ id @ account::quiet: true }])),
        predecessors: sorted_ids(
            find!(v: Id, pattern!(facts, [{ id @ metadata::supersedes: ?v }])),
        ),
    })
}

fn dag_heads(graph: &BTreeMap<Id, Vec<Id>>, label: &str) -> Result<Vec<Id>> {
    for (&id, predecessors) in graph {
        for predecessor in predecessors {
            if !graph.contains_key(predecessor) {
                bail!("{label} node {id:x} names missing predecessor {predecessor:x}");
            }
        }
    }
    fn visit(
        id: Id,
        graph: &BTreeMap<Id, Vec<Id>>,
        visiting: &mut BTreeSet<Id>,
        done: &mut BTreeSet<Id>,
        label: &str,
    ) -> Result<()> {
        if done.contains(&id) {
            return Ok(());
        }
        if !visiting.insert(id) {
            bail!("{label} contains a cycle through {id:x}");
        }
        for predecessor in &graph[&id] {
            visit(*predecessor, graph, visiting, done, label)?;
        }
        visiting.remove(&id);
        done.insert(id);
        Ok(())
    }
    let mut visiting = BTreeSet::new();
    let mut done = BTreeSet::new();
    for &id in graph.keys() {
        visit(id, graph, &mut visiting, &mut done, label)?;
    }
    let superseded: BTreeSet<_> = graph.values().flatten().copied().collect();
    Ok(graph
        .keys()
        .filter(|id| !superseded.contains(*id))
        .copied()
        .collect())
}

pub fn account_head<P>(facts: &P, anchor: Id) -> Result<Head>
where
    P: TriblePattern,
{
    let mut graph = BTreeMap::new();
    for id in ids_of_kind(facts, KIND_ACCOUNT_CONFIG) {
        let record = account_config(facts, id)?;
        if record.account == anchor {
            graph.insert(id, record.predecessors);
        }
    }
    let heads = dag_heads(&graph, &format!("account {anchor:x} configuration DAG"))?;
    Ok(match heads.as_slice() {
        [] => Head::Missing,
        [id] => Head::Unique(*id),
        _ => Head::Forked(heads),
    })
}

// ── RFC 5322 identity and projection ──────────────────────────────────────

fn header_values(parsed: &mailparse::ParsedMail<'_>, name: &str) -> Vec<String> {
    parsed
        .headers
        .iter()
        .filter(|header| header.get_key().eq_ignore_ascii_case(name))
        .map(|header| header.get_value())
        .collect()
}

fn canonical_message_id_value(raw: &str) -> Result<String> {
    let raw = raw.trim();
    if raw.is_empty() {
        bail!("empty Message-ID field");
    }
    let value = if raw.starts_with('<') || raw.ends_with('>') {
        if !(raw.starts_with('<') && raw.ends_with('>')) || raw.len() < 3 {
            bail!("malformed Message-ID field {raw:?}");
        }
        &raw[1..raw.len() - 1]
    } else {
        raw
    };
    if value.trim() != value
        || value.is_empty()
        || value.chars().any(char::is_whitespace)
        || value.contains(['<', '>'])
    {
        bail!("Message-ID must contain one opaque id, got {raw:?}");
    }
    Ok(value.to_owned())
}

/// Canonical opaque Message-ID claim, or a structurally distinct full
/// raw-byte digest when that header is absent.
pub fn canonical_wire_identity(
    parsed: &mailparse::ParsedMail<'_>,
    raw: &[u8],
) -> Result<WireIdentity> {
    let values = header_values(parsed, "Message-ID");
    match values.as_slice() {
        [] => Ok(WireIdentity::RawDigest(Inline::new(
            *blake3::hash(raw).as_bytes(),
        ))),
        [value] => canonical_message_id_value(value).map(WireIdentity::Claimed),
        _ => bail!("message has multiple Message-ID fields"),
    }
}

fn message_id_list(value: &str) -> Result<Vec<String>> {
    let mut values = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let end = after
            .find('>')
            .ok_or_else(|| anyhow!("unterminated message id in {value:?}"))?;
        values.push(canonical_message_id_value(&format!("<{}>", &after[..end]))?);
        rest = &after[end + 1..];
    }
    if values.is_empty() && !value.trim().is_empty() {
        values.push(canonical_message_id_value(value)?);
    }
    Ok(values)
}

fn message_ids_from_headers(parsed: &mailparse::ParsedMail<'_>, name: &str) -> Result<Vec<String>> {
    let mut values = Vec::new();
    for header in header_values(parsed, name) {
        values.extend(message_id_list(&header)?);
    }
    values.sort();
    values.dedup();
    Ok(values)
}

fn collect_parts(
    part: &mailparse::ParsedMail<'_>,
    plain: &mut Option<String>,
    html: &mut Option<String>,
    attachments: &mut Vec<AttachmentData>,
) {
    let media_type = part.ctype.mimetype.to_ascii_lowercase();
    let disposition = part.get_content_disposition();
    let is_attachment = matches!(
        disposition.disposition,
        mailparse::DispositionType::Attachment
    ) || disposition
        .params
        .get("filename")
        .is_some_and(|name| !name.is_empty());

    if media_type.starts_with("multipart/") {
        for child in &part.subparts {
            collect_parts(child, plain, html, attachments);
        }
    } else if is_attachment || !media_type.starts_with("text/") {
        if let Ok(bytes) = part.get_body_raw() {
            let filename = disposition
                .params
                .get("filename")
                .or_else(|| part.ctype.params.get("name"))
                .cloned()
                .unwrap_or_else(|| "attachment.bin".to_owned());
            attachments.push(AttachmentData {
                filename,
                media_type: files::normalize_media_type_or_default(&media_type),
                bytes,
            });
        }
    } else if media_type == "text/plain" && plain.is_none() {
        *plain = part.get_body().ok();
    } else if media_type == "text/html" && html.is_none() {
        *html = part.get_body().ok();
    }
}

/// Parse immutable source bytes without inventing clock or identity evidence.
pub fn parse_rfc5322(raw: &[u8]) -> Result<ParsedMessage> {
    let parsed = mailparse::parse_mail(raw).context("parse RFC 5322")?;
    let identity = canonical_wire_identity(&parsed, raw)?;
    let singular = |name: &str| -> Result<Option<String>> {
        let values = header_values(&parsed, name);
        match values.as_slice() {
            [] => Ok(None),
            [value] => Ok(Some(value.trim().to_owned())),
            _ => bail!("message has multiple {name} fields"),
        }
    };
    let claimed_date = singular("Date")?
        .map(|value| {
            let seconds = mailparse::dateparse(&value).context("parse claimed Date header")?;
            let epoch = Epoch::from_unix_seconds(seconds as f64);
            (epoch, epoch)
                .try_to_inline()
                .map_err(|error| anyhow!("encode claimed Date: {error:?}"))
        })
        .transpose()?;
    let mut plain = None;
    let mut html = None;
    let mut attachments = Vec::new();
    collect_parts(&parsed, &mut plain, &mut html, &mut attachments);
    let spam = header_values(&parsed, "X-Spam-Status")
        .iter()
        .any(|value| value.trim_start().to_ascii_lowercase().starts_with("yes"));
    Ok(ParsedMessage {
        identity,
        from: singular("From")?,
        to: header_values(&parsed, "To"),
        cc: header_values(&parsed, "Cc"),
        bcc: header_values(&parsed, "Bcc"),
        subject: singular("Subject")?.unwrap_or_default(),
        body: plain.or(html).unwrap_or_default(),
        claimed_date,
        in_reply_to: message_ids_from_headers(&parsed, "In-Reply-To")?,
        references: message_ids_from_headers(&parsed, "References")?,
        spam,
        attachments,
    })
}

/// The Reply-To field values of exact raw message bytes, one per field.
///
/// The `RECIPE_RFC5322_V1` projection does not carry Reply-To, and a recipe
/// never changes its output in place, so a reply reads it from the source
/// evidence at the point of use.
pub fn reply_to_claims(raw: &[u8]) -> Result<Vec<String>> {
    let (headers, _) = mailparse::parse_headers(raw).context("parse RFC 5322 headers")?;
    Ok(headers
        .iter()
        .filter(|header| header.get_key().eq_ignore_ascii_case("Reply-To"))
        .map(|header| header.get_value())
        .collect())
}

/// One mailbox of a mailbox-list claim: `rendered` is a single mailbox that
/// draft rendering parses back, `address` the bare address, for comparison.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplyMailbox {
    pub rendered: String,
    pub address: String,
}

/// Split mailbox-list claims (`a@x, B <b@y>`, groups) into single mailboxes,
/// in claim order.
pub fn reply_mailboxes(values: impl IntoIterator<Item = String>) -> Result<Vec<ReplyMailbox>> {
    let mut out = Vec::new();
    for value in values {
        let parsed = mailparse::addrparse(&value)
            .with_context(|| format!("parse mailbox claim {value:?}"))?;
        for address in parsed.iter() {
            let singles = match address {
                mailparse::MailAddr::Single(single) => std::slice::from_ref(single),
                mailparse::MailAddr::Group(group) => group.addrs.as_slice(),
            };
            for single in singles {
                let addr: lettre::Address = single
                    .addr
                    .parse()
                    .with_context(|| format!("parse mailbox address {:?}", single.addr))?;
                let name = single
                    .display_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .map(str::to_owned);
                out.push(ReplyMailbox {
                    rendered: Mailbox::new(name, addr).to_string(),
                    address: single.addr.clone(),
                });
            }
        }
    }
    Ok(out)
}

fn claimed_wire_record(message_id: TextHandle) -> Fragment {
    entity! { metadata::tag: &KIND_WIRE_MESSAGE, wire::claimed_message_id: message_id }
}

fn digest_wire_record(digest: DigestValue) -> Fragment {
    entity! { metadata::tag: &KIND_WIRE_MESSAGE, wire::raw_digest: digest }
}

fn add_claimed_wire(fragment: &mut Fragment, message_id: &str) -> Result<Id> {
    let message_id = fragment.put(canonical_message_id_value(message_id)?);
    let wire = claimed_wire_record(message_id);
    let id = wire.root().expect("claimed wire message has one root");
    *fragment += wire;
    Ok(id)
}

pub fn wire_id_for(message_id: &str) -> Result<Id> {
    let mut fragment = Fragment::empty();
    add_claimed_wire(&mut fragment, message_id)
}

fn attachment_occurrence_record(source: Id, ordinal: u64, file: Id) -> Fragment {
    entity! {
        metadata::tag: &KIND_ATTACHMENT_OCCURRENCE,
        attachment_occurrence::source: &source,
        attachment_occurrence::recipe: &RECIPE_RFC5322_V1,
        attachment_occurrence::ordinal: ordinal,
        attachment_occurrence::file: &file,
    }
}

#[allow(clippy::too_many_arguments)]
fn projection_record(
    source: Id,
    from: Option<TextHandle>,
    to: &[TextHandle],
    cc: &[TextHandle],
    bcc: &[TextHandle],
    subject: TextHandle,
    body: TextHandle,
    claimed_date: Option<IntervalValue>,
    in_reply_to: &[Id],
    references: &[Id],
    spam: bool,
    attachments: &[Id],
) -> Fragment {
    entity! {
        metadata::tag: &KIND_PARSED_PROJECTION,
        projection::source: &source,
        projection::recipe: &RECIPE_RFC5322_V1,
        projection::from?: from.as_ref(),
        projection::to*: to.iter(),
        projection::cc*: cc.iter(),
        projection::bcc*: bcc.iter(),
        projection::subject: subject,
        projection::body: body,
        projection::claimed_date?: claimed_date.as_ref(),
        projection::in_reply_to*: in_reply_to.iter(),
        projection::reference*: references.iter(),
        projection::spam: spam,
        projection::attachment*: attachments.iter(),
    }
}

fn finish_source_projection(
    mut mail_fragment: Fragment,
    parsed: ParsedMessage,
    wire: Id,
    observation: Id,
) -> Result<SourcePublication> {
    let mut files_fragment = Fragment::empty();
    let mut occurrence_ids = Vec::new();
    for (index, attachment) in parsed.attachments.iter().enumerate() {
        let file_fragment = files::stage(
            attachment.bytes.clone(),
            attachment.filename.clone(),
            &attachment.media_type,
        )?;
        let file = file_fragment.root().expect("canonical file root");
        files_fragment += file_fragment;
        let occurrence = attachment_occurrence_record(observation, index as u64, file);
        occurrence_ids.push(occurrence.root().expect("attachment occurrence root"));
        mail_fragment += occurrence;
    }

    let from = parsed.from.map(|value| mail_fragment.put(value));
    let to: Vec<TextHandle> = parsed
        .to
        .into_iter()
        .map(|value| mail_fragment.put(value))
        .collect();
    let cc: Vec<TextHandle> = parsed
        .cc
        .into_iter()
        .map(|value| mail_fragment.put(value))
        .collect();
    let bcc: Vec<TextHandle> = parsed
        .bcc
        .into_iter()
        .map(|value| mail_fragment.put(value))
        .collect();
    let subject = mail_fragment.put(parsed.subject);
    let body = mail_fragment.put(parsed.body);
    let mut in_reply_to = Vec::new();
    for value in &parsed.in_reply_to {
        in_reply_to.push(add_claimed_wire(&mut mail_fragment, value)?);
    }
    let mut references = Vec::new();
    for value in &parsed.references {
        references.push(add_claimed_wire(&mut mail_fragment, value)?);
    }
    let projection_fragment = projection_record(
        observation,
        from,
        &to,
        &cc,
        &bcc,
        subject,
        body,
        parsed.claimed_date,
        &in_reply_to,
        &references,
        parsed.spam,
        &occurrence_ids,
    );
    let projection = projection_fragment.root().expect("projection root");
    mail_fragment += projection_fragment;
    Ok(SourcePublication {
        mail: mail_fragment,
        files: files_fragment,
        wire,
        observation,
        projection,
    })
}

/// Where one source observation came from.
enum SourceCoordinates<'a> {
    /// Legacy POP evidence, only ever rebuilt for validation.
    Pop {
        account: Id,
        config: Id,
        uidl: &'a str,
    },
    Imap {
        account: Id,
        config: Id,
        mailbox: &'a str,
        uidvalidity: u32,
        uid: u32,
    },
    Outgoing {
        attempt: Id,
    },
}

fn source_publication(coordinates: SourceCoordinates<'_>, raw: &[u8]) -> Result<SourcePublication> {
    let parsed = parse_rfc5322(raw)?;
    let mut mail_fragment = Fragment::empty();
    let wire = match &parsed.identity {
        WireIdentity::Claimed(message_id) => add_claimed_wire(&mut mail_fragment, message_id)?,
        WireIdentity::RawDigest(digest) => {
            let wire_fragment = digest_wire_record(*digest);
            let wire = wire_fragment.root().expect("digest wire root");
            mail_fragment += wire_fragment;
            wire
        }
    };
    let raw_handle: BytesHandle = mail_fragment.put(raw.to_vec());
    let observation_fragment = match coordinates {
        SourceCoordinates::Pop {
            account: account_id,
            config: config_id,
            uidl,
        } => {
            let uidl = mail_fragment.put(canonical_nonempty(uidl, "POP UIDL")?);
            entity! {
                metadata::tag: &KIND_POP_OBSERVATION,
                observation::wire: &wire,
                observation::account: &account_id,
                observation::config: &config_id,
                observation::uidl: uidl,
                observation::raw: raw_handle,
            }
        }
        SourceCoordinates::Imap {
            account: account_id,
            config: config_id,
            mailbox,
            uidvalidity,
            uid,
        } => {
            if mailbox.is_empty() {
                bail!("IMAP mailbox name must not be empty");
            }
            if uidvalidity == 0 || uid == 0 {
                bail!("IMAP UIDVALIDITY and UID must be non-zero");
            }
            let mailbox = mail_fragment.put(mailbox.to_owned());
            entity! {
                metadata::tag: &KIND_IMAP_OBSERVATION,
                observation::wire: &wire,
                observation::account: &account_id,
                observation::config: &config_id,
                observation::mailbox: mailbox,
                observation::uidvalidity: u64::from(uidvalidity),
                observation::uid: u64::from(uid),
                observation::raw: raw_handle,
            }
        }
        SourceCoordinates::Outgoing {
            attempt: attempt_id,
        } => entity! {
            metadata::tag: &KIND_OUTGOING_OBSERVATION,
            observation::wire: &wire,
            observation::attempt: &attempt_id,
            observation::raw: raw_handle,
        },
    };
    let observation = observation_fragment.root().expect("observation root");
    mail_fragment += observation_fragment;
    finish_source_projection(mail_fragment, parsed, wire, observation)
}

/// Rebuild one legacy POP observation. Mail no longer fetches over POP; this
/// exists so evidence recorded before the IMAP cutover stays verifiable.
pub fn pop_publication(
    account: Id,
    config: Id,
    uidl: &str,
    raw: &[u8],
) -> Result<SourcePublication> {
    source_publication(
        SourceCoordinates::Pop {
            account,
            config,
            uidl,
        },
        raw,
    )
}

/// One message observed read-only over IMAP at (config, mailbox,
/// UIDVALIDITY, UID), with its exact raw bytes.
pub fn imap_publication(
    account: Id,
    config: Id,
    mailbox: &str,
    uidvalidity: u32,
    uid: u32,
    raw: &[u8],
) -> Result<SourcePublication> {
    source_publication(
        SourceCoordinates::Imap {
            account,
            config,
            mailbox,
            uidvalidity,
            uid,
        },
        raw,
    )
}

pub fn outgoing_publication(attempt: Id, raw: &[u8]) -> Result<SourcePublication> {
    source_publication(SourceCoordinates::Outgoing { attempt }, raw)
}

/// Rebuild one honest imported observation from an exact historical payload.
/// No POP mailbox, UIDL, account snapshot, send attempt, or SMTP acceptance is
/// invented. Raw-backed observations receive the same reproducible RFC parser
/// projection as native transport evidence; raw-less drafts remain imported
/// records with no fabricated modern intent/effect state.
pub fn imported_publication(
    legacy_entity: Id,
    direction: Id,
    payload: ArchiveHandle,
    message_id: &str,
    raw: Option<&[u8]>,
) -> Result<ImportedPublication> {
    if ![IMPORT_RECEIVED, IMPORT_SENT, IMPORT_DRAFT].contains(&direction) {
        bail!("unknown imported Mail direction {direction:x}");
    }
    if direction == IMPORT_DRAFT && raw.is_some() {
        bail!("an imported unsent draft cannot claim raw transport bytes");
    }
    if direction != IMPORT_DRAFT && raw.is_none() {
        bail!("imported transmitted mail requires exact raw bytes");
    }

    let canonical_message_id = canonical_message_id_value(message_id)?;
    let mut mail_fragment = Fragment::empty();
    let wire = add_claimed_wire(&mut mail_fragment, &canonical_message_id)?;
    let parsed = raw.map(parse_rfc5322).transpose()?;
    if let Some(parsed) = &parsed {
        match &parsed.identity {
            WireIdentity::Claimed(claimed) if claimed == &canonical_message_id => {}
            WireIdentity::Claimed(claimed) => bail!(
                "legacy Message-ID {canonical_message_id:?} differs from raw claim {claimed:?}"
            ),
            WireIdentity::RawDigest(_) => {
                bail!("legacy raw mail has no Message-ID matching its stored identity")
            }
        }
    }
    let raw_handle =
        raw.map(|bytes| mail_fragment.put::<blobencodings::RawBytes, _>(bytes.to_vec()));
    let observation_fragment = entity! {
        metadata::tag: &KIND_IMPORTED_OBSERVATION,
        imported::legacy_entity: &legacy_entity,
        imported::direction: &direction,
        imported::payload: payload,
        observation::wire: &wire,
        observation::raw?: raw_handle.as_ref(),
    };
    let observation = observation_fragment
        .root()
        .expect("imported observation has one root");
    mail_fragment += observation_fragment;

    if let Some(mut parsed) = parsed {
        // Historical Files identities predate media-type entities and are
        // preserved additively by the Files migration. Re-parsing the raw MIME
        // bytes would therefore mint a second set of modern file identities.
        // Keep the useful canonical message projection but leave attachments
        // on the exact embedded/top-level legacy evidence until a separate
        // cross-collection derivation explicitly maps that identity epoch.
        parsed.attachments.clear();
        let publication = finish_source_projection(mail_fragment, parsed, wire, observation)?;
        Ok(ImportedPublication {
            mail: publication.mail,
            files: publication.files,
            wire,
            observation,
            projection: Some(publication.projection),
        })
    } else {
        Ok(ImportedPublication {
            mail: mail_fragment,
            files: Fragment::empty(),
            wire,
            observation,
            projection: None,
        })
    }
}

// ── immutable draft / attempt / acceptance values ─────────────────────────

fn draft_record(record: &DraftRecord) -> Fragment {
    entity! {
        metadata::tag: &KIND_DRAFT_INTENT,
        draft::nonce: &record.nonce,
        draft::account: &record.account,
        draft::envelope_from: record.envelope_from,
        draft::to*: record.to.iter(),
        draft::cc*: record.cc.iter(),
        draft::bcc*: record.bcc.iter(),
        draft::subject: record.subject,
        draft::body: record.body,
        draft::attachment*: record.attachments.iter(),
        draft::in_reply_to*: record.in_reply_to.iter(),
        draft::reference*: record.references.iter(),
        metadata::created_at: record.created_at,
    }
}

pub fn draft_decision_id(draft_id: Id) -> Id {
    entity! { draft::decision_for: &draft_id }
        .root()
        .expect("draft decision derivation has one root")
}

pub fn draft_publication(input: DraftInput) -> Result<DraftPublication> {
    point_interval(input.created_at, "draft creation time")?;
    if input.to.is_empty() && input.cc.is_empty() && input.bcc.is_empty() {
        bail!("draft has no recipients");
    }
    let mut mail_fragment = Fragment::empty();
    let to = input
        .to
        .into_iter()
        .map(|value| Ok(mail_fragment.put(canonical_nonempty(value, "To recipient")?)))
        .collect::<Result<Vec<_>>>()?;
    let cc = input
        .cc
        .into_iter()
        .map(|value| Ok(mail_fragment.put(canonical_nonempty(value, "Cc recipient")?)))
        .collect::<Result<Vec<_>>>()?;
    let bcc = input
        .bcc
        .into_iter()
        .map(|value| Ok(mail_fragment.put(canonical_nonempty(value, "Bcc recipient")?)))
        .collect::<Result<Vec<_>>>()?;
    let record = DraftRecord {
        id: input.account,
        nonce: input.nonce,
        account: input.account,
        envelope_from: mail_fragment.put(canonical_nonempty(
            input.envelope_from,
            "draft envelope sender",
        )?),
        to,
        cc,
        bcc,
        subject: mail_fragment.put(exact_text(input.subject, "draft subject")?),
        body: mail_fragment.put(exact_text(input.body, "draft body")?),
        attachments: sorted_ids(input.attachments),
        in_reply_to: sorted_ids(input.in_reply_to),
        references: sorted_ids(input.references),
        created_at: input.created_at,
    };
    let draft_fragment = draft_record(&record);
    let draft_id = draft_fragment.root().expect("draft has one intrinsic root");
    mail_fragment += draft_fragment;
    let decision = draft_decision_id(draft_id);
    let title = format!(
        "Send mail: {}",
        read_local_text(&mail_fragment, record.subject)?
    );
    let (decisions_fragment, _) = decisions::decision_fragment(
        decision,
        title,
        Some("Authorize exactly this immutable DraftIntent".to_owned()),
        Some(draft_id),
        record.created_at,
    )?;
    Ok(DraftPublication {
        mail: mail_fragment,
        decisions: decisions_fragment,
        draft: draft_id,
        decision,
    })
}

fn attempt_record(record: &SendAttemptRecord) -> Fragment {
    entity! {
        metadata::tag: &KIND_SEND_ATTEMPT,
        attempt::draft: &record.draft,
        attempt::config: &record.config,
        attempt::decision: &record.decision,
        attempt::decision_head*: record.decision_heads.iter(),
        attempt::raw: record.raw,
        attempt::envelope_from: record.envelope_from,
        attempt::to*: record.to.iter(),
        attempt::cc*: record.cc.iter(),
        attempt::bcc*: record.bcc.iter(),
    }
}

pub fn send_attempt_fragment(input: SendAttemptInput) -> Result<(Fragment, Id)> {
    if input.to.is_empty() && input.cc.is_empty() && input.bcc.is_empty() {
        bail!("send attempt has no recipients");
    }
    let mut fragment = Fragment::empty();
    let record = SendAttemptRecord {
        id: input.draft,
        draft: input.draft,
        config: input.config,
        decision: input.decision,
        decision_heads: sorted_ids(input.decision_heads),
        raw: fragment.put(input.raw),
        envelope_from: fragment.put(canonical_nonempty(
            input.envelope_from,
            "attempt envelope sender",
        )?),
        to: input
            .to
            .into_iter()
            .map(|value| Ok(fragment.put(canonical_nonempty(value, "attempt To recipient")?)))
            .collect::<Result<_>>()?,
        cc: input
            .cc
            .into_iter()
            .map(|value| Ok(fragment.put(canonical_nonempty(value, "attempt Cc recipient")?)))
            .collect::<Result<_>>()?,
        bcc: input
            .bcc
            .into_iter()
            .map(|value| Ok(fragment.put(canonical_nonempty(value, "attempt Bcc recipient")?)))
            .collect::<Result<_>>()?,
    };
    let attempt = attempt_record(&record);
    let id = attempt.root().expect("send attempt has one intrinsic root");
    fragment += attempt;
    Ok((fragment, id))
}

pub fn smtp_acceptance_fragment(
    attempt_id: Id,
    response_code: u64,
    response: impl Into<String>,
) -> Result<(Fragment, Id)> {
    if !(200..=299).contains(&response_code) {
        bail!("SMTP acceptance code must be a final positive 2xx reply");
    }
    let mut fragment = Fragment::empty();
    let response = fragment.put(canonical_nonempty(response, "SMTP acceptance response")?);
    let acceptance = entity! {
        metadata::tag: &KIND_SMTP_ACCEPTANCE,
        acceptance::attempt: &attempt_id,
        acceptance::response_code: response_code,
        acceptance::response: response,
    };
    let id = acceptance.root().expect("SMTP acceptance has one root");
    fragment += acceptance;
    Ok((fragment, id))
}

/// Record that `reader` opened any resident wire value. Direction is not part
/// of read evidence; inbox projection separately restricts unread state to
/// inbound sources.
pub fn read_observation_fragment(wire_id: Id, reader: Id) -> (Fragment, Id) {
    let fragment = entity! {
        metadata::tag: &KIND_READ_OBSERVATION,
        read::wire: &wire_id,
        read::reader: &reader,
    };
    let id = fragment.root().expect("read observation has one root");
    (fragment, id)
}

fn read_local_text(fragment: &Fragment, handle: TextHandle) -> Result<String> {
    let mut local = fragment.clone();
    let reader = local
        .blobs_mut()
        .snapshot()
        .expect("memory blob reader creation is infallible");
    let value: View<str> = reader.get(handle).context("read staged Mail text")?;
    Ok(value.to_string())
}

pub fn read_text(reader: &impl BlobStoreGet, handle: TextHandle) -> Result<String> {
    let value: View<str> = reader
        .get(handle)
        .with_context(|| format!("read Mail text blob {}", hex::encode(handle.raw)))?;
    Ok(value.to_string())
}

fn text_union<Overlay: BlobStoreGet>(
    reader: &impl BlobStoreGet,
    overlay: Option<&Overlay>,
    handle: TextHandle,
) -> Result<String> {
    if let Ok(value) = reader.get::<View<str>, _>(handle) {
        return Ok(value.to_string());
    }
    if let Some(overlay) = overlay {
        let value: View<str> = overlay
            .get(handle)
            .with_context(|| format!("read staged Mail text {}", hex::encode(handle.raw)))?;
        return Ok(value.to_string());
    }
    bail!("missing Mail text blob {}", hex::encode(handle.raw))
}

fn bytes_union<Overlay: BlobStoreGet>(
    reader: &impl BlobStoreGet,
    overlay: Option<&Overlay>,
    handle: BytesHandle,
) -> Result<Vec<u8>> {
    if let Ok(value) = reader.get::<anybytes::Bytes, _>(handle) {
        return Ok(value.as_ref().to_vec());
    }
    if let Some(overlay) = overlay {
        let value: anybytes::Bytes = overlay
            .get(handle)
            .with_context(|| format!("read staged Mail bytes {}", hex::encode(handle.raw)))?;
        return Ok(value.as_ref().to_vec());
    }
    bail!("missing Mail byte blob {}", hex::encode(handle.raw))
}

/// The blobs of one attachment, selected by an ordinary typed read.
struct AttachmentHandles {
    content: BytesHandle,
    name: TextHandle,
    media_type: TextHandle,
}

/// Select one attachment's blobs without reading any of them.
///
/// The query itself selects the Files vocabulary this consumer understands.
/// Unlike import validation, this path neither reconstructs the intrinsic id
/// nor rejects additive facts that another version may understand.
fn file_attachment_handles<P>(facts: &P, file: Id) -> Result<AttachmentHandles>
where
    P: TriblePattern,
{
    let content = required(
        find!(value: BytesHandle, pattern!(facts, [{ file @
            metadata::tag: &KIND_FILE,
            file_schema::content: ?value,
        }]))
        .collect(),
        "file content",
    )?;
    let name = required(
        find!(value: TextHandle, pattern!(facts, [{ file @
            metadata::tag: &KIND_FILE,
            file_schema::name: ?value,
        }]))
        .collect(),
        "file name",
    )?;
    let media_type = required(
        find!(value: TextHandle, pattern!(facts, [
            { file @
                metadata::tag: &KIND_FILE,
                file_schema::media_type: _?media_type,
            },
            { _?media_type @
                metadata::tag: &KIND_MEDIA_TYPE,
                metadata::name: ?value,
            },
        ]))
        .collect(),
        "media type name",
    )?;
    Ok(AttachmentHandles {
        content,
        name,
        media_type,
    })
}

/// One Files record an inbound attachment occurrence names, with the
/// handles of its blobs. Selecting it reads no blob.
#[derive(Clone, Copy, Debug)]
pub struct OccurrenceFile {
    pub content: BytesHandle,
    pub name: TextHandle,
    pub media_type: TextHandle,
}

/// One attachment occurrence of a parser projection, as resident facts say.
/// `named_file` is the Files entity the occurrence names; `file` is `None`
/// when no Files record for that entity is resident.
#[derive(Clone, Copy, Debug)]
pub struct OccurrenceRow {
    pub occurrence: Id,
    pub ordinal: Option<u64>,
    pub named_file: Option<Id>,
    pub file: Option<OccurrenceFile>,
}

/// The attachment occurrences of one parser projection, ordered by ordinal.
///
/// Open world: every (ordinal, file) pair and every Files record the facts
/// hold yields its own row, and an occurrence whose facts or Files record
/// are not resident yields one row without them, never an error. Reads no
/// blob.
pub fn projection_attachment_rows<M, F>(
    mail_facts: &M,
    files_facts: &F,
    projection_id: Id,
) -> Vec<OccurrenceRow>
where
    M: TriblePattern,
    F: TriblePattern,
{
    let mut rows = Vec::new();
    let occurrences = sorted_ids(find!(
        v: Id,
        pattern!(mail_facts, [{ projection_id @ projection::attachment: ?v }])
    ));
    for occurrence in occurrences {
        let named: BTreeSet<(u64, Id)> = find!(
            (ordinal: u64, file: Id),
            pattern!(mail_facts, [{ occurrence @
                metadata::tag: &KIND_ATTACHMENT_OCCURRENCE,
                attachment_occurrence::ordinal: ?ordinal,
                attachment_occurrence::file: ?file,
            }])
        )
        .collect();
        if named.is_empty() {
            rows.push(OccurrenceRow {
                occurrence,
                ordinal: None,
                named_file: None,
                file: None,
            });
        }
        for (ordinal, file) in named {
            let records: BTreeSet<(BytesHandle, TextHandle, TextHandle)> = find!(
                (content: BytesHandle, name: TextHandle, media: TextHandle),
                pattern!(files_facts, [
                    { file @
                        metadata::tag: &KIND_FILE,
                        file_schema::content: ?content,
                        file_schema::name: ?name,
                        file_schema::media_type: _?media_type,
                    },
                    { _?media_type @
                        metadata::tag: &KIND_MEDIA_TYPE,
                        metadata::name: ?media,
                    },
                ])
            )
            .collect();
            if records.is_empty() {
                rows.push(OccurrenceRow {
                    occurrence,
                    ordinal: Some(ordinal),
                    named_file: Some(file),
                    file: None,
                });
            }
            for (content, name, media_type) in records {
                rows.push(OccurrenceRow {
                    occurrence,
                    ordinal: Some(ordinal),
                    named_file: Some(file),
                    file: Some(OccurrenceFile {
                        content,
                        name,
                        media_type,
                    }),
                });
            }
        }
    }
    rows.sort_by_key(|row| (row.ordinal.is_none(), row.ordinal, row.occurrence));
    rows
}

fn draft_from_facts<P>(facts: &P, id: Id) -> Result<DraftRecord>
where
    P: TriblePattern,
{
    Ok(DraftRecord {
        id,
        nonce: required(
            find!(v: Id, pattern!(facts, [{ id @ draft::nonce: ?v }])).collect(),
            "draft nonce",
        )?,
        account: required(
            find!(v: Id, pattern!(facts, [{ id @ draft::account: ?v }])).collect(),
            "draft account",
        )?,
        envelope_from: required(
            find!(v: TextHandle, pattern!(facts, [{ id @ draft::envelope_from: ?v }])).collect(),
            "draft envelope sender",
        )?,
        to: find!(v: TextHandle, pattern!(facts, [{ id @ draft::to: ?v }]))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        cc: find!(v: TextHandle, pattern!(facts, [{ id @ draft::cc: ?v }]))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        bcc: find!(v: TextHandle, pattern!(facts, [{ id @ draft::bcc: ?v }]))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        subject: required(
            find!(v: TextHandle, pattern!(facts, [{ id @ draft::subject: ?v }])).collect(),
            "draft subject",
        )?,
        body: required(
            find!(v: TextHandle, pattern!(facts, [{ id @ draft::body: ?v }])).collect(),
            "draft body",
        )?,
        attachments: sorted_ids(find!(v: Id, pattern!(facts, [{ id @ draft::attachment: ?v }]))),
        in_reply_to: sorted_ids(find!(v: Id, pattern!(facts, [{ id @ draft::in_reply_to: ?v }]))),
        references: sorted_ids(find!(v: Id, pattern!(facts, [{ id @ draft::reference: ?v }]))),
        created_at: required(
            find!(v: IntervalValue, pattern!(facts, [{ id @ metadata::created_at: ?v }])).collect(),
            "draft creation time",
        )?,
    })
}

pub fn draft_value<P>(facts: &P, id: Id) -> Result<DraftRecord>
where
    P: TriblePattern,
{
    if !ids_of_kind(facts, KIND_DRAFT_INTENT).contains(&id) {
        bail!("unknown draft {id:x}");
    }
    draft_from_facts(facts, id)
}

fn attempt_from_facts<P>(facts: &P, id: Id) -> Result<SendAttemptRecord>
where
    P: TriblePattern,
{
    Ok(SendAttemptRecord {
        id,
        draft: required(
            find!(v: Id, pattern!(facts, [{ id @ attempt::draft: ?v }])).collect(),
            "attempt draft",
        )?,
        config: required(
            find!(v: Id, pattern!(facts, [{ id @ attempt::config: ?v }])).collect(),
            "attempt config",
        )?,
        decision: required(
            find!(v: Id, pattern!(facts, [{ id @ attempt::decision: ?v }])).collect(),
            "attempt decision",
        )?,
        decision_heads: sorted_ids(
            find!(v: Id, pattern!(facts, [{ id @ attempt::decision_head: ?v }])),
        ),
        raw: required(
            find!(v: BytesHandle, pattern!(facts, [{ id @ attempt::raw: ?v }])).collect(),
            "attempt raw message",
        )?,
        envelope_from: required(
            find!(v: TextHandle, pattern!(facts, [{ id @ attempt::envelope_from: ?v }])).collect(),
            "attempt envelope sender",
        )?,
        to: find!(v: TextHandle, pattern!(facts, [{ id @ attempt::to: ?v }]))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        cc: find!(v: TextHandle, pattern!(facts, [{ id @ attempt::cc: ?v }]))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
        bcc: find!(v: TextHandle, pattern!(facts, [{ id @ attempt::bcc: ?v }]))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect(),
    })
}

pub fn send_attempt<P>(facts: &P, id: Id) -> Result<SendAttemptRecord>
where
    P: TriblePattern,
{
    if !ids_of_kind(facts, KIND_SEND_ATTEMPT).contains(&id) {
        bail!("unknown send attempt {id:x}");
    }
    attempt_from_facts(facts, id)
}

pub fn attempts_for_draft<P>(facts: &P, draft_id: Id) -> Vec<Id>
where
    P: TriblePattern,
{
    find!(id: Id, pattern!(facts, [{ ?id @ metadata::tag: &KIND_SEND_ATTEMPT, attempt::draft: &draft_id }]))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub fn acceptances_for_attempt<P>(facts: &P, attempt_id: Id) -> Vec<Id>
where
    P: TriblePattern,
{
    find!(id: Id, pattern!(facts, [{ ?id @ metadata::tag: &KIND_SMTP_ACCEPTANCE, acceptance::attempt: &attempt_id }]))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Resolve the exact Decisions frontier which authorizes a send.
pub fn authorized_send<P>(
    reader: &impl BlobStoreGet,
    decisions_facts: &P,
    draft_id: Id,
) -> Result<(Id, Vec<Id>)>
where
    P: TriblePattern,
{
    let decision = draft_decision_id(draft_id);
    let snapshots = match decisions::resolution(decisions_facts, decision) {
        Resolution::Unique(snapshot) => vec![snapshot],
        Resolution::Agreed(snapshots) => snapshots,
        Resolution::Missing => bail!("send decision {decision:x} has no resolution"),
        Resolution::Forked(heads) => {
            bail!(
                "send decision {decision:x} has divergent heads: {:?}",
                sorted_ids(heads.into_iter().map(|head| head.id))
            )
        }
        Resolution::Invalid(error) => bail!("send decision {decision:x} is invalid: {error}"),
    };
    if snapshots.is_empty() {
        bail!("send decision {decision:x} resolved to an empty frontier");
    }
    for snapshot in &snapshots {
        let outcome = decisions::read_text(reader, snapshot.outcome)?;
        if outcome != "send" {
            bail!("send decision {decision:x} resolves to {outcome:?}, not exact outcome \"send\"");
        }
    }
    Ok((
        decision,
        sorted_ids(snapshots.into_iter().map(|snapshot| snapshot.id)),
    ))
}

/// Materialize one current account and open its exact referenced Secrets
/// version with the pile's durable signing key.
///
/// The account schema stores one immutable secret id, so this path performs no
/// name or “latest version” arbitration and has no password-identity fallback.
pub fn open_account<R, P>(
    mail_reader: &impl BlobStoreGet,
    mail_facts: &P,
    secrets: &SecretsSnapshot<R>,
    anchor: Id,
    signing_key: &SigningKey,
) -> Result<OpenAccount>
where
    R: BlobStoreGet,
    P: TriblePattern,
{
    let config_id = match account_head(mail_facts, anchor)? {
        Head::Unique(id) => id,
        Head::Missing => bail!("mail account {anchor:x} has no configuration"),
        Head::Forked(ids) => {
            bail!("mail account {anchor:x} has forked configuration heads: {ids:?}")
        }
    };
    let config = account_config(mail_facts, config_id)?;
    let password = secrets.open(config.credential, signing_key)?;
    Ok(OpenAccount {
        anchor,
        config: config_id,
        address: read_text(mail_reader, config.address)?,
        display_name: read_text(mail_reader, config.display_name)?,
        imap_endpoint: config
            .imap_endpoint
            .map(|handle| read_text(mail_reader, handle))
            .transpose()?,
        smtp_endpoint: read_text(mail_reader, config.smtp_endpoint)?,
        username: read_text(mail_reader, config.username)?,
        password: String::from_utf8(password).context("mailbox password is not UTF-8")?,
        enabled: config.enabled,
    })
}

fn normalized_mailboxes(
    values: impl IntoIterator<Item = String>,
) -> Result<Vec<(Option<String>, String)>> {
    let mut out = Vec::new();
    for value in values {
        let parsed = mailparse::addrparse(&value)
            .with_context(|| format!("parse mailbox claim {value:?}"))?;
        for address in parsed.iter() {
            match address {
                mailparse::MailAddr::Single(single) => {
                    out.push((single.display_name.clone(), single.addr.clone()));
                }
                mailparse::MailAddr::Group(group) => {
                    out.extend(
                        group
                            .addrs
                            .iter()
                            .map(|single| (single.display_name.clone(), single.addr.clone())),
                    );
                }
            }
        }
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Project every inbound parser result and its read state for one exact
/// persona component.  A later observation of the same WireMessage does not
/// reopen it because read evidence is keyed by `(wire, reader)`.
pub fn inbox_projection<M, R>(
    mail_facts: &M,
    relations_facts: &R,
    persona: Id,
) -> Result<Vec<InboxProjection>>
where
    M: TriblePattern,
    R: TriblePattern,
{
    let identities = IdentityComponents::from_facts(relations_facts)?;
    let component = identities.component(persona)?;
    let read_wires: BTreeSet<Id> = find!(
        (wire_id: Id, reader_id: Id),
        pattern!(mail_facts, [{ _?read @
            metadata::tag: &KIND_READ_OBSERVATION,
            read::wire: ?wire_id,
            read::reader: ?reader_id,
        }])
    )
    .filter_map(|(wire_id, reader_id)| component.contains(&reader_id).then_some(wire_id))
    .collect();
    let mut sources: BTreeMap<Id, Id> = find!(
        (source: Id, wire_id: Id),
        pattern!(mail_facts, [{ ?source @
            metadata::tag: &KIND_POP_OBSERVATION,
            observation::wire: ?wire_id,
        }])
    )
    .collect();
    sources.extend(find!(
        (source: Id, wire_id: Id),
        pattern!(mail_facts, [{ ?source @
            metadata::tag: &KIND_IMAP_OBSERVATION,
            observation::wire: ?wire_id,
        }])
    ));
    sources.extend(find!(
        (source: Id, wire_id: Id),
        pattern!(mail_facts, [{ ?source @
            metadata::tag: &KIND_IMPORTED_OBSERVATION,
            imported::direction: &IMPORT_RECEIVED,
            observation::wire: ?wire_id,
        }])
    ));
    let mut rows = Vec::new();
    for (source, wire) in sources {
        for projection_id in find!(
            id: Id,
            pattern!(mail_facts, [{ ?id @
                metadata::tag: &KIND_PARSED_PROJECTION,
                projection::source: &source,
                projection::recipe: &RECIPE_RFC5322_V1,
            }])
        ) {
            rows.push(InboxProjection {
                wire,
                projection: projection_id,
                source,
                unread: !read_wires.contains(&wire),
            });
        }
    }
    rows.sort_by_key(|row| (row.wire, row.source, row.projection));
    Ok(rows)
}

fn text_values<P>(
    reader: &impl BlobStoreGet,
    facts: &P,
    id: Id,
    attribute: &Attribute<inlineencodings::Handle<blobencodings::UTF8String>>,
) -> Result<Vec<String>>
where
    P: TriblePattern,
{
    let handles: BTreeSet<TextHandle> =
        find!(handle: TextHandle, pattern!(facts, [{ id @ attribute: ?handle }])).collect();
    handles
        .into_iter()
        .map(|handle| read_text(reader, handle))
        .collect()
}

/// Read the structural inbox-summary projection without touching any blob.
///
/// This is deliberately smaller than [`projection_view`]: a watcher can
/// decide whether a wire is unread and inspect residency of only the From and
/// Subject it would actually print. Missing Body or attachment bytes are not
/// relevant to that operation.
pub fn projection_summary_record<P>(facts: &P, projection_id: Id) -> Result<ProjectionSummaryRecord>
where
    P: TriblePattern,
{
    if !ids_of_kind(facts, KIND_PARSED_PROJECTION).contains(&projection_id) {
        bail!("unknown Mail projection {projection_id:x}");
    }
    let source = required(
        find!(v: Id, pattern!(facts, [{ projection_id @ projection::source: ?v }])).collect(),
        "projection source",
    )?;
    Ok(ProjectionSummaryRecord {
        id: projection_id,
        source,
        wire: required(
            find!(v: Id, pattern!(facts, [{ source @ observation::wire: ?v }])).collect(),
            "source wire message",
        )?,
        from: one(
            find!(v: TextHandle, pattern!(facts, [{ projection_id @ projection::from: ?v }]))
                .collect(),
            "projection From",
        )?,
        subject: required(
            find!(v: TextHandle, pattern!(facts, [{ projection_id @ projection::subject: ?v }]))
                .collect(),
            "projection subject",
        )?,
        claimed_date: one(
            find!(v: IntervalValue, pattern!(facts, [{ projection_id @ projection::claimed_date: ?v }]))
                .collect(),
            "projection claimed date",
        )?,
        spam: required(
            find!(v: bool, pattern!(facts, [{ projection_id @ projection::spam: ?v }])).collect(),
            "projection spam flag",
        )?,
    })
}

pub fn projection_view<P>(
    reader: &impl BlobStoreGet,
    facts: &P,
    projection_id: Id,
) -> Result<ProjectionView>
where
    P: TriblePattern,
{
    let summary = projection_summary_record(facts, projection_id)?;
    let body = required(
        find!(v: TextHandle, pattern!(facts, [{ projection_id @ projection::body: ?v }])).collect(),
        "projection body",
    )?;
    Ok(ProjectionView {
        id: projection_id,
        source: summary.source,
        wire: summary.wire,
        message_id: wire_claimed_message_id(reader, facts, summary.wire)?,
        from: summary
            .from
            .map(|handle| read_text(reader, handle))
            .transpose()?,
        to: text_values(reader, facts, projection_id, &projection::to)?,
        cc: text_values(reader, facts, projection_id, &projection::cc)?,
        bcc: text_values(reader, facts, projection_id, &projection::bcc)?,
        subject: read_text(reader, summary.subject)?,
        body: read_text(reader, body)?,
        claimed_date: summary.claimed_date,
        in_reply_to: sorted_ids(
            find!(v: Id, pattern!(facts, [{ projection_id @ projection::in_reply_to: ?v }])),
        ),
        references: sorted_ids(
            find!(v: Id, pattern!(facts, [{ projection_id @ projection::reference: ?v }])),
        ),
        spam: summary.spam,
        attachments: sorted_ids(
            find!(v: Id, pattern!(facts, [{ projection_id @ projection::attachment: ?v }])),
        ),
    })
}

/// Resolve the exact immutable transport evidence behind one projection.
/// Conflicting tags or imported directions are rejected rather than reduced
/// to a priority order.
pub fn projection_direction<P>(facts: &P, source: Id) -> Result<ProjectionDirection>
where
    P: TriblePattern,
{
    let mut candidates = BTreeSet::new();
    let tags = find!(value: Id, pattern!(facts, [{ source @ metadata::tag: ?value }]))
        .collect::<BTreeSet<_>>();
    if tags.contains(&KIND_POP_OBSERVATION) || tags.contains(&KIND_IMAP_OBSERVATION) {
        candidates.insert(0_u8);
    }
    if tags.contains(&KIND_OUTGOING_OBSERVATION) {
        candidates.insert(1_u8);
    }
    if tags.contains(&KIND_IMPORTED_OBSERVATION) {
        let direction = required(
            find!(value: Id, pattern!(facts, [{ source @ imported::direction: ?value }])).collect(),
            "imported Mail direction",
        )?;
        let code = if direction == IMPORT_RECEIVED {
            0
        } else if direction == IMPORT_SENT {
            1
        } else if direction == IMPORT_DRAFT {
            2
        } else {
            bail!("Mail source {source:x} has unknown imported direction {direction:x}");
        };
        candidates.insert(code);
    }
    let code = required(candidates, "Mail projection source direction")?;
    Ok(match code {
        0 => ProjectionDirection::Received,
        1 => ProjectionDirection::Sent,
        2 => ProjectionDirection::Draft,
        _ => unreachable!("direction codes are closed above"),
    })
}

pub fn wire_claimed_message_id<P>(
    reader: &impl BlobStoreGet,
    facts: &P,
    wire_id: Id,
) -> Result<Option<String>>
where
    P: TriblePattern,
{
    if !ids_of_kind(facts, KIND_WIRE_MESSAGE).contains(&wire_id) {
        bail!("unknown wire message {wire_id:x}");
    }
    let claimed = one(
        find!(v: TextHandle, pattern!(facts, [{ wire_id @ wire::claimed_message_id: ?v }]))
            .collect(),
        "wire claimed Message-ID",
    )?;
    let digest = one(
        find!(v: DigestValue, pattern!(facts, [{ wire_id @ wire::raw_digest: ?v }])).collect(),
        "wire raw digest",
    )?;
    match (claimed, digest) {
        (Some(handle), None) => Ok(Some(canonical_message_id_value(&read_text(
            reader, handle,
        )?)?)),
        (None, Some(_)) => Ok(None),
        (Some(_), Some(_)) => bail!("wire {wire_id:x} mixes claimed and raw-digest identity"),
        (None, None) => bail!("wire {wire_id:x} has no identity"),
    }
}

/// Read one draft and every attachment it names, refusing with
/// [`DraftOverBudget`] when the attachments together exceed
/// `attachment_limit` bytes.
///
/// The draft names each attachment once: its attachments are a set of Files
/// entities. Two entities with the same content are two attachments and are
/// charged twice, because each is copied into the draft.
///
/// The budget is checked against the lengths the reader's metadata reports,
/// before any attachment blob is read with `get` or copied. (A pile answers
/// metadata from its mapped record, which it may hash to validate; that
/// copies nothing.) The metadata answers only for blobs the snapshot holds
/// locally. A blob it does not hold has no length to read beforehand (an
/// acquiring reader fetches it from a peer on `get`), so that blob is
/// charged after it is fetched and before it is copied: its fetch is not
/// bounded by this budget, only its copy.
pub fn materialize_draft<R, M, F>(
    reader: &R,
    mail_facts: &M,
    files_facts: &F,
    id: Id,
    attachment_limit: u64,
) -> Result<MaterializedDraft>
where
    R: BlobStoreGet + BlobStoreMeta,
    M: TriblePattern,
    F: TriblePattern,
{
    let record = draft_value(mail_facts, id)?;
    let read_all = |handles: &[TextHandle]| -> Result<Vec<String>> {
        handles
            .iter()
            .map(|&handle| read_text(reader, handle))
            .collect()
    };
    let handles = record
        .attachments
        .iter()
        .map(|&file| file_attachment_handles(files_facts, file))
        .collect::<Result<Vec<_>>>()?;
    let mut over = DraftOverBudget {
        draft: id,
        attachments: handles.len(),
        total: 0,
        complete: true,
        limit: attachment_limit,
    };
    let mut counted = Vec::with_capacity(handles.len());
    let mut uncounted = 0usize;
    for handle in &handles {
        let length = reader
            .metadata(handle.content)
            .map_err(anyhow::Error::new)
            .with_context(|| {
                format!(
                    "read the length of Mail attachment blob {}",
                    hex::encode(handle.content.raw)
                )
            })?
            .map(|metadata| metadata.length);
        match length {
            Some(length) => over.total = over.total.saturating_add(length),
            None => uncounted += 1,
        }
        counted.push(length.is_some());
    }
    over.complete = uncounted == 0;
    if over.total > attachment_limit {
        return Err(over.into());
    }
    let mut attachments = Vec::with_capacity(handles.len());
    for (handle, counted) in handles.iter().zip(counted) {
        let bytes: anybytes::Bytes = reader
            .get(handle.content)
            .with_context(|| format!("read Mail byte blob {}", hex::encode(handle.content.raw)))?;
        if !counted {
            uncounted -= 1;
            over.total = over.total.saturating_add(bytes.len() as u64);
            over.complete = uncounted == 0;
            if over.total > attachment_limit {
                return Err(over.into());
            }
        }
        attachments.push(AttachmentData {
            filename: read_text(reader, handle.name)?,
            media_type: read_text(reader, handle.media_type)?,
            bytes: bytes.as_ref().to_vec(),
        });
    }
    let wires = ids_of_kind(mail_facts, KIND_WIRE_MESSAGE);
    let resolve_wire = |wire: Id, field: &str| -> Result<String> {
        if !wires.contains(&wire) {
            bail!("unknown wire message {wire:x}");
        }
        wire_claimed_message_id(reader, mail_facts, wire)?
            .ok_or_else(|| anyhow!("draft names digest-only {field} wire {wire:x}"))
    };
    Ok(MaterializedDraft {
        id,
        account: record.account,
        envelope_from: read_text(reader, record.envelope_from)?,
        to: read_all(&record.to)?,
        cc: read_all(&record.cc)?,
        bcc: read_all(&record.bcc)?,
        subject: read_text(reader, record.subject)?,
        body: read_text(reader, record.body)?,
        attachments,
        in_reply_to: record
            .in_reply_to
            .iter()
            .map(|&wire| resolve_wire(wire, "In-Reply-To"))
            .collect::<Result<_>>()?,
        references: record
            .references
            .iter()
            .map(|&wire| resolve_wire(wire, "References"))
            .collect::<Result<_>>()?,
        created_at: record.created_at,
    })
}

/// Format a draft once. The returned bytes and envelope are what a caller must
/// freeze into SendAttempt before invoking SMTP. The draft is consumed so its
/// attachment bytes move into the message instead of being copied.
pub fn render_draft(draft: MaterializedDraft, account: &OpenAccount) -> Result<RenderedMail> {
    if draft.account != account.anchor {
        bail!("draft and account do not share an anchor");
    }
    if draft.envelope_from != account.address {
        bail!("draft sender does not match the frozen account address");
    }
    let from: Mailbox = format!("{} <{}>", account.display_name, draft.envelope_from)
        .parse()
        .context("parse draft From mailbox")?;
    let mut builder = Message::builder()
        .from(from)
        .message_id(Some(format!("<mail-{}@triblespace>", draft.id)))
        .subject(draft.subject.clone());
    let (start, _): (Epoch, Epoch) = draft
        .created_at
        .try_from_inline()
        .map_err(|error| anyhow!("decode draft creation time: {error:?}"))?;
    let unix = start.to_unix_seconds();
    if unix >= 0.0 {
        builder = builder.date(std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(unix));
    }
    for value in &draft.to {
        let mailbox: Mailbox = value
            .parse()
            .with_context(|| format!("parse To {value:?}"))?;
        builder = builder.to(mailbox);
    }
    for value in &draft.cc {
        let mailbox: Mailbox = value
            .parse()
            .with_context(|| format!("parse Cc {value:?}"))?;
        builder = builder.cc(mailbox);
    }
    for value in &draft.bcc {
        let mailbox: Mailbox = value
            .parse()
            .with_context(|| format!("parse Bcc {value:?}"))?;
        builder = builder.bcc(mailbox);
    }
    if !draft.in_reply_to.is_empty() {
        builder = builder.in_reply_to(
            draft
                .in_reply_to
                .iter()
                .map(|value| format!("<{value}>"))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    if !draft.references.is_empty() {
        builder = builder.references(
            draft
                .references
                .iter()
                .map(|value| format!("<{value}>"))
                .collect::<Vec<_>>()
                .join(" "),
        );
    }
    let message = if draft.attachments.is_empty() {
        builder
            .header(header::ContentType::TEXT_PLAIN)
            .body(draft.body.clone())
            .context("format draft")?
    } else {
        // Lettre otherwise invents a random boundary here.  The immutable
        // draft id commits to the body and attachment identities, making it a
        // canonical boundary seed and therefore making raw rendering—and the
        // intrinsic SendAttempt that contains it—repeatable.
        let boundary = format!("=_triblespace_{:x}", draft.id);
        let mut multipart = MultiPart::mixed().boundary(boundary).singlepart(
            SinglePart::builder()
                .header(header::ContentType::TEXT_PLAIN)
                .body(draft.body.clone()),
        );
        for attachment in draft.attachments {
            let content_type = header::ContentType::parse(&attachment.media_type)
                .unwrap_or(header::ContentType::parse(files::DEFAULT_MEDIA_TYPE).unwrap());
            multipart = multipart.singlepart(
                lettre::message::Attachment::new(attachment.filename)
                    .body(attachment.bytes, content_type),
            );
        }
        builder
            .multipart(multipart)
            .context("format multipart draft")?
    };
    Ok(RenderedMail {
        raw: message.formatted(),
        envelope: smtp_envelope(&draft.envelope_from, &draft.to, &draft.cc, &draft.bcc)?,
    })
}

// ── irreversible-effect protocol seams ────────────────────────────────────

pub use crate::mail_imap::{SearchCriteria, SearchDate, MAX_MESSAGE_BYTES};

/// Which mailboxes one IMAP fetch examines.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum MailboxSelection {
    /// Only `INBOX`.
    #[default]
    Inbox,
    /// Exactly these server mailbox names, in this order.
    Named(Vec<String>),
    /// Every selectable mailbox the server lists.
    All,
}

/// One read-only IMAP session. The trait is the whole receive vocabulary:
/// nothing here can flag, move, delete or expunge a message. Dropping an
/// implementation disconnects without sending anything.
pub trait ImapSource {
    /// Every selectable mailbox (`LIST`).
    fn mailboxes(&mut self) -> Result<Vec<String>>;
    /// Open one mailbox read-only (`EXAMINE`) and return its UIDVALIDITY.
    fn examine(&mut self, mailbox: &str) -> Result<u32>;
    /// UIDs matching `criteria` in the examined mailbox (`UID SEARCH`).
    fn search(&mut self, criteria: &SearchCriteria) -> Result<Vec<u32>>;
    /// Sizes of the named UIDs that still exist (`UID FETCH (RFC822.SIZE)`).
    fn sizes(&mut self, uids: &[u32]) -> Result<Vec<(u32, u64)>>;
    /// Exact message bytes without setting `\Seen` (`UID FETCH BODY.PEEK[]`),
    /// or `None` when the UID vanished.
    fn fetch(&mut self, uid: u32) -> Result<Option<Vec<u8>>>;
    /// End the session (`LOGOUT`).
    fn logout(self) -> Result<()>;
}

/// What one mailbox contributed to a fetch.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MailboxFetched {
    pub mailbox: String,
    pub uidvalidity: u32,
    /// UIDs the search matched.
    pub matched: usize,
    /// Of those, already observed under this UIDVALIDITY and skipped.
    pub held: usize,
    /// Newly observed and published.
    pub fetched: usize,
    /// Matched but gone by the time they were fetched.
    pub vanished: usize,
    /// `(uid, size)` of messages over [`MAX_MESSAGE_BYTES`], left unread.
    pub oversized: Vec<(u32, u64)>,
    /// `(uid, reason)` of messages whose bytes Mail could not parse into an
    /// observation. They stay on the server and are retried next fetch.
    pub unparsed: Vec<(u32, String)>,
}

/// Observe every not-yet-held message matching `criteria` in the selected
/// mailboxes, publishing each exact raw message as an IMAP observation under
/// one frozen account configuration.
///
/// `held(mailbox, uidvalidity)` answers which UIDs are already observed in
/// that namespace; it is asked once per mailbox, after `EXAMINE`, so a changed
/// UIDVALIDITY simply names a namespace with nothing held yet. Old
/// observations are never touched, and nothing is ever written to the server.
pub fn fetch_imap<T, H, F>(
    mut source: T,
    account_id: Id,
    config_id: Id,
    selection: &MailboxSelection,
    criteria: &SearchCriteria,
    mut held: H,
    mut publish: F,
) -> Result<Vec<MailboxFetched>>
where
    T: ImapSource,
    H: FnMut(&str, u32) -> Result<BTreeSet<u32>>,
    F: FnMut(&SourcePublication) -> Result<()>,
{
    let mailboxes = match selection {
        MailboxSelection::Inbox => vec!["INBOX".to_owned()],
        MailboxSelection::Named(names) => names.clone(),
        MailboxSelection::All => source.mailboxes()?,
    };
    let mut reports = Vec::new();
    for mailbox in mailboxes {
        let uidvalidity = source
            .examine(&mailbox)
            .with_context(|| format!("examine IMAP mailbox {mailbox:?}"))?;
        let matched = source
            .search(criteria)
            .with_context(|| format!("search IMAP mailbox {mailbox:?}"))?;
        let already = held(&mailbox, uidvalidity)?;
        let wanted: Vec<u32> = matched
            .iter()
            .copied()
            .filter(|uid| !already.contains(uid))
            .collect();
        let mut report = MailboxFetched {
            mailbox: mailbox.clone(),
            uidvalidity,
            matched: matched.len(),
            held: matched.len() - wanted.len(),
            ..MailboxFetched::default()
        };
        let sizes = if wanted.is_empty() {
            Vec::new()
        } else {
            source.sizes(&wanted)?
        };
        report.vanished = wanted.len() - sizes.len();
        for (uid, size) in sizes {
            if size > MAX_MESSAGE_BYTES {
                report.oversized.push((uid, size));
                continue;
            }
            let Some(raw) = source.fetch(uid)? else {
                report.vanished += 1;
                continue;
            };
            let publication =
                match imap_publication(account_id, config_id, &mailbox, uidvalidity, uid, &raw) {
                    Ok(publication) => publication,
                    Err(error) => {
                        report.unparsed.push((uid, format!("{error:#}")));
                        continue;
                    }
                };
            publish(&publication)?;
            report.fetched += 1;
        }
        reports.push(report);
    }
    source
        .logout()
        .context("IMAP LOGOUT failed; every published observation is already durable")?;
    Ok(reports)
}

/// UIDs already observed for one account in (mailbox, UIDVALIDITY) on the
/// same server login: the observation's config must name this IMAP endpoint
/// and username, so a re-pointed account never inherits another server's
/// UID namespace. A query at the point of use, not a cache.
pub fn imap_held_uids<P>(
    facts: &P,
    account_id: Id,
    endpoint: TextHandle,
    username: TextHandle,
    mailbox: &str,
    uidvalidity: u32,
) -> BTreeSet<u32>
where
    P: TriblePattern,
{
    let mailbox: TextHandle = Fragment::empty().put(mailbox.to_owned());
    let uidvalidity: CountValue = u64::from(uidvalidity).to_inline();
    find!(
        uid: u32,
        pattern!(facts, [
            { _?observation @
                metadata::tag: &KIND_IMAP_OBSERVATION,
                observation::account: &account_id,
                observation::config: _?config,
                observation::mailbox: mailbox,
                observation::uidvalidity: uidvalidity,
                observation::uid: ?uid,
            },
            { _?config @
                account::imap_endpoint: endpoint,
                account::username: username,
            }
        ])
    )
    .collect()
}

impl<S> ImapSource for crate::mail_imap::ImapSession<S>
where
    S: std::io::Read + std::io::Write,
{
    fn mailboxes(&mut self) -> Result<Vec<String>> {
        Ok(self.list()?)
    }

    fn examine(&mut self, mailbox: &str) -> Result<u32> {
        Ok(crate::mail_imap::ImapSession::examine(self, mailbox)?.uidvalidity)
    }

    fn search(&mut self, criteria: &SearchCriteria) -> Result<Vec<u32>> {
        Ok(self.uid_search(criteria)?)
    }

    fn sizes(&mut self, uids: &[u32]) -> Result<Vec<(u32, u64)>> {
        Ok(self.uid_sizes(uids)?)
    }

    fn fetch(&mut self, uid: u32) -> Result<Option<Vec<u8>>> {
        Ok(self.uid_fetch_body(uid)?)
    }

    fn logout(self) -> Result<()> {
        crate::mail_imap::ImapSession::logout(self)?;
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SmtpEnvelope {
    pub from: String,
    pub recipients: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedReply {
    pub code: u16,
    pub message: String,
}

pub trait SmtpSubmit {
    /// Return only a final positive SMTP completion reply. Rejection or an
    /// indeterminate transport outcome is an error, after which the durable
    /// attempt must not be retried automatically.
    fn submit(&mut self, envelope: &SmtpEnvelope, raw: &[u8]) -> Result<AcceptedReply>;
}

fn smtp_envelope(from: &str, to: &[String], cc: &[String], bcc: &[String]) -> Result<SmtpEnvelope> {
    let senders = normalized_mailboxes([from.to_owned()])?;
    let [(_, from)] = senders.as_slice() else {
        bail!("SMTP envelope sender must contain exactly one mailbox");
    };
    let recipients = normalized_mailboxes(to.iter().chain(cc).chain(bcc).cloned())?
        .into_iter()
        .map(|(_, address)| address)
        .collect::<Vec<_>>();
    if recipients.is_empty() {
        bail!("SMTP envelope has no recipients");
    }
    Ok(SmtpEnvelope {
        from: from.clone(),
        recipients,
    })
}

fn smtp_envelope_for_attempt(input: &SendAttemptInput) -> Result<SmtpEnvelope> {
    smtp_envelope(&input.envelope_from, &input.to, &input.cc, &input.bcc)
}

/// Freeze and validate one exact SMTP effect plan against one local snapshot.
///
/// The stored Decisions heads are the complete frontier observed by this
/// executor at preparation time. A later resolution does not retroactively
/// invalidate that historical authorization. Conversely, the eventual union
/// cannot prove that this local snapshot was globally fresh or complete: SMTP
/// execution is therefore an affine authority which deployments must
/// serialize per account rather than running concurrently on replicas.
pub fn prepare_send<R, M, F, D>(
    mail_reader: &R,
    decisions_reader: &impl BlobStoreGet,
    mail_facts: &M,
    files_facts: &F,
    decisions_facts: &D,
    input: SendAttemptInput,
    attachment_limit: u64,
) -> Result<PreparedSend>
where
    R: BlobStoreGet + BlobStoreMeta,
    M: TriblePattern,
    F: TriblePattern,
    D: TriblePattern,
{
    let draft = draft_from_facts(mail_facts, input.draft)
        .with_context(|| format!("prepare send for unknown draft {:x}", input.draft))?;
    match account_head(mail_facts, draft.account)? {
        Head::Unique(current) if current == input.config => {}
        Head::Unique(current) => bail!(
            "send config {:x} is stale; current account config is {current:x}",
            input.config
        ),
        Head::Missing => bail!("draft account {:x} has no configuration", draft.account),
        Head::Forked(heads) => bail!(
            "draft account {:x} has forked configurations {heads:?}",
            draft.account
        ),
    }
    let (current_decision, current_heads) =
        authorized_send(decisions_reader, decisions_facts, input.draft)?;
    if input.decision != current_decision
        || sorted_ids(input.decision_heads.iter().copied()) != current_heads
    {
        bail!("send attempt does not cite the exact locally observed Decisions frontier");
    }

    let envelope = smtp_envelope_for_attempt(&input)?;
    let config = account_config(mail_facts, input.config)?;
    let materialized = materialize_draft(
        mail_reader,
        mail_facts,
        files_facts,
        input.draft,
        attachment_limit,
    )?;
    let expected = render_draft(
        materialized,
        &OpenAccount {
            anchor: config.account,
            config: config.id,
            address: read_text(mail_reader, config.address)?,
            display_name: read_text(mail_reader, config.display_name)?,
            imap_endpoint: None,
            smtp_endpoint: read_text(mail_reader, config.smtp_endpoint)?,
            username: read_text(mail_reader, config.username)?,
            password: String::new(),
            enabled: config.enabled,
        },
    )?;
    if input.raw != expected.raw {
        let first_difference = input
            .raw
            .iter()
            .zip(&expected.raw)
            .position(|(actual, expected)| actual != expected)
            .unwrap_or_else(|| input.raw.len().min(expected.raw.len()));
        bail!(
            "send attempt bytes or envelope do not equal the authorized DraftIntent rendering: raw lengths {} != {} or first byte difference at {first_difference}",
            input.raw.len(),
            expected.raw.len(),
        );
    }
    if envelope != expected.envelope {
        bail!(
            "send attempt bytes or envelope do not equal the authorized DraftIntent rendering: SMTP envelope differs"
        );
    }
    drop(expected);
    let raw = input.raw.clone();
    let (attempt, attempt_id) = send_attempt_fragment(input)?;
    let outgoing = outgoing_publication(attempt_id, &raw)?;
    Ok(PreparedSend {
        attempt,
        attempt_id,
        outgoing,
        envelope,
        raw,
    })
}

/// Persist attempt-before-effect and acceptance-after-effect.  Any error after
/// attempt publication leaves an intentionally uncertain attempt which callers
/// must never retry automatically. The prepared value binds every byte and
/// address crossing the effect boundary to the exact validated attempt.
pub fn submit_once<T, PA, PC>(
    transport: &mut T,
    prepared: &PreparedSend,
    mut publish_attempt: PA,
    mut publish_acceptance: PC,
) -> Result<AcceptedReply>
where
    T: SmtpSubmit,
    PA: FnMut(&Fragment) -> Result<()>,
    PC: FnMut(&Fragment) -> Result<()>,
{
    publish_attempt(&prepared.attempt)?;
    let accepted = transport.submit(&prepared.envelope, &prepared.raw)?;
    if !(200..=299).contains(&accepted.code) {
        bail!(
            "SMTP submitter returned non-acceptance code {}; attempt outcome remains uncertain",
            accepted.code
        );
    }
    let (mut fragment, _) = smtp_acceptance_fragment(
        prepared.attempt_id,
        u64::from(accepted.code),
        accepted.message.clone(),
    )?;
    fragment += prepared.outgoing.mail.clone();
    publish_acceptance(&fragment)?;
    Ok(accepted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::fs::File;
    use std::io::{self, Cursor, Read, Write};
    use std::path::PathBuf;
    use std::rc::Rc;

    use crate::collection_names::open_configured;
    use crate::relations::{self, ProfileInput};
    use crate::schemas::{
        decide as decide_schema, files as files_schema, mail as mail_schema,
        relations as relations_schema,
    };
    use crate::secrets::storage as secret_storage;
    use crate::storage::{
        load_signer, open_pile_strict, open_secrets_collection, open_secrets_collection_read,
        publish_fragment,
    };
    use crate::test_support::initialize_open_collection_fixture;
    use triblespace::core::repo::pile::PileSnapshot;

    fn id(byte: u8) -> Id {
        Id::new([byte; 16]).unwrap()
    }

    fn at(second: u8) -> IntervalValue {
        let epoch = Epoch::from_gregorian_utc(2026, 8, 8, 0, 0, second, 0);
        (epoch, epoch).try_to_inline().unwrap()
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        pile: PathBuf,
        key: PathBuf,
    }

    struct CollectionView {
        facts: TribleSet,
        reader: PileSnapshot,
    }

    struct Views {
        mail: CollectionView,
        files: CollectionView,
        decisions: CollectionView,
        relations: CollectionView,
        secrets: SecretsSnapshot<PileSnapshot>,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let pile = directory.path().join("mail.pile");
            let key = directory.path().join("mail.key");
            File::create(&pile).unwrap();
            initialize_open_collection_fixture(&pile, Some(&key));
            Self {
                _directory: directory,
                pile,
                key,
            }
        }

        fn publish(&self, scope: Id, fragment: Fragment) {
            publish_fragment(&self.pile, Some(&self.key), scope, fragment).unwrap();
        }

        fn views(&self) -> Views {
            let signer = load_signer(&self.pile, Some(&self.key)).unwrap();
            let mut pile = open_pile_strict(&self.pile).unwrap();
            let secrets_collection =
                open_secrets_collection_read(&mut pile, signer.verifying_key()).unwrap();
            let secrets = pollster::block_on(secret_storage::ensure_and_snapshot(
                &mut pile,
                secrets_collection,
                &signer,
            ))
            .unwrap();
            let mail = open_configured(
                &mut pile,
                mail_schema::DEFAULT_SCOPE_ID,
                signer.verifying_key(),
            )
            .unwrap();
            let files = open_configured(
                &mut pile,
                files_schema::DEFAULT_SCOPE_ID,
                signer.verifying_key(),
            )
            .unwrap();
            let decide = open_configured(
                &mut pile,
                decide_schema::DEFAULT_SCOPE_ID,
                signer.verifying_key(),
            )
            .unwrap();
            let relations = open_configured(
                &mut pile,
                relations_schema::DEFAULT_SCOPE_ID,
                signer.verifying_key(),
            )
            .unwrap();
            let store_snapshot = pile.snapshot().unwrap();
            let materialize = |collection| {
                let (facts, _) =
                    crate::storage::read_fact_collection(collection, &store_snapshot).unwrap();
                CollectionView {
                    facts,
                    reader: store_snapshot.clone(),
                }
            };
            let views = Views {
                mail: materialize(mail),
                files: materialize(files),
                decisions: materialize(decide),
                relations: materialize(relations),
                secrets,
            };
            pile.close().unwrap();
            views
        }

        fn signer(&self) -> SigningKey {
            load_signer(&self.pile, Some(&self.key)).unwrap()
        }

        fn add_secret(&self, name: &str, plaintext: &[u8], created_at: IntervalValue) -> Id {
            let signer = self.signer();
            let mut pile = open_pile_strict(&self.pile).unwrap();
            let collection = open_secrets_collection(&mut pile, signer.verifying_key()).unwrap();
            let secret = secret_storage::add_secret(
                &mut pile, &signer, collection, name, plaintext, created_at,
            )
            .unwrap();
            pile.close().unwrap();
            secret
        }
    }

    fn add_person(fixture: &Fixture, person: Id) {
        let (fragment, _, _) = relations::person_fragment(
            person,
            ProfileInput {
                label: "operator".into(),
                display_name: Some("Operator".into()),
                emails: vec!["jp@example.test".into()],
                ..ProfileInput::default()
            },
        )
        .unwrap();
        fixture.publish(relations_schema::DEFAULT_SCOPE_ID, fragment);
    }

    fn add_account(fixture: &Fixture, account_id: Id) -> (Id, Id) {
        let credential_id = fixture.add_secret("mail/test", b"smtp-secret", at(3));
        let mut fragment = Fragment::empty();
        let (config, config_id) = account_config_fragment(
            account_id,
            AccountConfigInput {
                address: "sender@example.test".into(),
                display_name: "Sender".into(),
                imap_endpoint: "imap.example.test:993".into(),
                smtp_endpoint: "smtp.example.test:465".into(),
                username: "sender@example.test".into(),
                credential: credential_id,
                enabled: true,
                quiet: false,
                predecessors: Vec::new(),
            },
        )
        .unwrap();
        fragment += config;
        fixture.publish(mail_schema::DEFAULT_SCOPE_ID, fragment);
        (config_id, credential_id)
    }

    const RAW_INBOUND: &[u8] = b"From: Alice <alice@example.test>\r\nTo: sender@example.test\r\nSubject: Hello\r\nDate: Sat, 8 Aug 2026 00:00:01 +0000\r\nMessage-ID: <CaseSensitive@Example.TEST>\r\nContent-Type: multipart/mixed; boundary=demo\r\n\r\n--demo\r\nContent-Type: text/plain\r\n\r\nhello body\r\n--demo\r\nContent-Type: application/octet-stream; name=note.bin\r\nContent-Disposition: attachment; filename=note.bin\r\nContent-Transfer-Encoding: base64\r\n\r\nAQID\r\n--demo--\r\n";

    #[test]
    fn message_identity_is_opaque_and_missing_id_hashes_full_raw_bytes() {
        let parsed = parse_rfc5322(RAW_INBOUND).unwrap();
        assert_eq!(
            parsed.identity,
            WireIdentity::Claimed("CaseSensitive@Example.TEST".into())
        );
        assert_eq!(parsed.claimed_date.unwrap(), at(1));
        assert_eq!(parsed.attachments[0].bytes, vec![1, 2, 3]);

        let first = parse_rfc5322(b"From: a@b\r\n\r\none").unwrap();
        let same = parse_rfc5322(b"From: a@b\r\n\r\none").unwrap();
        let other = parse_rfc5322(b"From: a@b\r\n\r\ntwo").unwrap();
        assert_eq!(first.identity, same.identity);
        assert_ne!(first.identity, other.identity);

        let WireIdentity::RawDigest(digest) = first.identity else {
            panic!("missing Message-ID must use raw digest identity")
        };
        let claimed = format!(
            "From: a@b\r\nMessage-ID: <raw-blake3:{}>\r\n\r\nclaim",
            hex::encode(digest.raw)
        );
        let fallback =
            pop_publication(id(86), id(87), "fallback", b"From: a@b\r\n\r\none").unwrap();
        let deliberate = pop_publication(id(86), id(87), "claimed", claimed.as_bytes()).unwrap();
        assert_ne!(fallback.wire, deliberate.wire);
        let rotated = pop_publication(id(86), id(88), "fallback", b"From: a@b\r\n\r\none").unwrap();
        assert_eq!(fallback.wire, rotated.wire);
        assert_ne!(fallback.observation, rotated.observation);
        assert!(parse_rfc5322(b"Message-ID: <a@b>\r\nMessage-ID: <c@d>\r\n\r\n").is_err());
    }

    #[test]
    fn imap_held_uids_are_scoped_to_their_namespace() {
        let fixture = Fixture::new();
        let account = id(97);
        let config = add_account(&fixture, account).0;
        let raw: &[u8] = b"From: a@example.test\r\nTo: sender@example.test\r\nMessage-ID: <imap@example.test>\r\nSubject: imap\r\n\r\none";
        let first = imap_publication(account, config, "INBOX", 41, 7, raw).unwrap();
        let views = fixture.views();
        let mut files = views.files.facts.clone();
        files += first.files.facts().clone();
        fixture.publish(files_schema::DEFAULT_SCOPE_ID, first.files.clone());
        fixture.publish(mail_schema::DEFAULT_SCOPE_ID, first.mail.clone());

        let views = fixture.views();
        let record = account_config(&views.mail.facts, config).unwrap();
        let endpoint = record.imap_endpoint.unwrap();
        let held = |endpoint, mailbox, uidvalidity| {
            imap_held_uids(
                &views.mail.facts,
                account,
                endpoint,
                record.username,
                mailbox,
                uidvalidity,
            )
        };
        assert_eq!(held(endpoint, "INBOX", 41), BTreeSet::from([7]));
        // A changed UIDVALIDITY is a fresh namespace with nothing held.
        assert!(held(endpoint, "INBOX", 42).is_empty());
        assert!(held(endpoint, "Archive", 41).is_empty());
        let elsewhere: TextHandle = Fragment::empty().put("imap.elsewhere.test:993".to_owned());
        assert!(held(elsewhere, "INBOX", 41).is_empty());
        assert_eq!(
            projection_direction(&views.mail.facts, first.observation).unwrap(),
            ProjectionDirection::Received
        );

        // The same message seen again under a new UIDVALIDITY is a second
        // observation of the same wire value; nothing old is replaced.
        let again = imap_publication(account, config, "INBOX", 42, 1, raw).unwrap();
        assert_eq!(again.wire, first.wire);
        assert_ne!(again.observation, first.observation);
    }

    #[test]
    fn a_pop_era_configuration_stays_readable_without_an_imap_endpoint() {
        let fixture = Fixture::new();
        let account = id(96);
        let credential = fixture.add_secret("mail/legacy", b"legacy-secret", at(4));
        let mut fragment = Fragment::empty();
        let record = AccountConfigRecord {
            id: account,
            account,
            address: fragment.put("old@example.test".to_owned()),
            display_name: fragment.put("Old".to_owned()),
            pop_endpoint: Some(fragment.put("pop.example.test:995".to_owned())),
            imap_endpoint: None,
            smtp_endpoint: fragment.put("smtp.example.test:465".to_owned()),
            username: fragment.put("old@example.test".to_owned()),
            credential,
            enabled: true,
            quiet: false,
            predecessors: Vec::new(),
        };
        let config = account_config_record(&record);
        let config_id = config.root().unwrap();
        fragment += account_anchor_record(account);
        fragment += config;
        fixture.publish(mail_schema::DEFAULT_SCOPE_ID, fragment);

        let views = fixture.views();
        let read = account_config(&views.mail.facts, config_id).unwrap();
        assert!(read.pop_endpoint.is_some());
        assert!(read.imap_endpoint.is_none());
        let open = open_account(
            &views.mail.reader,
            &views.mail.facts,
            &views.secrets,
            account,
            &fixture.signer(),
        )
        .unwrap();
        assert_eq!(open.imap_endpoint, None);
    }

    /// One fixed draft with two attachments, its Mail and Files facts and
    /// blobs in one fragment, and the account it renders for.
    fn golden_draft_parts() -> (Fragment, Id, OpenAccount) {
        let binary: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
        let first = files::stage(binary, "data.bin", "application/octet-stream").unwrap();
        let second = files::stage(
            b"plain attachment text\r\nsecond line\n".to_vec(),
            "notes.txt",
            "text/plain",
        )
        .unwrap();
        let draft = draft_publication(DraftInput {
            nonce: id(93),
            account: id(94),
            envelope_from: "sender@example.test".into(),
            to: vec!["Receiver <receiver@example.test>".into()],
            cc: vec!["cc@example.test".into()],
            bcc: vec!["hidden@example.test".into()],
            subject: "Golden rendering".into(),
            body: "Body text for the golden rendering.".into(),
            attachments: vec![first.root().unwrap(), second.root().unwrap()],
            in_reply_to: Vec::new(),
            references: Vec::new(),
            created_at: at(5),
        })
        .unwrap();
        let mut staged = draft.mail;
        staged += first;
        staged += second;
        let account = OpenAccount {
            anchor: id(94),
            config: id(95),
            address: "sender@example.test".into(),
            display_name: "Sender".into(),
            imap_endpoint: Some("imap.example.test:993".into()),
            smtp_endpoint: "smtp.example.test:465".into(),
            username: "sender@example.test".into(),
            password: "unused".into(),
            enabled: true,
        };
        (staged, draft.draft, account)
    }

    /// The rendering of [`golden_draft_parts`] at faculties fe66c43e, before
    /// the attachment budget: its blake3 hash and length.
    const GOLDEN_RENDER_BLAKE3: &str =
        "823a0486504d114b83d7920129d08c8115d6b8313fdce90d3545fca837d8a099";
    const GOLDEN_RENDER_LEN: usize = 6550;

    #[test]
    fn a_draft_within_the_attachment_budget_renders_as_before() {
        let (mut staged, draft, account) = golden_draft_parts();
        let facts = staged.facts().clone();
        let reader = staged.blobs_mut().snapshot().unwrap();
        let attachment_bytes = 4096 + 35;
        let materialized =
            materialize_draft(&reader, &facts, &facts, draft, attachment_bytes).unwrap();
        let rendered = render_draft(materialized, &account).unwrap();
        if let Some(path) = std::env::var_os("MAIL_GOLDEN_RENDER_OUT") {
            std::fs::write(path, &rendered.raw).unwrap();
        }
        assert_eq!(rendered.raw.len(), GOLDEN_RENDER_LEN);
        assert_eq!(
            blake3::hash(&rendered.raw).to_hex().as_str(),
            GOLDEN_RENDER_BLAKE3
        );
    }

    /// A reader that counts its `get` calls and can hide blob lengths, as a
    /// reader does for a blob it would have to fetch from a peer.
    struct Counting<'a, R> {
        inner: &'a R,
        lengths: bool,
        gets: std::cell::Cell<usize>,
    }

    impl<'a, R> Counting<'a, R> {
        fn new(inner: &'a R, lengths: bool) -> Self {
            Self {
                inner,
                lengths,
                gets: std::cell::Cell::new(0),
            }
        }
    }

    impl<R: BlobStoreGet> BlobStoreGet for Counting<'_, R> {
        type GetError<E: std::error::Error + Send + Sync + 'static> = R::GetError<E>;

        fn get<T, S>(
            &self,
            handle: triblespace::core::inline::Inline<
                triblespace::core::inline::encodings::hash::Handle<S>,
            >,
        ) -> Result<T, Self::GetError<<T as triblespace::core::blob::TryFromBlob<S>>::Error>>
        where
            S: triblespace::core::blob::BlobEncoding + 'static,
            T: triblespace::core::blob::TryFromBlob<S>,
            triblespace::core::inline::encodings::hash::Handle<S>:
                triblespace::core::inline::InlineEncoding,
        {
            self.gets.set(self.gets.get() + 1);
            self.inner.get(handle)
        }
    }

    impl<R: BlobStoreMeta> BlobStoreMeta for Counting<'_, R> {
        type MetaError = R::MetaError;

        fn metadata<S>(
            &self,
            handle: triblespace::core::inline::Inline<
                triblespace::core::inline::encodings::hash::Handle<S>,
            >,
        ) -> Result<Option<triblespace::core::repo::BlobMetadata>, Self::MetaError>
        where
            S: triblespace::core::blob::BlobEncoding + 'static,
            triblespace::core::inline::encodings::hash::Handle<S>:
                triblespace::core::inline::InlineEncoding,
        {
            if self.lengths {
                self.inner.metadata(handle)
            } else {
                Ok(None)
            }
        }
    }

    fn over_budget(error: &anyhow::Error) -> DraftOverBudget {
        error
            .chain()
            .find_map(|cause| cause.downcast_ref::<DraftOverBudget>())
            .unwrap_or_else(|| panic!("not an attachment budget refusal: {error:#}"))
            .clone()
    }

    #[test]
    fn the_attachment_budget_sums_every_attachment_before_reading_any() {
        let (mut staged, draft, _) = golden_draft_parts();
        let facts = staged.facts().clone();
        let store = staged.blobs_mut().snapshot().unwrap();
        let total = 4096 + 35;

        let reader = Counting::new(&store, true);
        materialize_draft(&reader, &facts, &facts, draft, total).unwrap();

        // One byte under the sum of both attachments refuses the draft, and
        // with their lengths known it does so before reading any blob.
        let reader = Counting::new(&store, true);
        let error = materialize_draft(&reader, &facts, &facts, draft, total - 1).unwrap_err();
        assert_eq!(
            over_budget(&error),
            DraftOverBudget {
                draft,
                attachments: 2,
                total,
                complete: true,
                limit: total - 1,
            }
        );
        assert_eq!(reader.gets.get(), 0);
        let message = format!("{error:#}");
        assert!(message.contains(&format!("draft {draft:x}")), "{message}");
        assert!(
            message.contains(&format!("total {total} bytes")),
            "{message}"
        );
    }

    #[test]
    fn an_attachment_without_a_known_length_is_charged_before_it_is_copied() {
        let (mut staged, draft, _) = golden_draft_parts();
        let facts = staged.facts().clone();
        let store = staged.blobs_mut().snapshot().unwrap();

        let reader = Counting::new(&store, false);
        materialize_draft(&reader, &facts, &facts, draft, 4096 + 35).unwrap();

        // Without lengths, the first attachment fetched already exceeds the
        // budget, and nothing after it is read.
        let reader = Counting::new(&store, false);
        let error = materialize_draft(&reader, &facts, &facts, draft, 10).unwrap_err();
        let refused = over_budget(&error);
        assert_eq!(refused.attachments, 2);
        assert!(!refused.complete);
        assert!(refused.total == 4096 || refused.total == 35, "{refused:?}");
        assert_eq!(reader.gets.get(), 1);
        assert!(format!("{error:#}").contains("at least"));
    }

    #[test]
    fn a_file_named_twice_is_one_attachment_and_equal_content_is_charged_per_file() {
        let content = vec![7u8; 100];
        let first = files::stage(content.clone(), "a.bin", "application/octet-stream").unwrap();
        let second = files::stage(content, "b.bin", "application/octet-stream").unwrap();
        let draft_with = |nonce: u8, attachments: Vec<Id>| {
            draft_publication(DraftInput {
                nonce: id(nonce),
                account: id(94),
                envelope_from: "sender@example.test".into(),
                to: vec!["receiver@example.test".into()],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Attachments".into(),
                body: "Body".into(),
                attachments,
                in_reply_to: Vec::new(),
                references: Vec::new(),
                created_at: at(6),
            })
            .unwrap()
        };
        let twice = draft_with(96, vec![first.root().unwrap(), first.root().unwrap()]);
        let pair = draft_with(97, vec![first.root().unwrap(), second.root().unwrap()]);
        let (twice_id, pair_id) = (twice.draft, pair.draft);
        let mut staged = twice.mail;
        staged += pair.mail;
        staged += first;
        staged += second;
        let facts = staged.facts().clone();
        let reader = staged.blobs_mut().snapshot().unwrap();

        let materialized = materialize_draft(&reader, &facts, &facts, twice_id, 100).unwrap();
        assert_eq!(materialized.attachments.len(), 1);

        let error = materialize_draft(&reader, &facts, &facts, pair_id, 199).unwrap_err();
        let refused = over_budget(&error);
        assert_eq!((refused.attachments, refused.total), (2, 200));
        let materialized = materialize_draft(&reader, &facts, &facts, pair_id, 200).unwrap();
        assert_eq!(materialized.attachments.len(), 2);
    }

    #[test]
    fn multipart_draft_rendering_is_byte_deterministic() {
        let draft = MaterializedDraft {
            id: id(80),
            account: id(81),
            envelope_from: "sender@example.test".into(),
            to: vec!["Receiver <receiver@example.test>".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Deterministic multipart".into(),
            body: "same body every time".into(),
            attachments: vec![AttachmentData {
                filename: "bytes.bin".into(),
                media_type: "application/octet-stream".into(),
                bytes: vec![0, 1, 2, 3, 254, 255],
            }],
            in_reply_to: Vec::new(),
            references: Vec::new(),
            created_at: at(4),
        };
        let account = OpenAccount {
            anchor: draft.account,
            config: id(82),
            address: draft.envelope_from.clone(),
            display_name: "Sender".into(),
            imap_endpoint: Some("imap.example.test:993".into()),
            smtp_endpoint: "smtp.example.test:465".into(),
            username: "sender@example.test".into(),
            password: "unused".into(),
            enabled: true,
        };

        let first = render_draft(draft.clone(), &account).unwrap();
        let second = render_draft(draft.clone(), &account).unwrap();
        assert_eq!(first, second);
        let boundary = format!("boundary=\"=_triblespace_{:x}\"", draft.id);
        assert!(first
            .raw
            .windows(boundary.len())
            .any(|window| window == boundary.as_bytes()));
    }

    #[test]
    fn account_snapshot_superseding_every_fork_head_rejoins_the_dag() {
        let anchor = id(83);
        let credential = id(84);
        let input = |display_name: &str, predecessors: Vec<Id>| AccountConfigInput {
            address: "me@example.test".into(),
            display_name: display_name.into(),
            imap_endpoint: "imap.example.test:993".into(),
            smtp_endpoint: "smtp.example.test:465".into(),
            username: "me@example.test".into(),
            credential,
            enabled: true,
            quiet: false,
            predecessors,
        };
        let (first, first_id) =
            account_config_fragment(anchor, input("First branch", Vec::new())).unwrap();
        let (second, second_id) =
            account_config_fragment(anchor, input("Second branch", Vec::new())).unwrap();
        let mut facts = first.into_facts();
        facts += second.into_facts();
        // Heads come back in id order, which content-derived ids make arbitrary.
        assert_eq!(
            account_head(&facts, anchor).unwrap(),
            Head::Forked(sorted_ids(vec![first_id, second_id]))
        );

        let (joined, joined_id) =
            account_config_fragment(anchor, input("Reconciled", vec![second_id, first_id]))
                .unwrap();
        facts += joined.into_facts();
        assert_eq!(
            account_head(&facts, anchor).unwrap(),
            Head::Unique(joined_id)
        );
    }

    #[test]
    fn opening_an_account_requires_utf8_mailbox_secret_bytes() {
        let fixture = Fixture::new();
        let secret = fixture.add_secret("mail/non-utf8", &[0xff, 0xfe], at(4));
        let account = id(85);
        let (config, _) = account_config_fragment(
            account,
            AccountConfigInput {
                address: "me@example.test".into(),
                display_name: "Me".into(),
                imap_endpoint: "imap.example.test:993".into(),
                smtp_endpoint: "smtp.example.test:465".into(),
                username: "me@example.test".into(),
                credential: secret,
                enabled: true,
                quiet: false,
                predecessors: Vec::new(),
            },
        )
        .unwrap();
        fixture.publish(mail_schema::DEFAULT_SCOPE_ID, config);
        let views = fixture.views();
        let error = open_account(
            &views.mail.reader,
            &views.mail.facts,
            &views.secrets,
            account,
            &fixture.signer(),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("not UTF-8"));
    }

    #[test]
    fn opening_an_account_uses_the_durable_signer_key() {
        let fixture = Fixture::new();
        let account = id(86);
        add_account(&fixture, account);
        let views = fixture.views();

        let opened = open_account(
            &views.mail.reader,
            &views.mail.facts,
            &views.secrets,
            account,
            &fixture.signer(),
        )
        .unwrap();
        assert_eq!(opened.password, "smtp-secret");

        let outsider = SigningKey::from_bytes(&[99; 32]);
        let error = open_account(
            &views.mail.reader,
            &views.mail.facts,
            &views.secrets,
            account,
            &outsider,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("no usable wrap for this signing key"),
            "{error:#}"
        );
    }

    #[test]
    fn smtp_acceptance_requires_a_final_positive_reply() {
        assert!(smtp_acceptance_fragment(id(85), 199, "not accepted").is_err());
        assert!(smtp_acceptance_fragment(id(85), 300, "not accepted").is_err());
        assert!(smtp_acceptance_fragment(id(85), 550, "rejected").is_err());
        assert!(smtp_acceptance_fragment(id(85), 250, "queued").is_ok());
    }

    #[test]
    fn collection_roundtrip_enforces_unread_and_exact_outbound_bytes() {
        let fixture = Fixture::new();
        let persona = id(1);
        let account_id = id(2);
        add_person(&fixture, persona);
        let (account_config, _) = add_account(&fixture, account_id);

        let views = fixture.views();
        let incoming =
            pop_publication(account_id, account_config, "UidL-CaSe", RAW_INBOUND).unwrap();
        let mut files_union = views.files.facts.clone();
        files_union += incoming.files.facts().clone();
        fixture.publish(files_schema::DEFAULT_SCOPE_ID, incoming.files.clone());
        fixture.publish(mail_schema::DEFAULT_SCOPE_ID, incoming.mail.clone());

        let views = fixture.views();
        let inbox = inbox_projection(&views.mail.facts, &views.relations.facts, persona).unwrap();
        assert_eq!(inbox.len(), 1);
        assert!(inbox[0].unread);
        assert!(projection_ids(&views.mail.facts).contains(&incoming.projection));
        let presented =
            projection_view(&views.mail.reader, &views.mail.facts, incoming.projection).unwrap();
        assert_eq!(presented.wire, incoming.wire);
        assert_eq!(
            projection_direction(&views.mail.facts, presented.source).unwrap(),
            ProjectionDirection::Received
        );

        let (read_fragment, _) = read_observation_fragment(incoming.wire, persona);
        fixture.publish(mail_schema::DEFAULT_SCOPE_ID, read_fragment);

        // A new source observation of the same wire message does not reopen it.
        let replay =
            pop_publication(account_id, account_config, "second-uidl", RAW_INBOUND).unwrap();
        fixture.publish(mail_schema::DEFAULT_SCOPE_ID, replay.mail);
        let views = fixture.views();
        assert!(
            inbox_projection(&views.mail.facts, &views.relations.facts, persona)
                .unwrap()
                .iter()
                .all(|row| !row.unread)
        );

        // Draft and its deterministic Decisions genesis are independent signed
        // roots; Decide lands first.
        let draft = draft_publication(DraftInput {
            nonce: id(4),
            account: account_id,
            envelope_from: "sender@example.test".into(),
            to: vec!["Bob <bob@example.test>".into()],
            cc: Vec::new(),
            bcc: vec!["quiet@example.test".into()],
            subject: "Authorized subject".into(),
            body: "Authorized body".into(),
            attachments: Vec::new(),
            in_reply_to: Vec::new(),
            references: Vec::new(),
            created_at: at(2),
        })
        .unwrap();
        fixture.publish(decide_schema::DEFAULT_SCOPE_ID, draft.decisions);
        fixture.publish(mail_schema::DEFAULT_SCOPE_ID, draft.mail);

        let (resolution, resolution_id) =
            decisions::resolution_fragment(draft.decision, "send", None, true, &[], &[], at(3))
                .unwrap();
        fixture.publish(decide_schema::DEFAULT_SCOPE_ID, resolution);

        let views = fixture.views();
        let account = open_account(
            &views.mail.reader,
            &views.mail.facts,
            &views.secrets,
            account_id,
            &fixture.signer(),
        )
        .unwrap();
        assert_eq!(account.password, "smtp-secret");
        let materialized = materialize_draft(
            &views.mail.reader,
            &views.mail.facts,
            &views.files.facts,
            draft.draft,
            MAX_DRAFT_ATTACHMENT_BYTES,
        )
        .unwrap();
        let rendered = render_draft(materialized.clone(), &account).unwrap();
        let (decision, heads) =
            authorized_send(&views.decisions.reader, &views.decisions.facts, draft.draft).unwrap();
        assert_eq!(heads, vec![resolution_id]);
        let base_attempt = SendAttemptInput {
            draft: draft.draft,
            config: account.config,
            decision,
            decision_heads: heads,
            raw: rendered.raw.clone(),
            envelope_from: materialized.envelope_from.clone(),
            to: materialized.to.clone(),
            cc: materialized.cc.clone(),
            bcc: materialized.bcc.clone(),
        };

        // Substituting the body while preserving the envelope and authorization
        // is rejected before a byte is published or SMTP is touched.
        let mut corrupt = rendered.raw.clone();
        let offset = corrupt
            .windows("Authorized body".len())
            .position(|window| window == b"Authorized body")
            .unwrap();
        corrupt.splice(
            offset..offset + "Authorized body".len(),
            b"Substituted body".iter().copied(),
        );
        let error = prepare_send(
            &views.mail.reader,
            &views.decisions.reader,
            &views.mail.facts,
            &views.files.facts,
            &views.decisions.facts,
            SendAttemptInput {
                raw: corrupt,
                ..base_attempt.clone()
            },
            MAX_DRAFT_ATTACHMENT_BYTES,
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("bytes or envelope"));

        let prepared = prepare_send(
            &views.mail.reader,
            &views.decisions.reader,
            &views.mail.facts,
            &views.files.facts,
            &views.decisions.facts,
            base_attempt,
            MAX_DRAFT_ATTACHMENT_BYTES,
        )
        .unwrap();
        let attempt_id = prepared.attempt_id();
        assert!(prepared.outgoing_files().facts().is_empty());

        let events = Rc::new(RefCell::new(Vec::new()));
        let mut smtp = FakeSmtp {
            events: events.clone(),
            fail: false,
        };

        submit_once(
            &mut smtp,
            &prepared,
            |fragment| {
                events.borrow_mut().push("publish-attempt");
                fixture.publish(mail_schema::DEFAULT_SCOPE_ID, fragment.clone());
                Ok(())
            },
            |fragment| {
                events.borrow_mut().push("publish-post-effect");
                fixture.publish(mail_schema::DEFAULT_SCOPE_ID, fragment.clone());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(
            events.borrow().as_slice(),
            ["publish-attempt", "smtp", "publish-post-effect"]
        );
        let views = fixture.views();
        assert_eq!(
            acceptances_for_attempt(&views.mail.facts, attempt_id).len(),
            1
        );
    }

    #[test]
    fn send_authorization_is_current_at_effect_time_but_historical_after_publication() {
        let fixture = Fixture::new();
        let account_id = id(90);
        add_account(&fixture, account_id);

        let draft = draft_publication(DraftInput {
            nonce: id(92),
            account: account_id,
            envelope_from: "sender@example.test".into(),
            to: vec!["receiver@example.test".into()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "Snapshot authorization".into(),
            body: "The authorization is historical after the effect starts.".into(),
            attachments: Vec::new(),
            in_reply_to: Vec::new(),
            references: Vec::new(),
            created_at: at(1),
        })
        .unwrap();
        fixture.publish(decide_schema::DEFAULT_SCOPE_ID, draft.decisions);
        fixture.publish(mail_schema::DEFAULT_SCOPE_ID, draft.mail);

        let (send, send_id) =
            decisions::resolution_fragment(draft.decision, "send", None, true, &[], &[], at(2))
                .unwrap();
        fixture.publish(decide_schema::DEFAULT_SCOPE_ID, send);
        let views = fixture.views();
        let account = open_account(
            &views.mail.reader,
            &views.mail.facts,
            &views.secrets,
            account_id,
            &fixture.signer(),
        )
        .unwrap();
        let materialized = materialize_draft(
            &views.mail.reader,
            &views.mail.facts,
            &views.files.facts,
            draft.draft,
            MAX_DRAFT_ATTACHMENT_BYTES,
        )
        .unwrap();
        let rendered = render_draft(materialized.clone(), &account).unwrap();
        let (decision, heads) =
            authorized_send(&views.decisions.reader, &views.decisions.facts, draft.draft).unwrap();
        assert_eq!(heads, vec![send_id]);
        let prepared = prepare_send(
            &views.mail.reader,
            &views.decisions.reader,
            &views.mail.facts,
            &views.files.facts,
            &views.decisions.facts,
            SendAttemptInput {
                draft: draft.draft,
                config: account.config,
                decision,
                decision_heads: heads,
                raw: rendered.raw,
                envelope_from: materialized.envelope_from,
                to: materialized.to,
                cc: materialized.cc,
                bcc: materialized.bcc,
            },
            MAX_DRAFT_ATTACHMENT_BYTES,
        )
        .unwrap();
        fixture.publish(
            mail_schema::DEFAULT_SCOPE_ID,
            prepared.attempt_fragment().clone(),
        );

        // A concurrent disagreeing resolution makes the current frontier a
        // fork. It prevents a new effect, but it cannot rewrite the evidence
        // of which genuine send head the executor previously observed.
        let (reject, reject_id) =
            decisions::resolution_fragment(draft.decision, "reject", None, true, &[], &[], at(3))
                .unwrap();
        fixture.publish(decide_schema::DEFAULT_SCOPE_ID, reject);
        let views = fixture.views();
        assert!(format!(
            "{:#}",
            authorized_send(&views.decisions.reader, &views.decisions.facts, draft.draft)
                .unwrap_err()
        )
        .contains("divergent heads"));

        // Joining that fork as a rejection likewise governs only future
        // effects. The final union cannot reconstruct whether the executor's
        // earlier local frontier was globally complete; that is its affine
        // authority attestation, not a fact derivable after the event.
        let (reject_join, _) = decisions::resolution_fragment(
            draft.decision,
            "reject",
            None,
            true,
            &[],
            &[send_id, reject_id],
            at(4),
        )
        .unwrap();
        fixture.publish(decide_schema::DEFAULT_SCOPE_ID, reject_join);
        let views = fixture.views();
        assert!(format!(
            "{:#}",
            authorized_send(&views.decisions.reader, &views.decisions.facts, draft.draft)
                .unwrap_err()
        )
        .contains("not exact outcome \"send\""));
    }

    struct FakeSmtp {
        events: Rc<RefCell<Vec<&'static str>>>,
        fail: bool,
    }

    impl SmtpSubmit for FakeSmtp {
        fn submit(&mut self, _envelope: &SmtpEnvelope, _raw: &[u8]) -> Result<AcceptedReply> {
            self.events.borrow_mut().push("smtp");
            if self.fail {
                bail!("scripted SMTP uncertainty");
            }
            Ok(AcceptedReply {
                code: 250,
                message: "queued".into(),
            })
        }
    }

    struct ScriptedImapStream {
        reads: Cursor<Vec<u8>>,
        writes: Rc<RefCell<Vec<u8>>>,
    }

    impl Read for ScriptedImapStream {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.reads.read(buffer)
        }
    }

    impl Write for ScriptedImapStream {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.writes.borrow_mut().extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn real_imap_session_examines_searches_peeks_and_logs_out() {
        let mut script = b"* OK IMAP4rev1 ready\r\n".to_vec();
        script.extend_from_slice(
            b"* 2 EXISTS\r\n* OK [UIDVALIDITY 77] ok\r\nA0001 OK [READ-ONLY] done\r\n",
        );
        script.extend_from_slice(b"* SEARCH 3 5\r\nA0002 OK\r\n");
        script.extend_from_slice(b"* 2 FETCH (UID 5 RFC822.SIZE 600)\r\nA0003 OK\r\n");
        script.extend_from_slice(
            format!("* 2 FETCH (UID 5 BODY[] {{{}}}\r\n", RAW_INBOUND.len()).as_bytes(),
        );
        script.extend_from_slice(RAW_INBOUND);
        script.extend_from_slice(b")\r\nA0004 OK\r\n* BYE bye\r\nA0005 OK\r\n");
        let writes = Rc::new(RefCell::new(Vec::new()));
        let session = crate::mail_imap::ImapSession::new(ScriptedImapStream {
            reads: Cursor::new(script),
            writes: writes.clone(),
        })
        .unwrap();
        let mut published = Vec::new();
        let criteria = SearchCriteria {
            from: Some("alice".into()),
            since: SearchDate::parse("2026-08-01"),
            ..SearchCriteria::default()
        };

        let reports = fetch_imap(
            session,
            id(9),
            id(10),
            &MailboxSelection::Inbox,
            &criteria,
            |mailbox, uidvalidity| {
                assert_eq!((mailbox, uidvalidity), ("INBOX", 77));
                Ok(BTreeSet::from([3]))
            },
            |publication| {
                published.push(publication.observation);
                Ok(())
            },
        )
        .unwrap();

        let expected = imap_publication(id(9), id(10), "INBOX", 77, 5, RAW_INBOUND).unwrap();
        assert_eq!(published, [expected.observation]);
        assert_eq!(
            reports,
            [MailboxFetched {
                mailbox: "INBOX".into(),
                uidvalidity: 77,
                matched: 2,
                held: 1,
                fetched: 1,
                ..MailboxFetched::default()
            }]
        );
        assert_eq!(
            String::from_utf8(writes.borrow().clone()).unwrap(),
            "A0001 EXAMINE \"INBOX\"\r\n\
             A0002 UID SEARCH FROM \"alice\" SINCE \"1-Aug-2026\"\r\n\
             A0003 UID FETCH 5 (UID RFC822.SIZE)\r\n\
             A0004 UID FETCH 5 (UID BODY.PEEK[])\r\n\
             A0005 LOGOUT\r\n"
        );
    }

    struct FakeImap {
        events: Rc<RefCell<Vec<String>>>,
        mailboxes: Vec<(&'static str, u32, Vec<(u32, Vec<u8>)>)>,
        examined: Option<usize>,
        oversized: Vec<u32>,
    }

    impl ImapSource for FakeImap {
        fn mailboxes(&mut self) -> Result<Vec<String>> {
            self.events.borrow_mut().push("list".into());
            Ok(self
                .mailboxes
                .iter()
                .map(|(name, ..)| (*name).into())
                .collect())
        }

        fn examine(&mut self, mailbox: &str) -> Result<u32> {
            self.events.borrow_mut().push(format!("examine:{mailbox}"));
            let index = self
                .mailboxes
                .iter()
                .position(|(name, ..)| *name == mailbox)
                .ok_or_else(|| anyhow!("no mailbox {mailbox}"))?;
            self.examined = Some(index);
            Ok(self.mailboxes[index].1)
        }

        fn search(&mut self, _criteria: &SearchCriteria) -> Result<Vec<u32>> {
            let index = self.examined.unwrap();
            Ok(self.mailboxes[index]
                .2
                .iter()
                .map(|(uid, _)| *uid)
                .collect())
        }

        fn sizes(&mut self, uids: &[u32]) -> Result<Vec<(u32, u64)>> {
            self.events.borrow_mut().push(format!("sizes:{uids:?}"));
            let index = self.examined.unwrap();
            Ok(self.mailboxes[index]
                .2
                .iter()
                .filter(|(uid, _)| uids.contains(uid))
                .map(|(uid, raw)| {
                    let size = if self.oversized.contains(uid) {
                        MAX_MESSAGE_BYTES + 1
                    } else {
                        raw.len() as u64
                    };
                    (*uid, size)
                })
                .collect())
        }

        fn fetch(&mut self, uid: u32) -> Result<Option<Vec<u8>>> {
            self.events.borrow_mut().push(format!("fetch:{uid}"));
            assert!(
                !self.oversized.contains(&uid),
                "an oversized message was fetched"
            );
            let index = self.examined.unwrap();
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

    #[test]
    fn all_mailboxes_skip_oversized_and_unparsable_messages_without_aborting() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let source = FakeImap {
            events: events.clone(),
            mailboxes: vec![
                (
                    "INBOX",
                    1,
                    vec![
                        (1, RAW_INBOUND.to_vec()),
                        (2, b"Subject: a\r\nSubject: b\r\n\r\nx".to_vec()),
                        (3, b"Subject: big\r\n\r\nbig".to_vec()),
                    ],
                ),
                ("Archive", 9, vec![(1, RAW_INBOUND.to_vec())]),
            ],
            examined: None,
            oversized: vec![3],
        };
        let mut published = 0;
        let reports = fetch_imap(
            source,
            id(9),
            id(10),
            &MailboxSelection::All,
            &SearchCriteria::default(),
            |_, _| Ok(BTreeSet::new()),
            |_| {
                published += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(published, 2);
        assert_eq!(reports[0].fetched, 1);
        assert_eq!(reports[0].oversized, [(3, MAX_MESSAGE_BYTES + 1)]);
        assert_eq!(reports[0].unparsed.len(), 1);
        assert_eq!(reports[0].unparsed[0].0, 2);
        assert_eq!(
            (reports[1].mailbox.as_str(), reports[1].fetched),
            ("Archive", 1)
        );
        assert_eq!(
            events.borrow().as_slice(),
            [
                "list",
                "examine:INBOX",
                "sizes:[1, 2, 3]",
                "fetch:1",
                "fetch:2",
                "examine:Archive",
                "sizes:[1]",
                "fetch:1",
                "logout"
            ]
        );
    }

    #[test]
    fn a_failed_publication_stops_the_fetch_and_never_logs_out() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let source = FakeImap {
            events: events.clone(),
            mailboxes: vec![("INBOX", 1, vec![(1, RAW_INBOUND.to_vec())])],
            examined: None,
            oversized: Vec::new(),
        };
        let error = fetch_imap(
            source,
            id(9),
            id(10),
            &MailboxSelection::Named(vec!["INBOX".into()]),
            &SearchCriteria::default(),
            |_, _| Ok(BTreeSet::new()),
            |_| bail!("disk full"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("disk full"));
        assert_eq!(
            events.borrow().as_slice(),
            ["examine:INBOX", "sizes:[1]", "fetch:1"]
        );
    }

    #[test]
    fn imap_observation_identity_names_its_whole_coordinate() {
        let base = imap_publication(id(9), id(10), "INBOX", 7, 1, RAW_INBOUND).unwrap();
        for other in [
            imap_publication(id(9), id(10), "Archive", 7, 1, RAW_INBOUND).unwrap(),
            imap_publication(id(9), id(10), "INBOX", 8, 1, RAW_INBOUND).unwrap(),
            imap_publication(id(9), id(10), "INBOX", 7, 2, RAW_INBOUND).unwrap(),
        ] {
            assert_ne!(other.observation, base.observation);
            assert_eq!(other.wire, base.wire, "the wire identity is shared");
        }
        assert!(imap_publication(id(9), id(10), "INBOX", 0, 1, RAW_INBOUND).is_err());
        assert!(imap_publication(id(9), id(10), "", 7, 1, RAW_INBOUND).is_err());
    }
}
