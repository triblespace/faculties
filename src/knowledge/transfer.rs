//! `knowledge transfer`: republish exact revisions from one workspace's Knowledge into
//! another's, keeping their identity.
//!
//! A native revision is an authored artifact whose id is intrinsic over
//! (author, title, content, tags, supersedes), and a COMMIT only records its
//! signer's curation. Transfer therefore copies the revision's own facts, its
//! authorship and source-provenance records, its author description and its
//! tag names, and the text bytes they name, into a destination commit signed
//! by the node. Nothing is re-authored: the destination holds the same id,
//! readable through the same citation.
//!
//! Scope is explicit. By default only the named revision crosses. Its parent
//! revision ids stay part of its identity, so the destination carries them as
//! opaque references, never their facts or text. History, linked revisions and
//! attached files cross only by opt-in, and every relation is reported either
//! as carried or as a reference. A source-provenance bundle is always a
//! reference. Same-root and repeated transfers converge: only facts the
//! destination does not already hold are published.
//!
//! This copies no collection, adds no grant and never writes the source.

use super::*;
use crate::schemas::archive::import_schema;
use crate::schemas::code::attrs as code;
use crate::schemas::files::KIND_MEDIA_TYPE;
use triblespace::core::attestation;
use triblespace::core::collection::CollectionHandle;
use triblespace::core::inline::InlineEncoding;
use triblespace::core::repo::BlobStoreGet;
use triblespace::core::trible::Trible;

/// Explicit opt-ins beyond the one named revision.
#[derive(Clone, Copy, Debug, Default)]
pub struct TransferOptions {
    /// Every source ancestor reachable through supersedes.
    pub history: bool,
    /// The exact source revisions the carried revisions cite, one hop.
    pub links: bool,
    /// The Files leaves the carried revisions link, with their bytes.
    pub files: bool,
    /// Report the scope without publishing.
    pub dry_run: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    Requested,
    History,
    Linked,
    Attachment,
}

impl Reason {
    pub fn label(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::History => "history",
            Self::Linked => "linked",
            Self::Attachment => "attachment",
        }
    }
}

/// One entity that crosses, with how many of its facts are new there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Carried {
    pub id: Id,
    pub reason: Reason,
    /// The revision title or the file name.
    pub label: String,
    pub facts: usize,
    pub new_facts: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ReferenceKind {
    /// A predecessor id inside a carried revision's identity.
    Parent,
    /// A `wiki:` link target.
    Link,
    /// A `files:` link target.
    File,
    /// The Files id a source-provenance record cites.
    ProvenanceBundle,
}

impl ReferenceKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Parent => "parent",
            Self::Link => "link",
            Self::File => "file",
            Self::ProvenanceBundle => "provenance bundle",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Presence {
    Present,
    Absent,
    /// The destination has no Files role to look in.
    Unknown,
}

/// A relation that does not cross: only its id or hash is named.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reference {
    pub kind: ReferenceKind,
    /// The revision whose facts or content name it.
    pub from: Id,
    /// Lowercase hex as the source names it.
    pub target: String,
    pub at_destination: Presence,
}

#[derive(Clone, Debug)]
pub struct TransferReport {
    pub revision: Id,
    pub from: Id,
    pub to: Id,
    pub source_root: CollectionHandle,
    pub destination_root: CollectionHandle,
    pub carried: Vec<Carried>,
    pub references: Vec<Reference>,
    pub new_knowledge_facts: usize,
    pub new_file_facts: usize,
    pub publications: usize,
    pub dry_run: bool,
    /// Every carried fact was read back through the destination.
    pub verified: bool,
}

impl TransferReport {
    pub fn same_root(&self) -> bool {
        self.source_root == self.destination_root
    }
}

/// The values of one attribute, copied as facts of `entity`.
fn carry<P: TriblePattern, S: InlineEncoding>(
    space: &P,
    entity: Id,
    attribute: &Attribute<S>,
    out: &mut TribleSet,
) -> Vec<Inline<S>> {
    let values: Vec<Inline<S>> = find!(
        value: Inline<S>,
        pattern!(space, [{ entity @ attribute: ?value }])
    )
    .collect();
    for value in &values {
        out.insert(&Trible::force(&entity, &attribute.id(), value));
    }
    values
}

/// What one carried entity is in a given space, and the texts it names.
struct Gathered {
    facts: TribleSet,
    texts: Vec<schema::TextHandle>,
    contents: Vec<ContentHandle>,
}

