//! `code share`: post one catalogued source span to a Discord channel.
//!
//! The caller names a REFERENCE — a selector, optionally a revision, a path
//! and a line range — and this module moves the bytes. The model never
//! carries the source text through its own context, and so can neither
//! paraphrase it nor truncate it: models choose references, software moves
//! bytes.
//!
//! The bytes come from the unit the declaration is placed in, as the pile
//! holds it, never from a working tree on disk. A working tree may have moved
//! on since the scan; the header names the revision the bytes are from, and
//! the bytes are exactly that revision's.
//!
//! Resolution is strict where `code show` is lenient. `show` answers with the
//! first definition it meets, which is fine for reading; posting the wrong one
//! of two `fn new` to a channel is not, so here more than one candidate
//! location refuses with the list, and nothing is sent.

use anyhow::{anyhow, bail, ensure, Context, Result};
use triblespace::core::repo::BlobStoreGet;
use triblespace::prelude::*;

use super::operations::{
    hit, keeps, placements_of_item, select_scans, Code, Filter, ScanSummary, Texts,
};
use crate::code;
use crate::discord::{AllowedMentions, Discord, Https, SendAttachment, SendReceipt, SendTransport};
use crate::schemas::code::Language;

/// Discord's message length limit, in characters, as `discord send` checks it.
pub const MESSAGE_LIMIT: usize = 2000;

/// How many candidates an ambiguity refusal lists before it says how many more.
const LISTED_CANDIDATES: usize = 20;

/// An inclusive, 1-based range of file lines.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LineRange {
    pub start: u64,
    pub end: u64,
}

impl LineRange {
    /// `A-B`, or `A` for a single line. Line numbers are the FILE's, as
    /// `code find` and `code show` print them.
    pub fn parse(raw: &str) -> Result<Self> {
        let raw = raw.trim();
        let (start, end) = match raw.split_once('-') {
            Some((start, end)) => (start.trim(), end.trim()),
            None => (raw, raw),
        };
        let number = |text: &str| -> Result<u64> {
            text.parse::<u64>()
                .map_err(|_| anyhow!("line range must be A-B or A with line numbers, got {raw:?}"))
        };
        let range = Self {
            start: number(start)?,
            end: number(end)?,
        };
        ensure!(range.start >= 1, "line numbers start at 1, got {raw:?}");
        ensure!(
            range.start <= range.end,
            "line range {raw:?} ends before it starts"
        );
        Ok(range)
    }
}

/// What to share: the same selector and revision vocabulary as `code show`
/// and `code find`, plus a path to tell placements apart.
#[derive(Clone, Debug, Default)]
pub struct ShareRequest {
    /// An item id prefix (at least four hex digits), or a declaration name.
    pub selector: String,
    /// Which revisions to resolve in, and the kind/repo narrowing.
    pub filter: Filter,
    /// Only a placement at exactly this repo-relative path.
    pub path: Option<String>,
    /// A slice of the declaration, in file line numbers.
    pub lines: Option<LineRange>,
    /// Literal prose posted with the header, above the code.
    pub note: Option<String>,
}

/// How the span travels.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Delivery {
    /// Inline, in a fenced code block in the message itself.
    Block,
    /// As a file attached to a short header message, for this reason.
    Attachment { filename: String, reason: String },
}

/// One resolved, formatted share, before anything leaves the process.
#[derive(Clone, Debug)]
pub struct SharePlan {
    pub item: Id,
    pub repo: String,
    pub path: String,
    /// The scan's full revision: a git commit, or `worktree:<digest>`.
    pub commit: String,
    pub kind: Option<String>,
    pub name: Option<String>,
    /// The declaration's whole span.
    pub declaration: LineRange,
    /// The span shared, inside the declaration's.
    pub lines: LineRange,
    pub language: Option<Language>,
    /// The exact bytes of the shared lines, as the unit holds them.
    pub bytes: Vec<u8>,
    pub delivery: Delivery,
    /// The message body that is posted.
    pub text: String,
    pub attachments: Vec<SendAttachment>,
}

/// A completed share: what was posted, and Discord's stored observation.
#[derive(Clone, Debug)]
pub struct ShareReceipt {
    pub plan: SharePlan,
    pub sent: SendReceipt,
}

/// One place a selector could mean.
struct Candidate {
    scan: ScanSummary,
    hit: super::operations::Hit,
}

impl Code {
    /// Resolve and format a share without sending anything.
    pub fn share_plan(&self, request: &ShareRequest) -> Result<SharePlan> {
        if let Some(note) = &request.note {
            validate_note(note)?;
        }
        self.with_operation(|code| code.share_plan_scoped(request))
    }

