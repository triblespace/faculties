//! Text projection of one observation. Pure: JSON in, text out.

use serde_json::Value;

/// Entities farther than this many metres are left out of the projection.
pub const NEAR_METRES: f64 = 60.0;
/// At most this many entities are listed, nearest first.
pub const MAX_ENTITIES: usize = 12;
/// Parts with a horizontal side this long are terrain or walls, not things.
pub const SCENERY_METRES: f64 = 40.0;
/// At least this many entities sharing a base name are listed as one group.
pub const GROUP_AT: usize = 3;

pub fn vec3(value: &Value) -> Option<[f64; 3]> {
    match value {
        Value::Array(items) if items.len() == 3 => {
            Some([items[0].as_f64()?, items[1].as_f64()?, items[2].as_f64()?])
        }
        Value::Object(map) => Some([
            map.get("x")?.as_f64()?,
            map.get("y")?.as_f64()?,
            map.get("z")?.as_f64()?,
        ]),
        _ => None,
    }
}

fn fmt3(p: [f64; 3]) -> String {
    format!("({:.1}, {:.1}, {:.1})", p[0], p[1], p[2])
}

fn short(value: &Value) -> String {
    match value {
        Value::String(s) => s.chars().take(60).collect(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
        other => {
            let s = other.to_string();
            if s.len() > 60 {
                format!(
                    "{}…",
                    &s[..s.char_indices().nth(59).map_or(s.len(), |(i, _)| i)]
                )
            } else {
                s
            }
        }
    }
}

/// The player's own position, if the observation carries one.
pub fn own_position(obs: &Value) -> Option<[f64; 3]> {
    vec3(obs.get("player")?.get("position")?)
}

/// Render everything except the tick line. Two observations that render the
/// same body are, for a reader of this text, the same situation.
pub fn body(obs: &Value) -> String {
    let mut out = Vec::new();
    let me = own_position(obs);
    let status = obs
        .get("game_status")
        .map(short)
        .unwrap_or_else(|| "unknown".into());
    let mut line = format!("status {status}");
    if let Some(player) = obs.get("player") {
        if let Some(hp) = player.get("health") {
            line.push_str(&format!("  health {}", short(hp)));
        }
        if let Some(p) = me {
            line.push_str(&format!("  at {}", fmt3(p)));
        }
        if let Some(Value::Object(attrs)) = player.get("attributes") {
            let fx = attrs.get("ViewForwardX").and_then(Value::as_f64);
            let fz = attrs.get("ViewForwardZ").and_then(Value::as_f64);
            if let (Some(fx), Some(fz)) = (fx, fz) {
                line.push_str(&format!(
                    "  facing (dx {:+.1}, dz {:+.1})",
                    fx + 0.0,
                    fz + 0.0
                ));
            }
            let kv = shown_attributes(attrs, usize::MAX);
            if !kv.is_empty() {
                line.push_str(&format!("  [{}]", kv.join(" ")));
            }
        }
    }
    out.push(line);

    let mut near: Vec<(f64, String)> = Vec::new();
    if let Some(Value::Array(players)) = obs.get("other_players") {
        for p in players {
            let name = p.get("name").map(short).unwrap_or_else(|| "someone".into());
            let pos = p.get("position").and_then(vec3);
            let dist = p
                .get("distance")
                .and_then(Value::as_f64)
                .or_else(|| Some(distance(me?, pos?)));
            let d = dist.unwrap_or(f64::INFINITY);
            near.push((d, format!("player {name}{}", describe_place(me, pos, dist))));
        }
    }
    if let Some(Value::Array(entities)) = obs.get("world").and_then(|w| w.get("entities")) {
        let mut groups: std::collections::BTreeMap<String, Vec<(f64, String)>> = Default::default();
        for e in entities {
            let scenery = e
                .get("size")
                .and_then(vec3)
                .is_some_and(|size| size[0] >= SCENERY_METRES || size[2] >= SCENERY_METRES);
            if scenery {
                continue;
            }
            let pos = e.get("position").and_then(vec3);
            let dist = me.zip(pos).map(|(a, b)| distance(a, b));
            if dist.is_some_and(|d| d > NEAR_METRES) {
                continue;
            }
            let name = e.get("name").map(short).unwrap_or_else(|| "?".into());
            // Bodies are players, listed above; a root part is not a thing.
            if name == "HumanoidRootPart" {
                continue;
            }
            let id = e.get("id").map(short).unwrap_or_default();
            let mut s = format!("{name}#{id}{}", describe_place(me, pos, dist));
            if e.get("anchored").and_then(Value::as_bool) == Some(false) {
                s.push_str(" loose");
            }
            if let Some(Value::Object(attrs)) = e.get("attributes") {
                let kv = shown_attributes(attrs, 3);
                if !kv.is_empty() {
                    s.push_str(&format!(" [{}]", kv.join(" ")));
                }
            }
            groups
                .entry(base_name(&name).to_owned())
                .or_default()
                .push((dist.unwrap_or(f64::INFINITY), s));
        }
        for (base, mut members) in groups {
            members.sort_by(|a, b| a.0.total_cmp(&b.0));
            if members.len() >= GROUP_AT {
                let (d, nearest) = &members[0];
                near.push((*d, format!("{} x {base}, nearest {nearest}", members.len())));
            } else {
                near.extend(members);
            }
        }
    }
    near.sort_by(|a, b| a.0.total_cmp(&b.0));
    let total = near.len();
    for (_, s) in near.iter().take(MAX_ENTITIES) {
        out.push(format!("  {s}"));
    }
    if total > MAX_ENTITIES {
        out.push(format!(
            "  … {} more within {NEAR_METRES:.0} m",
            total - MAX_ENTITIES
        ));
    }
    if total == 0 {
        out.push("  nothing within sight".into());
    }

    let mut events = Vec::new();
    for key in ["events", "recent_events"] {
        if let Some(Value::Array(list)) = obs.get(key) {
            for ev in list.iter().rev().take(5).rev() {
                let name = ev.get("name").map(short).unwrap_or_else(|| short(ev));
                let tick = ev
                    .get("tick")
                    .map(|t| format!(" t{}", short(t)))
                    .unwrap_or_default();
                let payload = ev
                    .get("payload")
                    .filter(|p| !p.is_null())
                    .map(|p| format!(" {}", short(p)))
                    .unwrap_or_default();
                events.push(format!("  {name}{tick}{payload}"));
            }
        }
    }
    if !events.is_empty() {
        out.push("events:".into());
        out.extend(events);
    }
    out.join("\n")
}

/// One complete observation as text, tick first.
pub fn observation(obs: &Value) -> String {
    let tick = obs.get("tick").map(short).unwrap_or_else(|| "?".into());
    format!("tick {tick}\n{}\n", body(obs))
}

/// `Block_12` and `Block_3` share the base `Block`; other names are their own.
pub fn base_name(name: &str) -> &str {
    match name.rsplit_once('_') {
        Some((base, n))
            if !base.is_empty() && !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()) =>
        {
            base
        }
        _ => name,
    }
}

