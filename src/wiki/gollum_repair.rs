//! Correct already imported Gollum histories whose images an earlier converter
//! rendered as links.
//!
//! Imported revisions are immutable and a repeated import reuses them, so a
//! better converter alone changes nothing that was imported before it. The
//! correction is one successor per current head of every affected entry,
//! superseding exactly that head: its own text with each earlier image link
//! replaced by the embedded image. Text a person wrote after the import is
//! kept, and an image is only replaced where its place is certain:
//!
//! - when the head is still the imported text, and that text is exactly the
//!   converter's earlier rendering (page-link targets aside), each image is
//!   replaced at its own position;
//! - otherwise only where the whole run of lines around the image occurs
//!   exactly once in the imported ancestor and exactly once in the head.
//!
//! Everything else is reported and left alone. Revision ids derive from
//! author, title, content, tags and predecessors, so repeating a correction
//! converges and a corrected head has nothing left to replace.

use super::*;

/// What one head of an imported entry receives.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadRepair {
    pub entry: Id,
    pub head: Id,
    pub title: String,
    /// The successor revision; the same id whether or not it was published.
    pub successor: Option<Id>,
    pub applied: usize,
    /// One line per image left as it is, with the reason.
    pub skipped: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageRepairReport {
    /// Heads whose nearest imported ancestor embeds images, in entry order.
    pub heads: Vec<HeadRepair>,
    /// Heads that gained a successor while this run was planning; left alone.
    pub moved: Vec<Id>,
    pub publications: usize,
}

impl ImageRepairReport {
    pub fn corrected(&self) -> usize {
        self.heads
            .iter()
            .filter(|head| head.successor.is_some())
            .count()
    }
}

