//! Extras for KEF speakers on the W2 / StreamSDK platform. Their local HTTP API says whether the speaker is on,
//! which source it uses and what it plays, and shows volume changes made on the speaker. Read-only: KEF only
//! accepts setting changes from its own app.

use serde_json::Value;
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::time::Duration;

fn get(host: &str, path: &str) -> Option<Value> {
    let url = format!("http://{host}/api/getData?path={path}&roles=value");
    let reply: Value = ureq::get(&url).timeout(Duration::from_secs(3)).call().ok()?.into_json().ok()?;
    reply.get(0).cloned()
}

/// The payload of a typed value such as {"type":"i32_","i32_":24}.
fn value(host: &str, path: &str) -> Option<Value> {
    let v = get(host, path)?;
    v.as_object()?.iter().find(|(k, _)| k.as_str() != "type").map(|(_, v)| v.clone())
}

/// Some(true) for a KEF, Some(false) for another speaker, None when the speaker can't be reached.
pub fn probe(host: &str, port: u16) -> Option<bool> {
    let ip: IpAddr = host.parse().ok()?;
    TcpStream::connect_timeout(&SocketAddr::new(ip, port), Duration::from_secs(2)).ok()?;
    Some(value(host, "settings:/system/manufacturer").and_then(|v| v.as_str().map(|s| s == "KEF")).unwrap_or(false))
}

pub struct Status {
    pub on: bool,
    pub source: Option<String>,
    pub playing: bool,
    pub service: Option<String>,
    pub volume: Option<u32>,
}

/// None when the speaker can't be reached.
pub fn status(host: &str) -> Option<Status> {
    let power = value(host, "settings:/kef/host/speakerStatus")?;
    let player = get(host, "player:player/data");
    Some(Status {
        on: power.as_str() == Some("powerOn"),
        source: value(host, "settings:/kef/play/physicalSource").and_then(|v| v.as_str().map(String::from)),
        playing: player.as_ref().and_then(|p| p.get("state")).and_then(Value::as_str) == Some("playing"),
        service: player
            .as_ref()
            .and_then(|p| p.pointer("/mediaRoles/mediaData/metaData/serviceID"))
            .and_then(Value::as_str)
            .map(String::from),
        volume: value(host, "player:volume").and_then(|v| v.as_u64()).map(|v| v as u32),
    })
}
