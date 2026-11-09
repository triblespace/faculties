//! Home operations: resolve the bridge, read it live, send one change, and
//! record that change. No argv, no output formatting.

use std::collections::BTreeSet;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::SigningKey;
use serde_json::Value;
use triblespace::core::blob::encodings::succinctarchive::{
    Rank9AcceleratedSuccinctArchiveBlob, SuccinctArchiveBlob,
};
use triblespace::core::collection::{CollectionCommit, CollectionStoreExt};
use triblespace::core::metadata;
use triblespace::core::query::TriblePattern;
use triblespace::core::repo::{BlobStoreGet, SnapshotSource};
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::Handle;
use triblespace::prelude::*;
use zeroize::Zeroizing;

use super::bridge::{self, Bridge, Change, Discovered, Group, Light, Scene};
use crate::schemas::home::{home, DEFAULT_SCOPE_ID, KIND_ACTION, KIND_BRIDGE};
use crate::storage::{AcquiringReader, FactArchive, FacultyAccess, FacultyStore, Storage};

type TextHandle = Inline<Handle<UTF8String>>;

/// Configured Home operations. The bridge comes from the pile's binding
/// unless a trusted caller overrides its address or username source.
#[derive(Clone)]
pub struct Home {
    storage: Storage,
    address: Option<String>,
    secret: Option<Id>,
    username: Option<Zeroizing<String>>,
}

impl std::fmt::Debug for Home {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Home")
            .field("address", &self.address)
            .field("secret", &self.secret)
            .field("username", &self.username.as_ref().map(|_| "<redacted>"))
            .finish_non_exhaustive()
    }
}

/// The bridge bindings in the pile: the current revisions and what they name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BindingStatus {
    pub heads: BTreeSet<Id>,
    pub bridge_ids: BTreeSet<String>,
    pub addresses: BTreeSet<String>,
    pub secrets: BTreeSet<Id>,
}

impl BindingStatus {
    /// A settled binding, never an arbitrary winner among concurrent ones.
    pub fn selected(&self) -> Result<Binding> {
        if self.heads.is_empty() {
            bail!("no Hue bridge is bound; run `home discover`, then `home bridge set ADDRESS SECRET`");
        }
        if self.heads.len() != 1
            || self.bridge_ids.len() != 1
            || self.addresses.len() != 1
            || self.secrets.len() != 1
        {
            bail!("the Hue bridge binding is unsettled; inspect `home bridge` and rebind with `home bridge set`");
        }
        Ok(Binding {
            revision: *self.heads.first().expect("one head"),
            bridge_id: self.bridge_ids.first().expect("one id").clone(),
            address: self.addresses.first().expect("one address").clone(),
            secret: *self.secrets.first().expect("one secret"),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub revision: Id,
    pub bridge_id: String,
    pub address: String,
    pub secret: Id,
}

/// What a light-changing command asks for, before names are resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    On { bri: Option<u8> },
    Off,
    Dim { percent: u8 },
    Scene { name: String },
}

impl Request {
    /// Check ranges before any credential or network is touched.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::On { bri: Some(bri) } if !(1..=bridge::MAX_BRI).contains(bri) => {
                bail!("brightness must be 1..={}", bridge::MAX_BRI)
            }
            Self::Dim { percent } if !(1..=100).contains(percent) => {
                bail!("percent must be 1..=100 (use off to turn a room off)")
            }
            Self::Scene { name } if name.trim().is_empty() => bail!("scene name is empty"),
            _ => Ok(()),
        }
    }
}

/// One change sent to the bridge and its answer.
#[derive(Clone, Debug)]
pub struct ActionReport {
    pub bridge_id: Option<String>,
    pub target: String,
    pub group: Group,
    pub scene: Option<Scene>,
    pub body: Value,
    pub response: Value,
    pub errors: Vec<String>,
}

impl ActionReport {
    /// Every entry of the answer was an error: nothing changed.
    pub fn refused(&self) -> bool {
        let entries = self.response.as_array().map_or(0, Vec::len);
        !self.errors.is_empty() && self.errors.len() >= entries
    }
}

#[derive(Clone, Debug)]
pub struct RoomsView {
    pub rooms: Vec<Group>,
}