/// Typst of an attachment-only plan link resolved; page links stay as they are.
fn resolve_attachments(text: &str, attachments: &HashMap<String, ContentHandle>) -> String {
    static PLAN: OnceLock<regex::Regex> = OnceLock::new();
    let re = PLAN.get_or_init(|| {
        regex::Regex::new(r#"(#link\(|#image\(|#figure\(image\()"plan:([^"]+)""#).unwrap()
    });
    re.replace_all(text, |captures: &regex::Captures| {
        match attachments.get(&captures[2]) {
            Some(handle) => format!("{}\"files:{}\"", &captures[1], content_hash_hex(*handle)),
            None => captures[0].to_owned(),
        }
    })
    .into_owned()
}

/// Page-link targets differ between a plan and its import; compare without them.
fn mask_pages(text: &str) -> String {
    static PAGE: OnceLock<regex::Regex> = OnceLock::new();
    let re = PAGE.get_or_init(|| regex::Regex::new(r#"#link\("(?:wiki|plan):[^"]*"\)"#).unwrap());
    re.replace_all(text, "#link(\"\u{a7}\")").into_owned()
}

fn names_page(text: &str) -> bool {
    mask_pages(text) != text
}

/// Byte offset of the `ordinal`-th non-overlapping occurrence of `needle`.
fn nth(haystack: &str, needle: &str, ordinal: usize) -> Option<usize> {
    haystack
        .match_indices(needle)
        .nth(ordinal)
        .map(|(at, _)| at)
}

/// Byte offsets of every line start, and one past the end.
fn line_starts(text: &str) -> Vec<usize> {
    let mut starts = vec![0];
    starts.extend(text.match_indices('\n').map(|(at, _)| at + 1));
    starts
}

/// Byte range of every run of `lines` consecutive lines equal to `run` (masked).
fn runs(text: &str, run: &str) -> Vec<(usize, usize)> {
    let lines = run.split('\n').count();
    let want = mask_pages(run);
    let starts = line_starts(text);
    let line_end = |index: usize| -> usize {
        starts
            .get(index + 1)
            .map(|next| next - 1)
            .unwrap_or(text.len())
    };
    let mut found = Vec::new();
    for first in 0..starts.len() {
        let last = first + lines - 1;
        if last >= starts.len() {
            break;
        }
        let (start, end) = (starts[first], line_end(last));
        if start > end {
            continue;
        }
        if mask_pages(&text[start..end]) == want {
            found.push((start, end));
        }
    }
    found
}

struct Site<'a> {
    image: &'a PlanImage,
    legacy: String,
    display: String,
    anchor: String,
}

/// The edits for one head: (start, end, replacement) in its text.
fn plan_edits(
    sites: &[Site<'_>],
    base: &str,
    head: &str,
    verbatim: bool,
    skipped: &mut Vec<String>,
) -> Vec<(usize, usize, String)> {
    let mut edits = Vec::new();
    for site in sites {
        let label = site.legacy.replace('\n', "\u{23ce}");
        let refuse = |skipped: &mut Vec<String>, reason: &str| {
            skipped.push(format!("{reason}: {label}"));
        };
        if names_page(&site.legacy) {
            refuse(skipped, "its earlier rendering links a page");
            continue;
        }
        if verbatim {
            match site
                .image
                .ordinal
                .and_then(|ordinal| nth(head, &site.legacy, ordinal))
            {
                Some(at) => edits.push((at, at + site.legacy.len(), site.display.clone())),
                None => refuse(skipped, "not found at its place"),
            }
            continue;
        }
        let (in_base, in_head) = (runs(base, &site.anchor), runs(head, &site.anchor));
        match (
            in_base.as_slice(),
            in_head.as_slice(),
            site.image.anchor_ordinal,
        ) {
            ([_], [(start, end)], Some(ordinal)) => {
                match nth(&head[*start..*end], &site.legacy, ordinal) {
                    Some(at) => edits.push((
                        start + at,
                        start + at + site.legacy.len(),
                        site.display.clone(),
                    )),
                    None => refuse(skipped, "its lines were edited"),
                }
            }
            (_, [], _) => refuse(skipped, "its lines were edited or removed"),
            ([], _, _) => refuse(skipped, "the imported text has other lines here"),
            _ => refuse(skipped, "its lines occur more than once"),
        }
    }
    edits.sort();
    let mut kept: Vec<(usize, usize, String)> = Vec::new();
    for edit in edits {
        if kept.last().is_some_and(|last| last.1 > edit.0) {
            skipped.push(format!("overlaps another image: {}", &head[edit.0..edit.1]));
            continue;
        }
        kept.push(edit);
    }
    kept
}

pub(super) fn repair(
    storage: WikiStorage<'_>,
    plan: ImportPlan,
    apply: bool,
) -> Result<ImageRepairReport> {
    let mut attachments: HashMap<String, ContentHandle> = HashMap::new();
    let mut occurrences: HashMap<(schema::TextHandle, schema::TextHandle), &PlanRevision> =
        HashMap::new();
    for commit in &plan.commits {
        for attachment in &commit.attachments {
            let blob: Blob<blobencodings::RawBytes> = attachment.bytes.clone().to_blob();
            attachments.insert(attachment.key.clone(), blob.get_handle());
        }
        let sha: schema::TextHandle = commit.commit.clone().to_blob().get_handle();
        // Every occurrence, with or without images: the walk from a head stops
        // at the nearest imported revision, so a later source commit that
        // turned an image into a link is never "corrected" back.
        for revision in &commit.revisions {
            let path: schema::TextHandle = revision.path.clone().to_blob().get_handle();
            occurrences.insert((sha, path), revision);
        }
    }

    let (author_fragment, author) = storage.author_fragment()?;
    let (report, staged) = storage.view_with_scope(FILES_SCOPE_ID, "Files", |view, files| {
        use crate::schemas::code::attrs as code;
        let mut imported: HashMap<Id, Vec<&PlanRevision>> = HashMap::new();
        for (revision, commit, path) in find!(
            (revision: Id, commit: schema::TextHandle, path: schema::TextHandle),
            pattern!(&view.facts, [{
                _?origin @ metadata::tag: &schema::KIND_SOURCE_PROVENANCE,
                schema::attrs::revision: ?revision,
                code::commit: ?commit,
                file::source_path: ?path,
            }])
        ) {
            if let Some(plan_revision) = occurrences.get(&(commit, path)) {
                imported.entry(revision).or_default().push(plan_revision);
            }
        }
        let resolver = ReferenceResolver {
            wiki: &view.facts,
            files: Some(files),
        };
        let images = FilesImages {
            files: Some(files),
            reader: &view.reader,
        };
        let mut report = ImageRepairReport::default();
        let mut staged: Vec<(Id, Fragment)> = Vec::new();
        for entry in wiki_model::entries(&view.facts, &view.latest) {
            for head in &entry.frontier {
                // The nearest ancestors, head included, that this plan imported.
                let mut level = BTreeSet::from([head.id]);
                let mut seen = BTreeSet::new();
                let ancestors = loop {
                    let found: BTreeSet<Id> = level
                        .iter()
                        .copied()
                        .filter(|id| imported.contains_key(id))
                        .collect();
                    if !found.is_empty() || level.is_empty() {
                        break found;
                    }
                    seen.extend(level.iter().copied());
                    level = level
                        .iter()
                        .flat_map(|id| wiki_model::revision_records(&view.facts, *id))
                        .flat_map(|record| record.supersedes)
                        .filter(|id| !seen.contains(id))
                        .collect();
                };
                let with_images = |id: &Id| imported[id].iter().any(|r| !r.images.is_empty());
                if !ancestors.iter().any(with_images) {
                    continue;
                }
                let title = read_string(&view.reader, head.title)?;
                let mut repair = HeadRepair {
                    entry: entry.label(),
                    head: head.id,
                    title: title.clone(),
                    successor: None,
                    applied: 0,
                    skipped: Vec::new(),
                };
                let ancestor = match ancestors.iter().collect::<Vec<_>>().as_slice() {
                    [one] => **one,
                    _ => {
                        repair
                            .skipped
                            .push("several imported ancestors; correct it by hand".to_owned());
                        report.heads.push(repair);
                        continue;
                    }
                };
                let base_records = wiki_model::revision_records(&view.facts, ancestor);
                let base_contents: BTreeSet<_> =
                    base_records.iter().map(|record| record.content).collect();
                let [base] = base_contents.into_iter().collect::<Vec<_>>()[..] else {
                    repair
                        .skipped
                        .push("its imported ancestor has several texts".to_owned());
                    report.heads.push(repair);
                    continue;
                };
                let base = read_string(&view.reader, base)?;
                let text = read_string(&view.reader, head.content)?;
                // The plan occurrence whose earlier rendering is this text, if any.
                let rendered = |revision: &PlanRevision| {
                    revision.legacy_content.as_deref().map(|legacy| {
                        mask_pages(&resolve_attachments(
                            &lint_fix(legacy, resolver),
                            &attachments,
                        ))
                    })
                };
                let candidates: Vec<&PlanRevision> = imported[&ancestor]
                    .iter()
                    .copied()
                    .filter(|revision| !revision.images.is_empty())
                    .collect();
                if candidates.len() < imported[&ancestor].len() {
                    repair.skipped.push(
                        "its imported ancestor stands for several source occurrences, not all with images"
                            .to_owned(),
                    );
                    report.heads.push(repair);
                    continue;
                }
                // Imported with images already embedded: nothing to correct.
                let masked_base = mask_pages(&base);
                if candidates.iter().any(|revision| {
                    mask_pages(&resolve_attachments(
                        &lint_fix(&revision.content, resolver),
                        &attachments,
                    )) == masked_base
                }) {
                    continue;
                }
                let exact = candidates
                    .iter()
                    .copied()
                    .find(|revision| rendered(revision).as_deref() == Some(&masked_base));
                let source = exact.unwrap_or(candidates[0]);
                let sites: Vec<Site<'_>> = source
                    .images
                    .iter()
                    .map(|image| Site {
                        image,
                        legacy: resolve_attachments(
                            &lint_fix(&image.legacy, resolver),
                            &attachments,
                        ),
                        display: resolve_attachments(&image.display, &attachments),
                        anchor: resolve_attachments(
                            &lint_fix(&image.anchor, resolver),
                            &attachments,
                        ),
                    })
                    .collect();
                let verbatim = exact.is_some() && text == base;
                let edits = plan_edits(&sites, &base, &text, verbatim, &mut repair.skipped);
                if edits.is_empty() {
                    report.heads.push(repair);
                    continue;
                }
                let mut corrected = text.clone();
                for (start, end, replacement) in edits.iter().rev() {
                    corrected.replace_range(*start..*end, replacement);
                }
                let corrected =
                    match prepare_content(&corrected, &view.facts, Some(files), true, &images) {
                        Ok(content) => content,
                        Err(error) => {
                            repair
                                .skipped
                                .push(format!("the corrected text is refused: {error:#}"));
                            report.heads.push(repair);
                            continue;
                        }
                    };
                let (record, successor) = wiki_model::revision_record(RevisionDraft {
                    title,
                    content: corrected,
                    tags: head.tags.clone(),
                    predecessors: BTreeSet::from([head.id]),
                    author,
                    authored_at: now_interval()?,
                })?;
                staged.push((head.id, record));
                repair.successor = Some(successor);
                repair.applied = edits.len();
                report.heads.push(repair);
            }
        }
        Ok((report, staged))
    })?;
    let mut report = report;
    if !apply || report.corrected() == 0 {
        return Ok(report);
    }
    // Recheck the heads just before publishing: a head that gained a
    // successor meanwhile keeps it, and its correction is not published.
    let planned: BTreeSet<Id> = report
        .heads
        .iter()
        .filter(|head| head.successor.is_some())
        .map(|head| head.head)
        .collect();
    let moved: BTreeSet<Id> = storage.view(|view| {
        Ok(planned
            .iter()
            .copied()
            .filter(|head| {
                exists!(
                    (),
                    pattern!(&view.facts, [{ _?later @ metadata::supersedes: head }])
                )
            })
            .collect())
    })?;
    let mut publish = Fragment::empty();
    for (head, record) in staged {
        if !moved.contains(&head) {
            publish += record;
        }
    }
    for head in &mut report.heads {
        if moved.contains(&head.head) {
            head.successor = None;
        }
    }
    report.moved = moved.into_iter().collect();
    if report.corrected() == 0 {
        return Ok(report);
    }
    let mut output = author_fragment;
    output += publish;
    storage.publish(output)?;
    report.publications = 1;
    Ok(report)
}
