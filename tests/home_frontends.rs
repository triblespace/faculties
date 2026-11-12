//! Home talks to a loopback fake bridge here, never to a real one: no request
//! in this file can reach a light. The fake bridge speaks TLS with a
//! self-signed Ed25519 certificate made in the test, so every path below runs
//! through the same certificate pinning a real bridge gets. mDNS is faked
//! too: every Home that could search is handed a scripted discovery, so no
//! query leaves the machine.
use faculties::home::{self, Fingerprint, Home, Request};
use faculties::mcp::{Faculty, InvalidArguments};
use faculties::out::{Out, Part};
use faculties::schemas::home::{home as attrs, DEFAULT_SCOPE_ID, KIND_ACTION};
use faculties::storage::{discover_target, initialize_signer, load_signer, open_pile_strict};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use triblespace::core::metadata;
use triblespace::core::repo::BlobStoreGet;
use triblespace::prelude::blobencodings::UTF8String;
use triblespace::prelude::inlineencodings::Handle;
use triblespace::prelude::*;

const USER: &str = "fixtureUser0123456789";

#[derive(Clone, Debug, PartialEq)]
struct Seen {
    method: String,
    path: String,
    body: Option<Value>,
}

struct FakeBridge {
    base: String,
    /// SHA-256 of the certificate this bridge presents.
    pin: Fingerprint,
    seen: Arc<Mutex<Vec<Seen>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for FakeBridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl FakeBridge {
    fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }
    fn puts(&self) -> Vec<Seen> {
        self.seen()
            .into_iter()
            .filter(|s| s.method == "PUT")
            .collect()
    }
}

fn groups() -> Value {
    json!({
        "1": {"name": "Guest Room", "type": "Room", "lights": ["1"],
              "state": {"any_on": true, "all_on": true}, "action": {"on": true, "bri": 254}},
        "2": {"name": "Living room", "type": "Room", "lights": ["2", "3"],
              "state": {"any_on": true, "all_on": false}, "action": {"on": true, "bri": 127}},
        "3": {"name": "Kim's Bedroom", "type": "Room", "lights": ["4"],
              "state": {"any_on": false, "all_on": false}, "action": {"on": false, "bri": 10}},
        "81": {"name": "Kim's lights", "type": "Zone", "lights": ["2", "4"],
               "state": {"any_on": true, "all_on": false}, "action": {"on": true, "bri": 127}},
        "200": {"name": "TV area", "type": "Entertainment", "lights": ["2"],
                "state": {"any_on": true, "all_on": true}, "action": {"on": true}}
    })
}

fn lights() -> Value {
    json!({
        "1": {"name": "Guest lamp", "type": "Extended color light",
              "state": {"on": true, "bri": 254, "reachable": true}},
        "2": {"name": "Sofa", "type": "Dimmable light",
              "state": {"on": true, "bri": 127, "reachable": true}},
        "3": {"name": "Shelf", "type": "Dimmable light",
              "state": {"on": false, "bri": 1, "reachable": false}},
        "4": {"name": "Bedside", "type": "Extended color light",
              "state": {"on": false, "bri": 10, "reachable": true}}
    })
}

fn scenes() -> Value {
    json!({
        "abc": {"name": "Relax", "type": "GroupScene", "group": "2", "lights": ["2", "3"]},
        "def": {"name": "Relax", "type": "GroupScene", "group": "3", "lights": ["4"]},
        "ghi": {"name": "app internal", "type": "GroupScene", "group": "2",
                "lights": ["2"], "recycle": true},
        "jkl": {"name": "Shelf only", "type": "LightScene", "lights": ["3"]}
    })
}

fn route(method: &str, path: &str, body: Option<&Value>) -> Value {
    let unauthorized =
        json!([{"error": {"type": 1, "address": "/", "description": "unauthorized user"}}]);
    if method == "GET" && path == "/api/config" {
        return json!({"name": "Fake Bridge", "bridgeid": "001788FFFE000001",
                      "modelid": "BSB002", "apiversion": "1.78.0"});
    }
    let Some(rest) = path.strip_prefix(&format!("/api/{USER}/")) else {
        return unauthorized;
    };
    match (method, rest) {
        ("GET", "config") => json!({"name": "Fake Bridge", "whitelist": {}}),
        ("GET", "groups") => groups(),
        ("GET", "lights") => lights(),
        ("GET", "scenes") => scenes(),
        ("PUT", "groups/81/action") => json!([
            {"error": {"type": 201, "address": "/groups/81/action/on",
                       "description": "parameter, on, is not modifiable"}}
        ]),
        ("PUT", target) => {
            let address = format!("/{target}");
            Value::Array(
                body.and_then(Value::as_object)
                    .into_iter()
                    .flatten()
                    .map(|(key, value)| json!({"success": {format!("{address}/{key}"): value}}))
                    .collect(),
            )
        }
        _ => json!([{"error": {"type": 3, "address": path.replace(USER, "<u>"),
                               "description": "resource not available"}}]),
    }
}

