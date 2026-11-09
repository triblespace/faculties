//! Home talks to a loopback fake bridge here, never to a real one: no request
//! in this file can reach a light.
use faculties::home::{self, Home, Request};
use faculties::mcp::{Faculty, InvalidArguments};
use faculties::out::{Out, Part};
use faculties::schemas::home::{home as attrs, DEFAULT_SCOPE_ID, KIND_ACTION};
use faculties::storage::{discover_target, initialize_signer, load_signer, open_pile_strict};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
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
        return json!({"name": "Fake Bridge", "bridgeid": "ECB5FAFFFE000001",
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

fn fake_bridge() -> FakeBridge {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let thread = {
        let (seen, stop) = (seen.clone(), stop.clone());
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let mut stream = match listener.accept() {
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
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request_line = String::new();
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
                let response = serde_json::to_vec(&route(&method, &path, body.as_ref())).unwrap();
                seen.lock().unwrap().push(Seen { method, path, body });
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    response.len()
                )
                .unwrap();
                stream.write_all(&response).unwrap();
            }
        })
    };
    FakeBridge {
        base,
        seen,
        stop,
        thread: Some(thread),
    }
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
        .with_username(USER);
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
    let operations = Home::new(pile.clone(), Some(key.clone()));
    let binding = operations.bridge_set(&bridge.base, secret).unwrap();
    assert_eq!(binding.bridge_id, "ecb5fafffe000001");
    assert_eq!(binding.secret, secret);
    assert_eq!(
        operations.bridge_set(&bridge.base, secret).unwrap(),
        binding
    );
    assert!(
        bridge.puts().is_empty(),
        "binding proves the username with GETs only"
    );

    let adapter = home::mcp::Home::with_storage(faculties::storage::Storage::new(
        pile.clone(),
        Some(key.clone()),
    ));
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
        bridge_id: Some("ecb5fafffe000001".into()),
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
    assert_eq!(adapter.tools().len(), 7);
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
        .with_username("someOtherUser42");
    let error = format!("{:#}", wrong.rooms().unwrap_err());
    assert!(error.contains("unauthorized user (type 1)"), "{error}");
    assert!(!error.contains("someOtherUser42"), "{error}");

    let closed = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let unreachable = Home::new(pile.clone(), None)
        .with_address(address)
        .with_username(USER);
    let error = format!("{:#}", unreachable.change("2", &Request::Off).unwrap_err());
    assert!(!error.contains(USER), "{error}");
    assert!(error.contains("GET groups"), "{error}");
    assert!(bridge.puts().is_empty());
    let debug = format!("{unreachable:?}");
    assert!(!debug.contains(USER), "{debug}");
    assert!(!pile.exists());
}