/// Every fact Transfer understands about one revision. Facts it does not
/// model are not carried; facts it models are carried whole, so a legacy
/// version keeps its own identity facts too.
fn gather_revision<P: TriblePattern>(space: &P, revision: Id) -> Gathered {
    use schema::attrs;
    let mut facts = TribleSet::new();
    let mut texts = Vec::new();
    let tags = carry(space, revision, &metadata::tag, &mut facts);
    texts.extend(carry(space, revision, &attrs::title, &mut facts));
    texts.extend(carry(space, revision, &attrs::content, &mut facts));
    let mut authors = carry(space, revision, &attrs::author, &mut facts);
    carry(space, revision, &metadata::supersedes, &mut facts);
    carry(space, revision, &metadata::created_at, &mut facts);
    carry(space, revision, &attrs::fragment, &mut facts);

    let authorships: BTreeSet<Id> = find!(
        id: Id,
        pattern!(space, [{ ?id @ metadata::tag: &schema::KIND_AUTHORSHIP, attrs::revision: &revision }])
    )
    .collect();
    for record in authorships {
        carry(space, record, &metadata::tag, &mut facts);
        carry(space, record, &attrs::revision, &mut facts);
        authors.extend(carry(space, record, &attrs::author, &mut facts));
        carry(space, record, &metadata::created_at, &mut facts);
    }
    let provenance: BTreeSet<Id> = find!(
        id: Id,
        pattern!(space, [{ ?id @ metadata::tag: &schema::KIND_SOURCE_PROVENANCE, attrs::revision: &revision }])
    )
    .collect();
    for record in provenance {
        carry(space, record, &metadata::tag, &mut facts);
        carry(space, record, &attrs::revision, &mut facts);
        texts.extend(carry(space, record, &code::commit, &mut facts));
        texts.extend(carry(space, record, &file::source_path, &mut facts));
        texts.extend(carry(
            space,
            record,
            &import_schema::source_author,
            &mut facts,
        ));
        carry(space, record, &import_schema::source_created_at, &mut facts);
        carry(space, record, &attrs::references_file, &mut facts);
        carry(space, record, &attrs::references_file_content, &mut facts);
    }
    for author in authors {
        let Ok(author) = author.try_from_inline::<Id>() else {
            continue;
        };
        carry(space, author, &attestation::signed_by, &mut facts);
    }
    for tag in tags {
        let Ok(tag) = tag.try_from_inline::<Id>() else {
            continue;
        };
        if tag != schema::KIND_REVISION && tag != schema::KIND_VERSION_ID {
            texts.extend(carry(space, tag, &metadata::name, &mut facts));
        }
    }
    texts.sort_by_key(|handle| handle.raw);
    texts.dedup();
    Gathered {
        facts,
        texts,
        contents: Vec::new(),
    }
}

/// One Files leaf: its identity facts and its media-type entity. Import
/// records, source paths and Files tags belong to the source workspace.
fn gather_file<P: TriblePattern>(space: &P, leaf: Id) -> Gathered {
    let mut facts = TribleSet::new();
    let mut texts = Vec::new();
    let tag: Inline<inlineencodings::GenId> = KIND_FILE.to_inline();
    if exists!((), pattern!(space, [{ leaf @ metadata::tag: &KIND_FILE }])) {
        facts.insert(&Trible::force(&leaf, &metadata::tag.id(), &tag));
    }
    let contents = carry(space, leaf, &file::content, &mut facts);
    texts.extend(carry(space, leaf, &file::name, &mut facts));
    for media in carry(space, leaf, &file::media_type, &mut facts) {
        let Ok(media) = media.try_from_inline::<Id>() else {
            continue;
        };
        if exists!(
            (),
            pattern!(space, [{ media @ metadata::tag: &KIND_MEDIA_TYPE }])
        ) {
            let kind: Inline<inlineencodings::GenId> = KIND_MEDIA_TYPE.to_inline();
            facts.insert(&Trible::force(&media, &metadata::tag.id(), &kind));
        }
        texts.extend(carry(space, media, &metadata::name, &mut facts));
    }
    Gathered {
        facts,
        texts,
        contents,
    }
}

fn is_file<P: TriblePattern>(space: &P, id: Id) -> bool {
    exists!(
        (),
        pattern!(space, [{ id @ metadata::tag: &KIND_FILE, file::content: _?content }])
    )
}

/// The Files leaves a `files:` link target names in `space`.
fn file_leaves<P: TriblePattern>(space: &P, token: &str) -> Result<BTreeSet<Id>> {
    Ok(match token.len() {
        64 => {
            let hash = inlineencodings::Hash::<inlineencodings::Blake3>::from_hex(token)
                .map_err(|_| anyhow!("invalid content hash '{token}'"))?;
            let handle: ContentHandle = inlineencodings::Handle::from_hash(hash);
            find!(
                id: Id,
                pattern!(space, [{ ?id @ metadata::tag: &KIND_FILE, file::content: &handle }])
            )
            .collect()
        }
        32 => Id::from_hex(token)
            .filter(|id| is_file(space, *id))
            .into_iter()
            .collect(),
        _ => BTreeSet::new(),
    })
}