/// One DER element: tag, definite length, content.
fn der(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = vec![tag];
    let length = content.len();
    if length < 0x80 {
        out.push(length as u8);
    } else {
        let bytes: Vec<u8> = length
            .to_be_bytes()
            .into_iter()
            .skip_while(|byte| *byte == 0)
            .collect();
        out.push(0x80 | bytes.len() as u8);
        out.extend(bytes);
    }
    out.extend_from_slice(content);
    out
}

fn sequence(parts: &[&[u8]]) -> Vec<u8> {
    der(0x30, &parts.concat())
}

/// A self-signed X.509 v3 certificate for an Ed25519 key, and the key as
/// PKCS#8 v1. Built by hand so the test needs no certificate generator; the
/// fake bridge, like a real one, carries no subjectAltName.
fn self_signed(seed: u8) -> (Vec<u8>, Vec<u8>) {
    use ed25519_dalek::Signer;
    let key = ed25519_dalek::SigningKey::from_bytes(&[seed; 32]);
    let ed25519 = sequence(&[&der(0x06, &[0x2b, 0x65, 0x70])]);
    let name = sequence(&[&der(
        0x31,
        &sequence(&[&der(0x06, &[0x55, 0x04, 0x03]), &der(0x0c, b"fake bridge")]),
    )]);
    let mut public_key = vec![0];
    public_key.extend_from_slice(key.verifying_key().as_bytes());
    let tbs = sequence(&[
        &der(0xa0, &der(0x02, &[2])),
        &der(0x02, &[seed]),
        &ed25519,
        &name,
        &sequence(&[&der(0x17, b"250101000000Z"), &der(0x18, b"20991231235959Z")]),
        &name,
        &sequence(&[&ed25519, &der(0x03, &public_key)]),
    ]);
    let mut signature = vec![0];
    signature.extend_from_slice(&key.sign(&tbs).to_bytes());
    let certificate = sequence(&[&tbs, &ed25519, &der(0x03, &signature)]);
    let pkcs8 = sequence(&[
        &der(0x02, &[0]),
        &ed25519,
        &der(0x04, &der(0x04, key.as_bytes())),
    ]);
    (certificate, pkcs8)
}

fn fake_bridge() -> FakeBridge {
    fake_bridge_with_key(1)
}

/// A fake bridge whose certificate is made from `seed`: two seeds, two
/// certificates, one `bridgeid`.
fn fake_bridge_with_key(seed: u8) -> FakeBridge {
    fake_bridge_with(seed, &[&rustls::version::TLS13, &rustls::version::TLS12])
}

/// A real bridge negotiates TLS 1.2, whose handshake signature takes a
/// different verifier path from TLS 1.3.
fn fake_bridge_with(
    seed: u8,
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> FakeBridge {
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    let (certificate, key) = self_signed(seed);
    let pin = Fingerprint::of(&certificate);
    let tls = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(versions)
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(certificate)],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)),
        )
        .unwrap(),
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("https://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let (seen, stop) = (seen.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let stream = match listener.accept() {
                    Ok((stream, _)) => stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                        continue;
                    }
                    Err(error) => panic!("fake bridge accept failed: {error}"),
                };
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut stream = rustls::StreamOwned::new(
                    rustls::ServerConnection::new(tls.clone()).unwrap(),
                    stream,
                );
                let mut reader = BufReader::new(&mut stream);
                let mut request_line = String::new();
                // A client that refuses the certificate ends the handshake
                // here, before any request is read.
                if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                    continue;
                }
                let mut parts = request_line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_owned();
                let path = parts.next().unwrap_or_default().to_owned();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                    if line == "\r\n" {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':') {
                        if name.eq_ignore_ascii_case("content-length") {
                            length = value.trim().parse::<usize>().unwrap();
                        }
                    }
                }
                let body = (length > 0).then(|| {
                    let mut bytes = vec![0; length];
                    reader.read_exact(&mut bytes).unwrap();
                    serde_json::from_slice::<Value>(&bytes).unwrap()
                });
                drop(reader);
                let response = serde_json::to_vec(&route(&method, &path, body.as_ref())).unwrap();
                seen.lock().unwrap().push(Seen { method, path, body });
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                )
                .unwrap();
                stream.write_all(&response).unwrap();
                stream.conn.send_close_notify();
                let _ = stream.flush();
            }
        })
    };
    FakeBridge {
        base,
        pin,
        seen,
        stop,
        thread: Some(thread),
    }
}

