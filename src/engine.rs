//! The supervisor: keeps the stream connected whenever the speaker is on and free, and gets out of the way when
//! the speaker is switched off or another source takes over. It never wakes a KEF that is in standby.

use crate::capture::Capture;
use crate::config::{Config, SpeakerRef};
use crate::devices::{self, Volume, ALL_ROLES};
use crate::discovery::Speaker;
use crate::raop::{FirewallBlocked, Session};
use crate::{i18n, kef, tray, update};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::eMultimedia;

const POLL: Duration = Duration::from_secs(2);
/// After another source took over, it must stay silent this long before the stream returns.
const QUIET_BEFORE_RECONNECT: Duration = Duration::from_secs(5);
pub const LATENCIES_MS: [u32; 7] = [60, 80, 100, 120, 150, 200, 300];

pub enum Command {
    Connect,
    Disconnect,
    SetLatency(u32),
    SelectSpeaker(Speaker),
    SetCapture(String),
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum State {
    Starting,
    Connected,
    Off,
    Busy,
    Error,
    Paused,
}

pub struct Shared {
    pub config: Mutex<Config>,
    pub speakers: Arc<Mutex<Vec<Speaker>>>,
    pub status: Mutex<(State, String)>,
    pub balloon: Mutex<Option<String>>,
    pub session: Mutex<Option<Arc<Session>>>,
    pub capture: Mutex<Option<Arc<Capture>>>,
    pub hwnd: AtomicIsize,
    pub stopping: AtomicBool,
    /// A verified download waiting to be installed: (tag, path).
    pub update_ready: Mutex<Option<(String, PathBuf)>>,
    pub restart_for_update: AtomicBool,
}

pub fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    pub fn new(config: Config) -> Arc<Self> {
        Arc::new(Self {
            config: Mutex::new(config),
            speakers: Arc::default(),
            status: Mutex::new((State::Starting, i18n::starting().to_string())),
            balloon: Mutex::new(None),
            session: Mutex::new(None),
            capture: Mutex::new(None),
            hwnd: AtomicIsize::new(0),
            stopping: AtomicBool::new(false),
            update_ready: Mutex::new(None),
            restart_for_update: AtomicBool::new(false),
        })
    }

    /// Shows a notification without changing the status.
    pub fn notify(&self, text: String) {
        *lock(&self.balloon) = Some(text);
        tray::refresh(self);
    }

    /// Shows a new status in the tray; with `balloon`, also as a notification (only when the status changes).
    fn set_status(&self, state: State, text: String, balloon: bool) {
        {
            let mut status = lock(&self.status);
            if status.0 == state && status.1 == text {
                return;
            }
            *status = (state, text.clone());
        }
        if balloon {
            *lock(&self.balloon) = Some(text);
        }
        tray::refresh(self);
    }
}

pub fn run(shared: Arc<Shared>, commands: Receiver<Command>) {
    devices::com_init();
    let mut supervisor = Supervisor::new(shared);
    loop {
        match commands.recv_timeout(POLL) {
            Ok(Command::Quit) | Err(RecvTimeoutError::Disconnected) => break,
            Ok(command) => supervisor.handle(command),
            Err(RecvTimeoutError::Timeout) => {}
        }
        supervisor.tick();
    }
    supervisor.shutdown();
}

/// Windows volume keys -> speaker volume, and undoing the Windows volume in the capture.
pub fn volume_loop(shared: Arc<Shared>) {
    devices::com_init();
    let mut endpoint: Option<(String, Volume)> = None;
    while !shared.stopping.load(Relaxed) {
        thread::sleep(Duration::from_millis(50));
        let Some(capture) = lock(&shared.capture).clone() else { continue };
        if endpoint.as_ref().map_or(true, |(id, _)| *id != capture.device_id) {
            endpoint = Volume::of(&capture.device_id).ok().map(|v| (capture.device_id.clone(), v));
        }
        let Some((_, volume)) = &endpoint else { continue };
        let Ok((slider, db, muted)) = volume.read() else {
            endpoint = None;
            continue;
        };
        capture.set_gain(if muted { 1.0 } else { 10f32.powf(-db / 20.0) });
        let Some(session) = lock(&shared.session).clone() else { continue };
        let slider = (slider * 100.0).round() as u32;
        let wanted = if muted { 0 } else { slider.min(lock(&shared.config).max_volume) };
        if (muted || slider != session.volume()) && wanted != session.volume() {
            if let Err(e) = session.set_volume(wanted) {
                log::warn!("sending the volume: {e:#}");
                session.note_volume(wanted); // don't retry every 50 ms; the supervisor notices a dead session
            }
        }
    }
}

