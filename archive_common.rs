#![allow(dead_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use ed25519_dalek::SigningKey;
use hifitime::Epoch;
use rand_core::OsRng;
use std::fs;
use triblespace::core::id::ExclusiveId;
use triblespace::core::metadata;
use triblespace::core::repo::branch as branch_proto;
use triblespace::core::repo::pile::Pile;
use triblespace::core::repo::{PushResult, Repository, Workspace};
use triblespace::prelude::blobschemas::{LongString, SimpleArchive};
use triblespace::prelude::valueschemas::{Blake3, Handle, NsTAIInterval};
use triblespace::prelude::*;

pub mod archive_schema {
    use triblespace::core::metadata;
    use triblespace::macros::id_hex;
    pub use triblespace::prelude::blobschemas::FileBytes;
    use triblespace::prelude::blobschemas::LongString;
    use triblespace::prelude::valueschemas::{
        Blake3, GenId, Handle, NsTAIInterval, ShortString, U256BE,
    };
    use triblespace::prelude::*;

    /// A unified archive projection for externally sourced conversations.
    ///
    /// This schema is used by archive importers (ChatGPT, Codex, Copilot, Gemini, ...)
    /// to store a common message/author/attachment graph, while keeping the raw
    /// source artifacts separately (e.g. JSON trees, HTML, etc).
    pub mod archive {
        use super::*;

        attributes! {
            "5F10520477A04E5FB322C85CC78C6762" as pub kind: GenId;

            "0D9195A7B1B20DE312A08ECE39168079" as pub reply_to: GenId;
            "838CC157FFDD37C6AC7CC5A472E43ADB" as pub author: GenId;
            "E63EE961ABDB1D1BEC0789FDAFFB9501" as pub author_name: Handle<Blake3, LongString>;
            "2D15150501ACCD9DFD96CB4BF19D1883" as pub author_role: Handle<Blake3, LongString>;
            "4FE6A8A43658BC2F61FEDF5CFB29EEFC" as pub author_model: Handle<Blake3, LongString>;
            "1F127324384335D12ECFE0CB84840925" as pub author_provider: Handle<Blake3, LongString>;
            "ACF09FF3D62B73983A222313FF0C52D2" as pub content: Handle<Blake3, LongString>;
            "0DA5DD275AA34F86B0297CC35F1B7395" as pub created_at: NsTAIInterval;

            "D8A469EAC2518D1A85692E0BEBF20D6C" as pub content_type: ShortString;
            "8334E282F24A4C7779C8899191B29E00" as pub attachment: GenId;

            "C9132D7400892F65B637BCBE92E230FB" as pub attachment_source_id: Handle<Blake3, LongString>;
            "A8F6CF04A9B2391A26F04BC84B77217D" as pub attachment_source_pointer: Handle<Blake3, LongString>;
            "9ADD88D3FFD9E4F91E0DC08126D9180A" as pub attachment_name: Handle<Blake3, LongString>;
            "EEFDB32D37B7B2834D99ACCF159B6507" as pub attachment_mime: ShortString;
            "D233E7BE0E973B09BD51E768E528ACA5" as pub attachment_size_bytes: U256BE;
            "5937E1072AF2F8E493321811B483C57B" as pub attachment_width_px: U256BE;
            "B252F4F77929E54FF8472027B7603EE9" as pub attachment_height_px: U256BE;
            "B0D18159D6035C576AE6B5D871AB4D63" as pub attachment_data: Handle<Blake3, FileBytes>;
        }

        /// Tag for message payloads.
        #[allow(non_upper_case_globals)]
        pub const kind_message: Id = id_hex!("1A0841C92BBDA0A26EA9A8252D6ECD9B");
        /// Tag for author entities.
        #[allow(non_upper_case_globals)]
        pub const kind_author: Id = id_hex!("4E4512EFB0BF0CD42265BD107AE7F082");
        /// Tag for attachment entities.
        #[allow(non_upper_case_globals)]
        pub const kind_attachment: Id = id_hex!("B465C85DD800633F58FE211B920AF2D9");

