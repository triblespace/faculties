//! The Hue bridge's v1 local API, read and written over HTTPS.
//!
//! Everything here is a live answer from the bridge. Rows are parsed for
//! presentation and resolution within one command and then dropped; nothing
//! is cached. The username travels in the request path, so every transport
//! error is stripped of its URL before it can reach output or a log. Every
//! connection to a bridge is pinned to its certificate (module `tls`).

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Map, Value};
use zeroize::Zeroizing;

use super::tls::{Fingerprint, Transport, Trust};

/// Hue brightness is 1..=254; a percentage maps onto that range.
pub const MAX_BRI: u8 = 254;

/// One configured client for one bridge.
pub(crate) struct Bridge {
    base: String,
    username: Zeroizing<String>,
    http: Transport,
}

impl std::fmt::Debug for Bridge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bridge")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

/// The bridge's unauthenticated `/api/config`: its `bridgeid` and model,
/// and the certificate it presented while answering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BridgeConfig {
    pub bridge_id: String,
    pub name: Option<String>,
    pub model: Option<String>,
    pub api_version: Option<String>,
    pub certificate: Fingerprint,
}

pub(crate) fn public_config(http: &Transport, base: &str) -> Result<BridgeConfig> {
    let value = http
        .json(http.get(format!("{base}/api/config")))
        .context("read the bridge's public configuration")?;
    let certificate = http
        .presented()
        .ok_or_else(|| anyhow!("the bridge answered without presenting a certificate"))?;
    let text = |key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let bridge_id = text("bridgeid")
        .ok_or_else(|| anyhow!("bridge configuration carries no bridgeid"))?
        .to_ascii_lowercase();
    Ok(BridgeConfig {
        bridge_id,
        name: text("name"),
        model: text("modelid"),
        api_version: text("apiversion"),
        certificate,
    })
}

impl Bridge {
    pub(crate) fn new(base: String, username: Zeroizing<String>, trust: Trust) -> Result<Self> {
        if username.is_empty()
            || !username
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            bail!("the configured Hue username is empty or not a bridge username");
        }
        Ok(Self {
            base,
            username,
            http: Transport::new(trust)?,
        })
    }

    pub(crate) fn transport(&self) -> &Transport {
        &self.http
    }

    fn url(&self, path: &str) -> String {
        format!("{}/api/{}/{}", self.base, self.username.as_str(), path)
    }

    pub(crate) fn get(&self, path: &str) -> Result<Value> {
        let value = self
            .http
            .json(self.http.get(self.url(path)))
            .with_context(|| format!("GET {path} from the bridge"))?;
        refused(&value).with_context(|| format!("GET {path}"))?;
        Ok(value)
    }

    /// The bridge's answer to one state change, as sent back: an array of
    /// `success` and `error` entries, one per attribute.
    pub(crate) fn put(&self, path: &str, body: &Value) -> Result<Value> {
        self.http
            .json(self.http.put(self.url(path)).json(body))
            .with_context(|| format!("PUT {path} on the bridge"))
    }

    pub(crate) fn groups(&self) -> Result<Vec<Group>> {
        Ok(entries(&self.get("groups")?)
            .map(|(id, value)| Group::parse(id, value))
            .collect())
    }

    pub(crate) fn lights(&self) -> Result<Vec<Light>> {
        Ok(entries(&self.get("lights")?)
            .map(|(id, value)| Light::parse(id, value))
            .collect())
    }

    pub(crate) fn scenes(&self) -> Result<Vec<Scene>> {
        Ok(entries(&self.get("scenes")?)
            .map(|(id, value)| Scene::parse(id, value))
            .filter(|scene| !scene.recycle)
            .collect())
    }
}

/// A whole-request refusal is a JSON array of `error` entries in place of
/// the object asked for (an unknown username is `type 1`).
fn refused(value: &Value) -> Result<()> {
    let errors = response_errors(value);
    if value.is_array() && !errors.is_empty() {
        bail!("bridge refused: {}", errors.join("; "));
    }
    Ok(())
}