/// The bridge at an `https://127.0.0.1:PORT` base, as mDNS would announce it.
fn announced(base: &str) -> home::Discovered {
    let (address, port) = base
        .strip_prefix("https://")
        .and_then(|rest| rest.rsplit_once(':'))
        .unwrap();
    home::Discovered {
        bridge_id: Some("001788fffe000001".into()),
        address: address.into(),
        port: Some(port.parse().unwrap()),
        model: Some("BSB002".into()),
        instance: None,
        via: "mdns",
    }
}

/// A scripted mDNS, and the record of what each search offered.
#[derive(Clone, Default)]
struct Searches(Arc<Mutex<Vec<Vec<String>>>>);

impl Searches {
    /// Answers with `bases` in this order, then stays silent until the
    /// window closes, as a network with no further bridge would.
    fn announcing(
        &self,
        bases: &[&str],
    ) -> impl Fn(Duration, &mut dyn FnMut(&home::Discovered) -> bool) -> anyhow::Result<()>
           + Send
           + Sync
           + 'static {
        let log = self.0.clone();
        let offers: Vec<_> = bases.iter().map(|base| announced(base)).collect();
        move |wait, accept| {
            let started = Instant::now();
            let mut offered = Vec::new();
            for bridge in &offers {
                offered.push(format!("{}:{}", bridge.address, bridge.port.unwrap()));
                if accept(bridge) {
                    log.lock().unwrap().push(offered);
                    return Ok(());
                }
            }
            log.lock().unwrap().push(offered);
            std::thread::sleep(wait.saturating_sub(started.elapsed()));
            Ok(())
        }
    }

    fn log(&self) -> Vec<Vec<String>> {
        self.0.lock().unwrap().clone()
    }
}

/// `host:port` of a base URL, as `Searches` logs it.
fn host(base: &str) -> String {
    base.trim_start_matches("https://").to_owned()
}

/// An address nothing listens on.
fn dead_base() -> String {
    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    format!("https://{}", closed.local_addr().unwrap())
}

fn call(adapter: &dyn Faculty, tool: &str, arguments: Value) -> anyhow::Result<String> {
    let mut text = String::new();
    adapter.call(
        tool,
        serde_json::to_vec(&arguments).unwrap().into(),
        &mut Out::new(&mut |part| {
            let Part::Text { text: value } = part else {
                panic!("expected text")
            };
            text.push_str(&value);
            Ok(())
        }),
    )?;
    Ok(text)
}

fn fresh_pile() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("home.pile");
    let key = directory.path().join("home.key");
    std::fs::File::create(&pile).unwrap();
    initialize_signer(&pile, Some(&key)).unwrap();
    (directory, pile, key)
}

#[test]
fn reads_are_live_gets_and_never_touch_storage() {
    let bridge = fake_bridge();
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("absent.pile");
    let operations = Home::new(pile.clone(), None)
        .with_address(bridge.base.clone())
        .with_username(USER)
        .with_pin(bridge.pin);
    let adapter = home::mcp::Home::from_operations(operations);

    let rooms = call(&adapter, "home_rooms", json!({})).unwrap();
    assert_eq!(
        rooms,
        "[1] Guest Room (room): on, 100%, 1 light\n\
         [2] Living room (room): partly on, 50%, 2 lights\n\
         [3] Kim's Bedroom (room): off, 1 light\n\
         [81] Kim's lights (zone): partly on, 50%, 2 lights\n"
    );
    let lights = call(&adapter, "home_lights", json!({"room": "living ROOM"})).unwrap();
    assert_eq!(
        lights,
        "Living room (room): partly on, 50%\n\
         [2] Sofa: on, 50%\n\
         [3] Shelf: off, unreachable\n"
    );
    let all = call(&adapter, "home_lights", json!({})).unwrap();
    assert!(all.contains("[4] Bedside in Kim's Bedroom: off\n"), "{all}");
    let scenes = call(&adapter, "home_scenes", json!({"room": "living room"})).unwrap();
    assert_eq!(
        scenes, "[abc] Relax\n[jkl] Shelf only\n",
        "recycled scenes are hidden"
    );

    assert!(bridge.seen().iter().all(|seen| seen.method == "GET"));
    assert!(bridge.puts().is_empty());
    assert!(!pile.exists());
}