        #[allow(dead_code)]
        pub fn describe<B>(blobs: &mut B) -> std::result::Result<TribleSet, B::PutError>
        where
            B: BlobStore<Blake3>,
        {
            let mut tribles = TribleSet::new();
            let kind_message_entity = super::super::aquire_or_force(kind_message);
            let kind_author_entity = super::super::aquire_or_force(kind_author);
            let kind_attachment_entity = super::super::aquire_or_force(kind_attachment);

            tribles += entity! { &kind_message_entity @
                metadata::name: blobs.put("kind_message".to_string())?,
                metadata::description: blobs.put("Message payload kind.".to_string())?,
            };

            tribles += entity! { &kind_author_entity @
                metadata::name: blobs.put("kind_author".to_string())?,
                metadata::description: blobs.put("Author entity kind.".to_string())?,
            };

            tribles += entity! { &kind_attachment_entity @
                metadata::name: blobs.put("kind_attachment".to_string())?,
                metadata::description: blobs.put("Attachment entity kind.".to_string())?,
            };

            Ok(tribles)
        }
    }

    pub fn build_archive_metadata<B>(blobs: &mut B) -> std::result::Result<TribleSet, B::PutError>
    where
        B: BlobStore<Blake3>,
    {
        let mut metadata = archive::describe(blobs)?;

        metadata += <GenId as metadata::ConstDescribe>::describe(blobs)?;
        metadata += <ShortString as metadata::ConstDescribe>::describe(blobs)?;
        metadata += <U256BE as metadata::ConstDescribe>::describe(blobs)?;
        metadata += <NsTAIInterval as metadata::ConstDescribe>::describe(blobs)?;
        metadata += <Handle<Blake3, LongString> as metadata::ConstDescribe>::describe(blobs)?;
        metadata += <Handle<Blake3, FileBytes> as metadata::ConstDescribe>::describe(blobs)?;
        metadata += <FileBytes as metadata::ConstDescribe>::describe(blobs)?;

        metadata += metadata::Describe::describe(&archive::kind, blobs)?;
        metadata += metadata::Describe::describe(&archive::reply_to, blobs)?;
        metadata += metadata::Describe::describe(&archive::author, blobs)?;
        metadata += metadata::Describe::describe(&archive::author_name, blobs)?;
        metadata += metadata::Describe::describe(&archive::author_role, blobs)?;
        metadata += metadata::Describe::describe(&archive::author_model, blobs)?;
        metadata += metadata::Describe::describe(&archive::author_provider, blobs)?;
        metadata += metadata::Describe::describe(&archive::content, blobs)?;
        metadata += metadata::Describe::describe(&archive::created_at, blobs)?;

        metadata += metadata::Describe::describe(&archive::content_type, blobs)?;
        metadata += metadata::Describe::describe(&archive::attachment, blobs)?;
        metadata += metadata::Describe::describe(&archive::attachment_source_id, blobs)?;
        metadata += metadata::Describe::describe(&archive::attachment_source_pointer, blobs)?;
        metadata += metadata::Describe::describe(&archive::attachment_name, blobs)?;
        metadata += metadata::Describe::describe(&archive::attachment_mime, blobs)?;
        metadata += metadata::Describe::describe(&archive::attachment_size_bytes, blobs)?;
        metadata += metadata::Describe::describe(&archive::attachment_width_px, blobs)?;
        metadata += metadata::Describe::describe(&archive::attachment_height_px, blobs)?;
        metadata += metadata::Describe::describe(&archive::attachment_data, blobs)?;

        Ok(metadata)
    }
}

pub mod import_schema {
    use triblespace::core::metadata;
    use triblespace::macros::id_hex;
    use triblespace::prelude::blobschemas::LongString;
    use triblespace::prelude::valueschemas::{Blake3, GenId, Handle, NsTAIInterval, ShortString};
    use triblespace::prelude::*;