/// Looks for a new release a minute after start and then every six hours, and installs a downloaded one once
/// the audio has been quiet for a minute, so an update never cuts into a film or a song.
pub fn updater_loop(shared: Arc<Shared>) {
    const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
    const QUIET: Duration = Duration::from_secs(60);
    let mut next_check = Instant::now() + Duration::from_secs(60);
    while !shared.stopping.load(Relaxed) {
        thread::sleep(Duration::from_secs(5));
        if !lock(&shared.config).auto_update {
            continue;
        }
        if Instant::now() >= next_check {
            next_check = Instant::now() + CHECK_EVERY;
            update::clean_up();
            if lock(&shared.update_ready).is_none() {
                check_for_update(&shared);
            }
        }
        let quiet = lock(&shared.capture).as_ref().map_or(Duration::MAX, |c| c.quiet_for());
        if lock(&shared.update_ready).is_some() && quiet >= QUIET {
            log::info!("the audio has been quiet for a minute: installing the update");
            restart_for_update(&shared);
            return;
        }
    }
}

fn check_for_update(shared: &Shared) {
    match update::latest_tag() {
        Ok(tag) if update::is_newer(&tag) => match update::download_verified(&tag) {
            Ok(path) => {
                log::info!("downloaded and verified {tag}");
                *lock(&shared.update_ready) = Some((tag, path));
                tray::refresh(shared);
            }
            Err(e) => log::warn!("downloading {tag}: {e:#}"),
        },
        Ok(tag) => log::info!("up to date (latest release {tag})"),
        Err(e) => log::warn!("checking for updates: {e:#}"),
    }
}

/// Quits so that `main` can swap in the downloaded version and start it.
pub fn restart_for_update(shared: &Shared) {
    shared.restart_for_update.store(true, Relaxed);
    tray::close(shared);
}

struct Supervisor {
    shared: Arc<Shared>,
    previous_defaults: Vec<String>,
    is_kef: Option<bool>,
    unreachable: u32,
    taken_over: bool,
    force: bool,
    paused: bool,
    quiet_since: Option<Instant>,
    retry_at: Option<Instant>,
    seen_volume: Option<u32>,
    stream: Option<JoinHandle<()>>,
}

impl Supervisor {
    fn new(shared: Arc<Shared>) -> Self {
        Self {
            previous_defaults: ALL_ROLES.iter().filter_map(|&r| devices::default_output(r).ok()).collect(),
            shared,
            is_kef: None,
            unreachable: 0,
            taken_over: false,
            force: false,
            paused: false,
            quiet_since: None,
            retry_at: None,
            seen_volume: None,
            stream: None,
        }
    }

    fn status(&self, state: State, text: String) {
        self.shared.set_status(state, text, false);
    }

    fn session(&self) -> Option<Arc<Session>> {
        lock(&self.shared.session).clone()
    }

    fn update_config(&self, change: impl FnOnce(&mut Config)) {
        let mut config = lock(&self.shared.config);
        change(&mut config);
        config.save();
    }

    fn handle(&mut self, command: Command) {
        match command {
            Command::Connect => {
                self.paused = false;
                self.reconnect();
            }
            Command::Disconnect => {
                self.drop_session("disconnected from the menu");
                self.paused = true;
            }
            Command::SetLatency(ms) => {
                self.update_config(|c| c.latency_ms = ms);
                if self.session().is_some() {
                    self.reconnect();
                }
            }
            Command::SelectSpeaker(speaker) => {
                self.update_config(|c| {
                    c.speaker = Some(SpeakerRef { name: speaker.name.clone(), host: speaker.host.clone(), port: speaker.port })
                });
                self.is_kef = None;
                self.paused = false;
                self.reconnect();
            }
            Command::SetCapture(id) => {
                self.drop_session("capture output changed");
                if let Some(capture) = lock(&self.shared.capture).take() {
                    capture.stop();
                }
                self.update_config(|c| c.capture_device = Some(id));
                self.reconnect();
            }
            Command::Quit => {}
        }
    }

    fn reconnect(&mut self) {
        self.drop_session("reconnecting");
        self.force = true;
        self.taken_over = false;
        self.retry_at = None;
    }

    fn tick(&mut self) {
        if self.paused {
            return self.status(State::Paused, i18n::paused().into());
        }
        let config = lock(&self.shared.config).clone();
        let Some(speaker) = config.speaker.clone().or_else(|| self.auto_select()) else {
            return self.status(State::Error, i18n::no_speaker().into());
        };
        let Some(capture) = self.ensure_capture(&config) else { return };
        let is_kef = match self.is_kef {
            Some(known) => known,
            None => match kef::probe(&speaker.host, speaker.port) {
                Some(found) => *self.is_kef.insert(found),
                None => return self.unreachable_tick(&speaker),
            },
        };
        if is_kef {
            self.kef_tick(&speaker, &config, &capture);
        } else {
            self.generic_tick(&speaker, &config, &capture);
        }
    }

