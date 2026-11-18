//! Watches the path to the speaker with ICMP echo, five times a second, and keeps the last half hour of round
//! trips. The automatic latency follows the worst of them, so Wi-Fi hiccups don't cut the sound.

use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::NetworkManagement::IpHelper::{IcmpCreateFile, IcmpSendEcho, ICMP_ECHO_REPLY};

const KEEP: Duration = Duration::from_secs(30 * 60);
const TIMEOUT_MS: u32 = 1000;

pub struct NetMonitor {
    target: Mutex<Option<Ipv4Addr>>,
    /// (when, round trip in ms or None when lost)
    samples: Mutex<VecDeque<(Instant, Option<u32>)>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl NetMonitor {
    pub fn start() -> Arc<Self> {
        let monitor = Arc::new(Self { target: Mutex::new(None), samples: Mutex::new(VecDeque::new()) });
        let m = monitor.clone();
        if let Err(e) = thread::Builder::new().name("netmon".into()).spawn(move || m.run()) {
            log::warn!("starting the network monitor: {e}");
        }
        monitor
    }

    pub fn set_target(&self, ip: Ipv4Addr) {
        let mut target = lock(&self.target);
        if *target != Some(ip) {
            *target = Some(ip);
            lock(&self.samples).clear();
        }
    }

    fn run(&self) {
        let handle = match unsafe { IcmpCreateFile() } {
            Ok(h) => h,
            Err(e) => return log::warn!("ICMP is unavailable, automatic latency can't measure: {e}"),
        };
        let payload = [0x4cu8; 32];
        let mut reply = vec![0u64; 64]; // 512 bytes, aligned for ICMP_ECHO_REPLY
        loop {
            thread::sleep(Duration::from_millis(200));
            let Some(ip) = *lock(&self.target) else { continue };
            let count = unsafe {
                IcmpSendEcho(
                    handle,
                    u32::from_ne_bytes(ip.octets()),
                    payload.as_ptr().cast(),
                    payload.len() as u16,
                    None,
                    reply.as_mut_ptr().cast(),
                    (reply.len() * 8) as u32,
                    TIMEOUT_MS,
                )
            };
            let rtt = (count > 0)
                .then(|| unsafe { std::ptr::read(reply.as_ptr().cast::<ICMP_ECHO_REPLY>()) })
                .filter(|echo| echo.Status == 0)
                .map(|echo| echo.RoundTripTime);
            let now = Instant::now();
            let mut samples = lock(&self.samples);
            samples.push_back((now, rtt));
            while samples.front().is_some_and(|(t, _)| now.duration_since(*t) > KEEP) {
                samples.pop_front();
            }
        }
    }

    /// Worst answered round trip within `window`, once samples cover at least `min_span`.
    pub fn worst(&self, window: Duration, min_span: Duration) -> Option<u32> {
        let samples = lock(&self.samples);
        if samples.front()?.0.elapsed() < min_span {
            return None;
        }
        let now = Instant::now();
        samples.iter().filter(|(t, _)| now.duration_since(*t) <= window).filter_map(|(_, rtt)| *rtt).max()
    }
}
