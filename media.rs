#!/usr/bin/env -S rust-script
//! ```cargo
//! [dependencies]
//! anyhow = "1.0"
//! clap = { version = "4.5.4", features = ["derive"] }
//! ed25519-dalek = "2.1.1"
//! hifitime = "4"
//! rand_core = "0.6.4"
//! reqwest = { version = "0.12", default-features = false, features = ["blocking", "rustls-tls"] }
//! triblespace = "0.16.0"
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use clap::{CommandFactory, Parser, Subcommand};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use rand_core::OsRng;
use reqwest::blocking::Client;
use triblespace::core::blob::Bytes;
use triblespace::core::metadata;
use triblespace::core::repo::Repository;
use triblespace::core::repo::pile::Pile;
use triblespace::prelude::blobschemas::{FileBytes, LongString};
use triblespace::prelude::valueschemas::{Blake3, GenId, Handle, Hash, NsTAIInterval, ShortString};
use triblespace::prelude::*;

const ATLAS_BRANCH: &str = "atlas";

#[derive(Parser)]
#[command(
    name = "media",
    about = "Capture/fetch images and emit inline blob markers"
)]
struct Cli {
    /// Path to the pile file to use.
    #[arg(long, default_value = "self.pile", global = true)]
    pile: PathBuf,
    /// Branch name to store media entities into (created if missing).
    #[arg(long, default_value = "media", global = true)]
    branch: String,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Capture a local image file into the pile.
    Capture {
        path: PathBuf,
        /// Explicit MIME type override.
        #[arg(long)]
        mime: Option<String>,
        /// Optional filename override for marker metadata.
        #[arg(long)]
        name: Option<String>,
        /// Optional alt text for the marker.
        #[arg(long)]
        alt: Option<String>,
    },
    /// Fetch an image URL into the pile.
    Fetch {
        url: String,
        /// Explicit MIME type override.
        #[arg(long)]
        mime: Option<String>,
        /// Optional filename override for marker metadata.
        #[arg(long)]
        name: Option<String>,
        /// Optional alt text for the marker.
        #[arg(long)]
        alt: Option<String>,
        /// Maximum response size in bytes.
        #[arg(long, default_value_t = 8 * 1024 * 1024)]
        max_bytes: usize,
    },
}

mod media_schema {
    use super::*;

    // Minted with `trible genid`.
    attributes! {
        "56F68B7AC5761170D846730AC87BE25A" as bytes: Handle<Blake3, FileBytes>;
        "77FE78D9EE452EAF1E6F9CE990D67226" as about_item: GenId;
        "E51300D61D3BF44520B21CD9AA7DB851" as created_at: NsTAIInterval;
        "89178059127D90C0734A542054BE63A4" as mime: ShortString;
        "8DEFB75A373AA5550339A6862641FC44" as name: Handle<Blake3, LongString>;
        "F7CFF9D486DFF98CFE5C99DDD7F4F959" as source_url: Handle<Blake3, LongString>;
        "D775F2FBB6260592F428E60E9DE00E8D" as alt: Handle<Blake3, LongString>;
    }

    #[allow(non_upper_case_globals)]
    pub const kind_item: Id = triblespace::macros::id_hex!("A9D189F9D74999D6FEEAE0BDD56897C4");
    #[allow(non_upper_case_globals)]
    pub const kind_record: Id = triblespace::macros::id_hex!("F6A12DAA72A773C811DAED4D45E073E6");
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Err(err) = emit_schema_to_atlas(&cli.pile) {
        eprintln!("atlas emit: {err}");
    }
    let Some(cmd) = cli.command.as_ref() else {
        let mut command = Cli::command();
        command.print_help()?;
        println!();
        return Ok(());
    };

    match cmd {
        Command::Capture {
            path,
            mime,
            name,
            alt,
        } => cmd_capture(&cli, path, mime.as_deref(), name.as_deref(), alt.as_deref()),
        Command::Fetch {
            url,
            mime,
            name,
            alt,
            max_bytes,
        } => cmd_fetch(
            &cli,
            url.as_str(),
            mime.as_deref(),
            name.as_deref(),
            alt.as_deref(),
            *max_bytes,
        ),
    }
}