    /// Post one catalogued span to `channel_id` as the workspace's Discord bot.
    pub fn share(
        &self,
        request: &ShareRequest,
        channel_id: &str,
        reply_to: Option<&str>,
        discord: &Discord,
    ) -> Result<ShareReceipt> {
        self.share_through(request, channel_id, reply_to, discord, &mut Https)
    }

    /// [`Self::share`] through an explicit Discord transport.
    ///
    /// Every refusal here — a malformed channel or reply id, an unknown or
    /// ambiguous selector, a line range outside the declaration — happens
    /// before the transport is touched. The send itself is `discord send`'s
    /// own path ([`Discord::send_through`]): WRITE admission, a reply target
    /// stored in this channel, then the returned message stored as an
    /// ordinary observation.
    ///
    /// Nobody is pinged: shared source is data, and a doc comment that spells
    /// `@everyone` must not reach the whole server ([`AllowedMentions::nobody`]).
    pub fn share_through(
        &self,
        request: &ShareRequest,
        channel_id: &str,
        reply_to: Option<&str>,
        discord: &Discord,
        transport: &mut dyn SendTransport,
    ) -> Result<ShareReceipt> {
        crate::discord::validate_snowflake(channel_id)
            .with_context(|| format!("invalid channel id {channel_id:?}"))?;
        crate::discord::operations::validate_reply_to(reply_to)?;
        // The plan's read closes before the send opens its own session.
        let plan = self.share_plan(request)?;
        let sent = discord.send_through(
            channel_id,
            &plan.text,
            &plan.attachments,
            Some(&AllowedMentions::nobody()),
            reply_to,
            transport,
        )?;
        Ok(ShareReceipt { plan, sent })
    }

    fn share_plan_scoped(&self, request: &ShareRequest) -> Result<SharePlan> {
        let observed = self.observe()?;
        let facts = crate::storage::acquire_attached_facts(&observed).context("read Code facts")?;
        let reader = observed.snapshot();
        let mut texts = Texts::new(reader);
        let scans = select_scans(&facts, &mut texts, &request.filter)?;
        ensure!(
            !scans.is_empty(),
            "no Code scan matches these revisions, so nothing was sent; `code stats` lists what \
             the catalogue holds"
        );

        let selector = request.selector.trim();
        ensure!(!selector.is_empty(), "empty selector");
        let mut candidates: Vec<Candidate> = Vec::new();
        let mut push = |scan: &ScanSummary, located: code::Located, texts: &mut Texts<'_>| {
            let hit = hit(&facts, texts, scan, located)?;
            if !keeps(&hit, &request.filter) {
                return Ok::<_, anyhow::Error>(());
            }
            if request.path.as_ref().is_some_and(|path| &hit.path != path) {
                return Ok(());
            }
            candidates.push(Candidate {
                scan: scan.clone(),
                hit,
            });
            Ok(())
        };
        // An id prefix and a name are both tried, and both count: a selector
        // that is a hex prefix of one item AND the name of another is
        // ambiguous, and saying so beats guessing which was meant.
        for item in items_with_prefix(&facts, selector) {
            for scan in &scans {
                for located in placements_of_item(&facts, scan.id, item) {
                    push(scan, located, &mut texts)?;
                }
            }
        }
        let name = code::text_handle(selector);
        for scan in &scans {
            for located in code::definitions_in_scan(&facts, scan.id, name) {
                push(scan, located, &mut texts)?;
            }
        }
        candidates.sort_by(|left, right| {
            (
                &left.hit.repo,
                &left.hit.path,
                left.hit.line,
                &left.scan.commit,
            )
                .cmp(&(
                    &right.hit.repo,
                    &right.hit.path,
                    right.hit.line,
                    &right.scan.commit,
                ))
        });
        candidates.dedup_by(|left, right| {
            left.scan.id == right.scan.id
                && left.hit.unit == right.hit.unit
                && left.hit.item == right.hit.item
                && left.hit.line == right.hit.line
                && left.hit.end_line == right.hit.end_line
        });

        let labels: Vec<String> = scans.iter().map(ScanSummary::label).collect();
        let chosen = match candidates.len() {
            0 => bail!(
                "no catalogued declaration matches {selector:?}{} at {}, so nothing was sent",
                narrowing(request),
                labels.join(" · ")
            ),
            1 => candidates.pop().expect("one candidate"),
            count => {
                let mut message = format!(
                    "{selector:?} matches {count} placements{}, so nothing was sent; narrow it \
                     with an item id prefix, --file, --kind, --repo or --at:",
                    narrowing(request)
                );
                for candidate in candidates.iter().take(LISTED_CANDIDATES) {
                    message.push_str(&format!("\n  {}", candidate_line(candidate)));
                }
                if count > LISTED_CANDIDATES {
                    message.push_str(&format!("\n  … and {} more", count - LISTED_CANDIDATES));
                }
                bail!(message)
            }
        };

