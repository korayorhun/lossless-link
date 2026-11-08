//! Finds AirPlay 1 (RAOP) receivers on the local network that accept unencrypted ALAC, via mDNS.

use mdns_sd::{ServiceDaemon, ServiceEvent};
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug, PartialEq)]
pub struct Speaker {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub model: String,
}

/// "841715090362@kef._raop._tcp.local." -> "kef"
fn speaker_name(fullname: &str) -> String {
    let instance = fullname.split("._raop").next().unwrap_or_default();
    instance.split_once('@').map_or(instance, |(_, name)| name).to_string()
}

pub fn start(list: Arc<Mutex<Vec<Speaker>>>) {
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => return log::warn!("mDNS is unavailable: {e}"),
    };
    let events = match daemon.browse("_raop._tcp.local.") {
        Ok(r) => r,
        Err(e) => return log::warn!("mDNS browse failed: {e}"),
    };
    let spawned = std::thread::Builder::new().name("discovery".into()).spawn(move || {
        let _daemon = daemon;
        while let Ok(event) = events.recv() {
            let mut list = list.lock().unwrap_or_else(|e| e.into_inner());
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    let props = info.get_properties();
                    let offers = |key: &str, want: &str| {
                        props.get_property_val_str(key).unwrap_or(want).split(',').any(|v| v.trim() == want)
                    };
                    // et=0: unencrypted audio accepted; cn=1: ALAC
                    if !offers("et", "0") || !offers("cn", "1") {
                        continue;
                    }
                    let Some(ip) = info.get_addresses().iter().find(|a| a.is_ipv4()).copied() else { continue };
                    let speaker = Speaker {
                        name: speaker_name(info.get_fullname()),
                        host: ip.to_string(),
                        port: info.get_port(),
                        model: props.get_property_val_str("am").unwrap_or_default().to_string(),
                    };
                    match list.iter_mut().find(|s| s.name == speaker.name) {
                        Some(known) => *known = speaker,
                        None => {
                            log::info!("found {} ({}) at {}:{}", speaker.name, speaker.model, speaker.host, speaker.port);
                            list.push(speaker);
                        }
                    }
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    let name = speaker_name(&fullname);
                    list.retain(|s| s.name != name);
                }
                _ => {}
            }
        }
    });
    if let Err(e) = spawned {
        log::warn!("starting discovery: {e}");
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn names_come_after_the_at_sign() {
        assert_eq!(super::speaker_name("841715090362@kef._raop._tcp.local."), "kef");
        assert_eq!(super::speaker_name("Kitchen._raop._tcp.local."), "Kitchen");
    }
}