/// Put every text the facts name into `blobs`, proving each is readable.
fn carry_texts(
    reader: &PileSnapshot,
    texts: &[schema::TextHandle],
    blobs: &mut Fragment,
    what: Id,
) -> Result<()> {
    for handle in texts {
        let text = read_string(reader, *handle).with_context(|| {
            format!(
                "read text {} of {what:x} in the source",
                hex::encode(handle.raw)
            )
        })?;
        let put: schema::TextHandle = blobs.put::<blobencodings::UTF8String, _>(text);
        anyhow::ensure!(
            put == *handle,
            "text of {what:x} does not hash to its handle"
        );
    }
    Ok(())
}

struct Planned {
    id: Id,
    reason: Reason,
    label: String,
    facts: TribleSet,
}

/// What the source phase decided, before the destination is looked at.
struct Stage {
    revisions: Vec<Planned>,
    files: Vec<Planned>,
    references: Vec<Reference>,
    knowledge_blobs: Fragment,
    file_blobs: Fragment,
}

fn workspace_roots(storage: &Storage, side: &str) -> Result<(Id, CollectionHandle, bool)> {
    let workspace = storage
        .selected_workspace()
        .ok_or_else(|| anyhow!("knowledge transfer needs an explicit {side} workspace"))?;
    let root = workspace
        .routes
        .get(&schema::DEFAULT_SCOPE_ID)
        .copied()
        .ok_or_else(|| anyhow!("{side} workspace {:X} has no Knowledge role", workspace.id))?;
    Ok((
        workspace.id,
        root,
        workspace.routes.contains_key(&FILES_SCOPE_ID),
    ))
}

fn files_scope(configured: bool) -> Vec<(Id, &'static str)> {
    if configured {
        vec![(FILES_SCOPE_ID, "Files")]
    } else {
        Vec::new()
    }
}