fn cmd_capture(
    cli: &Cli,
    path: &Path,
    mime_override: Option<&str>,
    name_override: Option<&str>,
    alt_override: Option<&str>,
) -> Result<()> {
    let bytes = fs::read(path).with_context(|| format!("read file {}", path.display()))?;
    let guessed_name = name_override
        .map(str::to_owned)
        .or_else(|| path.file_name().map(|n| n.to_string_lossy().to_string()));
    let mime = resolve_image_mime(mime_override, None, Some(path), bytes.as_slice())?;
    let alt = choose_alt(alt_override, guessed_name.as_deref());

    let marker = store_media(
        &cli.pile,
        &cli.branch,
        bytes.as_slice(),
        mime.as_str(),
        guessed_name.as_deref(),
        None,
        alt.as_str(),
    )?;
    println!("{marker}");
    Ok(())
}

fn cmd_fetch(
    cli: &Cli,
    url: &str,
    mime_override: Option<&str>,
    name_override: Option<&str>,
    alt_override: Option<&str>,
    max_bytes: usize,
) -> Result<()> {
    let client = Client::builder()
        .user_agent("playground-media-faculty/0")
        .build()
        .context("build http client")?;
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("fetch {url}"))?
        .error_for_status()
        .with_context(|| format!("fetch {url}"))?;

    let header_mime = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    let bytes = response.bytes().context("read response body")?;
    if bytes.len() > max_bytes {
        bail!(
            "image too large: {} bytes (limit {})",
            bytes.len(),
            max_bytes
        );
    }
    let guessed_name = name_override
        .map(str::to_owned)
        .or_else(|| infer_name_from_url(url));
    let mime = resolve_image_mime(mime_override, header_mime.as_deref(), None, bytes.as_ref())?;
    let alt = choose_alt(alt_override, guessed_name.as_deref());

    let marker = store_media(
        &cli.pile,
        &cli.branch,
        bytes.as_ref(),
        mime.as_str(),
        guessed_name.as_deref(),
        Some(url),
        alt.as_str(),
    )?;
    println!("{marker}");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn store_media(
    pile_path: &Path,
    branch_name: &str,
    bytes: &[u8],
    mime: &str,
    name: Option<&str>,
    source_url: Option<&str>,
    alt: &str,
) -> Result<String> {
    let (mut repo, branch_id) = open_repo(pile_path, branch_name)?;
    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull branch: {e:?}"))?;

    let file_handle: Value<Handle<Blake3, FileBytes>> =
        ws.put::<FileBytes, _>(Bytes::from_source(bytes.to_vec()));
    let item = entity! { _ @
        metadata::tag: media_schema::kind_item,
        media_schema::bytes: file_handle,
    };
    let item_id = item.root().expect("entity! root id");

    let mut change = TribleSet::new();
    change += item;

    let now = epoch_interval(now_epoch());
    let record_id = ufoid();
    change += entity! { &record_id @
        metadata::tag: media_schema::kind_record,
        media_schema::about_item: item_id,
        media_schema::created_at: now,
        media_schema::mime: mime,
    };
    if let Some(name) = name.filter(|s| !s.trim().is_empty()) {
        let handle = ws.put(name.to_owned());
        change += entity! { &record_id @ media_schema::name: handle };
    }
    if let Some(source_url) = source_url.filter(|s| !s.trim().is_empty()) {
        let handle = ws.put(source_url.to_owned());
        change += entity! { &record_id @ media_schema::source_url: handle };
    }
    if !alt.trim().is_empty() {
        let handle = ws.put(alt.to_owned());
        change += entity! { &record_id @ media_schema::alt: handle };
    }

    ws.commit(change, None, Some("media ingest"));
    repo.push(&mut ws)
        .map_err(|e| anyhow!("push media ingest: {e:?}"))?;
    repo.close().map_err(|e| anyhow!("close pile: {e:?}"))?;

    let marker = format_blob_marker(
        alt,
        digest_hex_for_file_handle(file_handle).as_str(),
        Some(mime),
        name,
    );
    Ok(marker)
}

fn choose_alt(alt_override: Option<&str>, name: Option<&str>) -> String {
    if let Some(alt) = alt_override.filter(|s| !s.trim().is_empty()) {
        return alt.trim().to_owned();
    }
    if let Some(name) = name.filter(|s| !s.trim().is_empty()) {
        return name.trim().to_owned();
    }
    "image".to_string()
}