#[derive(Clone, Debug)]
pub struct LightsView {
    pub room: Option<Group>,
    /// Each light with the name of the Room it is in, if any.
    pub lights: Vec<(Light, Option<String>)>,
}

#[derive(Clone, Debug)]
pub struct ScenesView {
    pub room: Option<Group>,
    /// Each scene with the name of the group it is bound to, if any.
    pub scenes: Vec<(Scene, Option<String>)>,
}

/// A connection resolved for one command.
struct Connection {
    bridge: Bridge,
    bridge_id: Option<String>,
    base: String,
}

impl Home {
    pub fn new(pile: std::path::PathBuf, key: Option<std::path::PathBuf>) -> Self {
        Self::with_storage(Storage::new(pile, key))
    }

    pub fn with_storage(storage: Storage) -> Self {
        Self {
            storage,
            address: None,
            secret: None,
            username: None,
        }
    }

    /// Reach the bridge at this address instead of the bound one.
    pub fn with_address(mut self, address: impl Into<String>) -> Self {
        self.address = Some(address.into());
        self
    }

    /// Open this exact Secrets version for the username instead of the bound one.
    pub fn with_secret(mut self, secret: Id) -> Self {
        self.secret = Some(secret);
        self
    }

    /// Trusted Rust injection of the username itself (tests, launchers).
    pub fn with_username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(Zeroizing::new(username.into()));
        self
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Bridges answering mDNS on the local network, or, with `cloud`, those
    /// Signify's discovery service knows for this network.
    pub fn discover(cloud: bool, wait: std::time::Duration) -> Result<Vec<Discovered>> {
        if cloud {
            bridge::discover_cloud(bridge::CLOUD_DISCOVERY)
        } else {
            bridge::discover_mdns(wait)
        }
    }

    /// Metadata only: this does not open a Secrets version.
    pub fn bridge_status(&self) -> Result<BindingStatus> {
        self.storage.with_store(|pile, signer, runtime| {
            let (facts, reader) = observe(&self.storage, pile, signer, runtime)?;
            binding_status(&facts, &reader)
        })
    }

    /// Bind the bridge at `address` with the username in Secrets version
    /// `secret`. The bridge's id comes from its own public configuration, and
    /// the username is proven with one authenticated read before anything is
    /// written. Only GET requests reach the bridge.
    pub fn bridge_set(&self, address: &str, secret: Id) -> Result<Binding> {
        let address = address.trim();
        if address.is_empty() {
            bail!("bridge address is empty");
        }
        let base = bridge::base_url(address);
        let config = bridge::public_config(&bridge::http_client()?, &base)?;
        self.storage.scope(|storage| {
            let username = open_username(storage, secret)?;
            Bridge::new(base.clone(), username)?
                .get("config")
                .context("prove the username with the bridge")?;
            storage.with_store(|pile, signer, runtime| {
                let (facts, reader) = observe(storage, pile, signer, runtime)?;
                let status = binding_status(&facts, &reader)?;
                if let Ok(current) = status.selected() {
                    if current.bridge_id == config.bridge_id
                        && current.address == address
                        && current.secret == secret
                    {
                        return Ok(current);
                    }
                }
                let fragment = binding_fragment(&config.bridge_id, address, secret, status.heads);
                let revision = fragment.root().expect("binding has one intrinsic root");
                commit(storage, pile, signer, runtime, fragment)?;
                Ok(Binding {
                    revision,
                    bridge_id: config.bridge_id.clone(),
                    address: address.to_owned(),
                    secret,
                })
            })
        })
    }