    attributes! {
        "41F6FA1633D8CB6AC7B2741BA0E140F4" as pub kind: GenId;
        "891508CAD6E1430B221ADA937EFBD982" as pub conversation: GenId;
        "E997DCAAF43BAA04790FCB0FA0FBFE3A" as pub source_format: ShortString;
        "973FB59D3452D3A8276172F8E3272324" as pub source_raw_root: GenId;
        "87B587A3906056038FD767F4225274F9" as pub source_conversation_id: Handle<Blake3, LongString>;
        "1B2A09FF44D2A5736FA320AB255026C1" as pub source_message_id: Handle<Blake3, LongString>;
        "AA3CF220F15CCF724276F1251AFE053B" as pub source_author: Handle<Blake3, LongString>;
        "B4C084B61FB46A932BFCA75B8BC621FA" as pub source_role: Handle<Blake3, LongString>;
        "220DA5084D6261B5420922EADC064A5A" as pub source_parent_id: Handle<Blake3, LongString>;
        "F672605621E56674127FD210CFFDFF2A" as pub source_created_at: NsTAIInterval;
    }

    /// Root id for describing the import metadata protocol.
    #[allow(non_upper_case_globals)]
    #[allow(dead_code)]
    pub const import_metadata: Id = id_hex!("5D57DD8335FECADB173616D780965F0C");

    /// Tag for import conversation entities.
    #[allow(non_upper_case_globals)]
    pub const kind_conversation: Id = id_hex!("573E4291B63CBA1B5AE090B0C25A2D34");

    /// Tag for import protocol metadata.
    #[allow(non_upper_case_globals)]
    pub const tag_protocol: Id = id_hex!("21395C6FE7F3EB9CEAF5CF221C0B22B8");
    /// Tag for kind constants in the import protocol.
    #[allow(non_upper_case_globals)]
    pub const tag_kind: Id = id_hex!("F7DC8B6E0C34FC58677E84A40580F8C2");
    /// Tag for attribute constants in the import protocol.
    #[allow(non_upper_case_globals)]
    pub const tag_attribute: Id = id_hex!("0C9208CE8629DAA77721307868356F88");
    /// Tag for tag constants in the import protocol.
    #[allow(non_upper_case_globals)]
    pub const tag_tag: Id = id_hex!("821901A883028FA3C9F5DEB03A7CA27E");

    #[allow(dead_code)]
    pub fn describe<B>(blobs: &mut B) -> std::result::Result<TribleSet, B::PutError>
    where
        B: BlobStore<Blake3>,
    {
        let mut tribles = TribleSet::new();
        let import_metadata_entity = super::aquire_or_force(import_metadata);
        let tag_protocol_entity = super::aquire_or_force(tag_protocol);
        let tag_kind_entity = super::aquire_or_force(tag_kind);
        let tag_attribute_entity = super::aquire_or_force(tag_attribute);
        let tag_tag_entity = super::aquire_or_force(tag_tag);
        let kind_conversation_entity = super::aquire_or_force(kind_conversation);

        tribles += entity! { &import_metadata_entity @
            metadata::name: blobs.put("import_metadata".to_string())?,
            metadata::description: blobs.put(
                "Root id for describing import metadata.".to_string(),
            )?,
            metadata::tag: tag_protocol,
        };

        tribles += entity! { &tag_protocol_entity @
            metadata::name: blobs.put("tag_protocol".to_string())?,
            metadata::description: blobs.put(
                "Tag for import protocol metadata.".to_string(),
            )?,
            metadata::tag: tag_tag,
        };

        tribles += entity! { &tag_kind_entity @
            metadata::name: blobs.put("tag_kind".to_string())?,
            metadata::description: blobs.put(
                "Tag for import protocol kind constants.".to_string(),
            )?,
            metadata::tag: tag_tag,
            metadata::tag: kind_conversation,
        };

        tribles += entity! { &tag_attribute_entity @
            metadata::name: blobs.put("tag_attribute".to_string())?,
            metadata::description: blobs.put(
                "Tag for import protocol attributes.".to_string(),
            )?,
            metadata::tag: tag_tag,
            metadata::tag: kind.id(),
            metadata::tag: conversation.id(),
            metadata::tag: source_format.id(),
            metadata::tag: source_raw_root.id(),
            metadata::tag: source_conversation_id.id(),
            metadata::tag: source_message_id.id(),
            metadata::tag: source_author.id(),
            metadata::tag: source_role.id(),
            metadata::tag: source_parent_id.id(),
            metadata::tag: source_created_at.id(),
        };

        tribles += entity! { &tag_tag_entity @
            metadata::name: blobs.put("tag_tag".to_string())?,
            metadata::description: blobs.put(
                "Tag for import protocol tag constants.".to_string(),
            )?,
            metadata::tag: tag_tag,
            metadata::tag: tag_protocol,
            metadata::tag: tag_kind,
            metadata::tag: tag_attribute,
        };

        tribles += entity! { &kind_conversation_entity @
            metadata::name: blobs.put("kind_conversation".to_string())?,
            metadata::description: blobs.put("Import conversation entity kind.".to_string())?,
        };

        Ok(tribles)
    }

