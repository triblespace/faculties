//! Where and how a zooid lives: the declaration, its one writer, its reader.
//!
//! A declaration is a fact about a zooid, not a message to a host. Its core
//! holds the zooid, one place (a host, any host, or at rest), the model it
//! binds when it binds one, and the declarations it follows, so two
//! declarations that follow the same one and differ are two entities and
//! both stay visible. What a zooid declares now is its heads: the
//! declarations about it that no other declaration about it follows.
//!
//! [`placed`] says what one host should consider. Only one head, with one
//! place that is this host, whose zooid has one name and one archived tail
//! (or none), leads anywhere from here. Every other declared zooid is
//! returned with the reason nothing follows here: a rest, an open place,
//! another host, several heads, or a history that cannot say where it ends.
//! Nothing is started, answered or written, and whether the zooid already
//! runs is not asked: not starting it twice at once is for its owner on that
//! host. The tip it returns is what this
//! scope's archive held when it was read; the node checks it again under its
//! own WRITE before opening.
//!
//! The readers are open world. A second place or model on one declaration, a
//! place this build does not know, or any other fact beside it is shown.

use std::collections::BTreeSet;
use std::fmt::Write as _;

use anyhow::{anyhow, bail, Result};
use triblespace::core::collection::CollectionHandle;
use triblespace::core::metadata;
use triblespace::core::repo::BlobStoreGet;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::*;

use super::operations::{ids, join, observe_scope, read_text, stored_names};
use super::{host_text, HostKey, IntervalValue, Spawn, TextHandle};
use crate::relations::Relations;
use crate::schemas::archive::archive as archive_attrs;
use crate::schemas::spawn::{
    spawn as attrs, DEFAULT_SCOPE_ID, KIND_DECLARATION, KIND_SPAWN, PLACE_ANY_HOST, PLACE_AT_REST,
};
use crate::storage::{FactArchive, Storage};

/// Where a zooid lives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Place {
    /// On the host with this node key.
    Host(HostKey),
    /// On whichever host takes it up.
    AnyHost,
    /// Nowhere: it exists and rests.
    Rest,
}

impl Place {
    fn host(self) -> Option<HostKey> {
        match self {
            Place::Host(host) => Some(host),
            Place::AnyHost | Place::Rest => None,
        }
    }

    fn open(self) -> Option<Id> {
        match self {
            Place::Host(_) => None,
            Place::AnyHost => Some(PLACE_ANY_HOST),
            Place::Rest => Some(PLACE_AT_REST),
        }
    }
}

/// A host key from 64 hex digits.
pub fn parse_host(text: &str) -> Result<HostKey> {
    let raw: [u8; 32] = hex::decode(text.trim())
        .ok()
        .and_then(|bytes| bytes.try_into().ok())
        .ok_or_else(|| anyhow!("host {text:?} is not a node key (64 hex digits)"))?;
    Ok(Inline::new(raw))
}

/// What one declaration says.
#[derive(Clone, Debug)]
pub struct DeclarationFacts {
    pub zooid: Id,
    pub place: Place,
    /// The immutable model specification it binds, when it binds one.
    pub model_spec: Option<Id>,
    /// The declarations it follows.
    pub follows: Vec<Id>,
    /// Who recorded it. Like everything below, an annotation.
    pub author: Option<Id>,
    pub declared_at: Option<IntervalValue>,
    pub host_label: Option<String>,
    pub model_label: Option<String>,
    pub reason: Option<String>,
}

/// A declaration: zooid, place, bound model and what it follows as its
/// intrinsic core, the rest as annotations. Returns the fragment and the id.
pub fn declaration_fragment(declaration: &DeclarationFacts) -> (Fragment, Id) {
    let follows = declaration.follows.clone();
    let core = entity! { _ @
        metadata::tag: KIND_DECLARATION,
        attrs::zooid: declaration.zooid,
        attrs::host?: declaration.place.host(),
        attrs::place?: declaration.place.open(),
        attrs::model_spec?: declaration.model_spec,
        attrs::follows*: follows,
    };
    let id = core.root().expect("a declaration has one intrinsic root");
    let mut fragment = Fragment::empty();
    fragment += core;
    let mut text = |text: &Option<String>| {
        text.as_ref()
            .map(|text| fragment.put::<UTF8String, _>(text.clone()))
    };
    let host_label = text(&declaration.host_label);
    let model_label = text(&declaration.model_label);
    let reason = text(&declaration.reason);
    fragment += entity! { ExclusiveId::force_ref(&id) @
        archive_attrs::author?: declaration.author,
        metadata::created_at?: declaration.declared_at,
        attrs::host_label?: host_label,
        attrs::model?: model_label,
        attrs::reason?: reason,
    };
    (fragment, id)
}

/// The declaration a run made true, annotating the run. A run's end, clean or
/// not, completes no declaration: it stands until another follows it, and a
/// later run goes on from the tail the history has then.
pub fn realises_fragment(run: Id, declaration: Id) -> Fragment {
    entity! { ExclusiveId::force_ref(&run) @ attrs::realises: declaration }
}

/// Every declaration about `zooid`.
fn declared<P: TriblePattern>(facts: &P, zooid: Id) -> BTreeSet<Id> {
    find!(
        declaration: Id,
        pattern!(facts, [{ ?declaration @
            metadata::tag: KIND_DECLARATION,
            attrs::zooid: zooid,
        }])
    )
    .collect()
}

/// What `zooid` declares now: the declarations about it that no other
/// declaration about it follows.
pub fn heads<P: TriblePattern>(facts: &P, zooid: Id) -> BTreeSet<Id> {
    let followed: BTreeSet<Id> = find!(
        (later: Id, earlier: Id),
        pattern!(facts, [{ ?later @
            metadata::tag: KIND_DECLARATION,
            attrs::zooid: zooid,
            attrs::follows: ?earlier,
        }])
    )
    .filter(|(later, earlier)| later != earlier)
    .map(|(_, earlier)| earlier)
    .collect();
    declared(facts, zooid)
        .difference(&followed)
        .copied()
        .collect()
}