        let Candidate { scan, hit } = chosen;
        let declaration = LineRange {
            start: hit.line,
            end: hit.end_line,
        };
        let lines = request.lines.unwrap_or(declaration);
        ensure!(
            declaration.start <= lines.start && lines.end <= declaration.end,
            "lines {}-{} are outside {}'s lines {}-{} in {}/{}, so nothing was sent",
            lines.start,
            lines.end,
            describe(&hit),
            declaration.start,
            declaration.end,
            hit.repo,
            hit.path,
        );

        let content = unit_bytes(&facts, reader, hit.unit)?.ok_or_else(|| {
            anyhow!(
                "the content of {}/{} at {} is not resident in this pile, so nothing was sent",
                hit.repo,
                hit.path,
                scan.label()
            )
        })?;
        let bytes = line_slice(&content, lines).ok_or_else(|| {
            anyhow!(
                "{}/{} at {} has fewer than {} lines; the catalogue's span disagrees with its \
                 content, so nothing was sent",
                hit.repo,
                hit.path,
                scan.label(),
                lines.end
            )
        })?;
        let language = code::unit_languages(&facts, hit.unit).into_iter().min();

        let header = header(
            &hit.repo,
            &hit.path,
            &scan.commit,
            lines,
            &describe(&hit),
            request.note.as_deref(),
        );
        let (delivery, text, attachments) =
            format_message(&header, &hit.path, lines, language, bytes);
        Ok(SharePlan {
            item: hit.item,
            repo: hit.repo,
            path: hit.path,
            commit: scan.commit,
            kind: hit.kind,
            name: hit.name,
            declaration,
            lines,
            language,
            bytes: bytes.to_vec(),
            delivery,
            text,
            attachments,
        })
    }
}

fn validate_note(note: &str) -> Result<()> {
    ensure!(
        note.chars().count() <= MESSAGE_LIMIT / 2,
        "note exceeds {} characters",
        MESSAGE_LIMIT / 2
    );
    Ok(())
}

/// Item ids this selector is a hex prefix of: at least four hex digits, as
/// `code show` accepts them.
fn items_with_prefix(facts: &crate::storage::FactArchive, selector: &str) -> Vec<Id> {
    if selector.len() < 4
        || !selector
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    {
        return Vec::new();
    }
    let prefix = selector.to_ascii_lowercase();
    let mut items: Vec<Id> = code::all_item_ids(facts)
        .into_iter()
        .filter(|item| format!("{item:x}").starts_with(&prefix))
        .collect();
    items.sort();
    items.dedup();
    items
}