    pub fn build_import_metadata<B>(blobs: &mut B) -> std::result::Result<TribleSet, B::PutError>
    where
        B: BlobStore<Blake3>,
    {
        let mut metadata = describe(blobs)?;

        metadata += <GenId as metadata::ConstDescribe>::describe(blobs)?;
        metadata += <ShortString as metadata::ConstDescribe>::describe(blobs)?;
        metadata += <NsTAIInterval as metadata::ConstDescribe>::describe(blobs)?;
        metadata += <Handle<Blake3, LongString> as metadata::ConstDescribe>::describe(blobs)?;

        metadata += metadata::Describe::describe(&kind, blobs)?;
        metadata += metadata::Describe::describe(&conversation, blobs)?;
        metadata += metadata::Describe::describe(&source_format, blobs)?;
        metadata += metadata::Describe::describe(&source_raw_root, blobs)?;
        metadata += metadata::Describe::describe(&source_conversation_id, blobs)?;
        metadata += metadata::Describe::describe(&source_message_id, blobs)?;
        metadata += metadata::Describe::describe(&source_author, blobs)?;
        metadata += metadata::Describe::describe(&source_role, blobs)?;
        metadata += metadata::Describe::describe(&source_parent_id, blobs)?;
        metadata += metadata::Describe::describe(&source_created_at, blobs)?;

        Ok(metadata)
    }
}

pub use archive_schema::archive;

pub type Repo = Repository<Pile<Blake3>>;
pub type Ws = Workspace<Pile<Blake3>>;
pub type CommitHandle = Value<Handle<Blake3, SimpleArchive>>;

fn aquire_or_force(id: Id) -> ExclusiveId {
    id.aquire().unwrap_or_else(|| ExclusiveId::force(id))
}

const ATLAS_BRANCH: &str = "atlas";
const CONFIG_BRANCH_ID: Id = triblespace::macros::id_hex!("4790808CF044F979FC7C2E47FCCB4A64");
const CONFIG_KIND_ID: Id = triblespace::macros::id_hex!("A8DCBFD625F386AA7CDFD62A81183E82");

mod config_schema {
    use super::*;
    attributes! {
        "79F990573A9DCC91EF08A5F8CBA7AA25" as kind: valueschemas::GenId;
        "DDF83FEC915816ACAE7F3FEBB57E5137" as updated_at: valueschemas::NsTAIInterval;
        "047112FC535518D289E64FBE0B60F06E" as archive_branch_id: valueschemas::GenId;
    }
}

#[derive(Debug, Clone, Default)]
struct ConfigBranches {
    archive_branch_id: Option<Id>,
}

pub fn default_pile_path() -> PathBuf {
    PathBuf::from("self.pile")
}

pub fn open_repo_for_write(pile_path: &Path, branch_id: Id, branch_name: &str) -> Result<(Repo, Id)> {
    let mut repo = open_repo(pile_path)?;
    let res = (|| -> Result<(), anyhow::Error> {
        ensure_branch_with_id(&mut repo, branch_id, branch_name)?;
        seed_default_metadata(&mut repo)?;
        Ok(())
    })();
    if let Err(err) = res {
        let _ = repo.close();
        return Err(err);
    }
    Ok((repo, branch_id))
}

pub fn open_repo_for_read(pile_path: &Path, branch_id: Id, branch_name: &str) -> Result<(Repo, Id)> {
    let mut repo = open_repo(pile_path)?;
    let res = (|| -> Result<(), anyhow::Error> {
        if repo
            .storage_mut()
            .head(branch_id)
            .map_err(|e| anyhow!("branch head {branch_name}: {e:?}"))?
            .is_none()
        {
            return Err(anyhow!("unknown branch {branch_name} ({branch_id:x})"));
        }
        Ok(())
    })();
    if let Err(err) = res {
        let _ = repo.close();
        return Err(err);
    }
    Ok((repo, branch_id))
}

