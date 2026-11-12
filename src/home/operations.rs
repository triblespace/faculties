//! Home operations: resolve the bridge, read it live, send one change, and
//! record that change. No argv, no output formatting.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

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

use super::bridge::{self, Bridge, BridgeConfig, Change, Discovered, Group, Light, Scene};
use super::tls::{self, Fingerprint, Transport, Trust};
use crate::schemas::home::{home, Sha256, DEFAULT_SCOPE_ID, KIND_ACTION, KIND_BRIDGE};
use crate::storage::{AcquiringReader, FactArchive, FacultyAccess, FacultyStore, Storage};

type TextHandle = Inline<Handle<UTF8String>>;
type CertificateHash = Inline<inlineencodings::Hash<Sha256>>;

/// How Home finds bridges: each one is offered to the callback as it
/// answers, until the callback takes one or the wait runs out.
pub type Discovery =
    Arc<dyn Fn(Duration, &mut dyn FnMut(&Discovered) -> bool) -> Result<()> + Send + Sync>;

/// The longest a connection searches for its bridge. The search ends at the
/// first bridge presenting the pinned certificate, normally well within it.
pub const DISCOVERY_WAIT: Duration = Duration::from_secs(2);

/// The longest one candidate may take to connect, finish its handshake and
/// answer while searching, so a peer that accepts and never speaks cannot
/// hold the search. Requests to the chosen bridge keep the normal timeout.
pub const CANDIDATE_TIMEOUT: Duration = Duration::from_secs(1);

/// Configured Home operations. The bridge is the one the pile's binding pins,
/// found through mDNS, unless a trusted caller overrides its address,
/// username source or certificate pin.
#[derive(Clone)]
pub struct Home {
    storage: Storage,
    address: Option<String>,
    secret: Option<Id>,
    username: Option<Zeroizing<String>>,
    pin: Option<Fingerprint>,
    insecure_tls: bool,
    discovery: Discovery,
}

impl std::fmt::Debug for Home {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Home")
            .field("address", &self.address)
            .field("secret", &self.secret)
            .field("username", &self.username.as_ref().map(|_| "<redacted>"))
            .field("pin", &self.pin)
            .field("insecure_tls", &self.insecure_tls)
            .finish_non_exhaustive()
    }
}

/// The bridge bindings in the pile: the current revisions and what they name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BindingStatus {
    pub heads: BTreeSet<Id>,
    pub bridge_ids: BTreeSet<String>,
    pub secrets: BTreeSet<Id>,
    /// Certificate pins. A binding made before Home pinned certificates has
    /// none; it still names its bridge, but no connection trusts it.
    pub pins: BTreeSet<Fingerprint>,
}