fn narrowing(request: &ShareRequest) -> String {
    let mut parts = Vec::new();
    if let Some(kind) = &request.filter.kind {
        parts.push(format!("kind {kind}"));
    }
    if let Some(repo) = &request.filter.repo {
        parts.push(format!("repo {repo}"));
    }
    if let Some(path) = &request.path {
        parts.push(format!("file {path}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" (with {})", parts.join(", "))
    }
}

fn describe(hit: &super::operations::Hit) -> String {
    match (&hit.kind, &hit.name) {
        (Some(kind), Some(name)) => format!("{kind} {name}"),
        (None, Some(name)) => name.clone(),
        (Some(kind), None) => kind.clone(),
        (None, None) => format!("item {:x}", hit.item),
    }
}

fn candidate_line(candidate: &Candidate) -> String {
    format!(
        "{} {}/{}:{}-{} at {} (item {:x})",
        describe(&candidate.hit),
        candidate.hit.repo,
        candidate.hit.path,
        candidate.hit.line,
        candidate.hit.end_line,
        candidate.scan.label(),
        candidate.hit.item,
    )
}

/// The unit's bytes, from the first content handle this pile holds.
fn unit_bytes(
    facts: &crate::storage::FactArchive,
    reader: &super::operations::CodeReader,
    unit: Id,
) -> Result<Option<Vec<u8>>> {
    for handle in code::unit_content(facts, unit) {
        let bytes: anybytes::Bytes = match reader.get(handle) {
            Ok(bytes) => bytes,
            Err(_) => continue,
        };
        return Ok(Some(bytes.as_ref().to_vec()));
    }
    Ok(None)
}

/// Lines `start..=end` of `content`, each with its own line terminator, as
/// bytes. `None` when the content has fewer lines.
///
/// A line is what ends at `\n` (or at the end of the content), so `\r\n`
/// stays `\r\n` and a final line without a newline stays without one. The
/// slice is the file's bytes, never a re-joined `lines()`.
pub fn line_slice(content: &[u8], range: LineRange) -> Option<&[u8]> {
    let mut starts = vec![0usize];
    for (at, byte) in content.iter().enumerate() {
        if *byte == b'\n' && at + 1 < content.len() {
            starts.push(at + 1);
        }
    }
    if content.is_empty() {
        starts.clear();
    }
    let first = usize::try_from(range.start).ok()?.checked_sub(1)?;
    let last = usize::try_from(range.end).ok()?.checked_sub(1)?;
    if last >= starts.len() || first > last {
        return None;
    }
    let begin = starts[first];
    let finish = starts.get(last + 1).copied().unwrap_or(content.len());
    Some(&content[begin..finish])
}

/// The commit as a header names it: twelve hex digits of a git commit, or the
/// worktree digest's, marked as uncommitted.
fn short_revision(commit: &str) -> String {
    match commit.strip_prefix("worktree:") {
        Some(digest) => format!("worktree:{} (uncommitted)", &digest[..digest.len().min(12)]),
        None => commit.chars().take(12).collect(),
    }
}

fn header(
    repo: &str,
    path: &str,
    commit: &str,
    lines: LineRange,
    what: &str,
    note: Option<&str>,
) -> String {
    let span = if lines.start == lines.end {
        format!("line {}", lines.start)
    } else {
        format!("lines {}-{}", lines.start, lines.end)
    };
    let mut header = format!(
        "`{repo}/{path}` {span} at {repo}@{} ({what})",
        short_revision(commit)
    );
    if let Some(note) = note.map(str::trim).filter(|note| !note.is_empty()) {
        header.push('\n');
        header.push_str(note);
    }
    header
}

/// The fence tag Discord's highlighter knows for a language.
fn fence_tag(language: Option<Language>) -> &'static str {
    match language {
        Some(Language::Rust) => "rust",
        Some(Language::Toml) => "toml",
        Some(Language::Markdown) => "md",
        Some(Language::Shell) => "sh",
        Some(Language::Typst) => "typst",
        Some(Language::Text) | None => "",
    }
}

/// `operations.rs` lines 479-482 as `operations.L479-482.rs`, so the file
/// keeps the extension Discord previews and highlights by.
fn attachment_name(path: &str, lines: LineRange) -> String {
    let leaf = path.rsplit('/').next().unwrap_or(path);
    let span = format!("L{}-{}", lines.start, lines.end);
    match leaf.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => format!("{stem}.{span}.{extension}"),
        _ => format!("{leaf}.{span}"),
    }
}