pub fn resolve_archive_branch_id(
    pile_path: &Path,
    branch_name: &str,
    branch_id_override: Option<&str>,
) -> Result<Id> {
    let env_branch_id = std::env::var("TRIBLESPACE_BRANCH_ID").ok();
    let explicit = parse_optional_hex_id_labeled(
        branch_id_override.or(env_branch_id.as_deref()),
        "branch id",
    )?;
    let config = with_repo(pile_path, load_config_branches)?;
    resolve_branch_id(explicit, config.archive_branch_id, branch_name)
}

fn open_repo(pile_path: &Path) -> Result<Repo> {
    if let Some(parent) = pile_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|e| anyhow!("create pile dir {}: {e}", parent.display()))?;
    }

    let mut pile =
        Pile::<Blake3>::open(pile_path).map_err(|e| anyhow!("open pile: {e:?}"))?;
    if let Err(err) = pile.restore() {
        // Avoid Drop warnings on early errors.
        let _ = pile.close();
        return Err(anyhow!("restore pile: {err:?}"));
    }
    let signing_key = SigningKey::generate(&mut OsRng);
    Ok(Repository::new(pile, signing_key))
}

fn with_repo<T>(pile: &Path, f: impl FnOnce(&mut Repo) -> Result<T>) -> Result<T> {
    let mut repo = open_repo(pile)?;
    let result = f(&mut repo);
    let close_res = repo.close().map_err(|e| anyhow!("close pile: {e:?}"));
    if let Err(err) = close_res {
        if result.is_ok() {
            return Err(err);
        }
        eprintln!("warning: failed to close pile cleanly: {err:#}");
    }
    result
}

fn ensure_branch_with_id(
    repo: &mut Repository<Pile<Blake3>>,
    branch_id: Id,
    branch_name: &str,
) -> Result<()> {
    if repo
        .storage_mut()
        .head(branch_id)
        .map_err(|e| anyhow!("branch head {branch_name}: {e:?}"))?
        .is_some()
    {
        return Ok(());
    }
    let name_blob = branch_name.to_owned().to_blob();
    let name_handle = name_blob.get_handle::<Blake3>();
    repo.storage_mut()
        .put(name_blob)
        .map_err(|e| anyhow!("store branch name {branch_name}: {e:?}"))?;
    let metadata = branch_proto::branch_unsigned(branch_id, name_handle, None);
    let metadata_handle = repo
        .storage_mut()
        .put(metadata.to_blob())
        .map_err(|e| anyhow!("store branch metadata {branch_name}: {e:?}"))?;
    let result = repo
        .storage_mut()
        .update(branch_id, None, Some(metadata_handle))
        .map_err(|e| anyhow!("create branch {branch_name} ({branch_id:x}): {e:?}"))?;
    match result {
        PushResult::Success() | PushResult::Conflict(_) => Ok(()),
    }
}

fn find_branch_by_name(pile: &mut Pile<Blake3>, branch_name: &str) -> Result<Option<Id>> {
    let reader = pile.reader().map_err(|e| anyhow!("pile reader: {e:?}"))?;
    let iter = pile.branches().map_err(|e| anyhow!("list branches: {e:?}"))?;
    let expected = String::from(branch_name)
        .to_blob()
        .get_handle::<Blake3>()
        .to_value();

    for branch in iter {
        let branch_id = branch.map_err(|e| anyhow!("branch id: {e:?}"))?;
        let Some(head) = pile
            .head(branch_id)
            .map_err(|e| anyhow!("branch head: {e:?}"))?
        else {
            continue;
        };
        let metadata_set: TribleSet = reader
            .get(head)
            .map_err(|e| anyhow!("branch metadata: {e:?}"))?;
        let mut names = find!(
            (handle: Value<Handle<Blake3, LongString>>),
            pattern!(&metadata_set, [{ metadata::name: ?handle }])
        )
        .into_iter();
        let Some((handle,)) = names.next() else {
            continue;
        };
        if names.next().is_some() {
            continue;
        }
        if handle == expected {
            return Ok(Some(branch_id));
        }
    }
    Ok(None)
}