    /// With no speaker chosen yet and exactly one on the network, use that one.
    fn auto_select(&mut self) -> Option<SpeakerRef> {
        let speakers = lock(&self.shared.speakers).clone();
        let [only] = speakers.as_slice() else { return None };
        let chosen = SpeakerRef { name: only.name.clone(), host: only.host.clone(), port: only.port };
        log::info!("only one speaker on the network, using {}", chosen.name);
        self.update_config(|c| c.speaker = Some(chosen.clone()));
        Some(chosen)
    }

    fn ensure_capture(&mut self, config: &Config) -> Option<Arc<Capture>> {
        if let Some(capture) = lock(&self.shared.capture).clone() {
            return Some(capture);
        }
        let outputs = devices::outputs().unwrap_or_default();
        let chosen = config
            .capture_device
            .as_ref()
            .and_then(|id| outputs.iter().find(|o| &o.id == id))
            .or_else(|| {
                outputs.iter().find(|o| ["Digital Output", "S/PDIF", "SPDIF", "Optical"].iter().any(|k| o.name.contains(k)))
            });
        let Some(output) = chosen else {
            self.status(State::Error, i18n::no_capture().into());
            return None;
        };
        if config.capture_device.as_deref() != Some(output.id.as_str()) {
            let id = output.id.clone();
            self.update_config(|c| c.capture_device = Some(id));
        }
        log::info!("capturing from {}", output.name);
        devices::prepare_capture_output(&output.id);
        let capture = Capture::start(&output.id);
        *lock(&self.shared.capture) = Some(capture.clone());
        Some(capture)
    }

    fn kef_tick(&mut self, speaker: &SpeakerRef, config: &Config, capture: &Arc<Capture>) {
        let Some(status) = kef::status(&speaker.host) else { return self.unreachable_tick(speaker) };
        self.unreachable = 0;
        if let Some(session) = self.session() {
            if !status.on {
                self.drop_session("the speaker went to standby");
                self.taken_over = false;
            } else if session.closed_by_receiver() || status.source.as_deref().is_some_and(|s| s != "wifi") {
                self.drop_session("another source took over");
                self.taken_over = true;
                self.quiet_since = None;
            } else {
                self.follow_speaker_volume(&session, capture, status.volume);
                return self.ensure_default_output(capture);
            }
        }
        if !status.on {
            return self.status(State::Off, i18n::speaker_off(&speaker.name));
        }
        if status.playing && !self.force {
            self.quiet_since = None;
            return self.status(State::Busy, i18n::busy(&speaker.name, status.service.as_deref()));
        }
        let now = Instant::now();
        let quiet_since = *self.quiet_since.get_or_insert(now);
        if self.retry_at.is_some_and(|t| now < t)
            || (self.taken_over && !self.force && now - quiet_since < QUIET_BEFORE_RECONNECT)
        {
            return;
        }
        self.connect(speaker, config, capture, status.volume);
    }

    fn generic_tick(&mut self, speaker: &SpeakerRef, config: &Config, capture: &Arc<Capture>) {
        if let Some(session) = self.session() {
            if session.closed_by_receiver() {
                self.drop_session("the speaker closed the stream");
                self.taken_over = true;
                return self.status(State::Busy, i18n::taken_over(&speaker.name));
            }
            return self.ensure_default_output(capture);
        }
        if (self.taken_over && !self.force) || self.retry_at.is_some_and(|t| Instant::now() < t) {
            return;
        }
        self.connect(speaker, config, capture, None);
    }

    fn unreachable_tick(&mut self, speaker: &SpeakerRef) {
        self.unreachable += 1;
        if self.unreachable < 3 {
            return;
        }
        self.drop_session("the speaker can't be reached");
        self.is_kef = None;
        // Its address may have changed (DHCP): take the new one from discovery.
        let moved = lock(&self.shared.speakers).iter().find(|s| s.name == speaker.name && s.host != speaker.host).cloned();
        if let Some(found) = moved {
            log::info!("{} moved to {}", found.name, found.host);
            self.update_config(|c| {
                if let Some(s) = c.speaker.as_mut() {
                    s.host = found.host.clone();
                    s.port = found.port;
                }
            });
        }
        self.status(State::Off, i18n::unreachable(&speaker.name));
    }

