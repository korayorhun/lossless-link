//! Minimal AirPlay 1 (RAOP) sender: uncompressed ALAC over RTP/UDP with a latency the sender chooses.
//!
//! Receivers built on Apple's AirPlay SDK (the KEF LS LUXE among them) answer ANNOUNCE with 403 until
//! POST /auth-setup is sent. The KEF only plays ALAC (it accepts L16 PCM but stays silent) and honours
//! latencies down to at least 80 ms. During SETUP it sends timing requests from a port that changes every
//! session and waits for our answers, so the program needs an inbound firewall rule.

use crate::alac::alac_frame;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const RATE: u32 = 44_100;
pub const FRAMES: usize = 352; // frames per RTP packet
const NTP_EPOCH: f64 = 2_208_988_800.0;
const HISTORY: usize = 1024; // packets kept for retransmission, about 8 s
/// auth-setup body: 0x01 asks for an unencrypted session; the Curve25519 public key is arbitrary.
const AUTH_SETUP: [u8; 33] = [
    0x01, 0x59, 0x02, 0xed, 0xe9, 0x0d, 0x4e, 0xf2, 0xbd, 0x4c, 0xb6, 0x8a, 0x63, 0x30, 0x03, 0x82, 0x07, 0xa9,
    0x4d, 0xbd, 0x50, 0xd8, 0xaa, 0x46, 0x5b, 0x5d, 0x8c, 0x01, 0x2a, 0x0c, 0x7e, 0x1d, 0x4e,
];

/// SETUP timed out without a single timing request reaching us: almost always the Windows firewall.
#[derive(Debug)]
pub struct FirewallBlocked;

impl fmt::Display for FirewallBlocked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("no timing requests reached us during SETUP (blocked by the firewall?)")
    }
}

impl std::error::Error for FirewallBlocked {}

/// Wall clock with Instant's resolution, in seconds since the Unix epoch.
fn now() -> f64 {
    static BASE: OnceLock<(Instant, f64)> = OnceLock::new();
    let (instant, wall) = BASE.get_or_init(|| {
        (Instant::now(), SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64())
    });
    wall + instant.elapsed().as_secs_f64()
}

fn ntp(t: f64) -> [u8; 8] {
    let t = t + NTP_EPOCH;
    let secs = t.floor();
    let frac = ((t - secs) * 4_294_967_296.0) as u64 as u32;
    let mut b = [0u8; 8];
    b[..4].copy_from_slice(&(secs as u64 as u32).to_be_bytes());
    b[4..].copy_from_slice(&frac.to_be_bytes());
    b
}

/// AirPlay volume for a speaker volume 0–100: -30..0 dB linearly (that is how the KEF maps it), -144 = mute.
pub fn volume_db(volume: u32) -> f32 {
    if volume == 0 {
        -144.0
    } else {
        -30.0 + 0.3 * volume.min(100) as f32
    }
}

struct Rtsp {
    stream: TcpStream,
    cseq: u32,
    session: Option<String>,
    client_id: String,
    active_remote: String,
}

struct Response {
    status: u16,
    headers: HashMap<String, String>,
}