#[test]
fn binding_through_secrets_then_changes_are_sent_and_recorded() {
    let bridge = fake_bridge();
    let (_directory, pile, key) = fresh_pile();
    let secret = faculties::secrets::Secrets::new(pile.clone(), Some(key.clone()))
        .add("hue-bridge-fixture-username", USER.as_bytes())
        .unwrap();
    let searches = Searches::default();
    let operations = Home::new(pile.clone(), Some(key.clone()))
        .with_discovery(searches.announcing(&[&bridge.base]));
    let binding = operations.bridge_set(Some(&bridge.base), secret).unwrap();
    assert_eq!(binding.bridge_id, "001788fffe000001");
    assert_eq!(binding.secret, secret);
    assert_eq!(
        binding.pin,
        Some(bridge.pin),
        "binding records the certificate"
    );
    assert_eq!(
        operations.bridge_set(Some(&bridge.base), secret).unwrap(),
        binding
    );
    assert!(
        searches.log().is_empty(),
        "binding at an address searches nothing"
    );
    assert!(
        bridge.puts().is_empty(),
        "binding proves the username with GETs only"
    );

    let adapter = home::mcp::Home::from_operations(operations.clone());
    let mut output = String::new();
    output += &call(
        &adapter,
        "home_on",
        json!({"room": "living room", "percent": 50}),
    )
    .unwrap();
    output += &call(&adapter, "home_off", json!({"room": "3"})).unwrap();
    output += &call(
        &adapter,
        "home_dim",
        json!({"room": "kim's lights ", "percent": 1}),
    )
    .unwrap_err()
    .to_string();
    output += &call(
        &adapter,
        "home_scene",
        json!({"room": "Kim's Bedroom", "scene": "relax"}),
    )
    .unwrap();
    output += &call(&adapter, "home_on", json!({"room": "2", "record": false})).unwrap();
    assert!(!output.contains(USER), "{output}");
    assert!(output.contains("Living room (2): sent {\"bri\":127,\"on\":true} to groups/2/action"));
    assert!(output.contains("Kim's Bedroom (3): sent scene \"Relax\" to groups/3/action"));

    let puts: Vec<_> = bridge
        .puts()
        .into_iter()
        .map(|seen| (seen.path.replace(USER, "<u>"), seen.body.unwrap()))
        .collect();
    assert_eq!(
        puts,
        [
            (
                "/api/<u>/groups/2/action".to_owned(),
                json!({"on": true, "bri": 127})
            ),
            ("/api/<u>/groups/3/action".to_owned(), json!({"on": false})),
            (
                "/api/<u>/groups/81/action".to_owned(),
                json!({"on": true, "bri": 3})
            ),
            (
                "/api/<u>/groups/3/action".to_owned(),
                json!({"scene": "def"})
            ),
            ("/api/<u>/groups/2/action".to_owned(), json!({"on": true})),
        ]
    );

    // One binding plus four recorded actions: the refused one is recorded
    // with the bridge's answer, the record=false one is not.
    let signer = load_signer(&pile, Some(&key)).unwrap();
    let mut store = open_pile_strict(&pile).unwrap();
    let discovered = discover_target(&mut store, DEFAULT_SCOPE_ID, signer.verifying_key()).unwrap();
    assert_eq!(discovered.commits().len(), 5);
    store.close().unwrap();

    let status = operations.bridge_status().unwrap();
    assert_eq!(status.selected().unwrap(), binding);
}