    fn connect(&mut self, speaker: &SpeakerRef, config: &Config, capture: &Arc<Capture>, speaker_volume: Option<u32>) {
        self.force = false;
        let Ok(host) = speaker.host.parse::<IpAddr>() else {
            return self.status(State::Error, i18n::connect_failed(&speaker.name));
        };
        // Start from the speaker's own volume when it tells us (KEF), else from the Windows slider.
        let slider = Volume::of(&capture.device_id).ok();
        let (windows_volume, muted) =
            slider.as_ref().and_then(|v| v.read().ok()).map_or((30, false), |(s, _, m)| ((s * 100.0).round() as u32, m));
        let volume = speaker_volume.unwrap_or(windows_volume).min(config.max_volume);
        if let Some(v) = &slider {
            let _ = v.set_slider(volume as f32 / 100.0);
        }
        capture.reset();
        self.status(State::Starting, i18n::connecting(&speaker.name));
        match Session::connect(host, speaker.port, config.latency_ms, if muted { 0 } else { volume }) {
            Ok(session) => {
                session.note_volume(volume);
                let (s, c) = (session.clone(), capture.clone());
                self.stream = thread::Builder::new()
                    .name("raop-stream".into())
                    .spawn(move || {
                        devices::pro_audio_thread();
                        s.stream(|out| c.next_packet(out));
                    })
                    .ok();
                *lock(&self.shared.session) = Some(session);
                self.taken_over = false;
                self.retry_at = None;
                self.seen_volume = Some(volume);
                if let Err(e) = devices::set_default_output(&capture.device_id, &ALL_ROLES) {
                    log::warn!("making the capture output the default: {e:#}");
                }
                log::info!("connected to {} at {host}, latency {} ms, volume {volume}", speaker.name, config.latency_ms);
                self.shared.set_status(State::Connected, i18n::connected(&speaker.name, config.latency_ms), true);
            }
            Err(e) => {
                let firewall = e.is::<FirewallBlocked>();
                log::warn!("connecting to {} failed: {e:#}", speaker.name);
                self.retry_at = Some(Instant::now() + Duration::from_secs(if firewall { 30 } else { 10 }));
                if firewall {
                    self.shared.set_status(State::Error, i18n::firewall().into(), true);
                } else {
                    self.status(State::Error, i18n::connect_failed(&speaker.name));
                }
            }
        }
    }

    /// Volume changed with the speaker's remote or app: move the Windows slider along, once it is stable.
    fn follow_speaker_volume(&mut self, session: &Session, capture: &Capture, speaker_volume: Option<u32>) {
        let Some(v) = speaker_volume else { return };
        if v != session.volume() && self.seen_volume == Some(v) {
            if let Ok(slider) = Volume::of(&capture.device_id) {
                if slider.read().is_ok_and(|(_, _, muted)| !muted) {
                    session.note_volume(v);
                    let _ = slider.set_slider(v as f32 / 100.0);
                    log::info!("volume changed to {v} on the speaker; the Windows slider follows");
                }
            }
        }
        self.seen_volume = Some(v);
    }

    /// If Windows fell back to a useless output (a Steam virtual device, say), point it back at the capture.
    fn ensure_default_output(&self, capture: &Capture) {
        let Ok(current) = devices::default_output(eMultimedia) else { return };
        if current == capture.device_id {
            return;
        }
        let outputs = devices::outputs().unwrap_or_default();
        let name = outputs.iter().find(|o| o.id == current).map_or("", |o| o.name.as_str());
        if (name.is_empty() || name.contains("Steam Streaming"))
            && devices::set_default_output(&capture.device_id, &ALL_ROLES).is_ok()
        {
            log::info!("default output was {name:?}; set back to the capture output");
        }
    }

    fn drop_session(&mut self, reason: &str) {
        let Some(session) = lock(&self.shared.session).take() else { return };
        session.stop();
        if let Some(stream) = self.stream.take() {
            let _ = stream.join();
        }
        let stats = lock(&self.shared.capture).as_ref().map(|c| c.stats());
        log::info!("disconnected ({reason}); capture {stats:?}, retransmitted {}", session.resent.load(Relaxed));
    }

    fn shutdown(&mut self) {
        self.drop_session("quitting");
        if let Some(capture) = lock(&self.shared.capture).take() {
            capture.stop();
        }
        let outputs = devices::outputs().unwrap_or_default();
        let capture_id = lock(&self.shared.config).capture_device.clone();
        for (role, previous) in ALL_ROLES.iter().zip(&self.previous_defaults) {
            let usable = outputs.iter().any(|o| &o.id == previous && !o.name.contains("Steam Streaming"));
            if usable && Some(previous) != capture_id.as_ref() {
                let _ = devices::set_default_output(previous, &[*role]);
            }
        }
    }
}