impl Rtsp {
    fn request(&mut self, method: &str, uri: &str, headers: &[(&str, &str)], body: &[u8], protocol: &str)
        -> Result<Response> {
        self.cseq += 1;
        let mut head = format!(
            "{method} {uri} {protocol}\r\nCSeq: {}\r\nUser-Agent: AirPlay/550.10\r\nClient-Instance: {id}\r\n\
             DACP-ID: {id}\r\nActive-Remote: {}\r\n",
            self.cseq, self.active_remote, id = self.client_id
        );
        if let Some(session) = &self.session {
            head.push_str(&format!("Session: {session}\r\n"));
        }
        for (k, v) in headers {
            head.push_str(&format!("{k}: {v}\r\n"));
        }
        if !body.is_empty() {
            head.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        head.push_str("\r\n");
        let mut out = head.into_bytes();
        out.extend_from_slice(body);
        self.stream.write_all(&out)?;

        let mut buf = Vec::new();
        let mut chunk = [0u8; 4096];
        let end = loop {
            if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break p;
            }
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                bail!("speaker closed the connection during {method}");
            }
            buf.extend_from_slice(&chunk[..n]);
        };
        let text = String::from_utf8_lossy(&buf[..end]).into_owned();
        let mut lines = text.split("\r\n");
        let status_line = lines.next().unwrap_or_default();
        let status = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        let headers: HashMap<String, String> = lines
            .filter_map(|l| l.split_once(':'))
            .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
            .collect();
        let length: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
        let mut have = buf.len() - end - 4;
        while have < length {
            let n = self.stream.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            have += n;
        }
        log::info!("{method} {uri}: {status_line}");
        Ok(Response { status, headers })
    }

    fn expect_ok(&mut self, method: &str, uri: &str, headers: &[(&str, &str)], body: &[u8]) -> Result<Response> {
        let response = self.request(method, uri, headers, body, "RTSP/1.0")?;
        if response.status != 200 {
            bail!("{method} failed with status {}", response.status);
        }
        Ok(response)
    }
}