pub fn seed_default_metadata(repo: &mut Repo) -> Result<()> {
    let mut metadata = archive_schema::build_archive_metadata(repo.storage_mut())
        .map_err(|e| anyhow!("build archive metadata: {e:?}"))?;
    metadata += import_schema::build_import_metadata(repo.storage_mut())
        .map_err(|e| anyhow!("build import metadata: {e:?}"))?;
    repo.set_default_metadata(metadata)
        .map_err(|e| anyhow!("set default metadata: {e:?}"))?;
    Ok(())
}

pub fn emit_schema_to_atlas(pile_path: &Path) -> Result<()> {
    with_repo(pile_path, |repo| {
        let branch_id = match find_branch_by_name(repo.storage_mut(), ATLAS_BRANCH)? {
            Some(id) => id,
            None => repo
                .create_branch(ATLAS_BRANCH, None)
                .map_err(|e| anyhow!("create branch: {e:?}"))?
                .release(),
        };
        let mut metadata = archive_schema::build_archive_metadata(repo.storage_mut())
            .map_err(|e| anyhow!("build archive metadata: {e:?}"))?;
        metadata += import_schema::build_import_metadata(repo.storage_mut())
            .map_err(|e| anyhow!("build import metadata: {e:?}"))?;

        let mut ws = repo
            .pull(branch_id)
            .map_err(|e| anyhow!("pull atlas workspace: {e:?}"))?;
        let space = ws
            .checkout(..)
            .map_err(|e| anyhow!("checkout atlas workspace: {e:?}"))?;
        let delta = metadata.difference(&space);
        if !delta.is_empty() {
            ws.commit(delta, None, Some("atlas schema metadata"));
            repo.push(&mut ws)
                .map_err(|e| anyhow!("push atlas metadata: {e:?}"))?;
        }
        Ok(())
    })
}

fn load_config_branches(repo: &mut Repo) -> Result<ConfigBranches> {
    let Some(_) = repo
        .storage_mut()
        .head(CONFIG_BRANCH_ID)
        .map_err(|e| anyhow!("config branch head: {e:?}"))?
    else {
        return Ok(ConfigBranches::default());
    };

    let mut ws = repo
        .pull(CONFIG_BRANCH_ID)
        .map_err(|e| anyhow!("pull config workspace: {e:?}"))?;
    let space = ws
        .checkout(..)
        .map_err(|e| anyhow!("checkout config workspace: {e:?}"))?;

    let mut latest: Option<(Id, i128)> = None;
    for (config_id, updated_at) in find!(
        (config_id: Id, updated_at: Value<NsTAIInterval>),
        pattern!(&space, [{
            ?config_id @
            config_schema::kind: &CONFIG_KIND_ID,
            config_schema::updated_at: ?updated_at,
        }])
    ) {
        let key = interval_key(updated_at);
        if latest.is_none_or(|(_, current)| key > current) {
            latest = Some((config_id, key));
        }
    }
    let Some((config_id, _)) = latest else {
        return Ok(ConfigBranches::default());
    };

    let archive_branch_id = find!(
        (entity: Id, value: Value<valueschemas::GenId>),
        pattern!(&space, [{ ?entity @ config_schema::archive_branch_id: ?value }])
    )
    .into_iter()
    .find_map(|(entity, value)| (entity == config_id).then_some(value.from_value()));

    Ok(ConfigBranches { archive_branch_id })
}

fn resolve_branch_id(explicit: Option<Id>, configured: Option<Id>, branch_name: &str) -> Result<Id> {
    if let Some(id) = explicit {
        return Ok(id);
    }
    configured.ok_or_else(|| {
        anyhow!(
            "missing {branch_name} branch id in config (set via `playground config set archive-branch-id <hex-id>`)"
        )
    })
}

fn parse_optional_hex_id_labeled(raw: Option<&str>, label: &str) -> Result<Option<Id>> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let id = Id::from_hex(trimmed).ok_or_else(|| anyhow!("invalid {label} {trimmed}"))?;
    Ok(Some(id))
}

pub fn push_workspace(repo: &mut Repo, ws: &mut Ws) -> Result<()> {
    while let Some(mut conflict) = repo
        .try_push(ws)
        .map_err(|e| anyhow!("push workspace: {e:?}"))?
    {
        conflict
            .merge(ws)
            .map_err(|e| anyhow!("merge workspace: {e:?}"))?;
        *ws = conflict;
    }
    Ok(())
}