/// Attributes worth reading: rendering and camera plumbing is left out, and a
/// true `IsX` flag reads as just `x`.
fn shown_attributes(attrs: &serde_json::Map<String, Value>, limit: usize) -> Vec<String> {
    let mut kv: Vec<String> = attrs
        .iter()
        .filter(|(k, _)| !["Render", "Model", "View"].iter().any(|p| k.starts_with(p)))
        .filter_map(|(k, v)| match (k.strip_prefix("Is"), v) {
            (Some(flag), Value::Bool(true)) if !flag.is_empty() => Some(flag.to_lowercase()),
            (Some(flag), Value::Bool(false)) if !flag.is_empty() => None,
            _ => Some(format!("{k}={}", short(v))),
        })
        .collect();
    kv.sort();
    kv.dedup();
    kv.truncate(limit);
    kv
}

pub fn distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

/// Horizontal distance only: arrival should not depend on the body's height.
pub fn ground_distance(a: [f64; 3], b: [f64; 3]) -> f64 {
    ((a[0] - b[0]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn describe_place(me: Option<[f64; 3]>, pos: Option<[f64; 3]>, dist: Option<f64>) -> String {
    match (me, pos, dist) {
        (Some(m), Some(p), Some(d)) => {
            format!(" {d:.1} m (dx {:+.1}, dz {:+.1})", p[0] - m[0], p[2] - m[2])
        }
        (_, Some(p), _) => format!(" at {}", fmt3(p)),
        (_, _, Some(d)) => format!(" {d:.1} m"),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "tick": 120,
            "game_status": "playing",
            "player": {"id": "p1", "position": [0.0, 3.0, 0.0], "health": 100, "attributes": {}},
            "other_players": [{"name": "ana", "position": [9.0, 3.0, 0.0], "distance": 9.0}],
            "world": {"entities": [
                {"id": 1, "name": "Baseplate", "position": [0.0, 0.0, 0.0], "size": [512.0, 1.0, 512.0]},
                {"id": 81, "name": "Crate", "position": [2.0, 1.0, -1.0], "size": [1.0, 1.0, 1.0], "anchored": false},
                {"id": 99, "name": "FarTower", "position": [500.0, 1.0, 0.0], "size": [4.0, 20.0, 4.0]}
            ]},
            "events": [], "recent_events": [{"tick": 110, "name": "DoorOpened", "payload": null}]
        })
    }

    #[test]
    fn projection_is_plain_text_nearest_first_without_scenery() {
        let text = observation(&sample());
        assert!(text.starts_with("tick 120\nstatus playing  health 100  at (0.0, 3.0, 0.0)"));
        let crate_at = text.find("Crate#81").unwrap();
        let ana_at = text.find("player ana").unwrap();
        assert!(crate_at < ana_at, "{text}");
        assert!(text.contains("loose"));
        assert!(!text.contains("Baseplate"));
        assert!(!text.contains("FarTower"));
        assert!(text.contains("DoorOpened t110"));
        assert!(!text.trim_start().starts_with('{'));
    }

    #[test]
    fn many_alike_become_one_line_and_the_own_body_is_not_a_thing() {
        let mut obs = sample();
        let ents = obs["world"]["entities"].as_array_mut().unwrap();
        for i in 1..=5 {
            ents.push(json!({"id": 100 + i, "name": format!("Block_{i}"), "position": [i as f64, 0.4, 0.0],
                "size": [0.8, 0.8, 0.8], "anchored": false,
                "attributes": {"IsCarryable": true, "RenderRole": "PickupObject", "ItemColorName": "Slate"}}));
        }
        ents.push(json!({"id": 45, "name": "HumanoidRootPart", "position": [1.5, 4.7, 0.0], "size": [2.0, 5.0, 2.0]}));
        obs["player"]["attributes"] =
            json!({"ViewForwardX": -0.0, "ViewForwardZ": -1.0, "LastActionResult": "Ready"});
        let text = observation(&obs);
        assert!(text.contains("5 x Block, nearest Block_1#101"), "{text}");
        assert!(text.contains("[ItemColorName=Slate carryable]"), "{text}");
        assert!(!text.contains("RenderRole"), "{text}");
        assert!(!text.contains("HumanoidRootPart"), "{text}");
        assert!(
            text.contains("facing (dx +0.0, dz -1.0)  [LastActionResult=Ready]"),
            "{text}"
        );
    }

    #[test]
    fn body_ignores_the_tick() {
        let a = sample();
        let mut b = sample();
        b["tick"] = json!(121);
        assert_eq!(body(&a), body(&b));
        assert_ne!(observation(&a), observation(&b));
    }

    #[test]
    fn unknown_shapes_degrade_instead_of_failing() {
        let text = observation(&json!({"unexpected": true}));
        assert!(text.contains("status unknown"));
        assert!(text.contains("nothing within sight"));
    }
}