pub struct Session {
    rtsp: Mutex<Rtsp>,
    local: IpAddr,
    uri: String,
    audio: UdpSocket,
    ctrl: UdpSocket,
    timing: UdpSocket,
    server_audio: OnceLock<SocketAddr>,
    server_ctrl: OnceLock<SocketAddr>,
    latency: u32,
    seq0: u16,
    rtp0: u32,
    ssrc: u32,
    running: AtomicBool,
    volume: AtomicU32,
    pub resent: AtomicU64,
    timing_seen: AtomicBool,
    history: Mutex<Vec<Vec<u8>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Session {
    /// Opens an RTSP session and starts answering timing requests; call `stream` on a thread of its own.
    pub fn connect(host: IpAddr, port: u16, latency_ms: u32, volume: u32) -> Result<Arc<Self>> {
        let stream = TcpStream::connect_timeout(&SocketAddr::new(host, port), Duration::from_secs(5))
            .with_context(|| format!("connecting to {host}:{port}"))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_nodelay(true)?;
        let local = stream.local_addr()?.ip();
        let bind = || -> Result<UdpSocket> {
            let s = UdpSocket::bind(SocketAddr::new(local, 0))?;
            s.set_read_timeout(Some(Duration::from_millis(250)))?;
            Ok(s)
        };
        let sid = fastrand::u32(1_000_000_000..4_000_000_000);
        let session = Arc::new(Self {
            rtsp: Mutex::new(Rtsp {
                stream,
                cseq: 0,
                session: None,
                client_id: format!("{:016X}", fastrand::u64(..)),
                active_remote: fastrand::u32(..i32::MAX as u32).to_string(),
            }),
            local,
            uri: format!("rtsp://{local}/{sid}"),
            audio: bind()?,
            ctrl: bind()?,
            timing: bind()?,
            server_audio: OnceLock::new(),
            server_ctrl: OnceLock::new(),
            latency: latency_ms * RATE / 1000,
            seq0: fastrand::u16(..),
            rtp0: fastrand::u32(..),
            ssrc: fastrand::u32(..),
            running: AtomicBool::new(true),
            volume: AtomicU32::new(volume),
            resent: AtomicU64::new(0),
            timing_seen: AtomicBool::new(false),
            history: Mutex::new(vec![Vec::new(); HISTORY]),
        });
        let s = session.clone();
        thread::Builder::new().name("raop-timing".into()).spawn(move || s.timing_loop())?;
        if let Err(e) = session.handshake(host, sid) {
            session.running.store(false, Relaxed);
            return Err(e);
        }
        let s = session.clone();
        thread::Builder::new().name("raop-control".into()).spawn(move || s.control_loop())?;
        Ok(session)
    }

    fn handshake(&self, host: IpAddr, sid: u32) -> Result<()> {
        let mut rtsp = lock(&self.rtsp);
        rtsp.expect_ok("OPTIONS", "*", &[], &[])?;
        rtsp.request("POST", "/auth-setup", &[("Content-Type", "application/octet-stream")], &AUTH_SETUP, "HTTP/1.1")?;
        let sdp = format!(
            "v=0\r\no=iTunes {sid} 0 IN IP4 {}\r\ns=iTunes\r\nc=IN IP4 {host}\r\nt=0 0\r\nm=audio 0 RTP/AVP 96\r\n\
             a=rtpmap:96 AppleLossless\r\na=fmtp:96 {FRAMES} 0 16 40 10 14 2 255 0 0 {RATE}\r\n",
            self.local
        );
        rtsp.expect_ok("ANNOUNCE", &self.uri, &[("Content-Type", "application/sdp")], sdp.as_bytes())?;
        let transport = format!(
            "RTP/AVP/UDP;unicast;interleaved=0-1;mode=record;control_port={};timing_port={}",
            self.ctrl.local_addr()?.port(),
            self.timing.local_addr()?.port()
        );
        let setup = match rtsp.expect_ok("SETUP", &self.uri, &[("Transport", &transport)], &[]) {
            Ok(r) => r,
            Err(e) if !self.timing_seen.load(Relaxed) => {
                log::warn!("SETUP: {e:#}");
                return Err(anyhow::Error::new(FirewallBlocked));
            }
            Err(e) => return Err(e),
        };
        let port = |key: &str| -> Result<u16> {
            setup
                .headers
                .get("transport")
                .and_then(|t| t.split(';').find_map(|p| p.strip_prefix(key)?.strip_prefix('=')?.parse().ok()))
                .ok_or_else(|| anyhow!("SETUP reply has no {key}"))
        };
        let _ = self.server_audio.set(SocketAddr::new(host, port("server_port")?));
        let _ = self.server_ctrl.set(SocketAddr::new(host, port("control_port")?));
        rtsp.session = setup.headers.get("session").map(|s| s.split(';').next().unwrap_or(s).to_string());
        let info = format!("seq={};rtptime={}", self.seq0, self.rtp0);
        rtsp.expect_ok("RECORD", &self.uri, &[("Range", "npt=0-"), ("RTP-Info", &info)], &[])?;
        let body = format!("volume: {:.6}\r\n", volume_db(self.volume.load(Relaxed)));
        rtsp.expect_ok("SET_PARAMETER", &self.uri, &[("Content-Type", "text/parameters")], body.as_bytes())?;
        Ok(())
    }

    pub fn volume(&self) -> u32 {
        self.volume.load(Relaxed)
    }

    /// Records a volume the speaker already has (changed on the speaker itself) without sending it back.
    pub fn note_volume(&self, volume: u32) {
        self.volume.store(volume, Relaxed);
    }

    pub fn set_volume(&self, volume: u32) -> Result<()> {
        self.volume.store(volume, Relaxed);
        let body = format!("volume: {:.6}\r\n", volume_db(volume));
        lock(&self.rtsp).expect_ok("SET_PARAMETER", &self.uri, &[("Content-Type", "text/parameters")], body.as_bytes())?;
        Ok(())
    }

    /// True once the speaker has closed the RTSP connection, e.g. because another source took over.
    pub fn closed_by_receiver(&self) -> bool {
        let Ok(rtsp) = self.rtsp.try_lock() else { return false };
        if rtsp.stream.set_nonblocking(true).is_err() {
            return true;
        }
        let closed = match rtsp.stream.peek(&mut [0u8; 1]) {
            Ok(0) => true,
            Ok(_) => false,
            Err(e) => e.kind() != ErrorKind::WouldBlock,
        };
        let _ = rtsp.stream.set_nonblocking(false);
        closed
    }

    pub fn stop(&self) {
        if !self.running.swap(false, Relaxed) {
            return;
        }
        let mut rtsp = lock(&self.rtsp);
        let _ = rtsp.stream.set_read_timeout(Some(Duration::from_secs(2)));
        if let Err(e) = rtsp.expect_ok("TEARDOWN", &self.uri, &[], &[]) {
            log::info!("TEARDOWN: {e:#}");
        }
        let _ = rtsp.stream.shutdown(Shutdown::Both);
    }

    /// Sends audio until stopped; `fill` provides FRAMES interleaved stereo samples per packet.
    pub fn stream(&self, mut fill: impl FnMut(&mut [i16])) {
        let (Some(&audio_to), Some(&ctrl_to)) = (self.server_audio.get(), self.server_ctrl.get()) else { return };
        let start = Instant::now();
        let start_wall = now();
        let mut pcm = vec![0i16; FRAMES * 2];
        let mut packet = Vec::with_capacity(1500);
        let mut n: u64 = 0;
        while self.running.load(Relaxed) {
            let due = start + Duration::from_secs_f64((n * FRAMES as u64) as f64 / f64::from(RATE));
            let now_i = Instant::now();
            if due > now_i + Duration::from_millis(2) {
                thread::sleep((due - now_i).min(Duration::from_millis(10)));
                continue;
            }
            fill(&mut pcm);
            let rtp = self.rtp0.wrapping_add((n * FRAMES as u64) as u32);
            if n % 125 == 0 {
                let mut sync = [0u8; 20];
                sync[0] = if n == 0 { 0x90 } else { 0x80 };
                sync[1] = 0xd4;
                sync[3] = 0x07;
                sync[4..8].copy_from_slice(&rtp.wrapping_sub(self.latency).to_be_bytes());
                sync[8..16].copy_from_slice(&ntp(start_wall + (n * FRAMES as u64) as f64 / f64::from(RATE)));
                sync[16..20].copy_from_slice(&rtp.to_be_bytes());
                let _ = self.ctrl.send_to(&sync, ctrl_to);
            }
            let seq = self.seq0.wrapping_add(n as u16);
            packet.clear();
            packet.extend_from_slice(&[0x80, if n == 0 { 0xe0 } else { 0x60 }]);
            packet.extend_from_slice(&seq.to_be_bytes());
            packet.extend_from_slice(&rtp.to_be_bytes());
            packet.extend_from_slice(&self.ssrc.to_be_bytes());
            packet.extend_from_slice(&alac_frame(&pcm));
            let _ = self.audio.send_to(&packet, audio_to);
            let mut history = lock(&self.history);
            let slot = &mut history[seq as usize % HISTORY];
            slot.clear();
            slot.extend_from_slice(&packet);
            drop(history);
            n += 1;
        }
    }

    fn timing_loop(&self) {
        let mut buf = [0u8; 128];
        while self.running.load(Relaxed) {
            let Ok((n, from)) = self.timing.recv_from(&mut buf) else { continue };
            if n < 32 || buf[1] & 0x7f != 0x52 {
                continue;
            }
            let received = ntp(now());
            let mut reply = [0u8; 32];
            reply[0] = 0x80;
            reply[1] = 0xd3;
            reply[2..4].copy_from_slice(&buf[2..4]);
            reply[8..16].copy_from_slice(&buf[24..32]);
            reply[16..24].copy_from_slice(&received);
            reply[24..32].copy_from_slice(&ntp(now()));
            let _ = self.timing.send_to(&reply, from);
            if !self.timing_seen.swap(true, Relaxed) {
                log::info!("first timing request from {from}");
            }
        }
    }

    fn control_loop(&self) {
        let mut buf = [0u8; 64];
        while self.running.load(Relaxed) {
            let Ok((n, _)) = self.ctrl.recv_from(&mut buf) else { continue };
            if n < 8 || buf[1] & 0x7f != 0x55 {
                continue;
            }
            let first = u16::from_be_bytes([buf[4], buf[5]]);
            let count = u16::from_be_bytes([buf[6], buf[7]]);
            self.resent.fetch_add(u64::from(count), Relaxed);
            let Some(&to) = self.server_ctrl.get() else { continue };
            let history = lock(&self.history);
            for i in 0..count {
                let seq = first.wrapping_add(i);
                let packet = &history[seq as usize % HISTORY];
                if packet.len() > 12 && packet[2..4] == seq.to_be_bytes() {
                    let mut resend = Vec::with_capacity(packet.len() + 4);
                    resend.extend_from_slice(&[0x80, 0xd6, 0x00, 0x01]);
                    resend.extend_from_slice(packet);
                    let _ = self.ctrl.send_to(&resend, to);
                }
            }
        }
    }
}
