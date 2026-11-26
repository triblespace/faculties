//! A wake read by tools rather than given in the first input.
//!
//! agy cut one stream-json user event at 191,985 bytes, twice on 26 Nov
//! 2026: the first resident wake of an agy zooid (933,083 bytes, so the identity and
//! the brief at its end were lost) and a news batch. So a life whose wake is
//! larger than one event is given a small first input ([`first_input`]): who
//! it is, its brief, and the wake's parts to read ([`reading`]). The parts
//! are the same whole wake, cut at line ends into pieces one file read takes
//! whole ([`parts`]), so no reader needs offsets, and joined they are the
//! wake byte for byte. Nothing here is agy's: a claude resident that needs
//! the same is given the same.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// The most a provider is given in one user event, in bytes. agy was seen
/// cutting one at 191,985 bytes (26 Nov 2026, twice): an observation, not a
/// proved limit, so this keeps a third below it.
pub const EVENT_BYTES: usize = 128_000;

/// The largest wake part, in bytes: agy's file read takes at most 46,080
/// bytes in one call, claude's more.
pub const PART_BYTES: usize = 40_000;

/// The most lines in one wake part: agy's file read takes at most 800 in
/// one call.
pub const PART_LINES: usize = 700;

/// The directory a zooid's parts are written to holds only them: each
/// launch that writes parts removes the ones it finds there first.
const PART_PREFIX: &str = "part-";
const PART_SUFFIX: &str = ".md";

/// `wake` cut into parts of at most [`PART_BYTES`] and [`PART_LINES`], each
/// ending after a line's end. A line longer than a part is cut inside, at a
/// character boundary. Joined, the parts are `wake`.
pub fn parts(wake: &str) -> Vec<&str> {
    let mut parts = Vec::new();
    let mut rest = wake;
    while !rest.is_empty() {
        let end = part_end(rest);
        parts.push(&rest[..end]);
        rest = &rest[end..];
    }
    parts
}

fn part_end(rest: &str) -> usize {
    if rest.len() <= PART_BYTES && rest.lines().count() <= PART_LINES {
        return rest.len();
    }
    let mut end = 0;
    for (lines, (at, _)) in rest.match_indices('\n').enumerate() {
        let next = at + 1;
        if next > PART_BYTES || lines >= PART_LINES {
            break;
        }
        end = next;
    }
    if end > 0 {
        return end;
    }
    let mut end = PART_BYTES;
    while !rest.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// One part as a file: where it is written, and its text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part<'a> {
    pub path: PathBuf,
    pub text: &'a str,
}

/// The parts of `wake` as the files of `dir`, in order: `part-01-of-24.md`
/// and on, numbered to the width of the count, so they sort as they read.
pub fn files<'a>(dir: &Path, wake: &'a str) -> Vec<Part<'a>> {
    let parts = parts(wake);
    let count = parts.len();
    let width = count.to_string().len();
    parts
        .into_iter()
        .enumerate()
        .map(|(index, text)| Part {
            path: dir.join(format!(
                "{PART_PREFIX}{:0width$}-of-{count}{PART_SUFFIX}",
                index + 1
            )),
            text,
        })
        .collect()
}

