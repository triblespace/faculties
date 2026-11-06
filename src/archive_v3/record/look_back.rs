//! The R1 look-back: whether a rendering a record marks beside a tool
//! result is built, decided over the places before it on its parent's
//! line. The table's [`LookBack`] says where it applies: which records
//! carry the mark, at which field, and which records hold his prompts. The
//! Claude Code importer and the relations (`grammar::relations`) both
//! decide by it; neither owns it.

use std::collections::HashMap;

use super::json::Json;
use super::Failure;
use crate::archive_v3::fields::LookBack;

/// One place on the line before a beside-mark, as the R1 look-back reads
/// it: what its item is, and what the item's record claims. The importer's,
/// and the relations' (`grammar::relations`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Before {
    /// An item of an attachment record.
    pub(crate) attachment: bool,
    /// The head of a tool result, and a part of one.
    pub(crate) head: bool,
    pub(crate) part: bool,
    /// A tool call.
    pub(crate) call: bool,
    /// The span of unknown items a record outside the unit held.
    pub(crate) span: bool,
    /// A text of his typed prompt, and one of a prompt written as the plain
    /// string ([`typed_prompt`]).
    pub(crate) prompt: bool,
    pub(crate) plain_prompt: bool,
    /// Its record claims harness text, or a compaction summary.
    pub(crate) harness_written: bool,
    pub(crate) compaction_summary: bool,
}

/// R1'', R1''': whether an item of a record of the look-back's `prompt`
/// shape is a text of his typed prompt (`text`, outside any tool result:
/// neither its `head` nor a `part`), and whether the prompt is written as
/// the plain string, not blocks. The importer's rule, and the relations'
/// (`grammar::relations`).
pub(crate) fn typed_prompt(
    look_back: &LookBack,
    record: &Json,
    text: bool,
    head: bool,
    part: bool,
) -> (bool, bool) {
    // His typed prompt: the record's own text, unless it is harness text
    // or a compaction summary (R1'').
    let typed = !look_back
        .unprompted
        .iter()
        .any(|field| record.is_true(field));
    let plain = matches!(record.at(look_back.plain), Some(Json::Str(_)));
    let prompt = typed && text && !head && !part;
    (prompt, prompt && plain)
}

/// R1': whether the client marks a record's rendering at the look-back's
/// `mark` (Claude Code's `renderedBesideToolResult`). The mark is the JSON
/// boolean `true`; any other spelling fails. The importer's rule, and the
/// relations' (`grammar::relations`).
pub(crate) fn beside_marked(look_back: &LookBack, record: &Json) -> Result<bool, Failure> {
    Ok(match record.at(look_back.mark.0) {
        None => false,
        Some(Json::Bool(true)) => true,
        Some(_) => {
            return Err(Failure::new(
                look_back.mark.1,
                "a beside-mark that is not the boolean true",
            ))
        }
    })
}

/// R1: the place before a `renderedBesideToolResult` rendering, past
/// the renderings of the attachments before it, holds his typed prompt
/// (R1''), written as a plain string (R1''': every real mark in the
/// round-6 sample follows one; a prompt of blocks fails). Beside a result, between a call and its result (R1'), after
/// a span of unknown items, or after anything else (her text, harness
/// text, a compaction summary, the start of her message), the
/// rendering is not built. Of the 727 marks in the live transcripts
/// under 60 MB (round 6, counts only), 720 follow his prompt, 4 harness
/// text and 3 a compaction summary; none follows anything else.
/// `line` is the places before it, last first. The importer's rule, and
/// the relations' (`grammar::relations`).
pub(crate) fn beside_his_prompt(
    look_back: &LookBack,
    line: impl IntoIterator<Item = Before>,
) -> Result<(), Failure> {
    let path = look_back.mark.1;
    for item in line {
        if item.attachment {
            continue;
        }
        if item.head || item.part {
            return Err(Failure::new(
                path,
                "rendered beside a tool result: the one framed case held the text with a trailing newline, not the exact text; not built",
            ));
        }
        if item.call {
            return Err(Failure::new(
                path,
                "rendered between a call and its result: no real mark follows anything but a typed prompt; not built",
            ));
        }
        if item.span {
            return Err(Failure::new(
                path,
                "rendered after a span of unknown items: whether a tool result is beside it is unknown",
            ));
        }
        if item.plain_prompt {
            return Ok(());
        }
        // R1''': every real mark follows a prompt written as a plain
        // string; one of blocks (an image beside his text, a result
        // before it) is unverified.
        if item.prompt {
            return Err(Failure::new(
                path,
                "rendered after a prompt of blocks: every real mark follows a plain string prompt; not built",
            ));
        }
        // Real marks do follow harness text and compaction summaries,
        // but what her window held there is unverified.
        if item.harness_written {
            return Err(Failure::new(
                path,
                "rendered after harness text: what her window held there is unverified; not built",
            ));
        }
        if item.compaction_summary {
            return Err(Failure::new(
                path,
                "rendered after a compaction summary: what her window held there is unverified; not built",
            ));
        }
        break;
    }
    Err(Failure::new(
        path,
        "rendered after something other than his typed prompt: no real mark stands there; not built",
    ))
}

/// R1'': a beside-mark in the middle of one message: the message began
/// before the mark (`begun`) and the record after it continues it, up from
/// its `parent` through the attachments (`attachment_parents`: uuid -> its
/// parent's uuid, whether it is beside-marked). The importer's rule, and
/// the relations' (`grammar::relations`).
pub(crate) fn in_one_message(
    look_back: &LookBack,
    begun: bool,
    parent: Option<&str>,
    attachment_parents: &HashMap<String, (Option<String>, bool)>,
) -> Result<(), Failure> {
    if begun {
        let mut cursor = parent;
        // At most one step per attachment, whatever their parents say.
        for _ in 0..attachment_parents.len() {
            let Some((up, beside)) = cursor.and_then(|at| attachment_parents.get(at)) else {
                break;
            };
            if *beside {
                return Err(Failure::new(
                    look_back.mark.1,
                    "rendered in the middle of one message: no real mark stands there; not built",
                ));
            }
            cursor = up.as_deref();
        }
    }
    Ok(())
}