fn resolve_image_mime(
    mime_override: Option<&str>,
    header_mime: Option<&str>,
    path_hint: Option<&Path>,
    bytes: &[u8],
) -> Result<String> {
    let mime = mime_override
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            header_mime
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        })
        .or_else(|| path_hint.and_then(infer_mime_from_path))
        .or_else(|| sniff_image_mime(bytes).map(str::to_string))
        .ok_or_else(|| anyhow!("unable to infer image mime; pass --mime explicitly"))?;
    if !mime.starts_with("image/") {
        bail!("mime must start with image/: {mime}");
    }
    Ok(mime)
}

fn infer_name_from_url(url: &str) -> Option<String> {
    let before_query = url.split('?').next().unwrap_or(url);
    let last = before_query.rsplit('/').next()?;
    let trimmed = last.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_owned())
    }
}

fn infer_mime_from_path(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_string_lossy().to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        _ => return None,
    };
    Some(mime.to_string())
}

fn sniff_image_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 8 && bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("image/png");
    }
    if bytes.len() >= 3 && bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("image/jpeg");
    }
    if bytes.len() >= 6 && (bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a")) {
        return Some("image/gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("image/webp");
    }
    None
}

fn digest_hex_for_file_handle(handle: Value<Handle<Blake3, FileBytes>>) -> String {
    let digest: Value<Hash<Blake3>> = handle.into();
    Hash::<Blake3>::to_hex(&digest)
}

fn format_blob_marker(
    alt: &str,
    digest_hex: &str,
    mime: Option<&str>,
    name: Option<&str>,
) -> String {
    let mut marker = String::new();
    let safe_alt = alt.replace(']', " ");
    marker.push_str("![");
    marker.push_str(safe_alt.trim());
    marker.push_str("](blob:blake3:");
    marker.push_str(&digest_hex.to_ascii_uppercase());
    let mut query = Vec::new();
    if let Some(mime) = mime.filter(|s| !s.trim().is_empty()) {
        query.push(("mime", percent_encode(mime.trim())));
    }
    if let Some(name) = name.filter(|s| !s.trim().is_empty()) {
        query.push(("name", percent_encode(name.trim())));
    }
    if !query.is_empty() {
        marker.push('?');
        for (idx, (k, v)) in query.into_iter().enumerate() {
            if idx > 0 {
                marker.push('&');
            }
            marker.push_str(k);
            marker.push('=');
            marker.push_str(v.as_str());
        }
    }
    marker.push(')');
    marker
}

fn percent_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        let keep = b.is_ascii_alphanumeric() || std::matches!(b, b'-' | b'_' | b'.' | b'~');
        if keep {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push_str(&format!("{b:02X}"));
        }
    }
    out
}

fn epoch_interval(epoch: Epoch) -> Value<NsTAIInterval> {
    (epoch, epoch).to_value()
}

fn now_epoch() -> Epoch {
    Epoch::now().unwrap_or_else(|_| Epoch::from_gregorian_utc(1970, 1, 1, 0, 0, 0, 0))
}

fn emit_schema_to_atlas(pile_path: &Path) -> Result<()> {
    let (mut repo, branch_id) = open_repo(pile_path, ATLAS_BRANCH)?;
    let metadata = build_media_metadata(repo.storage_mut())
        .map_err(|e| anyhow!("build media metadata: {e:?}"))?;

    let mut ws = repo
        .pull(branch_id)
        .map_err(|e| anyhow!("pull atlas: {e:?}"))?;
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow!("checkout atlas: {e:?}"))?;
    let delta = metadata.difference(&space);
    if !delta.is_empty() {
        ws.commit(delta, None, Some("atlas schema metadata"));
        repo.push(&mut ws)
            .map_err(|e| anyhow!("push atlas metadata: {e:?}"))?;
    }
    repo.close().map_err(|e| anyhow!("close pile: {e:?}"))?;
    Ok(())
}