    fn connect(&self) -> Result<Connection> {
        if let (Some(address), Some(username)) = (&self.address, &self.username) {
            let base = bridge::base_url(address);
            return Ok(Connection {
                bridge: Bridge::new(base.clone(), username.clone())?,
                bridge_id: None,
                base,
            });
        }
        self.storage.scope(|storage| {
            let binding =
                if self.address.is_some() && (self.secret.is_some() || self.username.is_some()) {
                    None
                } else {
                    Some(storage.with_store(|pile, signer, runtime| {
                        let (facts, reader) = observe(storage, pile, signer, runtime)?;
                        binding_status(&facts, &reader)?.selected()
                    })?)
                };
            let address = self
                .address
                .clone()
                .or_else(|| binding.as_ref().map(|b| b.address.clone()))
                .expect("an address is configured");
            let username = match (&self.username, self.secret, &binding) {
                (Some(username), _, _) => username.clone(),
                (None, Some(secret), _) => open_username(storage, secret)?,
                (None, None, Some(binding)) => open_username(storage, binding.secret)?,
                (None, None, None) => unreachable!("a binding was read"),
            };
            let base = bridge::base_url(&address);
            Ok(Connection {
                bridge: Bridge::new(base.clone(), username)?,
                // The bound id names the bound bridge, not an overridden one.
                bridge_id: binding
                    .filter(|_| self.address.is_none())
                    .map(|binding| binding.bridge_id),
                base,
            })
        })
    }

    pub fn rooms(&self) -> Result<RoomsView> {
        let connection = self.connect()?;
        Ok(RoomsView {
            rooms: connection
                .bridge
                .groups()?
                .into_iter()
                .filter(Group::is_place)
                .collect(),
        })
    }

    pub fn lights(&self, room: Option<&str>) -> Result<LightsView> {
        let connection = self.connect()?;
        let groups = connection.bridge.groups()?;
        let room = room
            .map(|query| resolve_place(&groups, query).cloned())
            .transpose()?;
        let lights = connection
            .bridge
            .lights()?
            .into_iter()
            .filter(|light| {
                room.as_ref()
                    .is_none_or(|room| room.lights.contains(&light.id))
            })
            .map(|light| {
                let place = groups
                    .iter()
                    .find(|group| group.kind == "Room" && group.lights.contains(&light.id))
                    .map(|group| group.name.clone());
                (light, place)
            })
            .collect();
        Ok(LightsView { room, lights })
    }

    pub fn scenes(&self, room: Option<&str>) -> Result<ScenesView> {
        let connection = self.connect()?;
        let groups = connection.bridge.groups()?;
        let room = room
            .map(|query| resolve_place(&groups, query).cloned())
            .transpose()?;
        let scenes = connection
            .bridge
            .scenes()?
            .into_iter()
            .filter(|scene| room.as_ref().is_none_or(|room| scene.belongs_to(room)))
            .map(|scene| {
                let owner = scene
                    .group
                    .as_ref()
                    .and_then(|id| groups.iter().find(|group| group.id == *id))
                    .map(|group| group.name.clone());
                (scene, owner)
            })
            .collect();
        Ok(ScenesView { room, scenes })
    }

    /// Resolve `room` (and a scene name) against the bridge's live state and
    /// send one PUT to the group's action. The answer is returned whatever it
    /// says; nothing is recorded here.
    pub fn change(&self, room: &str, request: &Request) -> Result<ActionReport> {
        request.validate()?;
        let connection = self.connect()?;
        let groups = connection.bridge.groups()?;
        let group = resolve_place(&groups, room)?.clone();
        let (change, scene) = match request {
            Request::On { bri } => (Change::On { bri: *bri }, None),
            Request::Off => (Change::Off, None),
            Request::Dim { percent } => (
                Change::On {
                    bri: Some(bridge::percent_to_bri(*percent)),
                },
                None,
            ),
            Request::Scene { name } => {
                let scenes = connection.bridge.scenes()?;
                let scene = resolve_scene(&scenes, &group, name)?.clone();
                (
                    Change::Scene {
                        id: scene.id.clone(),
                    },
                    Some(scene),
                )
            }
        };
        let target = format!("groups/{}/action", group.id);
        let body = change.body();
        let response = connection.bridge.put(&target, &body)?;
        let errors = bridge::response_errors(&response);
        let bridge_id = match connection.bridge_id {
            Some(id) => Some(id),
            // An overridden bridge names itself; this read is unauthenticated.
            None => bridge::public_config(&bridge::http_client()?, &connection.base)
                .ok()
                .map(|config| config.bridge_id),
        };
        Ok(ActionReport {
            bridge_id,
            target,
            group,
            scene,
            body,
            response,
            errors,
        })
    }

