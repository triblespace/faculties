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
/// launch that writes parts empties it first ([`write`]), and only a file
/// named as a part ([`is_part_name`]) is one.
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

/// Whether `name` is a part's file name, as [`files`] names them:
/// `part-<n>-of-<count>.md`, both decimal, `n` from 1 to `count`.
pub fn is_part_name(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix(PART_PREFIX)
        .and_then(|rest| rest.strip_suffix(PART_SUFFIX))
    else {
        return false;
    };
    let Some((index, count)) = rest.split_once("-of-") else {
        return false;
    };
    let number = |text: &str| {
        (!text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| text.parse::<u64>().ok())
            .flatten()
    };
    match (number(index), number(count)) {
        (Some(index), Some(count)) => (1..=count).contains(&index),
        _ => false,
    }
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

/// Write `parts` (all in one directory) as the directory's only entries:
/// everything an earlier launch, or anyone, left there is removed first, so
/// a shorter wake leaves no stale tail and no other file there passes for a
/// part. Nothing is written through a link: a link found inside is removed
/// as an entry, each part is a new regular file, and a directory that is
/// itself a link is refused.
pub fn write(dir: &Path, parts: &[Part<'_>]) -> Result<()> {
    if let Some(part) = parts.iter().find(|part| part.path.parent() != Some(dir)) {
        bail!("{} is not in {}", part.path.display(), dir.display());
    }
    match std::fs::symlink_metadata(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?
        }
        Err(error) => return Err(error).with_context(|| format!("inspect {}", dir.display())),
        Ok(_) => {}
    }
    let meta =
        std::fs::symlink_metadata(dir).with_context(|| format!("inspect {}", dir.display()))?;
    if !meta.file_type().is_dir() {
        bail!(
            "{} is a link or a file, not the wake's own directory: nothing was written",
            dir.display()
        );
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry.with_context(|| format!("read an entry of {}", dir.display()))?;
        let path = entry.path();
        // The entry's own type: a link is a link here, never its target.
        let kind = entry
            .file_type()
            .with_context(|| format!("inspect {}", path.display()))?;
        if kind.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        }
        .with_context(|| format!("remove {}", path.display()))?;
    }
    for part in parts {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&part.path)
            .with_context(|| format!("create {}", part.path.display()))?;
        std::io::Write::write_all(&mut file, part.text.as_bytes())
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
        // The directory holds exactly the parts: anything else found there
        // is removed, so a read of it is never taken for a read of the wake.
        assert_eq!(names, ["part-1-of-1.md"]);
        assert!(write(dir.path(), &files(&dir.path().join("other"), "x")).is_err());
    }

    /// A link found in the directory is removed, never written through,
    /// and a directory that is itself a link is refused.
    #[cfg(unix)]
    #[test]
    fn a_link_in_the_wake_directory_is_never_written_through() {
        let root = tempfile::TempDir::new().unwrap();
        let outside = root.path().join("outside.txt");
        std::fs::write(&outside, "keep me").unwrap();
        let dir = root.path().join("n.wake");
        std::fs::create_dir(&dir).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("part-1-of-1.md")).unwrap();
        std::os::unix::fs::symlink(root.path(), dir.join("elsewhere")).unwrap();
        let parts = files(&dir, "the wake\n");
        write(&dir, &parts).unwrap();
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "keep me");
        let part = dir.join("part-1-of-1.md");
        assert!(std::fs::symlink_metadata(&part)
            .unwrap()
            .file_type()
            .is_file());
        assert_eq!(std::fs::read_to_string(&part).unwrap(), "the wake\n");
        assert!(std::fs::symlink_metadata(dir.join("elsewhere")).is_err());
        assert!(root.path().exists() && outside.exists());

        let linked = root.path().join("linked.wake");
        std::os::unix::fs::symlink(&dir, &linked).unwrap();
        let error = write(&linked, &files(&linked, "x\n")).unwrap_err();
        assert!(format!("{error:#}").contains("link"), "{error:#}");
        assert_eq!(std::fs::read_to_string(&part).unwrap(), "the wake\n");
    }

    #[test]
    fn a_part_name_is_one_of_the_parts_pattern_only() {
        assert!(is_part_name("part-01-of-24.md"));
        assert!(is_part_name("part-1-of-1.md"));
        for name in [
            "notes.txt",
            "part-01-of-24.txt",
            "part-25-of-24.md",
            "part-00-of-24.md",
            "part--of-24.md",
            "part-01-of-.md",
            "part-0x-of-24.md",
            "xpart-01-of-24.md",
        ] {
            assert!(!is_part_name(name), "{name}");
        }
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
