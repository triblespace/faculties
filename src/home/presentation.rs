//! Present already fetched bridge answers without asking the bridge again.
use super::bridge::{bri_to_percent, BridgeConfig, Discovered, Group};
use super::{ActionReport, Binding, BindingStatus, LightsView, RoomsView, ScenesView};
use crate::out::Out;
use anyhow::Result;

fn state(group: &Group) -> String {
    let bri = group
        .bri
        .filter(|_| group.any_on)
        .map(|bri| format!(", {}%", bri_to_percent(bri)))
        .unwrap_or_default();
    let on = if group.all_on {
        "on"
    } else if group.any_on {
        "partly on"
    } else {
        "off"
    };
    format!("{on}{bri}")
}

pub fn rooms(view: &RoomsView, out: &mut Out<'_>) -> Result<()> {
    if view.rooms.is_empty() {
        return out.line("no rooms or zones on this bridge");
    }
    for room in &view.rooms {
        out.line(format!(
            "[{}] {} ({}): {}, {} light{}",
            room.id,
            room.name,
            room.kind.to_lowercase(),
            state(room),
            room.lights.len(),
            if room.lights.len() == 1 { "" } else { "s" },
        ))?;
    }
    Ok(())
}

pub fn lights(view: &LightsView, out: &mut Out<'_>) -> Result<()> {
    if let Some(room) = &view.room {
        out.line(format!(
            "{} ({}): {}",
            room.name,
            room.kind.to_lowercase(),
            state(room)
        ))?;
    }
    if view.lights.is_empty() {
        return out.line("no lights");
    }
    for (light, place) in &view.lights {
        let state = if light.on {
            light.bri.map_or("on".to_owned(), |bri| {
                format!("on, {}%", bri_to_percent(bri))
            })
        } else {
            "off".to_owned()
        };
        let unreachable = if light.reachable == Some(false) {
            ", unreachable"
        } else {
            ""
        };
        let place = match (&view.room, place) {
            (None, Some(place)) => format!(" in {place}"),
            _ => String::new(),
        };
        out.line(format!(
            "[{}] {}{place}: {state}{unreachable}",
            light.id, light.name
        ))?;
    }
    Ok(())
}

pub fn scenes(view: &ScenesView, out: &mut Out<'_>) -> Result<()> {
    if view.scenes.is_empty() {
        return out.line(match &view.room {
            Some(room) => format!("{} has no scenes", room.name),
            None => "no scenes".to_owned(),
        });
    }
    for (scene, owner) in &view.scenes {
        let owner = match (&view.room, owner) {
            (None, Some(owner)) => format!(" ({owner})"),
            (None, None) => format!(" ({} lights)", scene.lights.len()),
            _ => String::new(),
        };
        out.line(format!("[{}] {}{owner}", scene.id, scene.name))?;
    }
    Ok(())
}

pub fn action(report: &ActionReport, out: &mut Out<'_>) -> Result<()> {
    let what = match &report.scene {
        Some(scene) => format!("scene {:?}", scene.name),
        None => report.body.to_string(),
    };
    out.line(format!(
        "{} ({}): sent {what} to {}",
        report.group.name, report.group.id, report.target
    ))?;
    for error in &report.errors {
        out.line(format!("bridge error: {error}"))?;
    }
    if report.errors.is_empty() {
        out.line("bridge accepted every change")?;
    }
    Ok(())
}

pub fn discovered(bridges: &[Discovered], out: &mut Out<'_>) -> Result<()> {
    if bridges.is_empty() {
        return out.line("no Hue bridge answered");
    }
    for bridge in bridges {
        let mut line = format!(
            "{} at {}",
            bridge.bridge_id.as_deref().unwrap_or("(unknown id)"),
            bridge.address
        );
        if let Some(port) = bridge.port {
            line.push_str(&format!(":{port}"));
        }
        if let Some(model) = &bridge.model {
            line.push_str(&format!(" model {model}"));
        }
        if let Some(instance) = &bridge.instance {
            line.push_str(&format!(" ({instance})"));
        }
        line.push_str(&format!(" via {}", bridge.via));
        out.line(line)?;
    }
    Ok(())
}

pub fn binding(binding: &Binding, out: &mut Out<'_>) -> Result<()> {
    out.line(format!(
        "[{:x}] bridge {}, username in Secrets version {:x}",
        binding.revision, binding.bridge_id, binding.secret
    ))?;
    out.line(match binding.pin {
        Some(pin) => format!("certificate SHA-256 {pin}"),
        None => "no certificate pin: bound before Home pinned certificates; rebind with `home bridge set`".to_owned(),
    })
}

pub fn probe(config: &BridgeConfig, out: &mut Out<'_>) -> Result<()> {
    let mut line = format!("bridge {}", config.bridge_id);
    if let Some(name) = &config.name {
        line.push_str(&format!(" {name:?}"));
    }
    if let Some(model) = &config.model {
        line.push_str(&format!(" model {model}"));
    }
    if let Some(version) = &config.api_version {
        line.push_str(&format!(" api {version}"));
    }
    out.line(line)?;
    out.line(format!(
        "certificate SHA-256 {} (not verified)",
        config.certificate
    ))
}

pub fn status(status: &BindingStatus, out: &mut Out<'_>) -> Result<()> {
    match status.selected() {
        Ok(selected) => binding(&selected, out),
        Err(error) => {
            out.line(format!("{error}"))?;
            for head in &status.heads {
                out.line(format!("head [{head:x}]"))?;
            }
            for id in &status.bridge_ids {
                out.line(format!("bridge {id}"))?;
            }
            for secret in &status.secrets {
                out.line(format!("username secret {secret:x}"))?;
            }
            for pin in &status.pins {
                out.line(format!("certificate SHA-256 {pin}"))?;
            }
            Ok(())
        }
    }
}