/// One declaration as an observation holds it. Every field is a list, for a
/// merge can hold several values and a partial replica none.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Declaration {
    pub id: Id,
    pub zooid: Vec<Id>,
    pub host: Vec<HostKey>,
    /// `PLACE_*`, or a place this build does not know.
    pub place: Vec<Id>,
    pub model_spec: Vec<Id>,
    pub follows: Vec<Id>,
    pub author: Vec<Id>,
    pub declared_at: Vec<IntervalValue>,
    pub host_label: Vec<String>,
    pub model_label: Vec<String>,
    pub reason: Vec<String>,
}

impl Declaration {
    /// Its one place. Several, none, or one this build does not know is an
    /// error saying which.
    pub fn single_place(&self) -> Result<Place> {
        match (self.host.as_slice(), self.place.as_slice()) {
            ([host], []) => Ok(Place::Host(*host)),
            ([], [place]) if *place == PLACE_ANY_HOST => Ok(Place::AnyHost),
            ([], [place]) if *place == PLACE_AT_REST => Ok(Place::Rest),
            ([], []) => bail!("declaration {:x} names no place", self.id),
            ([], [place]) => bail!(
                "declaration {:x} names a place this build does not know ({place:x})",
                self.id
            ),
            _ => bail!(
                "declaration {:x} names several places: {}",
                self.id,
                self.describe_place()
            ),
        }
    }

    /// Whether this says exactly that place and that bound model.
    fn says(&self, place: Place, model_spec: Option<Id>) -> bool {
        self.single_place().is_ok_and(|own| own == place)
            && self.model_spec == model_spec.into_iter().collect::<Vec<_>>()
    }

    /// Its place or places, in words.
    pub fn describe_place(&self) -> String {
        let mut places: Vec<String> = self
            .host
            .iter()
            .map(|host| match self.host_label.as_slice() {
                [] => format!("host {}", host_text(*host)),
                labels => format!("host {} ({})", host_text(*host), join(labels)),
            })
            .collect();
        places.extend(self.place.iter().map(|place| match *place {
            PLACE_ANY_HOST => "any host".to_owned(),
            PLACE_AT_REST => "at rest".to_owned(),
            other => format!("place {other:x}, unknown here"),
        }));
        join(&places)
    }
}

pub(super) fn declaration_view<R: BlobStoreGet + ?Sized>(
    facts: &FactArchive,
    reader: &R,
    id: Id,
) -> Declaration {
    let texts = |handles: Vec<TextHandle>| -> Vec<String> {
        handles
            .into_iter()
            .map(|handle| read_text(reader, handle).unwrap_or_else(|| super::handle_text(handle)))
            .collect()
    };
    Declaration {
        id,
        zooid: find!(id_: Id, pattern!(facts, [{ id @ attrs::zooid: ?id_ }])).collect(),
        host: find!(host: HostKey, pattern!(facts, [{ id @ attrs::host: ?host }])).collect(),
        place: find!(place: Id, pattern!(facts, [{ id @ attrs::place: ?place }])).collect(),
        model_spec: find!(spec: Id, pattern!(facts, [{ id @ attrs::model_spec: ?spec }])).collect(),
        follows: find!(to: Id, pattern!(facts, [{ id @ attrs::follows: ?to }])).collect(),
        author: find!(who: Id, pattern!(facts, [{ id @ archive_attrs::author: ?who }])).collect(),
        declared_at: find!(
            at: IntervalValue,
            pattern!(facts, [{ id @ metadata::created_at: ?at }])
        )
        .collect(),
        host_label: texts(
            find!(text: TextHandle, pattern!(facts, [{ id @ attrs::host_label: ?text }])).collect(),
        ),
        model_label: texts(
            find!(text: TextHandle, pattern!(facts, [{ id @ attrs::model: ?text }])).collect(),
        ),
        reason: texts(
            find!(text: TextHandle, pattern!(facts, [{ id @ attrs::reason: ?text }])).collect(),
        ),
    }
}

/// A zooid's declarations as a view holds them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Declared {
    /// What it declares now.
    pub heads: Vec<Declaration>,
    /// Earlier declarations, each followed by another.
    pub followed: usize,
}

pub(super) fn declared_view<R: BlobStoreGet + ?Sized>(
    facts: &FactArchive,
    reader: &R,
    zooid: Id,
) -> Declared {
    let heads = heads(facts, zooid);
    Declared {
        followed: declared(facts, zooid).len() - heads.len(),
        heads: heads
            .into_iter()
            .map(|id| declaration_view(facts, reader, id))
            .collect(),
    }
}

impl Declared {
    pub(super) fn show(&self, out: &mut String) {
        if self.heads.is_empty() && self.followed > 0 {
            writeln!(
                out,
                "declares nothing now: each of its {} declaration(s) is followed by another",
                self.followed
            )
            .unwrap();
        }
        if self.heads.len() > 1 {
            writeln!(
                out,
                "{} declarations stand, none following the others; nothing follows from them \
                 until one does",
                self.heads.len()
            )
            .unwrap();
        }
        for head in &self.heads {
            write!(
                out,
                "declares: {} (declaration {:x}",
                head.describe_place(),
                head.id
            )
            .unwrap();
            for at in &head.declared_at {
                write!(out, ", {}", crate::care::format_time(*at)).unwrap();
            }
            if !head.author.is_empty() {
                write!(out, ", by {}", ids(&head.author)).unwrap();
            }
            writeln!(out, ")").unwrap();
            if !head.model_spec.is_empty() {
                writeln!(out, "  binds model: {}", ids(&head.model_spec)).unwrap();
            }
            if !head.model_label.is_empty() {
                writeln!(out, "  model label: {}", join(&head.model_label)).unwrap();
            }
            for why in &head.reason {
                writeln!(out, "  why: {why}").unwrap();
            }
        }
    }
}