pub fn refresh_catalog(
    ws: &mut Ws,
    catalog: &mut TribleSet,
    catalog_head: &mut Option<CommitHandle>,
) -> Result<()> {
    let next_head = ws.head();
    if *catalog_head == next_head {
        return Ok(());
    }

    let delta = ws
        .checkout(*catalog_head..next_head)
        .context("checkout workspace delta")?;
    if !delta.is_empty() {
        *catalog += delta;
    }
    *catalog_head = next_head;
    Ok(())
}

pub fn commit_delta(
    repo: &mut Repo,
    ws: &mut Ws,
    catalog: &mut TribleSet,
    catalog_head: &mut Option<CommitHandle>,
    change: TribleSet,
    metadata: Option<&TribleSet>,
    message: &'static str,
) -> Result<bool> {
    let delta = change.difference(catalog);
    if delta.is_empty() {
        return Ok(false);
    }

    ws.commit(delta, metadata.cloned(), Some(message));
    push_workspace(repo, ws).with_context(|| format!("push {message}"))?;
    refresh_catalog(ws, catalog, catalog_head)
        .with_context(|| format!("refresh catalog after {message}"))?;
    Ok(true)
}

pub fn now_epoch() -> Epoch {
    Epoch::now().unwrap_or_else(|_| Epoch::from_gregorian_utc(1970, 1, 1, 0, 0, 0, 0))
}

pub fn unknown_epoch() -> Epoch {
    Epoch::from_gregorian_utc(1970, 1, 1, 0, 0, 0, 0)
}

pub fn epoch_from_seconds(value: f64) -> Option<Epoch> {
    if value.is_finite() {
        Some(Epoch::from_unix_seconds(value))
    } else {
        None
    }
}

pub fn epoch_interval(epoch: Epoch) -> Value<NsTAIInterval> {
    (epoch, epoch).to_value()
}

fn interval_key(interval: Value<NsTAIInterval>) -> i128 {
    let (lower, _upper): (Epoch, Epoch) = interval.from_value();
    lower.to_tai_duration().total_nanoseconds()
}

pub fn ensure_author(
    ws: &mut Ws,
    catalog: &TribleSet,
    name: &str,
    role: &str,
) -> Result<(Id, TribleSet)> {
    if let Some(author_id) = find_author_by_name(ws, catalog, name)? {
        let mut change = TribleSet::new();
        if author_role_handle(catalog, author_id).is_none() && !role.is_empty() {
            let handle = ws.put(role.to_owned());
            let author_entity = aquire_or_force(author_id);
            change += entity! { &author_entity @
                archive::author_role: handle
            };
        }
        return Ok((author_id, change));
    }

    let author_id = ufoid();
    let name_handle = ws.put(name.to_owned());
    let role_handle = (!role.is_empty()).then(|| ws.put(role.to_owned()));
    let mut change = TribleSet::new();
    change += entity! { &author_id @
        archive::kind: archive::kind_author,
        archive::author_name: name_handle,
        archive::author_role?: role_handle,
    };
    Ok((*author_id, change))
}

fn find_author_by_name(ws: &mut Ws, catalog: &TribleSet, target_name: &str) -> Result<Option<Id>> {
    for (author_id, name_handle) in find!(
        (author: Id, author_name: Value<Handle<Blake3, LongString>>),
        pattern!(catalog, [{
            ?author @
            archive::kind: archive::kind_author,
            archive::author_name: ?author_name,
        }])
    ) {
        let existing: View<str> = ws.get(name_handle).context("load author name")?;
        if existing.as_ref() == target_name {
            return Ok(Some(author_id));
        }
    }
    Ok(None)
}

fn author_role_handle(
    catalog: &TribleSet,
    author_id: Id,
) -> Option<Value<Handle<Blake3, LongString>>> {
    for (author, role) in find!(
        (author: Id, role: Value<Handle<Blake3, LongString>>),
        pattern!(catalog, [{ ?author @ archive::author_role: ?role }])
    ) {
        if author == author_id {
            return Some(role);
        }
    }
    None
}
