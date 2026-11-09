//! Byte inputs for MCP tools, by reference instead of by value.
//!
//! A model never has to emit a file's bytes into a tool call. A tool takes
//! either `file_id`, an exact Files entity or single-file import id, or
//! `path`, an absolute path on the host the MCP server runs on, read with the
//! server process's permissions. Those permissions are a capability, not
//! authority: a caller passes only paths the user or task authorizes it to
//! read. Selecting a workspace routes where results are stored; it grants no
//! filesystem authority either.

use super::invalid_arguments;
pub use crate::files::operations::NotAFile;
use anybytes::Bytes;
use anyhow::{Context, Result};
use std::fmt::Display;
use std::io::Read;
use std::path::Path;

/// The most bytes one tool call reads through all of its `file_id` and
/// `path` inputs together. Every input of a call draws on one
/// [`InputBudget`], so a batch of documents or attachments, or one file
/// named under several names, cannot multiply it.
pub const MAX_CALL_INPUT_BYTES: u64 = 1 << 30;

/// One tool call's input budget, shared by every `file_id` and `path` the
/// call reads. A read that would pass it fails with an invalid-argument
/// error naming the input and the budget.
#[derive(Debug)]
pub struct InputBudget {
    limit: u64,
    used: u64,
}

impl InputBudget {
    /// The budget of one tool call, [`MAX_CALL_INPUT_BYTES`].
    pub fn per_call() -> Self {
        Self::new(MAX_CALL_INPUT_BYTES)
    }

    pub fn new(limit: u64) -> Self {
        Self { limit, used: 0 }
    }

    pub fn remaining(&self) -> u64 {
        self.limit - self.used
    }

    fn exceeded(&self, input: impl Display) -> anyhow::Error {
        invalid_arguments(format!(
            "{input} would exceed the {}-byte input budget of one call \
             ({} bytes already read by its other inputs)",
            self.limit, self.used
        ))
    }

    fn charge(&mut self, input: impl Display, len: usize) -> Result<()> {
        let len = len as u64;
        if len > self.remaining() {
            return Err(self.exceeded(input));
        }
        self.used += len;
        Ok(())
    }

    /// One regular host file, as [`read_host_file`] reads it, within what is
    /// left of the budget. Its size is checked before it is read.
    pub fn host_file(&mut self, path: &str) -> Result<Bytes> {
        let checked = absolute(path)?;
        let bytes = read_within(checked, self.remaining())?
            .ok_or_else(|| self.exceeded(format_args!("path {path}")))?;
        self.charge(format_args!("path {path}"), bytes.len())?;
        Ok(Bytes::from_source(bytes))
    }

    /// One stored file, as [`read_file_id`] selects it, within what is left
    /// of the budget. The length is checked on the blob the store returns,
    /// before the tool copies or decodes it.
    pub fn file_id(&mut self, files: &crate::files::operations::Files, id: &str) -> Result<Bytes> {
        let bytes = read_file_id(files, id)?;
        self.charge(format_args!("file_id {id}"), bytes.len())?;
        Ok(bytes)
    }

    /// One byte source chosen from `file_id` or `path`.
    pub fn reference(
        &mut self,
        files: &crate::files::operations::Files,
        file_id: Option<&str>,
        path: Option<&str>,
    ) -> Result<Bytes> {
        match exactly_one([("file_id", file_id), ("path", path)])? {
            ("file_id", id) => self.file_id(files, id),
            (_, path) => self.host_file(path),
        }
    }
}

/// Exactly one of the named sources, or an invalid-argument error naming them.
pub fn exactly_one<'a, T>(
    sources: impl IntoIterator<Item = (&'a str, Option<T>)>,
) -> Result<(&'a str, T)> {
    let mut names = Vec::new();
    let mut chosen = None;
    let mut count = 0;
    for (name, value) in sources {
        names.push(name);
        if let Some(value) = value {
            count += 1;
            chosen = Some((name, value));
        }
    }
    match (count, chosen) {
        (1, Some(chosen)) => Ok(chosen),
        _ => Err(invalid_arguments(format!(
            "supply exactly one of {}",
            names.join(" or ")
        ))),
    }
}

/// Read one regular host file of at most `limit` bytes. The named path may
/// itself be a symlink, because the caller named it; what it resolves to
/// must be a regular file. FIFOs, devices and sockets are refused without
/// blocking, and a file that grows past the limit while being read fails.
pub fn read_host_file(path: &str, limit: u64) -> Result<Bytes> {
    let path = absolute(path)?;
    let bytes = read_within(path, limit)?.ok_or_else(|| {
        invalid_arguments(format!(
            "{} is larger than the {limit}-byte limit for a path input",
            path.display()
        ))
    })?;
    Ok(Bytes::from_source(bytes))
}

