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
use std::io::Read;
use std::path::Path;

/// The largest single host file a tool reads through `path`.
pub const MAX_HOST_FILE_BYTES: u64 = 1 << 30;

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
    let mut file = open_regular(path)?;
    let mut bytes = Vec::new();
    (&mut file)
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("read {}", path.display()))?;
    if bytes.len() as u64 > limit {
        return Err(invalid_arguments(format!(
            "{} is larger than the {limit}-byte limit for a path input",
            path.display()
        )));
    }
    Ok(Bytes::from_source(bytes))
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

/// One byte source chosen from `file_id` or `path`.
pub fn read_reference(
    files: &crate::files::operations::Files,
    file_id: Option<&str>,
    path: Option<&str>,
    limit: u64,
) -> Result<Bytes> {
    match exactly_one([("file_id", file_id), ("path", path)])? {
        ("file_id", id) => read_file_id(files, id),
        (_, path) => read_host_file(path, limit),
    }
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