/// What one host should consider about one declared zooid.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Standing {
    /// Its one head places it here, and its history says where it goes on.
    Here(Placement),
    /// Nothing follows on this host, and why.
    Nothing { reason: String },
}

/// A zooid placed on this host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Placement {
    /// The declaration a run that makes it true names ([`realises_fragment`]).
    pub declaration: Id,
    pub zooid: Id,
    /// Its one stored name, which its archived contexts carry.
    pub name: String,
    /// The immutable model specification it binds. `None` leaves the choice
    /// to the node, which records what it used.
    pub model_spec: Option<Id>,
    /// The archived node its history ended at in this scope, or `None` for a
    /// beginning. An observation: check it again before opening.
    pub tip: Option<Id>,
}

impl Placement {
    /// Whether a model with this specification may live as this zooid. A
    /// label never decides it.
    pub fn admits(&self, model_spec: Id) -> bool {
        self.model_spec.is_none_or(|bound| bound == model_spec)
    }
}

/// One declared zooid, its heads, and what follows from them on a host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclaredZooid {
    pub zooid: Id,
    /// Every stored name here, sorted.
    pub names: Vec<String>,
    pub heads: Vec<Declaration>,
    pub standing: Standing,
}

/// What a host should consider, and the collections it was read from.
#[derive(Clone, Debug)]
pub struct Placed {
    /// The Spawn collection the declarations were read from.
    pub spawn: CollectionHandle,
    /// The Archive collection the tips were read from; `None` when no zooid
    /// is placed on this host, so no archive was read.
    pub archive: Option<CollectionHandle>,
    /// Every declared zooid, by id.
    pub zooids: Vec<DeclaredZooid>,
}

impl Placed {
    /// The zooids placed on this host.
    pub fn here(&self) -> impl Iterator<Item = &Placement> {
        self.zooids
            .iter()
            .filter_map(|zooid| match &zooid.standing {
                Standing::Here(placement) => Some(placement),
                Standing::Nothing { .. } => None,
            })
    }
}