fn stage_source(
    source: KnowledgeStorage<'_>,
    source_files: bool,
    revision: Id,
    options: TransferOptions,
) -> Result<Stage> {
    source.views(&files_scope(source_files), |view, auxiliary| {
        let facts = &view.facts;
        let files = auxiliary.first();
        if !knowledge_model::is_revision(facts, revision) {
            bail!("{revision:x} is not a revision in the source workspace's Knowledge");
        }
        let mut order = vec![(revision, Reason::Requested)];
        let mut seen = BTreeSet::from([revision]);
        if options.history {
            let mut pending = vec![revision];
            while let Some(current) = pending.pop() {
                for parent in knowledge_model::id_values(facts, current, &metadata::supersedes) {
                    if knowledge_model::is_revision(facts, parent) && seen.insert(parent) {
                        order.push((parent, Reason::History));
                        pending.push(parent);
                    }
                }
            }
        }
        let mut stage = Stage {
            revisions: Vec::new(),
            files: Vec::new(),
            references: Vec::new(),
            knowledge_blobs: Fragment::empty(),
            file_blobs: Fragment::empty(),
        };
        let mut contents = BTreeMap::new();
        let mut index = 0;
        while index < order.len() {
            let (id, reason) = order[index];
            index += 1;
            let gathered = gather_revision(facts, id);
            carry_texts(&view.reader, &gathered.texts, &mut stage.knowledge_blobs, id)?;
            // Ids are opaque: the received id and its readable facts are what
            // cross, never a recomputation that could refuse an annotated or
            // randomly identified revision.
            let records = knowledge_model::revision_records(facts, id);
            let title = records
                .first()
                .map(|record| revision_title(&view.reader, record))
                .transpose()?
                .unwrap_or_default();
            let mut texts = Vec::new();
            for record in &records {
                texts.push(knowledge_model::content_with_original_sources(
                    facts,
                    id,
                    revision_content(&view.reader, record)?,
                ));
            }
            texts.sort();
            texts.dedup();
            if options.links && reason != Reason::Linked {
                for text in &texts {
                    for target in extract_link_targets(text) {
                        let Some(linked) = Id::from_hex(&target) else {
                            continue;
                        };
                        if knowledge_model::is_revision(facts, linked) && seen.insert(linked) {
                            order.push((linked, Reason::Linked));
                        }
                    }
                }
            }
            contents.insert(id, texts);
            stage.revisions.push(Planned {
                id,
                reason,
                label: title,
                facts: gathered.facts,
            });
        }

        // Relations that do not cross, and attachments that do on request.
        let mut file_seen = BTreeSet::new();
        let mut referenced = BTreeSet::new();
        for planned in &stage.revisions {
            let id = planned.id;
            for parent in knowledge_model::id_values(facts, id, &metadata::supersedes) {
                if !seen.contains(&parent) {
                    referenced.insert((ReferenceKind::Parent, id, format!("{parent:x}")));
                }
            }
            for text in &contents[&id] {
                for target in extract_link_targets(text) {
                    if Id::from_hex(&target).is_some_and(|linked| seen.contains(&linked)) {
                        continue;
                    }
                    referenced.insert((ReferenceKind::Link, id, target));
                }
                for token in link_targets(text, "files") {
                    let token = token.to_ascii_lowercase();
                    let leaves = match files {
                        Some(files) => file_leaves(files, &token)?,
                        None => BTreeSet::new(),
                    };
                    if !options.files || leaves.is_empty() {
                        referenced.insert((ReferenceKind::File, id, token));
                        continue;
                    }
                    for leaf in leaves {
                        if !file_seen.insert(leaf) {
                            continue;
                        }
                        let gathered = gather_file(files.expect("leaves came from Files"), leaf);
                        carry_texts(&view.reader, &gathered.texts, &mut stage.file_blobs, leaf)?;
                        for content in &gathered.contents {
                            let bytes: anybytes::Bytes = BlobStoreGet::get(&view.reader, *content)
                                .with_context(|| {
                                    format!("read the bytes of file {leaf:x} in the source")
                                })?;
                            let put: ContentHandle =
                                stage.file_blobs.put::<blobencodings::RawBytes, _>(bytes);
                            anyhow::ensure!(
                                put == *content,
                                "bytes of file {leaf:x} do not hash to their handle"
                            );
                        }
                        let label = gathered
                            .texts
                            .first()
                            .map(|name| read_string(&view.reader, *name))
                            .transpose()?
                            .unwrap_or_default();
                        stage.files.push(Planned {
                            id: leaf,
                            reason: Reason::Attachment,
                            label,
                            facts: gathered.facts,
                        });
                    }
                }
            }
            for bundle in find!(
                bundle: Id,
                pattern!(facts, [{
                    _?record @ metadata::tag: &schema::KIND_SOURCE_PROVENANCE,
                    schema::attrs::revision: &id,
                    schema::attrs::references_file: ?bundle,
                }])
            ) {
                referenced.insert((ReferenceKind::ProvenanceBundle, id, format!("{bundle:x}")));
            }
        }
        stage.references = referenced
            .into_iter()
            .map(|(kind, from, target)| Reference {
                kind,
                from,
                target,
                at_destination: Presence::Unknown,
            })
            .collect();
        Ok(stage)
    })
}

/// Facts of each planned entity the destination already holds, and where
/// each reference resolves there.
fn observe_destination(
    destination: KnowledgeStorage<'_>,
    destination_files: bool,
    stage: &Stage,
) -> Result<(Vec<TribleSet>, Vec<TribleSet>, Vec<Presence>)> {
    destination.views(&files_scope(destination_files), |view, auxiliary| {
        let files = auxiliary.first();
        let revisions = stage
            .revisions
            .iter()
            .map(|planned| gather_revision(&view.facts, planned.id).facts)
            .collect();
        let leaves = stage
            .files
            .iter()
            .map(|planned| match files {
                Some(files) => gather_file(files, planned.id).facts,
                None => TribleSet::new(),
            })
            .collect();
        let mut presence = Vec::with_capacity(stage.references.len());
        for reference in &stage.references {
            let found = match reference.kind {
                ReferenceKind::Parent | ReferenceKind::Link => Some(
                    Id::from_hex(&reference.target)
                        .is_some_and(|id| knowledge_model::is_revision(&view.facts, id)),
                ),
                ReferenceKind::File => match files {
                    Some(files) => Some(!file_leaves(files, &reference.target)?.is_empty()),
                    None => None,
                },
                ReferenceKind::ProvenanceBundle => files.map(|files| {
                    Id::from_hex(&reference.target).is_some_and(|id| {
                        exists!((), pattern!(files, [{ id @ metadata::tag: _?kind }]))
                    })
                }),
            };
            presence.push(match found {
                Some(true) => Presence::Present,
                Some(false) => Presence::Absent,
                None => Presence::Unknown,
            });
        }
        Ok((revisions, leaves, presence))
    })
}