    /// Record one sent change as an action in the `home` collection.
    pub fn record(&self, report: &ActionReport) -> Result<Id> {
        let fragment = action_fragment(report, crate::clock::point_now()?);
        let action = fragment.root().expect("action has one intrinsic root");
        self.storage.with_store(|pile, signer, runtime| {
            commit(&self.storage, pile, signer, runtime, fragment)?;
            Ok(action)
        })
    }
}

/// Hue names as people type them: case and runs of whitespace do not
/// matter, and typographic apostrophes and quotes match their ASCII forms.
/// The Hue app writes `Kim’s Bedroom` with U+2019; a keyboard writes `Kim's`.
pub fn same_name(name: &str, query: &str) -> bool {
    fn fold(text: &str) -> String {
        text.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .map(|c| match c {
                '\u{2018}' | '\u{2019}' | '\u{02BC}' | '\u{0060}' | '\u{00B4}' => '\'',
                '\u{201C}' | '\u{201D}' => '"',
                c => c,
            })
            .flat_map(char::to_lowercase)
            .collect()
    }
    fold(name) == fold(query)
}

/// Resolve a Room or Zone by its Hue name, case-insensitively, falling back
/// to an exact group id.
pub fn resolve_place<'a>(groups: &'a [Group], query: &str) -> Result<&'a Group> {
    let query = query.trim();
    let named: Vec<_> = groups
        .iter()
        .filter(|group| group.is_place() && same_name(&group.name, query))
        .collect();
    match named.as_slice() {
        [one] => return Ok(one),
        [] => {}
        many => bail!(
            "{} rooms or zones are named {query:?}: {}; name one by id",
            many.len(),
            many.iter()
                .map(|group| format!("{} ({})", group.id, group.kind))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
    if let Some(group) = groups.iter().find(|group| group.id == query) {
        return Ok(group);
    }
    bail!(
        "no room or zone is named {query:?}; known: {}",
        groups
            .iter()
            .filter(|group| group.is_place())
            .map(|group| format!("{:?} ({})", group.name, group.id))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Resolve a scene of `group` by name, case-insensitively, falling back to
/// an exact scene id.
pub fn resolve_scene<'a>(scenes: &'a [Scene], group: &Group, query: &str) -> Result<&'a Scene> {
    let query = query.trim();
    let own: Vec<_> = scenes
        .iter()
        .filter(|scene| scene.belongs_to(group))
        .collect();
    let named: Vec<_> = own
        .iter()
        .copied()
        .filter(|scene| same_name(&scene.name, query))
        .collect();
    match named.as_slice() {
        [one] => return Ok(one),
        [] => {}
        many => bail!(
            "{} scenes of {:?} are named {query:?}: {}; name one by id",
            many.len(),
            group.name,
            many.iter()
                .map(|scene| scene.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
    if let Some(scene) = own.iter().find(|scene| scene.id == query) {
        return Ok(scene);
    }
    bail!(
        "{:?} has no scene named {query:?}; its scenes: {}",
        group.name,
        own.iter()
            .map(|scene| format!("{:?}", scene.name))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn open_username(storage: &Storage, secret: Id) -> Result<Zeroizing<String>> {
    let bytes = crate::secrets::Secrets::with_storage(storage.clone())
        .get(secret)
        .with_context(|| format!("open the Hue username Secrets version {secret:X}"))?;
    let username = std::str::from_utf8(&bytes)
        .map_err(|_| anyhow!("the Hue username is not UTF-8"))?
        .trim();
    Ok(Zeroizing::new(username.to_owned()))
}

pub fn binding_fragment(
    bridge_id: &str,
    address: &str,
    secret: Id,
    predecessors: impl IntoIterator<Item = Id>,
) -> Fragment {
    let mut fragment = Fragment::empty();
    let bridge_id: TextHandle = fragment.put(bridge_id.to_owned());
    let address: TextHandle = fragment.put(address.to_owned());
    fragment += entity! { _ @
        metadata::tag: KIND_BRIDGE,
        home::bridge_id: bridge_id,
        home::address: address,
        home::username_secret: secret,
        metadata::supersedes*: predecessors,
    };
    fragment
}

pub fn action_fragment(
    report: &ActionReport,
    at: Inline<inlineencodings::NsTAIInterval>,
) -> Fragment {
    let mut fragment = Fragment::empty();
    let bridge_id: Option<TextHandle> =
        report.bridge_id.as_ref().map(|id| fragment.put(id.clone()));
    let target: TextHandle = fragment.put(report.target.clone());
    let target_name: TextHandle = fragment.put(report.group.name.clone());
    let request: TextHandle = fragment.put(report.body.to_string());
    let response: TextHandle = fragment.put(report.response.to_string());
    fragment += entity! { _ @
        metadata::tag: KIND_ACTION,
        home::bridge_id?: bridge_id,
        home::target: target,
        home::target_name: target_name,
        home::request: request,
        home::response: response,
        metadata::created_at: at,
    };
    fragment
}

fn read_text<R: BlobStoreGet + ?Sized>(
    reader: &R,
    handle: TextHandle,
    what: &str,
) -> Result<String> {
    let text: anybytes::View<str> = reader
        .get(handle)
        .with_context(|| format!("read Home {what}"))?;
    Ok(text.to_string())
}

/// The current binding revisions. Facts this reader does not model are
/// ignored; a text it cannot read fails the read.
pub fn binding_status<P, R>(facts: &P, reader: &R) -> Result<BindingStatus>
where
    P: TriblePattern,
    R: BlobStoreGet + ?Sized,
{
    let candidates = find!(revision: Id, pattern!(facts, [{
        ?revision @ metadata::tag: KIND_BRIDGE,
    }]));
    let heads = latest(facts, metadata::supersedes.id(), candidates);
    let mut status = BindingStatus {
        heads: heads.clone(),
        ..BindingStatus::default()
    };
    for head in heads {
        for handle in find!(h: TextHandle, pattern!(facts, [{ head @ home::bridge_id: ?h }])) {
            status
                .bridge_ids
                .insert(read_text(reader, handle, "bridge id")?);
        }
        for handle in find!(h: TextHandle, pattern!(facts, [{ head @ home::address: ?h }])) {
            status
                .addresses
                .insert(read_text(reader, handle, "bridge address")?);
        }
        status
            .secrets
            .extend(find!(s: Id, pattern!(facts, [{ head @ home::username_secret: ?s }])));
    }
    Ok(status)
}

/// Advance this key's own commits into the Rank9 view and freeze it.
fn observe(
    storage: &Storage,
    pile: &mut FacultyStore,
    signer: &SigningKey,
    runtime: &Arc<tokio::runtime::Runtime>,
) -> Result<(
    FactArchive,
    AcquiringReader<crate::storage::FacultySnapshot>,
)> {
    let source =
        storage.open_collection_read(pile, DEFAULT_SCOPE_ID, signer.verifying_key(), runtime)?;
    let succinct = pile.attach::<SuccinctArchiveBlob>(source, ())?;
    let rank9 = pile.attach::<Rank9AcceleratedSuccinctArchiveBlob>(source, succinct)?;
    pile.with_local(|local| {
        pollster::block_on(async {
            crate::storage::tolerate_own_lag(local.maintain_attached(succinct, signer).await)?;
            crate::storage::tolerate_own_lag(local.maintain_attached(rank9, signer).await)
        })
        .context("maintain Home fact collection")
    })?;
    let snapshot = AcquiringReader::new(
        pile.snapshot().context("freeze maintained Home snapshot")?,
        Arc::clone(runtime),
    );
    let facts =
        crate::storage::acquire_facts(&snapshot, rank9).context("read Home Rank9 collection")?;
    Ok((facts, snapshot))
}

fn commit(
    storage: &Storage,
    pile: &mut FacultyStore,
    signer: &SigningKey,
    runtime: &Arc<tokio::runtime::Runtime>,
    fragment: Fragment,
) -> Result<CollectionCommit> {
    let collection =
        storage.open_collection_write(pile, DEFAULT_SCOPE_ID, signer.verifying_key(), runtime)?;
    crate::collection_names::require_command_write_admission_acquiring(
        pile,
        collection,
        signer,
        "Home",
        "home bridge",
        runtime,
    )?;
    let commit = pile
        .commit(collection, signer, fragment)
        .context("commit Home record")?;
    {
        let mut local = pile.store();
        drop(
            pollster::block_on(crate::storage::ensure_downstream(
                &mut *local,
                collection,
                signer,
            ))
            .context("Home facts were committed, but ensuring their derived views failed")?,
        );
    }
    Ok(commit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn group(id: &str, name: &str, kind: &str, lights: &[&str]) -> Group {
        Group {
            id: id.into(),
            name: name.into(),
            kind: kind.into(),
            lights: lights.iter().map(|l| l.to_string()).collect(),
            any_on: false,
            all_on: false,
            bri: None,
        }
    }

    #[test]
    fn places_resolve_by_name_case_insensitively_then_by_id() {
        let groups = [
            group("1", "Guest Room", "Room", &["1"]),
            group("3", "Kim's Bedroom", "Room", &["2"]),
            group("81", "Kim's lights", "Zone", &["2"]),
            group("200", "TV", "Entertainment", &["2"]),
        ];
        assert_eq!(resolve_place(&groups, "kim's bedroom").unwrap().id, "3");
        assert_eq!(resolve_place(&groups, " KIM'S LIGHTS ").unwrap().id, "81");
        assert_eq!(resolve_place(&groups, "3").unwrap().id, "3");
        assert_eq!(resolve_place(&groups, "200").unwrap().id, "200");
        assert!(
            resolve_place(&groups, "TV").is_err(),
            "names resolve only rooms and zones"
        );
        assert!(
            resolve_place(&groups, "0").is_err(),
            "the all-lights group is never addressed"
        );
        let error = format!("{:#}", resolve_place(&groups, "kitchen").unwrap_err());
        assert!(error.contains("\"Kim's Bedroom\" (3)"), "{error}");
    }

    #[test]
    fn typographic_apostrophes_and_case_match_what_a_keyboard_types() {
        let groups = [group("3", "Kim\u{2019}s Bedroom", "Room", &["2"])];
        assert_eq!(resolve_place(&groups, "kim's bedroom").unwrap().id, "3");
        assert_eq!(
            resolve_place(&groups, "KIM\u{2019}S  BEDROOM").unwrap().id,
            "3"
        );
        assert!(same_name("Wohnzimmer Ä", "wohnzimmer ä"));
        assert!(!same_name("Kim's", "Kim's lights"));
    }

    #[test]
    fn duplicate_names_refuse_rather_than_guess() {
        let groups = [
            group("2", "Living room", "Room", &[]),
            group("9", "living ROOM", "Zone", &[]),
        ];
        let error = format!("{:#}", resolve_place(&groups, "living room").unwrap_err());
        assert!(error.contains("2 (Room), 9 (Zone)"), "{error}");
        assert_eq!(resolve_place(&groups, "9").unwrap().id, "9");
    }

    #[test]
    fn requests_are_range_checked_before_anything_else() {
        assert!(Request::On { bri: Some(0) }.validate().is_err());
        assert!(Request::On { bri: Some(255) }.validate().is_err());
        assert!(Request::On { bri: Some(254) }.validate().is_ok());
        assert!(Request::Dim { percent: 0 }.validate().is_err());
        assert!(Request::Dim { percent: 101 }.validate().is_err());
        assert!(Request::Scene { name: " ".into() }.validate().is_err());
    }

    #[test]
    fn binding_status_follows_supersession_and_refuses_a_fork() {
        let secret = Id::new([7; 16]).unwrap();
        let first = binding_fragment("ecb5", "10.0.0.1", secret, []);
        let first_id = first.root().unwrap();
        let second = binding_fragment("ecb5", "10.0.0.2", secret, [first_id]);
        let second_id = second.root().unwrap();
        let mut union = first.clone();
        union += second;
        let reader = union.blobs_mut().snapshot().unwrap();
        let status = binding_status(union.facts(), &reader).unwrap();
        assert_eq!(status.heads, [second_id].into());
        assert_eq!(status.selected().unwrap().address, "10.0.0.2");

        union += binding_fragment("ecb5", "10.0.0.3", secret, [first_id]);
        let reader = union.blobs_mut().snapshot().unwrap();
        let status = binding_status(union.facts(), &reader).unwrap();
        assert_eq!(status.heads.len(), 2);
        assert!(format!("{:#}", status.selected().unwrap_err()).contains("unsettled"));
    }
}