/// A regular file's bytes, or `None` when it holds more than `limit`: by
/// its size before reading, or by what the read finds if it grew.
fn read_within(path: &Path, limit: u64) -> Result<Option<Vec<u8>>> {
    let mut file = open_regular(path)?;
    let size = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?
        .len();
    if size > limit {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
}

/// `path` as a host path, refused unless absolute: a relative path would
/// depend on the server's working directory.
pub fn absolute(path: &str) -> Result<&Path> {
    let path = Path::new(path);
    if !path.is_absolute() {
        return Err(invalid_arguments(format!(
            "path {} must be absolute on the MCP server's host",
            path.display()
        )));
    }
    Ok(path)
}

/// Open `path` without blocking on a FIFO and confirm it is a regular file.
pub fn open_regular(path: &Path) -> Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .map_err(|error| invalid_arguments(format!("cannot open {}: {error}", path.display())))?;
    let meta = file
        .metadata()
        .with_context(|| format!("stat {}", path.display()))?;
    if !meta.is_file() {
        return Err(invalid_arguments(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    Ok(file)
}

/// The bytes of one stored file, selected exactly as `files_get` selects
/// them: an exact 32-hex file entity id, or an import id, followed to its
/// root as `files_get` follows it. Content hashes are refused, so they cannot reach unnamed blobs
/// outside the selected Files view. A directory or folder import fails with
/// [`NotAFile`] (`error.downcast_ref::<NotAFile>()`), never flattened.
///
/// Every MCP tool that consumes one stored file's bytes calls this, so the
/// semantics are one function's.
pub fn read_file_id(files: &crate::files::operations::Files, id: &str) -> Result<Bytes> {
    if id.len() != 32 || triblespace::prelude::Id::from_hex(id).is_none() {
        return Err(invalid_arguments(
            "file_id must be an exact 32-character Files entity or single-file import id",
        ));
    }
    files.get(id).map(|export| export.bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_reads_are_absolute_regular_and_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("a.bin");
        std::fs::write(&file, b"abcdef").unwrap();
        let file = file.to_str().unwrap();
        assert_eq!(read_host_file(file, 6).unwrap().as_ref(), b"abcdef");
        let error = read_host_file(file, 5).unwrap_err().to_string();
        assert!(error.contains("limit"), "{error}");
        assert!(read_host_file("a.bin", 6)
            .unwrap_err()
            .to_string()
            .contains("absolute"));
        let missing = directory.path().join("missing");
        let error = read_host_file(missing.to_str().unwrap(), 6)
            .unwrap_err()
            .to_string();
        assert!(error.contains("cannot open"), "{error}");
        let error = read_host_file(directory.path().to_str().unwrap(), 6)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn a_fifo_is_refused_without_blocking() {
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("pipe");
        let name = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let error = read_host_file(fifo.to_str().unwrap(), 6)
            .unwrap_err()
            .to_string();
        assert!(error.contains("not a regular file"), "{error}");
    }

    #[test]
    fn one_budget_bounds_every_path_input_of_a_call() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("a.bin");
        std::fs::write(&file, b"abcdef").unwrap();
        let file = file.to_str().unwrap();
        let mut budget = InputBudget::new(10);
        assert_eq!(budget.host_file(file).unwrap().as_ref(), b"abcdef");
        assert_eq!(budget.remaining(), 4);
        // The same file again would pass what is left: refused, not reread.
        let error = budget.host_file(file).unwrap_err();
        assert!(error
            .downcast_ref::<crate::mcp::InvalidArguments>()
            .is_some());
        let message = error.to_string();
        assert!(message.contains("10-byte input budget"), "{message}");
        assert!(message.contains("6 bytes already read"), "{message}");
        assert_eq!(budget.remaining(), 4);
        assert!(budget
            .host_file("a.bin")
            .unwrap_err()
            .to_string()
            .contains("absolute"));
    }

    #[test]
    fn file_id_reads_draw_on_the_same_budget() {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("input.pile");
        let key = directory.path().join("input.key");
        std::fs::File::create(&pile).unwrap();
        crate::storage::initialize_signer(&pile, Some(&key)).unwrap();
        let file = directory.path().join("a.bin");
        std::fs::write(&file, b"abcdef").unwrap();
        let files = crate::files::operations::Files::with_storage(crate::storage::Storage::new(
            pile,
            Some(key),
        ));
        let mut report = String::new();
        files
            .add_path(
                &file,
                None,
                &[],
                false,
                &mut crate::out::Out::new(&mut |part| {
                    if let crate::out::Part::Text { text } = part {
                        report.push_str(&text);
                    }
                    Ok(())
                }),
            )
            .unwrap();
        let id = report
            .lines()
            .find_map(|line| line.strip_prefix("Import: "))
            .unwrap()
            .trim()
            .to_owned();
        let mut budget = InputBudget::new(10);
        assert_eq!(budget.file_id(&files, &id).unwrap().as_ref(), b"abcdef");
        let error = budget.file_id(&files, &id).unwrap_err().to_string();
        assert!(
            error.contains(&format!("file_id {id} would exceed")),
            "{error}"
        );
        // A path and a file_id share it: 4 bytes left, a 6-byte path fails.
        let error = budget
            .reference(&files, None, Some(file.to_str().unwrap()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("10-byte input budget"), "{error}");
        assert_eq!(InputBudget::per_call().remaining(), MAX_CALL_INPUT_BYTES);
    }

    #[test]
    fn exactly_one_names_every_source() {
        let error = exactly_one([("file_id", None::<&str>), ("path", None)])
            .unwrap_err()
            .to_string();
        assert!(error.contains("exactly one of file_id or path"), "{error}");
        assert!(exactly_one([("file_id", Some("a")), ("path", Some("b"))]).is_err());
        assert_eq!(
            exactly_one([("file_id", None), ("path", Some("b"))]).unwrap(),
            ("path", "b")
        );
    }
}