pub(super) fn transfer(
    source: &Storage,
    destination: &Storage,
    revision: Id,
    options: TransferOptions,
) -> Result<TransferReport> {
    let (from, source_root, source_files) = workspace_roots(source, "source")?;
    let (to, destination_root, destination_files) = workspace_roots(destination, "destination")?;
    let source = KnowledgeStorage { storage: source };
    let destination = KnowledgeStorage {
        storage: destination,
    };
    let mut stage = stage_source(source, source_files, revision, options)?;
    if !stage.files.is_empty() && !destination_files {
        bail!("destination workspace {to:X} has no Files role, so the requested attachments cannot cross");
    }
    let (held_revisions, held_files, presence) =
        observe_destination(destination, destination_files, &stage)?;
    for (reference, presence) in stage.references.iter_mut().zip(presence) {
        reference.at_destination = presence;
    }

    let mut knowledge_new = TribleSet::new();
    let mut files_new = TribleSet::new();
    let mut carried = Vec::new();
    for (planned, held) in stage.revisions.iter().zip(&held_revisions) {
        let new = planned.facts.difference(held);
        carried.push(Carried {
            id: planned.id,
            reason: planned.reason,
            label: planned.label.clone(),
            facts: planned.facts.len(),
            new_facts: new.len(),
        });
        knowledge_new.union(new);
    }
    for (planned, held) in stage.files.iter().zip(&held_files) {
        let new = planned.facts.difference(held);
        carried.push(Carried {
            id: planned.id,
            reason: planned.reason,
            label: planned.label.clone(),
            facts: planned.facts.len(),
            new_facts: new.len(),
        });
        files_new.union(new);
    }
    let mut report = TransferReport {
        revision,
        from,
        to,
        source_root,
        destination_root,
        carried,
        references: stage.references.clone(),
        new_knowledge_facts: knowledge_new.len(),
        new_file_facts: files_new.len(),
        publications: 0,
        dry_run: options.dry_run,
        verified: false,
    };
    if options.dry_run {
        return Ok(report);
    }

    // Attachments first, so a revision that links one never lands without it.
    // A failure after the Files commit leaves only facts a replay converges on.
    if !files_new.is_empty() {
        let mut fragment = std::mem::replace(&mut stage.file_blobs, Fragment::empty());
        fragment += files_new;
        crate::files::Files::with_storage(destination.storage.clone())
            .publish(fragment)
            .context("publish transferred attachments to the destination Files")?;
        report.publications += 1;
    }
    if !knowledge_new.is_empty() {
        let mut fragment = std::mem::replace(&mut stage.knowledge_blobs, Fragment::empty());
        fragment += knowledge_new;
        destination
            .publish(fragment)
            .context("publish transferred revisions to the destination Knowledge")?;
        report.publications += 1;
    }

    // Read back through the destination alone.
    let (held_revisions, held_files, _) =
        observe_destination(destination, destination_files, &stage)?;
    for (planned, held) in stage
        .revisions
        .iter()
        .chain(&stage.files)
        .zip(held_revisions.iter().chain(&held_files))
    {
        let missing = planned.facts.difference(held);
        anyhow::ensure!(
            missing.is_empty(),
            "published, but {} fact(s) of {:x} do not read back from the destination",
            missing.len(),
            planned.id
        );
    }
    // Facts are not enough: every carried revision must read as a revision
    // with its texts, and every carried file's bytes must read, through the
    // destination. A destination that already held the facts but cannot
    // read their payload fails here rather than reporting success.
    destination.views(&files_scope(destination_files), |view, auxiliary| {
        for planned in &stage.revisions {
            let records = knowledge_model::revision_records(&view.facts, planned.id);
            anyhow::ensure!(
                !records.is_empty(),
                "{:x} does not read back as a revision from the destination",
                planned.id
            );
            for record in records {
                revision_title(&view.reader, &record)?;
                revision_content(&view.reader, &record)
                    .with_context(|| format!("read {:x} back from the destination", planned.id))?;
            }
        }
        for planned in &stage.files {
            let files = auxiliary
                .first()
                .expect("attachments need a destination Files role");
            let contents = gather_file(files, planned.id).contents;
            anyhow::ensure!(
                !contents.is_empty(),
                "file {:x} does not read back from the destination Files",
                planned.id
            );
            for content in contents {
                let _: anybytes::Bytes =
                    BlobStoreGet::get(&view.reader, content).with_context(|| {
                        format!(
                            "the destination holds file {:x} but cannot read its bytes",
                            planned.id
                        )
                    })?;
            }
        }
        Ok(())
    })?;
    report.verified = true;
    Ok(report)
}