/// What the declarations alone say `host` should consider for `zooid`: a
/// placement here with its tip still unread, or why nothing follows.
fn placed_here<R: BlobStoreGet + ?Sized>(
    facts: &FactArchive,
    reader: &R,
    heads: &[Declaration],
    zooid: Id,
    host: HostKey,
) -> Result<Placement> {
    let head = match heads {
        [head] => head,
        [] => bail!("every declaration is followed by another"),
        several => bail!(
            "{} declarations stand and none follows the others: {}",
            several.len(),
            several
                .iter()
                .map(|head| format!("{:x} ({})", head.id, head.describe_place()))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    };
    match head.single_place()? {
        Place::Host(own) if own == host => {}
        Place::Host(_) => bail!("it lives elsewhere: {}", head.describe_place()),
        Place::AnyHost => bail!("it leaves the host open, and no host takes it up from here"),
        Place::Rest => bail!("it is at rest"),
    }
    if head.zooid.len() != 1 {
        bail!(
            "declaration {:x} speaks about several zooids: {}",
            head.id,
            ids(&head.zooid)
        );
    }
    let model_spec = match head.model_spec.as_slice() {
        [] => None,
        [one] => Some(*one),
        several => bail!(
            "declaration {:x} binds several models: {}",
            head.id,
            ids(several)
        ),
    };
    // Its archived contexts carry the name, so a history is one zooid's only
    // where the name is.
    let (names, missing) = stored_names(facts, reader, zooid);
    if let Some(handle) = missing.first() {
        bail!("its name is not here yet ({})", super::handle_text(*handle));
    }
    let name = match names.as_slice() {
        [one] => one.clone(),
        [] => bail!("it has no name here"),
        several => bail!("it has several names: {}", several.join(", ")),
    };
    let handle = super::text_handle(&name);
    let sharing: BTreeSet<Id> = find!(
        other: Id,
        pattern!(facts, [{ ?other @ metadata::tag: KIND_SPAWN, attrs::name: handle }])
    )
    .filter(|other| *other != zooid)
    .collect();
    if !sharing.is_empty() {
        bail!(
            "it shares the name {name} with {}",
            ids(&sharing.into_iter().collect::<Vec<_>>())
        );
    }
    Ok(Placement {
        declaration: head.id,
        zooid,
        name,
        model_spec,
        tip: None,
    })
}

/// What `host` should consider, read from the Spawn and Archive collections
/// `storage` is scoped to: every declared zooid, with a placement where its
/// one head names this host and its history has one end or none. Writes no
/// fact. The Archive is read only when some zooid is placed here, and after
/// the declarations: both are observations, not one snapshot.
pub fn placed(storage: &Storage, host: HostKey) -> Result<Placed> {
    let (spawn, mut zooids) = observe_scope(
        storage,
        DEFAULT_SCOPE_ID,
        "Spawn",
        |facts, reader, spawn| {
            let declared: BTreeSet<Id> = find!(
                zooid: Id,
                pattern!(facts, [{ _?declaration @
                    metadata::tag: KIND_DECLARATION,
                    attrs::zooid: ?zooid,
                }])
            )
            .collect();
            let zooids = declared
                .into_iter()
                .map(|zooid| {
                    let heads = declared_view(facts, reader, zooid).heads;
                    let standing = match placed_here(facts, reader, &heads, zooid, host) {
                        Ok(placement) => Standing::Here(placement),
                        Err(why) => Standing::Nothing {
                            reason: format!("{why:#}"),
                        },
                    };
                    DeclaredZooid {
                        zooid,
                        names: stored_names(facts, reader, zooid).0,
                        heads,
                        standing,
                    }
                })
                .collect::<Vec<_>>();
            Ok((spawn, zooids))
        },
    )?;
    let mut archive = None;
    if zooids
        .iter()
        .any(|zooid| matches!(zooid.standing, Standing::Here(_)))
    {
        archive = Some(observe_scope(
            storage,
            crate::schemas::blockdag::DEFAULT_SCOPE_ID,
            "Archive",
            |facts, _, archive| {
                for zooid in &mut zooids {
                    let Standing::Here(placement) = &mut zooid.standing else {
                        continue;
                    };
                    match super::continuation::zooid_tail(facts, &placement.name) {
                        Ok(tip) => placement.tip = tip,
                        Err(why) => {
                            zooid.standing = Standing::Nothing {
                                reason: format!("{why:#}"),
                            }
                        }
                    }
                }
                Ok(archive)
            },
        )?);
    }
    Ok(Placed {
        spawn,
        archive,
        zooids,
    })
}

/// Where [`Spawn::declare`] places the zooid.
#[derive(Clone, Copy, Debug)]
pub enum PlaceWish {
    /// The host this store signs as.
    Here,
    At(Place),
}

impl PlaceWish {
    /// Exactly one of the four ways to say it.
    pub fn one_of(here: bool, host: Option<&str>, any_host: bool, rest: bool) -> Result<Self> {
        match (here, host, any_host, rest) {
            (true, None, false, false) => Ok(PlaceWish::Here),
            (false, Some(host), false, false) => Ok(PlaceWish::At(Place::Host(parse_host(host)?))),
            (false, None, true, false) => Ok(PlaceWish::At(Place::AnyHost)),
            (false, None, false, true) => Ok(PlaceWish::At(Place::Rest)),
            _ => bail!("name exactly one place: here, a host key, any host, or rest"),
        }
    }
}

/// What to declare.
#[derive(Clone, Debug)]
pub struct DeclareRequest {
    /// The zooid's name.
    pub name: String,
    pub place: PlaceWish,
    pub model_spec: Option<Id>,
    pub model_label: Option<String>,
    pub reason: Option<String>,
    /// The persona recording it (label or id).
    pub persona: String,
}

/// What a declare call recorded.
#[derive(Clone, Debug)]
pub struct Declaring {
    pub declaration: Id,
    pub place: Place,
    /// The heads the new declaration follows.
    pub follows: Vec<Id>,
    /// The one visible head already said this, so nothing was written.
    pub already: bool,
}

impl Declaring {
    pub(super) fn line(&self, name: &str) -> String {
        let place = match self.place {
            Place::Host(host) => format!("on host {}", host_text(host)),
            Place::AnyHost => "on any host".to_owned(),
            Place::Rest => "at rest".to_owned(),
        };
        if self.already {
            return format!(
                "{name} already declares that it lives {place} (declaration {:x}); nothing written",
                self.declaration
            );
        }
        match self.follows.as_slice() {
            [] => format!(
                "{name} declares that it lives {place} (declaration {:x}, its first here)",
                self.declaration
            ),
            follows => format!(
                "{name} declares that it lives {place} (declaration {:x}, following {})",
                self.declaration,
                ids(follows)
            ),
        }
    }
}

impl Spawn {
    /// Record where and how a zooid lives, following every head visible
    /// here. Starts, stops and asks nothing. A single head that already says
    /// it stands, and nothing is written.
    pub fn declare(&self, request: &DeclareRequest) -> Result<Declaring> {
        let author =
            match Relations::with_storage(self.storage().clone()).person_state(&request.persona)? {
                Some(state) if !state.retired => state.person,
                Some(_) => bail!("persona {} is retired", request.persona),
                None => bail!("no persona matches {}", request.persona),
            };
        let (place, host_label) = match request.place {
            PlaceWish::Here => (Place::Host(self.local_host()?), own_host_label()),
            PlaceWish::At(place) => (place, None),
        };
        if place == Place::Rest && (request.model_spec.is_some() || request.model_label.is_some()) {
            bail!("a zooid at rest binds no model");
        }
        let view = self.show(&request.name)?;
        if let [head] = view.declared.heads.as_slice() {
            if head.says(place, request.model_spec) {
                return Ok(Declaring {
                    declaration: head.id,
                    place,
                    follows: head.follows.clone(),
                    already: true,
                });
            }
        }
        let follows: Vec<Id> = view.declared.heads.iter().map(|head| head.id).collect();
        let (fragment, declaration) = declaration_fragment(&DeclarationFacts {
            zooid: view.id,
            place,
            model_spec: request.model_spec,
            follows: follows.clone(),
            author: Some(author),
            declared_at: Some(crate::clock::point_now()?),
            host_label,
            model_label: request.model_label.clone(),
            reason: request.reason.clone(),
        });
        self.publish(fragment)?;
        Ok(Declaring {
            declaration,
            place,
            follows,
            already: false,
        })
    }

    /// This host's node key: the key this store signs with.
    pub fn local_host(&self) -> Result<HostKey> {
        self.storage()
            .with_store(|_, signer, _| Ok(signer.verifying_key().to_inline()))
    }

    /// [`placed`], in this Spawn's own storage.
    pub fn placed(&self, host: HostKey) -> Result<Placed> {
        placed(self.storage(), host)
    }
}

/// This host's readable name, when it has one at hand.
fn own_host_label() -> Option<String> {
    std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schemas::spawn::KIND_RUN;
    use crate::spawn::continuation::tests::{ctx, Archive};
    use crate::spawn::operations::render_show;
    use crate::spawn::LaunchConfig;
    use crate::storage::initialize_signer;
    use hifitime::Epoch;

    struct Fixture {
        _directory: tempfile::TempDir,
        storage: Storage,
        spawn: Spawn,
        host: HostKey,
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let pile = directory.path().join("spawn.pile");
        let key = directory.path().join("spawn.key");
        std::fs::File::create(&pile).unwrap();
        initialize_signer(&pile, Some(&key)).unwrap();
        let storage = Storage::new(pile, Some(key));
        Relations::with_storage(storage.clone())
            .add(
                crate::relations::ProfileInput {
                    label: "agent-cc".to_owned(),
                    ..Default::default()
                },
                None,
                &[],
            )
            .unwrap();
        let spawn = Spawn::new(storage.clone(), LaunchConfig::under_home(directory.path()));
        let host = spawn.local_host().unwrap();
        Fixture {
            _directory: directory,
            storage,
            spawn,
            host,
        }
    }

    impl Fixture {
        /// A zooid that exists and has never run.
        fn zooid(&self, brief: &str) -> (Id, String) {
            let (facts, id, _, name) = super::super::spawn_fragment(brief).unwrap();
            self.spawn.publish(facts).unwrap();
            (id, name)
        }
        fn archive(&self, archive: Archive) {
            self.storage
                .with_store(|pile, signer, runtime| {
                    let collection = self.storage.open_collection_write(
                        pile,
                        crate::schemas::blockdag::DEFAULT_SCOPE_ID,
                        signer.verifying_key(),
                        runtime,
                    )?;
                    pile.commit(collection, signer, archive.facts)?;
                    Ok(())
                })
                .unwrap();
        }
        fn declare(&self, name: &str, place: PlaceWish) -> Declaring {
            self.spawn.declare(&request(name, place)).unwrap()
        }
        fn one(&self, host: HostKey) -> DeclaredZooid {
            let mut placed = self.spawn.placed(host).unwrap();
            assert_eq!(placed.zooids.len(), 1);
            placed.zooids.remove(0)
        }
        fn shown(&self, name: &str) -> String {
            render_show(&self.spawn.show(name).unwrap(), false)
        }
    }

    fn request(name: &str, place: PlaceWish) -> DeclareRequest {
        DeclareRequest {
            name: name.to_owned(),
            place,
            model_spec: None,
            model_label: None,
            reason: None,
            persona: "agent-cc".to_owned(),
        }
    }

    fn other_host() -> HostKey {
        Inline::new([42; 32])
    }

    fn byte_id(byte: u8) -> Id {
        Id::new([byte; 16]).unwrap()
    }

    /// A declaration as another replica wrote it.
    fn far(zooid: Id, place: Place, spec: Option<Id>, follows: &[Id]) -> (Fragment, Id) {
        declaration_fragment(&DeclarationFacts {
            zooid,
            place,
            model_spec: spec,
            follows: follows.to_vec(),
            author: None,
            declared_at: None,
            host_label: None,
            model_label: None,
            reason: None,
        })
    }

    fn reason(zooid: &DeclaredZooid) -> &str {
        match &zooid.standing {
            Standing::Nothing { reason } => reason,
            here => panic!("something follows: {here:?}"),
        }
    }

    fn head_ids(zooid: &DeclaredZooid) -> BTreeSet<Id> {
        zooid.heads.iter().map(|head| head.id).collect()
    }

    #[test]
    fn a_placement_here_is_found_with_its_tip_and_nothing_follows_elsewhere() {
        let f = fixture();
        let (zooid, name) = f.zooid("Lives here.");
        let mut archive = Archive::default();
        let (lived, foreign) = (ctx(), ctx());
        archive.named(lived, &name);
        let tip = archive.chain(None, lived, 3);
        archive.named(foreign, "someone-else");
        archive.chain(None, foreign, 5);
        f.archive(archive);
        let declared = f
            .spawn
            .declare(&DeclareRequest {
                reason: Some("it is warm here".to_owned()),
                ..request(&name, PlaceWish::Here)
            })
            .unwrap();
        assert!(!declared.already && declared.follows.is_empty());

        let placed = f.spawn.placed(f.host).unwrap();
        assert_eq!(
            placed.here().collect::<Vec<_>>(),
            [&Placement {
                declaration: declared.declaration,
                zooid,
                name: name.clone(),
                model_spec: None,
                tip: Some(tip),
            }]
        );
        // The collections it was read from, for the caller to bind.
        let collection = |scope| {
            f.storage
                .with_store(|pile, signer, runtime| {
                    Ok(f.storage
                        .open_collection_read(pile, scope, signer.verifying_key(), runtime)?
                        .handle())
                })
                .unwrap()
        };
        assert_eq!(placed.spawn, collection(DEFAULT_SCOPE_ID));
        assert_eq!(
            placed.archive,
            Some(collection(crate::schemas::blockdag::DEFAULT_SCOPE_ID))
        );

        let elsewhere = f.spawn.placed(other_host()).unwrap();
        assert_eq!(elsewhere.here().count(), 0);
        assert_eq!(elsewhere.archive, None);
        assert_eq!(
            head_ids(&elsewhere.zooids[0]),
            [declared.declaration].into()
        );
        assert!(reason(&elsewhere.zooids[0]).contains("lives elsewhere"));
        let shown = f.shown(&name);
        assert!(shown.contains(&host_text(f.host)), "{shown}");
        assert!(shown.contains("why: it is warm here"), "{shown}");
    }

    #[test]
    fn a_placed_zooid_with_no_history_is_a_beginning() {
        let f = fixture();
        let (_, name) = f.zooid("Has not lived yet.");
        f.declare(&name, PlaceWish::Here);
        let placed = f.spawn.placed(f.host).unwrap();
        assert_eq!(placed.here().map(|p| p.tip).collect::<Vec<_>>(), [None]);
    }

    #[test]
    fn a_rest_the_host_could_not_see_holds_whichever_order_they_arrive_in() {
        // The host saw the placement, then learned of the rest.
        let f = fixture();
        let (zooid, _) = f.zooid("Moves away.");
        let (placement, placement_id) = far(zooid, Place::Host(f.host), None, &[]);
        let (rest, rest_id) = far(zooid, Place::Rest, None, &[placement_id]);
        f.spawn.publish(placement.clone()).unwrap();
        assert_eq!(f.spawn.placed(f.host).unwrap().here().count(), 1);
        f.spawn.publish(rest).unwrap();
        let one = f.one(f.host);
        assert_eq!(head_ids(&one), [rest_id].into());
        assert!(reason(&one).contains("at rest"));
        // Nor does the placement come back by being published again.
        f.spawn.publish(placement).unwrap();
        assert!(reason(&f.one(f.host)).contains("at rest"));

        // The rest first, the placement it follows later.
        let f = fixture();
        let (zooid, _) = f.zooid("Moves away.");
        let (placement, placement_id) = far(zooid, Place::Host(f.host), None, &[]);
        let (rest, rest_id) = far(zooid, Place::Rest, None, &[placement_id]);
        f.spawn.publish(rest).unwrap();
        assert!(reason(&f.one(f.host)).contains("at rest"));
        f.spawn.publish(placement).unwrap();
        let one = f.one(f.host);
        assert_eq!(head_ids(&one), [rest_id].into());
        assert!(reason(&one).contains("at rest"));
    }

    #[test]
    fn a_concurrent_move_and_rest_are_two_heads_until_one_follows_both() {
        let f = fixture();
        let (zooid, name) = f.zooid("Torn.");
        let (first, first_id) = far(zooid, Place::Host(other_host()), None, &[]);
        let (moved, moved_id) = far(zooid, Place::Host(f.host), None, &[first_id]);
        let (rest, rest_id) = far(zooid, Place::Rest, None, &[first_id]);
        for facts in [first, moved, rest] {
            f.spawn.publish(facts).unwrap();
        }
        for host in [f.host, other_host()] {
            let one = f.one(host);
            assert_eq!(head_ids(&one), [moved_id, rest_id].into());
            assert!(reason(&one).contains("2 declarations stand"));
        }
        let shown = f.shown(&name);
        for expected in [format!("{moved_id:x}"), format!("{rest_id:x}")] {
            assert!(shown.contains(&expected), "missing {expected} in\n{shown}");
        }
        assert!(shown.contains("at rest") && shown.contains(&host_text(f.host)));
        assert!(!shown.contains(&format!("{first_id:x}")), "{shown}");

        // Saying again what one of two heads says is still worth saying.
        let settled = f.declare(&name, PlaceWish::Here);
        assert!(!settled.already);
        assert_eq!(
            settled.follows.iter().copied().collect::<BTreeSet<_>>(),
            [moved_id, rest_id].into()
        );
        assert_eq!(f.spawn.placed(f.host).unwrap().here().count(), 1);
    }

    #[test]
    fn declarations_that_follow_the_same_one_with_different_models_are_two() {
        let f = fixture();
        let (zooid, _) = f.zooid("Two models.");
        let (first, first_id) = far(zooid, Place::Rest, None, &[]);
        let place = Place::Host(f.host);
        let (left, left_id) = far(zooid, place, Some(byte_id(1)), &[first_id]);
        let (right, right_id) = far(zooid, place, Some(byte_id(2)), &[first_id]);
        let (unbound, unbound_id) = far(zooid, place, None, &[first_id]);
        let expected: BTreeSet<Id> = [left_id, right_id, unbound_id].into();
        assert_eq!(expected.len(), 3);
        for facts in [first, left, right, unbound] {
            f.spawn.publish(facts).unwrap();
        }
        let one = f.one(f.host);
        assert_eq!(head_ids(&one), expected);
        assert!(reason(&one).contains("3 declarations stand"));
    }

    #[test]
    fn what_annotates_a_declaration_is_not_part_of_it() {
        let plain = DeclarationFacts {
            zooid: byte_id(1),
            place: Place::AnyHost,
            model_spec: None,
            follows: vec![byte_id(9), byte_id(8)],
            author: None,
            declared_at: None,
            host_label: None,
            model_label: None,
            reason: None,
        };
        let annotated = DeclarationFacts {
            follows: vec![byte_id(8), byte_id(9)],
            author: Some(byte_id(3)),
            declared_at: Some(crate::clock::point_now().unwrap()),
            host_label: Some("testhost".to_owned()),
            model_label: Some("a label".to_owned()),
            reason: Some("why".to_owned()),
            ..plain.clone()
        };
        assert_eq!(
            declaration_fragment(&plain).1,
            declaration_fragment(&annotated).1
        );
    }

    #[test]
    fn a_zooid_that_declared_nothing_is_no_error_and_nothing_to_consider() {
        let f = fixture();
        let (_, name) = f.zooid("Never declared.");
        let placed = f.spawn.placed(f.host).unwrap();
        assert!(placed.zooids.is_empty() && placed.archive.is_none());
        assert!(!f.shown(&name).contains("declar"));
    }

    #[test]
    fn declaring_the_same_again_writes_nothing() {
        let f = fixture();
        let (_, name) = f.zooid("Says it twice.");
        let first = f.declare(&name, PlaceWish::Here);
        let before = f.spawn.show(&name).unwrap().declared;
        let again = f
            .spawn
            .declare(&DeclareRequest {
                reason: Some("again".to_owned()),
                model_label: Some("a label".to_owned()),
                ..request(&name, PlaceWish::At(Place::Host(f.host)))
            })
            .unwrap();
        assert!(again.already);
        assert_eq!(again.declaration, first.declaration);
        assert!(again.line(&name).contains("nothing written"));
        assert_eq!(f.spawn.show(&name).unwrap().declared, before);

        // Another model, or another place, is something else to say.
        let bound = f
            .spawn
            .declare(&DeclareRequest {
                model_spec: Some(byte_id(5)),
                ..request(&name, PlaceWish::Here)
            })
            .unwrap();
        assert!(!bound.already);
        assert_eq!(bound.follows, [first.declaration]);
        let rest = f.declare(&name, PlaceWish::At(Place::Rest));
        assert_eq!(rest.follows, [bound.declaration]);
        // And returning is new: it follows the rest.
        let back = f.declare(&name, PlaceWish::Here);
        assert!(!back.already && back.declaration != first.declaration);
        let declared = f.spawn.show(&name).unwrap().declared;
        assert_eq!((declared.heads.len(), declared.followed), (1, 3));
    }

    #[test]
    fn a_history_that_cannot_say_where_it_ends_is_returned_with_its_reason() {
        let f = fixture();
        let (_, name) = f.zooid("Lived twice at once.");
        let mut archive = Archive::default();
        let (left, right) = (ctx(), ctx());
        for context in [left, right] {
            archive.named(context, &name);
            archive.chain(None, context, 2);
        }
        f.archive(archive);
        let declared = f.declare(&name, PlaceWish::Here);
        let one = f.one(f.host);
        assert_eq!(head_ids(&one), [declared.declaration].into());
        assert!(reason(&one).contains("diverges"), "{}", reason(&one));
    }

    #[test]
    fn an_ambiguous_history_does_not_hide_a_neighbour() {
        let f = fixture();
        let (torn, torn_name) = f.zooid("Lived twice at once.");
        let (well, well_name) = f.zooid("Lived once.");
        let mut archive = Archive::default();
        for (name, lives) in [(&torn_name, 2), (&well_name, 1)] {
            for _ in 0..lives {
                let context = ctx();
                archive.named(context, name);
                archive.chain(None, context, 2);
            }
        }
        f.archive(archive);
        f.declare(&torn_name, PlaceWish::Here);
        f.declare(&well_name, PlaceWish::Here);
        let placed = f.spawn.placed(f.host).unwrap();
        assert_eq!(placed.here().map(|p| p.zooid).collect::<Vec<_>>(), [well]);
        assert!(placed
            .zooids
            .iter()
            .any(|z| z.zooid == torn && reason(z).contains("diverges")));
    }

    #[test]
    fn facts_this_reader_does_not_know_are_shown_and_never_an_error() {
        let unknown = byte_id(77);

        // Something it does not know beside a placement changes nothing.
        let f = fixture();
        let (_, name) = f.zooid("Carries more.");
        let id = f.declare(&name, PlaceWish::Here).declaration;
        f.spawn
            .publish(entity! { ExclusiveId::force_ref(&id) @
                metadata::tag: unknown,
                attrs::exit_code: 7u64,
            })
            .unwrap();
        assert_eq!(f.spawn.placed(f.host).unwrap().here().count(), 1);

        // A place it does not know beside the host is a second place.
        f.spawn
            .publish(entity! { ExclusiveId::force_ref(&id) @ attrs::place: unknown })
            .unwrap();
        let one = f.one(f.host);
        assert!(reason(&one).contains("several places"), "{}", reason(&one));
        let shown = f.shown(&name);
        assert!(
            shown.contains(&format!("place {unknown:x}")) && shown.contains(&host_text(f.host)),
            "{shown}"
        );

        // Two hosts by merge, two models by merge, an unknown place alone,
        // no place at all.
        let second = |facts: fn(Id, &Fixture) -> Fragment, expected: &str| {
            let f = fixture();
            let (zooid, name) = f.zooid("Merged.");
            f.spawn.publish(facts(zooid, &f)).unwrap();
            let one = f.one(f.host);
            assert!(reason(&one).contains(expected), "{}", reason(&one));
            assert!(f.shown(&name).contains("declares: "));
        };
        second(
            |zooid, f| {
                let (mut facts, id) = far(zooid, Place::Host(f.host), None, &[]);
                facts += entity! { ExclusiveId::force_ref(&id) @ attrs::host: other_host() };
                facts
            },
            "several places",
        );
        second(
            |zooid, f| {
                let (mut facts, id) = far(zooid, Place::Host(f.host), Some(byte_id(1)), &[]);
                facts += entity! { ExclusiveId::force_ref(&id) @ attrs::model_spec: byte_id(2) };
                facts
            },
            "several models",
        );
        second(
            |zooid, _| {
                entity! { _ @
                    metadata::tag: KIND_DECLARATION,
                    attrs::zooid: zooid,
                    attrs::place: byte_id(77),
                }
            },
            "does not know",
        );
        second(
            |zooid, _| entity! { _ @ metadata::tag: KIND_DECLARATION, attrs::zooid: zooid },
            "names no place",
        );
        second(
            |zooid, f| {
                let (mut facts, id) = far(zooid, Place::Host(f.host), None, &[]);
                facts += entity! { ExclusiveId::force_ref(&id) @ attrs::follows: id };
                let (rest, _) = far(zooid, Place::Rest, None, &[byte_id(50)]);
                facts += rest;
                facts
            },
            "2 declarations stand",
        );
    }

    #[test]
    fn a_declaration_is_followed_only_by_one_about_the_same_zooid() {
        let f = fixture();
        let (zooid, name) = f.zooid("Stays.");
        let (other, _) = f.zooid("Someone else.");
        let id = f.declare(&name, PlaceWish::Here).declaration;
        let (foreign, _) = far(other, Place::Rest, None, &[id]);
        f.spawn.publish(foreign).unwrap();
        let placed = f.spawn.placed(f.host).unwrap();
        assert_eq!(placed.here().map(|p| p.zooid).collect::<Vec<_>>(), [zooid]);
    }

    #[test]
    fn only_the_bound_model_matches_and_a_label_never_decides() {
        let f = fixture();
        let (_, name) = f.zooid("Binds a model.");
        let (bound, same_label) = (byte_id(1), byte_id(2));
        f.spawn
            .declare(&DeclareRequest {
                model_spec: Some(bound),
                model_label: Some("the-model".to_owned()),
                ..request(&name, PlaceWish::Here)
            })
            .unwrap();
        // Two local models are called `the-model`.
        let admitted = |f: &Fixture| -> Vec<Id> {
            let placed = f.spawn.placed(f.host).unwrap();
            let placement = placed.here().next().unwrap().clone();
            [bound, same_label]
                .into_iter()
                .filter(|spec| placement.admits(*spec))
                .collect()
        };
        assert_eq!(admitted(&f), [bound]);
        let shown = f.shown(&name);
        assert!(
            shown.contains(&format!("binds model: {bound:x}")),
            "{shown}"
        );
        assert!(shown.contains("model label: the-model"), "{shown}");

        // Without a binding the node chooses, whatever the label says.
        f.spawn
            .declare(&DeclareRequest {
                model_label: Some("the-model".to_owned()),
                ..request(&name, PlaceWish::Here)
            })
            .unwrap();
        assert_eq!(admitted(&f), [bound, same_label]);
    }

    #[test]
    fn a_run_that_names_its_declaration_leaves_the_declaration_as_it_is() {
        let f = fixture();
        let (zooid, name) = f.zooid("Lives.");
        f.declare(&name, PlaceWish::Here);
        let before = f.spawn.placed(f.host).unwrap().zooids;
        let declaration = before[0].heads[0].id;
        let run = entity! { _ @
            metadata::tag: KIND_RUN,
            attrs::of: zooid,
            metadata::started_at:
                crate::clock::point(Epoch::from_unix_seconds(1_800_000_000.0)).unwrap(),
        };
        let run_id = run.root().unwrap();
        let mut facts = run;
        facts += realises_fragment(run_id, declaration);
        facts += entity! { ExclusiveId::force_ref(&run_id) @
            metadata::finished_at:
                crate::clock::point(Epoch::from_unix_seconds(1_800_000_600.0)).unwrap(),
        };
        f.spawn.publish(facts).unwrap();
        assert_eq!(f.spawn.placed(f.host).unwrap().zooids, before);
        let view = f.spawn.show(&name).unwrap();
        assert_eq!(view.runs[0].realises, [declaration]);
        assert!(f
            .shown(&name)
            .contains(&format!("realises declaration: {declaration:x}")));
    }

    #[test]
    fn a_zooid_without_one_name_has_no_one_history_to_continue() {
        // Hosts named it apart.
        let f = fixture();
        let (zooid, name) = f.zooid("Named twice.");
        f.declare(&name, PlaceWish::Here);
        f.spawn
            .publish(super::super::name_fragment(zooid, "another-name", None))
            .unwrap();
        assert!(reason(&f.one(f.host)).contains("several names"));

        // Another zooid has the same name.
        let f = fixture();
        let (_, name) = f.zooid("Shares.");
        f.declare(&name, PlaceWish::Here);
        let (facts, twin, _) = super::super::spawn_core("Shares it.").unwrap();
        f.spawn.publish(facts).unwrap();
        f.spawn
            .publish(super::super::name_fragment(twin, &name, None))
            .unwrap();
        assert!(reason(&f.one(f.host)).contains("shares the name"));

        // The declaration arrived and the zooid's own facts did not.
        let f = fixture();
        f.spawn
            .publish(far(byte_id(3), Place::Host(f.host), None, &[]).0)
            .unwrap();
        assert!(reason(&f.one(f.host)).contains("no name here"));
    }

    #[test]
    fn mcp_declares_the_same_with_an_explicit_persona_and_exactly_one_place() {
        use crate::mcp::{Faculty as _, InvalidArguments};
        let f = fixture();
        let (_, name) = f.zooid("Declares for itself.");
        let mcp = crate::spawn::mcp::Spawn::with_storage(f.storage.clone())
            .with_launch(LaunchConfig::under_home(f._directory.path()));
        let call = |arguments: serde_json::Value| -> Result<String> {
            let mut text = String::new();
            let mut emit = |part| {
                if let crate::out::Part::Text { text: part } = part {
                    text.push_str(&part);
                }
                Ok(())
            };
            mcp.call(
                "zooid_declare",
                serde_json::to_vec(&arguments).unwrap().into(),
                &mut crate::out::Out::new(&mut emit),
            )?;
            Ok(text)
        };
        for bad in [
            serde_json::json!({"name": name, "persona": "agent-cc"}),
            serde_json::json!({"name": name, "persona": "agent-cc", "here": true, "rest": true}),
            serde_json::json!({"name": name, "here": true}),
            serde_json::json!({"name": name, "persona": "agent-cc", "here": true, "model_spec": "label"}),
        ] {
            let error = call(bad.clone()).unwrap_err();
            assert!(error.is::<InvalidArguments>(), "{bad}: {error:#}");
        }
        assert!(f.spawn.placed(f.host).unwrap().zooids.is_empty());
        let said = call(serde_json::json!({
            "name": name, "persona": "agent-cc", "here": true,
            "model_spec": format!("{:x}", byte_id(4)), "why": "to live",
        }))
        .unwrap();
        assert!(said.contains("its first here"), "{said}");
        let placed = f.spawn.placed(f.host).unwrap();
        assert_eq!(placed.here().next().unwrap().model_spec, Some(byte_id(4)));
        let author = Relations::with_storage(f.storage.clone())
            .person_state("agent-cc")
            .unwrap()
            .unwrap()
            .person;
        assert_eq!(placed.zooids[0].heads[0].author, [author]);
    }

    #[test]
    fn declaring_refuses_what_it_cannot_record() {
        let f = fixture();
        let (_, name) = f.zooid("Refused.");
        let error =
            |request: &DeclareRequest| format!("{:#}", f.spawn.declare(request).unwrap_err());
        assert!(error(&DeclareRequest {
            persona: "nobody".to_owned(),
            ..request(&name, PlaceWish::Here)
        })
        .contains("nobody"));
        assert!(error(&DeclareRequest {
            model_spec: Some(byte_id(1)),
            ..request(&name, PlaceWish::At(Place::Rest))
        })
        .contains("binds no model"));
        assert!(error(&request("no-such-zooid", PlaceWish::Here)).contains("no-such-zooid"));
        assert!(f.spawn.placed(f.host).unwrap().zooids.is_empty());

        for (here, host, any_host, rest) in [
            (false, None, false, false),
            (true, None, true, false),
            (true, None, false, true),
            (false, Some("ab"), false, false),
            (true, Some("ab"), false, false),
        ] {
            assert!(PlaceWish::one_of(here, host, any_host, rest).is_err());
        }
        let key = host_text(f.host);
        assert!(matches!(
            PlaceWish::one_of(false, Some(&key), false, false),
            Ok(PlaceWish::At(Place::Host(host))) if host == f.host
        ));
    }
}