fn build_media_metadata<B>(blobs: &mut B) -> std::result::Result<TribleSet, B::PutError>
where
    B: BlobStore<Blake3>,
{
    let mut out = TribleSet::new();
    out += <GenId as metadata::ConstDescribe>::describe(blobs)?;
    out += <ShortString as metadata::ConstDescribe>::describe(blobs)?;
    out += <NsTAIInterval as metadata::ConstDescribe>::describe(blobs)?;
    out += <Handle<Blake3, LongString> as metadata::ConstDescribe>::describe(blobs)?;
    out += <Handle<Blake3, FileBytes> as metadata::ConstDescribe>::describe(blobs)?;
    out += <LongString as metadata::ConstDescribe>::describe(blobs)?;
    out += <FileBytes as metadata::ConstDescribe>::describe(blobs)?;

    out += describe_attribute(blobs, &media_schema::bytes, "media_bytes")?;
    out += describe_attribute(blobs, &media_schema::about_item, "media_about_item")?;
    out += describe_attribute(blobs, &media_schema::created_at, "media_created_at")?;
    out += describe_attribute(blobs, &media_schema::mime, "media_mime")?;
    out += describe_attribute(blobs, &media_schema::name, "media_name")?;
    out += describe_attribute(blobs, &media_schema::source_url, "media_source_url")?;
    out += describe_attribute(blobs, &media_schema::alt, "media_alt")?;

    out += describe_kind(
        blobs,
        &media_schema::kind_item,
        "media_kind_item",
        "Canonical media item keyed by blob handle.",
    )?;
    out += describe_kind(
        blobs,
        &media_schema::kind_record,
        "media_kind_record",
        "Media ingest event with contextual metadata.",
    )?;
    Ok(out)
}

fn describe_attribute<B, S>(
    blobs: &mut B,
    attribute: &Attribute<S>,
    name: &str,
) -> std::result::Result<TribleSet, B::PutError>
where
    B: BlobStore<Blake3>,
    S: ValueSchema,
{
    let mut tribles = TribleSet::new();
    tribles += metadata::Describe::describe(attribute, blobs)?;
    let handle = blobs.put(name.to_owned())?;
    let attribute_id = attribute.id();
    tribles += entity! { ExclusiveId::force_ref(&attribute_id) @ metadata::name: handle };
    Ok(tribles)
}

fn describe_kind<B>(
    blobs: &mut B,
    id: &Id,
    name: &str,
    description: &str,
) -> std::result::Result<Fragment, B::PutError>
where
    B: BlobStore<Blake3>,
{
    Ok(entity! { ExclusiveId::force_ref(id) @
        metadata::name: blobs.put(name.to_string())?,
        metadata::description: blobs.put(description.to_string())?,
    })
}

fn open_repo(path: &Path, branch_name: &str) -> Result<(Repository<Pile<Blake3>>, Id)> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow!("create pile dir {}: {e}", parent.display()))?;
    }

    let mut pile =
        Pile::<Blake3>::open(path).map_err(|e| anyhow!("open pile {}: {e:?}", path.display()))?;
    pile.restore()
        .map_err(|e| anyhow!("restore pile {}: {e:?}", path.display()))?;

    let existing = find_branch_by_name(&mut pile, branch_name)?;
    let signing_key = SigningKey::generate(&mut OsRng);
    let mut repo = Repository::new(pile, signing_key);
    let branch_id = match existing {
        Some(id) => id,
        None => repo
            .create_branch(branch_name, None)
            .map_err(|e| anyhow!("create branch: {e:?}"))?
            .release(),
    };
    Ok((repo, branch_id))
}

fn find_branch_by_name(pile: &mut Pile<Blake3>, branch_name: &str) -> Result<Option<Id>> {
    let expected_name_handle = branch_name.to_owned().to_blob().get_handle::<Blake3>();
    let reader = pile.reader().map_err(|e| anyhow!("pile reader: {e:?}"))?;
    let iter = pile
        .branches()
        .map_err(|e| anyhow!("list branches: {e:?}"))?;

    let mut fallback: Option<Id> = None;
    for bid in iter {
        let bid = bid?;
        let Some(meta_handle) = pile.head(bid)? else {
            continue;
        };
        let meta: TribleSet = reader
            .get::<TribleSet, blobschemas::SimpleArchive>(meta_handle)
            .map_err(|e| anyhow!("load branch metadata: {e:?}"))?;
        let mut names = find!(
            (handle: Value<Handle<Blake3, LongString>>),
            pattern!(&meta, [{ metadata::name: ?handle }])
        )
        .into_iter();
        let Some(name) = names.next().map(|(handle,)| handle) else {
            continue;
        };
        if names.next().is_some() {
            continue;
        }
        if name.raw != expected_name_handle.raw {
            continue;
        }
        let has_commit_head = find!(
            (handle: Value<Handle<Blake3, blobschemas::SimpleArchive>>),
            pattern!(&meta, [{ triblespace::core::repo::head: ?handle }])
        )
        .into_iter()
        .next()
        .is_some();
        if has_commit_head {
            return Ok(Some(bid));
        }
        if fallback.is_none() {
            fallback = Some(bid);
        }
    }
    Ok(fallback)
}