/// A code block when the whole message fits Discord's limit and the bytes can
/// sit in a fence unchanged; otherwise an attachment carrying the exact bytes.
/// Nothing is ever cut to fit.
pub fn format_message(
    header: &str,
    path: &str,
    lines: LineRange,
    language: Option<Language>,
    bytes: &[u8],
) -> (Delivery, String, Vec<SendAttachment>) {
    let attach = |reason: String| {
        let filename = attachment_name(path, lines);
        let utf8 = std::str::from_utf8(bytes).is_ok();
        let text = format!(
            "{header}\nattached as {filename}: {} line(s), {} byte(s), exact",
            lines.end - lines.start + 1,
            bytes.len()
        );
        let attachment = SendAttachment {
            filename: filename.clone(),
            media_type: if utf8 {
                "text/plain".to_owned()
            } else {
                "application/octet-stream".to_owned()
            },
            bytes: bytes.to_vec(),
        };
        (
            Delivery::Attachment { filename, reason },
            text,
            vec![attachment],
        )
    };
    let Ok(source) = std::str::from_utf8(bytes) else {
        return attach("the bytes are not UTF-8".to_owned());
    };
    if source.contains("```") {
        return attach("the source contains a code fence".to_owned());
    }
    // The fence supplies the final newline, so one trailing terminator is
    // left to it rather than doubled; every other byte is kept.
    let body = source
        .strip_suffix("\r\n")
        .or_else(|| source.strip_suffix('\n'))
        .unwrap_or(source);
    let text = format!("{header}\n```{}\n{body}\n```", fence_tag(language));
    let length = text.chars().count();
    if length > MESSAGE_LIMIT {
        return attach(format!(
            "a code block would be {length} characters, over Discord's {MESSAGE_LIMIT}"
        ));
    }
    (Delivery::Block, text, Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(start: u64, end: u64) -> LineRange {
        LineRange { start, end }
    }

    #[test]
    fn a_line_range_parses_and_refuses_what_is_not_one() {
        assert_eq!(LineRange::parse("3-5").unwrap(), range(3, 5));
        assert_eq!(LineRange::parse(" 7 ").unwrap(), range(7, 7));
        assert_eq!(LineRange::parse("2 - 4").unwrap(), range(2, 4));
        for bad in ["5-3", "0-2", "0", "a-b", "", "-3", "3-", "1-2-3"] {
            assert!(LineRange::parse(bad).is_err(), "{bad:?} parsed");
        }
    }

    #[test]
    fn a_line_slice_is_the_files_own_bytes() {
        let content = b"one\r\ntwo\nthree";
        assert_eq!(line_slice(content, range(1, 1)).unwrap(), b"one\r\n");
        assert_eq!(line_slice(content, range(2, 3)).unwrap(), b"two\nthree");
        assert_eq!(line_slice(content, range(1, 3)).unwrap(), content);
        assert!(line_slice(content, range(3, 4)).is_none());
        assert!(line_slice(content, range(0, 1)).is_none());
        // A trailing newline ends the last line rather than starting another.
        assert_eq!(line_slice(b"a\nb\n", range(2, 2)).unwrap(), b"b\n");
        assert!(line_slice(b"a\nb\n", range(3, 3)).is_none());
        assert!(line_slice(b"", range(1, 1)).is_none());
    }

    #[test]
    fn the_header_names_path_lines_revision_and_declaration() {
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let text = header("faculties", "src/a.rs", commit, range(4, 9), "fn f", None);
        assert_eq!(
            text,
            "`faculties/src/a.rs` lines 4-9 at faculties@0123456789ab (fn f)"
        );
        let one = header(
            "r",
            "a.rs",
            "worktree:ffffeeeeddddccccbbbb",
            range(2, 2),
            "x",
            Some(" why "),
        );
        assert_eq!(
            one,
            "`r/a.rs` line 2 at r@worktree:ffffeeeedddd (uncommitted) (x)\nwhy"
        );
    }

    #[test]
    fn a_short_span_is_a_block_and_a_long_one_an_exact_attachment() {
        let (delivery, text, attachments) = format_message(
            "h",
            "src/a.rs",
            range(1, 2),
            Some(Language::Rust),
            b"fn f() {\n}\n",
        );
        assert_eq!(delivery, Delivery::Block);
        assert_eq!(text, "h\n```rust\nfn f() {\n}\n```");
        assert!(attachments.is_empty());

        // Exactly at the limit is still a block; one more character is not.
        let fixed = "h\n```rust\n\n```".chars().count();
        let fits = "x".repeat(MESSAGE_LIMIT - fixed);
        let (delivery, text, _) = format_message(
            "h",
            "a.rs",
            range(1, 1),
            Some(Language::Rust),
            fits.as_bytes(),
        );
        assert_eq!(delivery, Delivery::Block);
        assert_eq!(text.chars().count(), MESSAGE_LIMIT);
        let over = format!("{fits}x");
        let (delivery, text, attachments) = format_message(
            "h",
            "src/a.rs",
            range(3, 3),
            Some(Language::Rust),
            over.as_bytes(),
        );
        assert!(
            matches!(delivery, Delivery::Attachment { ref filename, .. } if filename == "a.L3-3.rs")
        );
        assert!(text.starts_with("h\nattached as a.L3-3.rs"), "{text}");
        assert_eq!(attachments[0].bytes, over.as_bytes());
        assert_eq!(attachments[0].media_type, "text/plain");
    }

    #[test]
    fn a_fence_or_non_utf8_bytes_go_as_an_attachment_unchanged() {
        let fenced = b"/// ```\n/// x\n/// ```\nfn f() {}\n";
        let (delivery, _, attachments) =
            format_message("h", "a.rs", range(1, 4), Some(Language::Rust), fenced);
        assert!(matches!(delivery, Delivery::Attachment { .. }));
        assert_eq!(attachments[0].bytes, fenced);
        let binary = b"\xff\xfe\n";
        let (delivery, _, attachments) = format_message("h", "Makefile", range(1, 1), None, binary);
        assert!(
            matches!(delivery, Delivery::Attachment { ref filename, .. } if filename == "Makefile.L1-1")
        );
        assert_eq!(attachments[0].media_type, "application/octet-stream");
        assert_eq!(attachments[0].bytes, binary);
    }
}