/// The `error` descriptions in a bridge answer array.
pub(crate) fn response_errors(value: &Value) -> Vec<String> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("error"))
        .map(|error| {
            let description = error
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            match error.get("type").and_then(Value::as_u64) {
                Some(kind) => format!("{description} (type {kind})"),
                None => description.to_owned(),
            }
        })
        .collect()
}

/// Object entries in bridge id order: numeric ids numerically, others after.
fn entries(value: &Value) -> impl Iterator<Item = (&str, &Map<String, Value>)> {
    let mut rows: Vec<_> = value
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(id, value)| value.as_object().map(|value| (id.as_str(), value)))
        .collect();
    rows.sort_by_key(|(id, _)| (id.parse::<u64>().map_or(u64::MAX, |n| n), id.to_string()));
    rows.into_iter()
}

fn text(value: &Map<String, Value>, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn ids(value: &Map<String, Value>, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn bri(value: Option<&Value>) -> Option<u8> {
    value
        .and_then(|state| state.get("bri"))
        .and_then(Value::as_u64)
        .map(|bri| bri.min(u64::from(MAX_BRI)) as u8)
}

/// A Hue group: a Room, a Zone, or one of the bridge's other kinds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
    pub id: String,
    pub name: String,
    /// Hue's `type`: `Room`, `Zone`, `Entertainment`, `LightGroup`, ...
    pub kind: String,
    pub lights: Vec<String>,
    pub any_on: bool,
    pub all_on: bool,
    /// The group's last action brightness.
    pub bri: Option<u8>,
}

impl Group {
    fn parse(id: &str, value: &Map<String, Value>) -> Self {
        let state = value.get("state");
        let flag = |key| {
            state
                .and_then(|state| state.get(key))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        Self {
            id: id.to_owned(),
            name: text(value, "name"),
            kind: text(value, "type"),
            lights: ids(value, "lights"),
            any_on: flag("any_on"),
            all_on: flag("all_on"),
            bri: bri(value.get("action")),
        }
    }

    /// Rooms and zones are what Home lists and resolves by name.
    pub fn is_place(&self) -> bool {
        self.kind == "Room" || self.kind == "Zone"
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Light {
    pub id: String,
    pub name: String,
    pub on: bool,
    pub bri: Option<u8>,
    pub reachable: Option<bool>,
    /// Hue's `type`, e.g. `Extended color light`.
    pub kind: String,
}

impl Light {
    fn parse(id: &str, value: &Map<String, Value>) -> Self {
        let state = value.get("state");
        Self {
            id: id.to_owned(),
            name: text(value, "name"),
            on: state
                .and_then(|state| state.get("on"))
                .and_then(Value::as_bool)
                .unwrap_or(false),
            bri: bri(state),
            reachable: state
                .and_then(|state| state.get("reachable"))
                .and_then(Value::as_bool),
            kind: text(value, "type"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scene {
    pub id: String,
    pub name: String,
    /// `GroupScene` (bound to one group) or `LightScene`.
    pub kind: String,
    pub group: Option<String>,
    pub lights: Vec<String>,
    /// Scenes the bridge may garbage-collect, made by apps for themselves.
    pub recycle: bool,
}

impl Scene {
    fn parse(id: &str, value: &Map<String, Value>) -> Self {
        Self {
            id: id.to_owned(),
            name: text(value, "name"),
            kind: text(value, "type"),
            group: value
                .get("group")
                .and_then(Value::as_str)
                .map(str::to_owned),
            lights: ids(value, "lights"),
            recycle: value
                .get("recycle")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        }
    }

    /// A group scene of this group, or a light scene confined to its lights.
    pub fn belongs_to(&self, group: &Group) -> bool {
        match &self.group {
            Some(owner) => *owner == group.id,
            None => {
                !self.lights.is_empty()
                    && self.lights.iter().all(|light| group.lights.contains(light))
            }
        }
    }
}

/// Percent (1..=100) to Hue brightness (1..=254).
pub fn percent_to_bri(percent: u8) -> u8 {
    let percent = u32::from(percent.clamp(1, 100));
    ((percent * u32::from(MAX_BRI) + 50) / 100).clamp(1, u32::from(MAX_BRI)) as u8
}

/// Hue brightness to the nearest percent.
pub fn bri_to_percent(bri: u8) -> u8 {
    ((u32::from(bri) * 100 + u32::from(MAX_BRI) / 2) / u32::from(MAX_BRI)).min(100) as u8
}

/// What a light-changing command asks of a group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    On { bri: Option<u8> },
    Off,
    Scene { id: String },
}

impl Change {
    pub fn body(&self) -> Value {
        match self {
            Self::On { bri: None } => json!({ "on": true }),
            Self::On { bri: Some(bri) } => json!({ "on": true, "bri": bri }),
            Self::Off => json!({ "on": false }),
            Self::Scene { id } => json!({ "scene": id }),
        }
    }
}

/// One Hue bridge seen on the network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovered {
    pub bridge_id: Option<String>,
    pub address: String,
    pub port: Option<u16>,
    pub model: Option<String>,
    pub instance: Option<String>,
    /// `mdns` or `cloud`.
    pub via: &'static str,
}

const MDNS_SERVICE: &str = "_hue._tcp.local";

/// Ask the local network for `_hue._tcp` and collect every answer for `wait`.
pub fn discover_mdns(wait: std::time::Duration) -> Result<Vec<Discovered>> {
    let mut found = Vec::new();
    discover_mdns_until(wait, &mut |bridge| {
        found.push(bridge.clone());
        false
    })?;
    Ok(found)
}

/// Ask the local network for `_hue._tcp` and offer each bridge to `accept`
/// as soon as its address is known, until `accept` takes one or `wait` runs
/// out. A bridge answers within a few hundred milliseconds, so a search that
/// is looking for one bridge ends long before the wait does.
pub fn discover_mdns_until(
    wait: std::time::Duration,
    accept: &mut dyn FnMut(&Discovered) -> bool,
) -> Result<()> {
    let group = std::net::SocketAddrV4::new(std::net::Ipv4Addr::new(224, 0, 0, 251), 5353);
    mdns_until(group, wait, accept)
}

/// The query is sent from an ephemeral port with the unicast-response bit, so
/// responders answer this socket directly (RFC 6762 §5.4, §6.7).
fn mdns_until(
    to: std::net::SocketAddrV4,
    wait: std::time::Duration,
    accept: &mut dyn FnMut(&Discovered) -> bool,
) -> Result<()> {
    use simple_dns::{Name, Packet, Question, CLASS, QTYPE, TYPE};
    use std::net::{Ipv4Addr, UdpSocket};

    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).context("bind mDNS query socket")?;
    let mut query = Packet::new_query(0);
    query.questions.push(Question::new(
        Name::new_unchecked(MDNS_SERVICE),
        QTYPE::TYPE(TYPE::PTR),
        CLASS::IN.into(),
        true,
    ));
    let bytes = query.build_bytes_vec().context("build mDNS query")?;
    socket.send_to(&bytes, to).context("send mDNS query")?;

    let deadline = std::time::Instant::now() + wait;
    let mut answers = Answers::default();
    let mut offered = std::collections::BTreeSet::<String>::new();
    let mut buffer = [0u8; 9000];
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        socket
            .set_read_timeout(Some(left))
            .context("set mDNS timeout")?;
        let length = match socket.recv_from(&mut buffer) {
            Ok((length, _)) => length,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break
            }
            Err(error) => return Err(error).context("receive mDNS answer"),
        };
        let Ok(packet) = Packet::parse(&buffer[..length]) else {
            continue;
        };
        answers.absorb(&packet);
        for instance in answers.instances.clone() {
            if offered.contains(&instance) {
                continue;
            }
            if let Some(bridge) = answers.bridge(&instance, false) {
                offered.insert(instance);
                if accept(&bridge) {
                    return Ok(());
                }
            }
        }
    }
    // A bridge whose host never resolved is still offered, by host name.
    for instance in &answers.instances {
        if offered.contains(instance) {
            continue;
        }
        if let Some(bridge) = answers.bridge(instance, true) {
            if accept(&bridge) {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// What mDNS answers have said so far, keyed by lowercase owner name.
#[derive(Default)]
struct Answers {
    /// Service instances, in the order they were first announced.
    instances: Vec<String>,
    services: std::collections::BTreeMap<String, (String, u16)>,
    texts: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    hosts: std::collections::BTreeMap<String, std::net::Ipv4Addr>,
}

impl Answers {
    fn absorb(&mut self, packet: &simple_dns::Packet<'_>) {
        use simple_dns::rdata::RData;
        for record in packet.answers.iter().chain(&packet.additional_records) {
            let owner = record.name.to_string().to_ascii_lowercase();
            match &record.rdata {
                RData::PTR(target) if owner == MDNS_SERVICE => {
                    let instance = target.0.to_string();
                    if !self
                        .instances
                        .iter()
                        .any(|known| known.eq_ignore_ascii_case(&instance))
                    {
                        self.instances.push(instance);
                    }
                }
                RData::SRV(srv) => {
                    self.services.insert(
                        owner,
                        (srv.target.to_string().to_ascii_lowercase(), srv.port),
                    );
                }
                RData::TXT(txt) => {
                    let entry = self.texts.entry(owner).or_default();
                    for (key, value) in txt.attributes() {
                        if let Some(value) = value {
                            entry.insert(key.to_ascii_lowercase(), value);
                        }
                    }
                }
                RData::A(a) => {
                    self.hosts
                        .insert(owner, std::net::Ipv4Addr::from(a.address));
                }
                _ => {}
            }
        }
    }

    /// The bridge behind `instance` once its service and, unless `by_name`,
    /// its host address are known.
    fn bridge(&self, instance: &str, by_name: bool) -> Option<Discovered> {
        let key = instance.to_ascii_lowercase();
        let (host, port) = self.services.get(&key)?;
        let address = match self.hosts.get(host) {
            Some(ip) => ip.to_string(),
            None if by_name => host.clone(),
            None => return None,
        };
        let txt = self.texts.get(&key);
        Some(Discovered {
            bridge_id: txt
                .and_then(|t| t.get("bridgeid"))
                .map(|id| id.to_ascii_lowercase()),
            address,
            port: Some(*port),
            model: txt.and_then(|t| t.get("modelid")).cloned(),
            instance: Some(instance.to_owned()),
            via: "mdns",
        })
    }
}

/// Signify's cloud discovery endpoint: bridges that reported in from this
/// network's public address. It is rate-limited, so it is asked only on request.
/// It is an ordinary public HTTPS service, verified against the WebPKI roots.
pub fn discover_cloud(url: &str) -> Result<Vec<Discovered>> {
    let value: Value = reqwest::blocking::Client::builder()
        .user_agent("faculties-home/0")
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("build Hue discovery HTTP client")?
        .get(url)
        .send()
        .and_then(|response| response.error_for_status())
        .and_then(|response| response.json())
        .map_err(|error| anyhow!(error.without_url()))
        .context("ask Hue cloud discovery")?;
    Ok(value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            Some(Discovered {
                bridge_id: entry
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_ascii_lowercase),
                address: entry.get("internalipaddress")?.as_str()?.to_owned(),
                port: entry.get("port").and_then(Value::as_u64).map(|p| p as u16),
                model: None,
                instance: None,
                via: "cloud",
            })
        })
        .collect())
}

pub const CLOUD_DISCOVERY: &str = "https://discovery.meethue.com/";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_and_bri_round_trip_within_the_hue_range() {
        assert_eq!(percent_to_bri(1), 3);
        assert_eq!(percent_to_bri(50), 127);
        assert_eq!(percent_to_bri(100), 254);
        assert_eq!(percent_to_bri(0), 3, "zero is clamped, never sent as off");
        assert_eq!(bri_to_percent(254), 100);
        assert_eq!(bri_to_percent(1), 0);
        for percent in 1..=100 {
            assert_eq!(bri_to_percent(percent_to_bri(percent)), percent);
        }
    }

    #[test]
    fn scenes_belong_by_group_or_by_confined_lights() {
        let room = Group {
            id: "3".into(),
            name: "Bedroom".into(),
            kind: "Room".into(),
            lights: vec!["1".into(), "2".into()],
            any_on: false,
            all_on: false,
            bri: None,
        };
        let scene = |group: Option<&str>, lights: &[&str]| Scene {
            id: "x".into(),
            name: "Read".into(),
            kind: "".into(),
            group: group.map(str::to_owned),
            lights: lights.iter().map(|l| l.to_string()).collect(),
            recycle: false,
        };
        assert!(scene(Some("3"), &["9"]).belongs_to(&room));
        assert!(!scene(Some("4"), &["1"]).belongs_to(&room));
        assert!(scene(None, &["1"]).belongs_to(&room));
        assert!(!scene(None, &["1", "9"]).belongs_to(&room));
        assert!(!scene(None, &[]).belongs_to(&room));
    }

    /// A loopback stand-in for a bridge's mDNS responder: it answers the
    /// first query with two bridges, the second one's host unresolved.
    fn responder() -> (std::net::SocketAddrV4, std::thread::JoinHandle<()>) {
        use simple_dns::rdata::{RData, A, PTR, SRV, TXT};
        use simple_dns::{Name, Packet, ResourceRecord, CLASS};
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let std::net::SocketAddr::V4(at) = socket.local_addr().unwrap() else {
            unreachable!()
        };
        let thread = std::thread::spawn(move || {
            let mut buffer = [0u8; 1500];
            let (_, from) = socket.recv_from(&mut buffer).unwrap();
            let record = |owner, rdata| {
                ResourceRecord::new(Name::new_unchecked(owner), CLASS::IN, 120, rdata)
            };
            let mut reply = Packet::new_reply(0);
            for (instance, host) in [
                ("fake-one._hue._tcp.local", "fake-one.local"),
                ("fake-two._hue._tcp.local", "fake-two.local"),
            ] {
                reply.answers.push(record(
                    MDNS_SERVICE,
                    RData::PTR(PTR(Name::new_unchecked(instance))),
                ));
                reply.additional_records.push(record(
                    instance,
                    RData::SRV(SRV {
                        priority: 0,
                        weight: 0,
                        port: 443,
                        target: Name::new_unchecked(host),
                    }),
                ));
            }
            reply.additional_records.push(record(
                "fake-one._hue._tcp.local",
                RData::TXT(TXT::new().with_string("bridgeid=001788FFFE000001").unwrap()),
            ));
            reply.additional_records.push(record(
                "fake-one.local",
                RData::A(A::from(std::net::Ipv4Addr::new(192, 0, 2, 7))),
            ));
            socket
                .send_to(&reply.build_bytes_vec().unwrap(), from)
                .unwrap();
        });
        (at, thread)
    }

    #[test]
    fn mdns_offers_bridges_as_they_answer_and_stops_at_the_first_taken() {
        use std::time::{Duration, Instant};
        let (at, thread) = responder();
        let started = Instant::now();
        let mut offered = Vec::new();
        mdns_until(at, Duration::from_secs(5), &mut |bridge| {
            offered.push(bridge.clone());
            true
        })
        .unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        thread.join().unwrap();
        assert_eq!(offered.len(), 1);
        assert_eq!(offered[0].address, "192.0.2.7");
        assert_eq!(offered[0].port, Some(443));
        assert_eq!(offered[0].bridge_id.as_deref(), Some("001788fffe000001"));

        // Declining every offer waits out the window; an unresolved host is
        // offered last, by name.
        let (at, thread) = responder();
        let started = Instant::now();
        let mut offered = Vec::new();
        mdns_until(at, Duration::from_millis(300), &mut |bridge| {
            offered.push(bridge.address.clone());
            false
        })
        .unwrap();
        assert!(started.elapsed() >= Duration::from_millis(300));
        thread.join().unwrap();
        assert_eq!(offered, ["192.0.2.7", "fake-two.local"]);
    }

    #[test]
    fn a_refusal_array_is_an_error_and_an_object_is_not() {
        let refusal = json!([{"error":{"type":1,"address":"/","description":"unauthorized user"}}]);
        let error = refused(&refusal).unwrap_err();
        assert_eq!(
            format!("{error}"),
            "bridge refused: unauthorized user (type 1)"
        );
        assert!(refused(&json!({"1":{"name":"a"}})).is_ok());
    }
}