/// Write `parts` (all in one directory), after removing every part file
/// an earlier launch left there, so a shorter wake never leaves a stale
/// tail. Entries that are not part files are left as they are.
pub fn write(dir: &Path, parts: &[Part<'_>]) -> Result<()> {
    if let Some(part) = parts.iter().find(|part| part.path.parent() != Some(dir)) {
        bail!("{} is not in {}", part.path.display(), dir.display());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry.with_context(|| format!("read an entry of {}", dir.display()))?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with(PART_PREFIX)
            && name.ends_with(PART_SUFFIX)
            && entry.file_type().is_ok_and(|kind| kind.is_file())
        {
            std::fs::remove_file(entry.path())
                .with_context(|| format!("remove {}", entry.path().display()))?;
        }
    }
    for part in parts {
        std::fs::write(&part.path, part.text)
            .with_context(|| format!("write {}", part.path.display()))?;
    }
    Ok(())
}

/// How a life is told to read its wake from `parts`. `margin_free` says
/// that a step reading only parts is left out of the life's step margin
/// (agy's life limit does so); a provider whose limit does not must not say it.
pub fn reading(parts: &[Part<'_>], margin_free: bool) -> String {
    let bytes: usize = parts.iter().map(|part| part.text.len()).sum();
    let count = parts.len();
    let mut text = format!(
        "Your wake is not in this message. It is the cover a window wakes into, rendered once \
         for this life, {bytes} bytes written as {count} part(s):\n\n"
    );
    for (index, part) in parts.iter().enumerate() {
        text.push_str(&format!(
            "{}. {} ({} bytes)\n",
            index + 1,
            part.path.display(),
            part.text.len()
        ));
    }
    let last = parts
        .last()
        .and_then(|part| part.path.file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    text.push_str(&format!(
        "\nRead them in order, the first to the last, before anything else in this life: they \
         are the shared self, what you believe and what you are doing. Each part is at most \
         {PART_BYTES} bytes and {PART_LINES} lines, so one file read takes it whole, with no \
         offsets; you may read several in one step. {last} is the last: the wake ends where it \
         ends. Joined, the parts are the whole wake; nothing of it was left out.",
    ));
    if margin_free {
        text.push_str(
            " A step that only reads these parts with your file-reading tool is not counted in \
             the margin your life keeps for its steps; read them another way and it is.",
        );
    }
    text.push('\n');
    text
}

/// The first input of a life that reads its wake by tools: who it is, the
/// brief, and how to read the wake ([`reading`]), in that order.
pub fn first_input(preamble: &str, brief: &str, reading: &str) -> String {
    let mut text = String::new();
    for (title, body) in [
        ("Who you are", preamble),
        ("Brief", brief),
        ("Wake", reading),
    ] {
        text.push_str("# ");
        text.push_str(title);
        text.push_str("\n\n");
        text.push_str(body.trim_end());
        text.push_str("\n\n");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_fit_one_read_end_at_lines_and_join_to_the_wake() {
        let mut wake = String::new();
        for line in 0..2_000 {
            wake.push_str(&format!("{line} é{}\n", "x".repeat(line % 150)));
        }
        // One line longer than a part, with characters of two bytes where
        // the cut falls.
        wake.push_str(&"ü".repeat(PART_BYTES));
        wake.push_str("\nthe end, no newline");
        let parts = parts(&wake);
        assert!(parts.len() > 3, "{}", parts.len());
        assert_eq!(parts.concat(), wake);
        for part in &parts {
            assert!(part.len() <= PART_BYTES, "{}", part.len());
            assert!(part.lines().count() <= PART_LINES);
        }
        // Every part ends a line but the two pieces of the over-long line
        // (80,000 bytes) and the unterminated end.
        let cut = parts.iter().filter(|part| !part.ends_with('\n')).count();
        assert_eq!(
            cut, 3,
            "the over-long line's two pieces and the unterminated end"
        );
        // Lines, not bytes, cut a wake of short lines.
        let short = "a\n".repeat(PART_LINES * 2 + 1);
        let pieces = super::parts(&short);
        assert_eq!(pieces.len(), 3);
        assert_eq!(pieces[0].lines().count(), PART_LINES);
        assert!(super::parts("").is_empty());
        assert_eq!(super::parts("one line"), ["one line"]);
    }

    #[test]
    fn files_sort_as_they_read_and_a_shorter_wake_leaves_no_stale_part() {
        let dir = tempfile::TempDir::new().unwrap();
        let wake = "line\n".repeat(PART_LINES * 11);
        let long = files(dir.path(), &wake);
        assert_eq!(long.len(), 11);
        assert!(long[0].path.ends_with("part-01-of-11.md"));
        assert!(long[10].path.ends_with("part-11-of-11.md"));
        write(dir.path(), &long).unwrap();
        std::fs::write(dir.path().join("notes.txt"), "not a part").unwrap();
        let short = files(dir.path(), "a short wake\n");
        write(dir.path(), &short).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["notes.txt", "part-1-of-1.md"]);
        assert!(write(dir.path(), &files(&dir.path().join("other"), "x")).is_err());
    }

    #[test]
    fn the_reading_names_every_part_in_order_and_the_last() {
        let wake = "w\n".repeat(PART_LINES * 3);
        let parts = files(Path::new("/s/n.wake"), &wake);
        let text = reading(&parts, true);
        let at = |needle: &str| text.find(needle).unwrap();
        assert!(at("/s/n.wake/part-1-of-3.md") < at("/s/n.wake/part-2-of-3.md"));
        assert!(text.contains("part-3-of-3.md is the last"), "{text}");
        assert!(text.contains(&format!("{} bytes", wake.len())));
        assert!(text.contains("not counted in the margin"));
        assert!(!reading(&parts, false).contains("margin"));
        let first = first_input("You are n.", "The brief.", &text);
        assert!(first.find("# Who you are") < first.find("# Brief"));
        assert!(first.find("# Brief") < first.find("# Wake"));
    }
}