impl BindingStatus {
    /// A settled binding, never an arbitrary winner among concurrent ones.
    pub fn selected(&self) -> Result<Binding> {
        if self.heads.is_empty() {
            bail!("no Hue bridge is bound; run `home discover`, then `home bridge set SECRET ADDRESS`");
        }
        if self.heads.len() != 1
            || self.bridge_ids.len() != 1
            || self.secrets.len() != 1
            || self.pins.len() > 1
        {
            bail!("the Hue bridge binding is unsettled; inspect `home bridge` and rebind with `home bridge set`");
        }
        Ok(Binding {
            revision: *self.heads.first().expect("one head"),
            bridge_id: self.bridge_ids.first().expect("one id").clone(),
            secret: *self.secrets.first().expect("one secret"),
            pin: self.pins.first().copied(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Binding {
    pub revision: Id,
    pub bridge_id: String,
    pub secret: Id,
    /// The bridge's certificate; `None` only for a binding made before Home
    /// pinned certificates.
    pub pin: Option<Fingerprint>,
}

/// What a light-changing command asks for, before names are resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request {
    On {
        bri: Option<u8>,
    },
    Off,
    Dim {
        percent: u8,
    },
    Scene {
        name: String,
    },
    /// Breathe the lights for about fifteen seconds and return them to
    /// their state: a signal, not a change.
    Alert,
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
            pin: None,
            insecure_tls: false,
            discovery: Arc::new(bridge::discover_mdns_until),
        }
    }

    /// Reach the bridge at this address instead of finding it through mDNS.
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

    /// Accept only the certificate with this SHA-256, in place of the bound
    /// pin. Binding with a pin refuses a bridge presenting any other.
    pub fn with_pin(mut self, pin: Fingerprint) -> Self {
        self.pin = Some(pin);
        self
    }

    /// Connect without a pin, accepting whatever certificate the bridge
    /// presents. A pin, explicit or bound, still wins.
    pub fn with_insecure_tls(mut self) -> Self {
        self.insecure_tls = true;
        self
    }

    /// Trusted Rust injection of how bridges are found (tests).
    pub fn with_discovery(
        mut self,
        discovery: impl Fn(Duration, &mut dyn FnMut(&Discovered) -> bool) -> Result<()>
            + Send
            + Sync
            + 'static,
    ) -> Self {
        self.discovery = Arc::new(discovery);
        self
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Bridges answering mDNS on the local network, or, with `cloud`, those
    /// Signify's discovery service knows for this network.
    pub fn discover(cloud: bool, wait: Duration) -> Result<Vec<Discovered>> {
        if cloud {
            bridge::discover_cloud(bridge::CLOUD_DISCOVERY)
        } else {
            bridge::discover_mdns(wait)
        }
    }

    /// The bridge at `address`, as it answers its unauthenticated
    /// `/api/config`, with the fingerprint of the certificate it presented.
    /// Nothing is verified, opened or written: this is how a pin is learned.
    pub fn probe(address: &str) -> Result<BridgeConfig> {
        bridge::public_config(&Transport::new(Trust::Observe)?, &tls::base_url(address)?)
    }

    /// Metadata only: this does not open a Secrets version.
    pub fn bridge_status(&self) -> Result<BindingStatus> {
        self.storage.with_store(|pile, signer, runtime| {
            let (facts, reader) = observe(&self.storage, pile, signer, runtime)?;
            binding_status(&facts, &reader)
        })
    }

    /// Bind a bridge with the username in Secrets version `secret`: the one
    /// at `address`, pinned to the certificate it presents (or to
    /// [`Home::with_pin`]), or, with a pin and no address, the one mDNS finds
    /// presenting that certificate. The binding records no address; every
    /// connection finds the bridge again by its certificate. The bridge's id
    /// comes from its own public configuration, and the username is proven
    /// with one authenticated read, over a connection pinned to that
    /// certificate, before anything is written. Only GET requests reach the
    /// bridge.
    ///
    /// Without a pin, the first certificate seen is trusted, but a bridge
    /// already bound under the same id that now presents a different
    /// certificate is refused before its username is opened: a changed
    /// certificate is accepted only when named explicitly.
    pub fn bridge_set(&self, address: Option<&str>, secret: Id) -> Result<Binding> {
        let (base, config) = match (address, self.pin) {
            (Some(address), pin) => {
                let base = tls::base_url(address)?;
                let trust = pin.map_or(Trust::Observe, Trust::Pin);
                let config = bridge::public_config(&Transport::new(trust)?, &base)?;
                (base, config)
            }
            (None, Some(pin)) => self.find(pin)?,
            (None, None) => bail!(
                "binding a bridge without its ADDRESS needs its certificate: pass --pin SHA256 \
                 (`home discover` lists bridges, `home bridge probe ADDRESS` shows a \
                 certificate), or give the ADDRESS to trust the certificate presented there"
            ),
        };
        let certificate = config.certificate;
        self.storage.scope(|storage| {
            let earlier = storage.with_store(|pile, signer, runtime| {
                let (facts, reader) = observe(storage, pile, signer, runtime)?;
                binding_status(&facts, &reader)
            })?;
            if self.pin.is_none()
                && earlier.bridge_ids.contains(&config.bridge_id)
                && !earlier.pins.is_empty()
                && !earlier.pins.contains(&certificate)
            {
                bail!(
                    "bridge {} now presents a certificate with SHA-256 {certificate}, but it \
                     was bound with {}; if its certificate really changed, rebind with \
                     `home bridge set SECRET --pin {certificate}`",
                    config.bridge_id,
                    earlier
                        .pins
                        .iter()
                        .map(Fingerprint::to_string)
                        .collect::<Vec<_>>()
                        .join(" or ")
                );
            }
            let username = open_username(storage, secret)?;
            Bridge::new(base.clone(), username, Trust::Pin(certificate))?
                .get("config")
                .context("prove the username with the bridge")?;
            storage.with_store(|pile, signer, runtime| {
                let (facts, reader) = observe(storage, pile, signer, runtime)?;
                let status = binding_status(&facts, &reader)?;
                if let Ok(current) = status.selected() {
                    if current.bridge_id == config.bridge_id
                        && current.secret == secret
                        && current.pin == Some(certificate)
                    {
                        return Ok(current);
                    }
                }
                let fragment =
                    binding_fragment(&config.bridge_id, secret, certificate, status.heads);
                let revision = fragment.root().expect("binding has one intrinsic root");
                commit(storage, pile, signer, runtime, fragment)?;
                Ok(Binding {
                    revision,
                    bridge_id: config.bridge_id.clone(),
                    secret,
                    pin: Some(certificate),
                })
            })
        })
    }

    /// The first bridge discovery offers that presents `pin`, with its
    /// public configuration. A bridge presenting any other certificate, or
    /// not answering, is passed over; no username is sent while searching.
    fn find(&self, pin: Fingerprint) -> Result<(String, BridgeConfig)> {
        let mut found = None;
        let mut passed = Vec::new();
        (self.discovery)(DISCOVERY_WAIT, &mut |candidate| {
            let address = match candidate.port {
                Some(port) => format!("{}:{port}", candidate.address),
                None => candidate.address.clone(),
            };
            let reached = tls::base_url(&address).and_then(|base| {
                let transport = Transport::with_timeout(Trust::Pin(pin), CANDIDATE_TIMEOUT)?;
                let config = bridge::public_config(&transport, &base)?;
                Ok((base, config))
            });
            match reached {
                Ok(reached) => {
                    found = Some(reached);
                    true
                }
                Err(error) => {
                    passed.push(format!("{address}: {error:#}"));
                    false
                }
            }
        })
        .context("discover Hue bridges over mDNS")?;
        found.ok_or_else(|| {
            let seen = if passed.is_empty() {
                "no Hue bridge answered".to_owned()
            } else {
                format!("passed over {}", passed.join("; "))
            };
            anyhow!(
                "no Hue bridge found through mDNS within {DISCOVERY_WAIT:?} presents the \
                 certificate {pin} ({seen}); pass --bridge ADDRESS to reach it directly"
            )
        })
    }

    fn connect(&self) -> Result<Connection> {
        if let (Some(address), Some(username)) = (&self.address, &self.username) {
            let trust = trust(self.pin, None, self.insecure_tls)?;
            let base = tls::base_url(address)?;
            return Ok(Connection {
                bridge: Bridge::new(base.clone(), username.clone(), trust)?,
                bridge_id: None,
                base,
            });
        }
        let (binding, trust, username) = self.storage.scope(|storage| {
            let binding =
                if self.address.is_some() && (self.secret.is_some() || self.username.is_some()) {
                    None
                } else {
                    Some(storage.with_store(|pile, signer, runtime| {
                        let (facts, reader) = observe(storage, pile, signer, runtime)?;
                        binding_status(&facts, &reader)?.selected()
                    })?)
                };
            // Decided before the username is opened, and so before it can
            // travel anywhere.
            let trust = trust(self.pin, binding.as_ref(), self.insecure_tls)?;
            if self.address.is_none() && trust == Trust::Observe {
                bail!(
                    "an unpinned binding cannot be found through mDNS, which would trust any \
                     bridge: pass --bridge ADDRESS, or rebind with `home bridge set SECRET ADDRESS`"
                );
            }
            let username = match (&self.username, self.secret, &binding) {
                (Some(username), _, _) => username.clone(),
                (None, Some(secret), _) => open_username(storage, secret)?,
                (None, None, Some(binding)) => open_username(storage, binding.secret)?,
                (None, None, None) => unreachable!("a binding was read"),
            };
            Ok((binding, trust, username))
        })?;
        let base = match (&self.address, trust) {
            (Some(address), _) => tls::base_url(address)?,
            (None, Trust::Pin(pin)) => self.find(pin)?.0,
            (None, Trust::Observe) => unreachable!("refused above"),
        };
        Ok(Connection {
            bridge: Bridge::new(base.clone(), username, trust)?,
            // The bound id names the bound bridge, not an overridden one.
            bridge_id: binding
                .filter(|_| self.address.is_none())
                .map(|binding| binding.bridge_id),
            base,
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
            Request::Alert => (Change::Alert, None),
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
            None => bridge::public_config(connection.bridge.transport(), &connection.base)
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

/// Which certificate a connection accepts. An explicit pin wins, then the
/// binding's: the bound username is only ever sent to the bound certificate,
/// whatever address it is reached at. Without either, only an explicit
/// insecure request connects.
fn trust(pin: Option<Fingerprint>, binding: Option<&Binding>, insecure: bool) -> Result<Trust> {
    if let Some(pin) = pin.or(binding.and_then(|binding| binding.pin)) {
        return Ok(Trust::Pin(pin));
    }
    if insecure {
        return Ok(Trust::Observe);
    }
    match binding {
        Some(_) => bail!(
            "the Hue bridge binding has no certificate pin (it was bound before Home pinned \
             certificates); run `home bridge set SECRET ADDRESS` again to record one"
        ),
        None => bail!(
            "a bridge override without a binding needs the bridge's certificate: pass \
             --pin SHA256 (`home bridge probe ADDRESS` shows it), or --insecure-tls to skip \
             the check"
        ),
    }
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
    secret: Id,
    certificate: Fingerprint,
    predecessors: impl IntoIterator<Item = Id>,
) -> Fragment {
    let mut fragment = Fragment::empty();
    let bridge_id: TextHandle = fragment.put(bridge_id.to_owned());
    let certificate: CertificateHash = Inline::new(certificate.0);
    fragment += entity! { _ @
        metadata::tag: KIND_BRIDGE,
        home::bridge_id: bridge_id,
        home::username_secret: secret,
        home::certificate_sha256: certificate,
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
        status
            .secrets
            .extend(find!(s: Id, pattern!(facts, [{ head @ home::username_secret: ?s }])));
        status.pins.extend(
            find!(h: CertificateHash, pattern!(facts, [{ head @ home::certificate_sha256: ?h }]))
                .map(|hash| Fingerprint(hash.raw)),
        );
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
    use triblespace::core::id::ExclusiveId;

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
        let rotated = Id::new([8; 16]).unwrap();
        let pin = Fingerprint([1; 32]);
        let first = binding_fragment("0017", secret, pin, []);
        let first_id = first.root().unwrap();
        let second = binding_fragment("0017", rotated, pin, [first_id]);
        let second_id = second.root().unwrap();
        let mut union = first.clone();
        union += second;
        let reader = union.blobs_mut().snapshot().unwrap();
        let status = binding_status(union.facts(), &reader).unwrap();
        assert_eq!(status.heads, [second_id].into());
        assert_eq!(status.selected().unwrap().secret, rotated);
        assert!(
            find!(h: TextHandle, pattern!(union.facts(), [{ second_id @ home::address: ?h }]))
                .next()
                .is_none(),
            "a binding records no address"
        );
        assert_eq!(status.selected().unwrap().pin, Some(pin));

        union += binding_fragment("0017", Id::new([9; 16]).unwrap(), pin, [first_id]);
        let reader = union.blobs_mut().snapshot().unwrap();
        let status = binding_status(union.facts(), &reader).unwrap();
        assert_eq!(status.heads.len(), 2);
        assert!(format!("{:#}", status.selected().unwrap_err()).contains("unsettled"));
    }

    #[test]
    fn two_pins_on_one_head_are_unsettled() {
        let secret = Id::new([7; 16]).unwrap();
        let mut union = binding_fragment("0017", secret, Fingerprint([1; 32]), []);
        let head = union.root().unwrap();
        let other: CertificateHash = Inline::new([2; 32]);
        union += entity! { ExclusiveId::force_ref(&head) @ home::certificate_sha256: other };
        let reader = union.blobs_mut().snapshot().unwrap();
        let status = binding_status(union.facts(), &reader).unwrap();
        assert_eq!(status.pins.len(), 2);
        assert!(format!("{:#}", status.selected().unwrap_err()).contains("unsettled"));
    }

    #[test]
    fn a_binding_from_before_pinning_reads_but_is_not_trusted() {
        // Such a binding also names the address it was bound at, which Home
        // no longer reads: the fact is ignored, not refused.
        let secret = Id::new([7; 16]).unwrap();
        let mut fragment = Fragment::empty();
        let bridge_id: TextHandle = fragment.put("0017".to_owned());
        let address: TextHandle = fragment.put("192.0.2.1".to_owned());
        fragment += entity! { _ @
            metadata::tag: KIND_BRIDGE,
            home::bridge_id: bridge_id,
            home::address: address,
            home::username_secret: secret,
        };
        let reader = fragment.blobs_mut().snapshot().unwrap();
        let binding = binding_status(fragment.facts(), &reader)
            .unwrap()
            .selected()
            .unwrap();
        assert_eq!(binding.pin, None);
        let error = format!("{:#}", trust(None, Some(&binding), false).unwrap_err());
        assert!(error.contains("no certificate pin"), "{error}");
        assert!(error.contains("home bridge set"), "{error}");
        assert_eq!(trust(None, Some(&binding), true).unwrap(), Trust::Observe);
    }

    #[test]
    fn trust_prefers_an_explicit_pin_then_the_bound_one_and_never_defaults_to_none() {
        let bound = Fingerprint([1; 32]);
        let explicit = Fingerprint([2; 32]);
        let binding = Binding {
            revision: Id::new([3; 16]).unwrap(),
            bridge_id: "0017".into(),
            secret: Id::new([7; 16]).unwrap(),
            pin: Some(bound),
        };
        assert_eq!(
            trust(Some(explicit), Some(&binding), false).unwrap(),
            Trust::Pin(explicit)
        );
        assert_eq!(
            trust(None, Some(&binding), true).unwrap(),
            Trust::Pin(bound),
            "--insecure-tls never loosens a pin that exists"
        );
        assert_eq!(
            trust(Some(explicit), None, false).unwrap(),
            Trust::Pin(explicit)
        );
        let error = format!("{:#}", trust(None, None, false).unwrap_err());
        assert!(
            error.contains("--pin") && error.contains("--insecure-tls"),
            "{error}"
        );
        assert_eq!(trust(None, None, true).unwrap(), Trust::Observe);
    }
}