#[test]
fn action_record_names_the_target_body_and_answer() {
    let report = home::ActionReport {
        bridge_id: Some("001788fffe000001".into()),
        target: "groups/3/action".into(),
        group: home::Group {
            id: "3".into(),
            name: "Kim's Bedroom".into(),
            kind: "Room".into(),
            lights: vec!["4".into()],
            any_on: false,
            all_on: false,
            bri: None,
        },
        scene: None,
        body: json!({"on": false}),
        response: json!([{"success": {"/groups/3/action/on": false}}]),
        errors: vec![],
    };
    let at = faculties::clock::point_now().unwrap();
    let mut fragment = home::action_fragment(&report, at);
    let action = fragment.root().unwrap();
    let reader = fragment.blobs_mut().snapshot().unwrap();
    let (target, name, body): (
        Inline<Handle<UTF8String>>,
        Inline<Handle<UTF8String>>,
        Inline<Handle<UTF8String>>,
    ) = find!(
        (target: Inline<Handle<UTF8String>>, name: Inline<Handle<UTF8String>>, body: Inline<Handle<UTF8String>>),
        pattern!(fragment.facts(), [{ action @
            metadata::tag: KIND_ACTION,
            attrs::target: ?target,
            attrs::target_name: ?name,
            attrs::request: ?body,
        }])
    )
    .next()
    .unwrap();
    let text = |handle| -> String {
        let view: anybytes::View<str> = reader.get(handle).unwrap();
        view.to_string()
    };
    assert_eq!(text(target), "groups/3/action");
    assert_eq!(text(name), "Kim's Bedroom");
    assert_eq!(text(body), r#"{"on":false}"#);
    assert_eq!(
        home::action_fragment(&report, at).root(),
        Some(action),
        "an exact retry converges"
    );
}

#[test]
fn invalid_requests_are_rejected_before_credentials_or_network() {
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("absent.pile");
    let adapter = home::mcp::Home::new(pile.clone(), None);
    assert_eq!(adapter.tools().len(), 8);
    for (tool, input) in [
        ("home_on", json!({"room": "x", "bri": 0})),
        ("home_on", json!({"room": "x", "bri": 255})),
        ("home_on", json!({"room": "x", "bri": 3, "percent": 4})),
        ("home_on", json!({"room": "x", "percent": 101})),
        (
            "home_on",
            json!({"room": "x", "username": "must-not-be-a-field"}),
        ),
        ("home_dim", json!({"room": "x", "percent": 0})),
        ("home_dim", json!({"room": "x"})),
        ("home_scene", json!({"room": "x", "scene": "  "})),
        ("home_rooms", json!({"address": "10.0.0.1"})),
        ("home_alert", json!({})),
        (
            "home_alert",
            json!({"room": "x", "body": {"alert": "none"}}),
        ),
    ] {
        let error = call(&adapter, tool, input.clone()).unwrap_err();
        assert!(
            error.downcast_ref::<InvalidArguments>().is_some(),
            "{tool} {input}: {error:#}"
        );
    }
    assert!(!pile.exists());
    assert!(Request::Dim { percent: 0 }.validate().is_err());
}

#[test]
fn refusals_and_transport_errors_never_carry_the_username() {
    let bridge = fake_bridge();
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("absent.pile");
    let wrong = Home::new(pile.clone(), None)
        .with_address(bridge.base.clone())
        .with_username("someOtherUser42")
        .with_pin(bridge.pin);
    let error = format!("{:#}", wrong.rooms().unwrap_err());
    assert!(error.contains("unauthorized user (type 1)"), "{error}");
    assert!(!error.contains("someOtherUser42"), "{error}");

    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("https://{}", closed.local_addr().unwrap());
    drop(closed);
    let unreachable = Home::new(pile.clone(), None)
        .with_address(address)
        .with_username(USER)
        .with_pin(bridge.pin);
    let error = format!("{:#}", unreachable.change("2", &Request::Off).unwrap_err());
    assert!(!error.contains(USER), "{error}");
    assert!(error.contains("GET groups"), "{error}");
    assert!(bridge.puts().is_empty());
    let debug = format!("{unreachable:?}");
    assert!(!debug.contains(USER), "{debug}");
    assert!(!pile.exists());
}

#[test]
fn a_wrong_pin_is_refused_in_the_handshake_naming_both_fingerprints() {
    let bridge = fake_bridge();
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("absent.pile");
    let wrong = Fingerprint([0x5a; 32]);
    let home = Home::new(pile.clone(), None)
        .with_address(bridge.base.clone())
        .with_username(USER)
        .with_pin(wrong);
    let error = format!("{:#}", home.rooms().unwrap_err());
    assert!(error.contains(&bridge.pin.to_string()), "{error}");
    assert!(error.contains(&wrong.to_string()), "{error}");
    assert!(error.contains("refusing"), "{error}");
    assert!(!error.contains(USER), "{error}");
    let error = format!("{:#}", home.change("2", &Request::Off).unwrap_err());
    assert!(error.contains("refusing"), "{error}");
    assert!(
        bridge.seen().is_empty(),
        "no request, and so no username, crossed a refused handshake"
    );

    let right = Home::new(pile.clone(), None)
        .with_address(bridge.base.clone())
        .with_username(USER)
        .with_pin(bridge.pin);
    assert_eq!(right.rooms().unwrap().rooms.len(), 4);
    assert!(!pile.exists());
}

#[test]
fn an_override_without_a_pin_refuses_unless_insecure_tls_is_explicit() {
    let bridge = fake_bridge();
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("absent.pile");
    let unpinned = Home::new(pile.clone(), None)
        .with_address(bridge.base.clone())
        .with_username(USER);
    let error = format!("{:#}", unpinned.rooms().unwrap_err());
    assert!(
        error.contains("--pin") && error.contains("--insecure-tls"),
        "{error}"
    );
    assert!(bridge.seen().is_empty(), "refused before the network");

    let insecure = unpinned.clone().with_insecure_tls();
    assert_eq!(insecure.rooms().unwrap().rooms.len(), 4);
    assert!(bridge.puts().is_empty());

    // An explicit pin still wins over --insecure-tls.
    let error = format!(
        "{:#}",
        insecure
            .with_pin(Fingerprint([0x5a; 32]))
            .rooms()
            .unwrap_err()
    );
    assert!(error.contains("refusing"), "{error}");

    // Plain HTTP carries no certificate and is refused, pin or not.
    let http = bridge.base.replace("https://", "http://");
    for home in [
        Home::new(pile.clone(), None)
            .with_address(http.clone())
            .with_username(USER)
            .with_pin(bridge.pin),
        Home::new(pile.clone(), None)
            .with_address(http.clone())
            .with_username(USER)
            .with_insecure_tls(),
    ] {
        let error = format!("{:#}", home.rooms().unwrap_err());
        assert!(error.contains("HTTPS only"), "{error}");
    }
    assert!(!pile.exists());
}

#[test]
fn probe_reports_the_presented_certificate() {
    let bridge = fake_bridge();
    let config = Home::probe(&bridge.base).unwrap();
    assert_eq!(config.bridge_id, "001788fffe000001");
    assert_eq!(config.certificate, bridge.pin);
    assert!(bridge
        .seen()
        .iter()
        .all(|seen| seen.method == "GET" && seen.path == "/api/config"));
}

#[test]
fn the_binding_pin_follows_the_bridge_and_a_changed_certificate_needs_naming() {
    let first = fake_bridge_with_key(1);
    let second = fake_bridge_with_key(2);
    assert_ne!(first.pin, second.pin);
    let (_directory, pile, key) = fresh_pile();
    let secret = faculties::secrets::Secrets::new(pile.clone(), Some(key.clone()))
        .add("hue-bridge-fixture-username", USER.as_bytes())
        .unwrap();
    // Both bridges answer mDNS; only the bound certificate is taken.
    let operations = Home::new(pile.clone(), Some(key.clone()))
        .with_discovery(Searches::default().announcing(&[&first.base, &second.base]));
    let binding = operations.bridge_set(Some(&first.base), secret).unwrap();
    assert_eq!(binding.pin, Some(first.pin));

    // The same bridge id behind another certificate: refused, both named,
    // before the username is opened or sent.
    let error = format!(
        "{:#}",
        operations
            .bridge_set(Some(&second.base), secret)
            .unwrap_err()
    );
    assert!(error.contains(&first.pin.to_string()), "{error}");
    assert!(error.contains(&second.pin.to_string()), "{error}");
    assert!(error.contains("--pin"), "{error}");
    assert!(
        second.seen().iter().all(|seen| seen.path == "/api/config"),
        "only the unauthenticated config was read"
    );

    // A pin that matches neither is refused in the handshake.
    let error = format!(
        "{:#}",
        operations
            .clone()
            .with_pin(Fingerprint([0x5a; 32]))
            .bridge_set(Some(&second.base), secret)
            .unwrap_err()
    );
    assert!(error.contains("refusing"), "{error}");

    // Naming the new certificate rebinds.
    let rebound = operations
        .clone()
        .with_pin(second.pin)
        .bridge_set(Some(&second.base), secret)
        .unwrap();
    assert_eq!(rebound.pin, Some(second.pin));
    assert_eq!(
        operations.bridge_status().unwrap().selected().unwrap(),
        rebound
    );
    assert_eq!(operations.rooms().unwrap().rooms.len(), 4);

    // The bound username goes only to the bound certificate, whatever
    // address the bridge is reached at.
    let before = first.seen().len();
    let error = format!(
        "{:#}",
        operations
            .clone()
            .with_address(first.base.clone())
            .rooms()
            .unwrap_err()
    );
    assert!(error.contains("refusing"), "{error}");
    assert_eq!(first.seen().len(), before);
    assert!(first.puts().is_empty() && second.puts().is_empty());
}

#[test]
fn a_tls12_bridge_is_pinned_the_same_way() {
    let bridge = fake_bridge_with(3, &[&rustls::version::TLS12]);
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("absent.pile");
    let home = |pin| {
        Home::new(pile.clone(), None)
            .with_address(bridge.base.clone())
            .with_username(USER)
            .with_pin(pin)
    };
    assert_eq!(home(bridge.pin).rooms().unwrap().rooms.len(), 4);
    let error = format!("{:#}", home(Fingerprint([0x5a; 32])).rooms().unwrap_err());
    assert!(error.contains("refusing"), "{error}");
    assert_eq!(Home::probe(&bridge.base).unwrap().certificate, bridge.pin);
}

#[test]
fn every_connection_finds_the_bound_bridge_by_its_certificate_and_stops_there() {
    let right = fake_bridge_with_key(1);
    let wrong = fake_bridge_with_key(2);
    let later = fake_bridge_with_key(3);
    let dead = dead_base();
    let (_directory, pile, key) = fresh_pile();
    let secret = faculties::secrets::Secrets::new(pile.clone(), Some(key.clone()))
        .add("hue-bridge-fixture-username", USER.as_bytes())
        .unwrap();
    let searches = Searches::default();
    let operations = Home::new(pile.clone(), Some(key.clone()))
        .with_discovery(searches.announcing(&[&dead, &wrong.base, &right.base, &later.base]));

    // A pin alone binds the bridge mDNS finds presenting it.
    let binding = operations
        .clone()
        .with_pin(right.pin)
        .bridge_set(None, secret)
        .unwrap();
    assert_eq!(binding.pin, Some(right.pin));
    assert_eq!(binding.bridge_id, "001788fffe000001");

    let started = Instant::now();
    assert_eq!(operations.rooms().unwrap().rooms.len(), 4);
    assert!(
        started.elapsed() < home::DISCOVERY_WAIT,
        "the first match ends the search: {:?}",
        started.elapsed()
    );
    let offered = vec![host(&dead), host(&wrong.base), host(&right.base)];
    assert_eq!(searches.log(), [offered.clone(), offered]);
    assert!(
        wrong.seen().is_empty() && later.seen().is_empty(),
        "a passed-over bridge completed no handshake and saw no request"
    );
    assert!(right
        .seen()
        .iter()
        .any(|seen| seen.path == format!("/api/{USER}/groups")));
    assert_eq!(
        operations.bridge_status().unwrap().selected().unwrap(),
        binding
    );
}

#[test]
fn a_bridge_presenting_another_certificate_is_never_taken() {
    let bound = fake_bridge_with_key(1);
    let impostor = fake_bridge_with_key(2);
    let other = fake_bridge_with_key(3);
    let (_directory, pile, key) = fresh_pile();
    let secret = faculties::secrets::Secrets::new(pile.clone(), Some(key.clone()))
        .add("hue-bridge-fixture-username", USER.as_bytes())
        .unwrap();
    let searches = Searches::default();
    let operations = Home::new(pile.clone(), Some(key.clone()))
        .with_discovery(searches.announcing(&[&impostor.base, &other.base]));
    let binding = operations.bridge_set(Some(&bound.base), secret).unwrap();
    drop(bound);

    let started = Instant::now();
    let error = format!("{:#}", operations.rooms().unwrap_err());
    assert!(started.elapsed() >= home::DISCOVERY_WAIT);
    assert!(error.contains(&binding.pin.unwrap().to_string()), "{error}");
    assert!(error.contains(&impostor.pin.to_string()), "{error}");
    assert!(error.contains(&other.pin.to_string()), "{error}");
    assert!(error.contains("--bridge ADDRESS"), "{error}");
    assert!(!error.contains(USER), "{error}");

    // Binding by pin alone takes neither, and nothing is written.
    let error = format!(
        "{:#}",
        operations
            .clone()
            .with_pin(binding.pin.unwrap())
            .bridge_set(None, secret)
            .unwrap_err()
    );
    assert!(error.contains("through mDNS"), "{error}");
    assert_eq!(searches.log().len(), 2);
    assert!(impostor.seen().is_empty() && other.seen().is_empty());
    let status = operations.bridge_status().unwrap();
    assert_eq!(status.heads.len(), 1);
    assert_eq!(status.selected().unwrap(), binding);

    // Without a pin and without an address there is nothing to search for.
    let error = format!("{:#}", operations.bridge_set(None, secret).unwrap_err());
    assert!(error.contains("--pin"), "{error}");
    assert_eq!(searches.log().len(), 2);
}

#[test]
fn an_explicit_address_never_searches() {
    let bound = fake_bridge_with_key(1);
    let wrong = fake_bridge_with_key(2);
    let (_directory, pile, key) = fresh_pile();
    let secret = faculties::secrets::Secrets::new(pile.clone(), Some(key.clone()))
        .add("hue-bridge-fixture-username", USER.as_bytes())
        .unwrap();
    let searches = Searches::default();
    // A search would find the bound bridge; none may run.
    let operations = Home::new(pile.clone(), Some(key.clone()))
        .with_discovery(searches.announcing(&[&bound.base]));
    operations.bridge_set(Some(&bound.base), secret).unwrap();

    // The wrong certificate at an explicit address is a hard refusal.
    let error = format!(
        "{:#}",
        operations
            .clone()
            .with_address(wrong.base.clone())
            .rooms()
            .unwrap_err()
    );
    assert!(error.contains("refusing"), "{error}");
    assert!(wrong.seen().is_empty());

    // A dead explicit address fails where it is.
    let error = format!(
        "{:#}",
        operations
            .clone()
            .with_address(dead_base())
            .rooms()
            .unwrap_err()
    );
    assert!(error.contains("GET groups"), "{error}");
    assert!(!error.contains(USER), "{error}");

    let reached = operations.clone().with_address(bound.base.clone());
    assert_eq!(reached.rooms().unwrap().rooms.len(), 4);
    assert!(searches.log().is_empty());
}

#[test]
fn a_candidate_that_never_speaks_cannot_hold_the_search() {
    let right = fake_bridge();
    // The kernel completes the TCP handshake into the backlog; nothing ever
    // answers the TLS ClientHello.
    let silent = TcpListener::bind("127.0.0.1:0").unwrap();
    let silent_base = format!("https://{}", silent.local_addr().unwrap());
    let (_directory, pile, key) = fresh_pile();
    let secret = faculties::secrets::Secrets::new(pile.clone(), Some(key.clone()))
        .add("hue-bridge-fixture-username", USER.as_bytes())
        .unwrap();
    let searches = Searches::default();
    let operations = Home::new(pile.clone(), Some(key.clone()))
        .with_discovery(searches.announcing(&[&silent_base, &right.base]));
    operations.bridge_set(Some(&right.base), secret).unwrap();

    let started = Instant::now();
    assert_eq!(operations.rooms().unwrap().rooms.len(), 4);
    let elapsed = started.elapsed();
    assert!(
        elapsed >= home::CANDIDATE_TIMEOUT && elapsed < Duration::from_secs(3),
        "the silent candidate is given up after its own timeout: {elapsed:?}"
    );
    assert_eq!(
        searches.log(),
        [vec![host(&silent_base), host(&right.base)]]
    );
    drop(silent);
}

#[test]
fn an_alert_breathes_a_room_or_zone_with_one_group_action() {
    let bridge = fake_bridge();
    let directory = tempfile::tempdir().unwrap();
    let pile = directory.path().join("absent.pile");
    let home = Home::new(pile.clone(), None)
        .with_address(bridge.base.clone())
        .with_username(USER)
        .with_pin(bridge.pin);
    let report = home.change("living ROOM", &Request::Alert).unwrap();
    assert_eq!(report.target, "groups/2/action");
    assert_eq!(report.group.name, "Living room");
    assert_eq!(report.body, json!({"alert": "lselect"}));
    assert!(!report.refused());
    // A zone by its typed name, typographic apostrophe or not.
    let zone = home.change("kim's lights", &Request::Alert).unwrap();
    assert_eq!(zone.target, "groups/81/action");
    let puts: Vec<_> = bridge
        .puts()
        .into_iter()
        .map(|seen| (seen.path.replace(USER, "<u>"), seen.body.unwrap()))
        .collect();
    assert_eq!(
        puts,
        [
            (
                "/api/<u>/groups/2/action".to_owned(),
                json!({"alert": "lselect"})
            ),
            (
                "/api/<u>/groups/81/action".to_owned(),
                json!({"alert": "lselect"})
            ),
        ]
    );
    // Nothing is written by the change itself.
    assert!(!pile.exists());
}
