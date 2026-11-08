//! Records what Windows plays into the capture output (WASAPI loopback), undoes the Windows volume, resamples
//! to 44.1 kHz and keeps ~30 ms of dithered 16-bit PCM ready for the sender. A second stream plays silence into
//! the same output so loopback keeps delivering data when nothing else plays.

use crate::devices::{self, com_init, pro_audio_thread};
use crate::raop::{FRAMES, RATE};
use anyhow::{bail, Result};
use rubato::{FftFixedIn, Resampler};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, IAudioRenderClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoTaskMemFree, CLSCTX_ALL};

const PACKET: usize = FRAMES * 2;
const TARGET: usize = RATE as usize * 30 / 1000 * 2; // 30 ms of interleaved stereo samples
const WINDOW: usize = RATE as usize * 12 / 1000 * 2; // drift band around the target

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub underruns: u64,
    pub dropped: u64,
    pub repeated: u64,
}

struct Fifo {
    data: VecDeque<i16>,
    primed: bool,
    level: f64,
    since_adjust: u32,
    stats: Stats,
}

pub struct Capture {
    pub device_id: String,
    fifo: Mutex<Fifo>,
    gain: AtomicU32,
    running: AtomicBool,
    started: Instant,
    last_sound_ms: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Samples that already are 16-bit values (a 16-bit source at 44.1 kHz; undoing the Windows volume leaves
/// errors far below 0.01 LSB) pass bit-exact. Anything finer gets triangular (TPDF) dither of ±1 LSB.
fn to_i16(x: f32) -> i16 {
    let scaled = x * 32768.0;
    let nearest = scaled.round();
    let value = if (scaled - nearest).abs() < 0.01 { nearest } else { (scaled + fastrand::f32() - fastrand::f32()).round() };
    value.clamp(-32768.0, 32767.0) as i16
}

#[cfg(test)]
mod tests {
    use super::to_i16;

    #[test]
    fn sixteen_bit_samples_pass_bit_exact() {
        for s in [-32768i16, -12345, -1, 0, 1, 777, 32767] {
            let x = f32::from(s) / 32768.0;
            let after_windows_volume_undone = x * 0.0832 / 0.0832; // -21.6 dB applied and divided out
            assert_eq!(to_i16(after_windows_volume_undone), s);
        }
    }
}

/// 44.1 kHz is passed through; anything else goes through rubato's FFT resampler.
enum Converter {
    Same,
    Resample(FftFixedIn<f32>),
}

impl Converter {
    fn new(rate: u32) -> Result<Self> {
        if rate == RATE {
            return Ok(Self::Same);
        }
        Ok(Self::Resample(FftFixedIn::new(rate as usize, RATE as usize, (rate / 100).max(64) as usize, 1, 2)?))
    }

    /// Consumes whole chunks of `left`/`right` and appends interleaved 44.1 kHz samples to `out`.
    fn process(&mut self, left: &mut Vec<f32>, right: &mut Vec<f32>, out: &mut Vec<f32>) -> Result<()> {
        match self {
            Self::Same => {
                for (l, r) in left.drain(..).zip(right.drain(..)) {
                    out.push(l);
                    out.push(r);
                }
            }
            Self::Resample(resampler) => loop {
                let need = resampler.input_frames_next();
                if left.len() < need {
                    break;
                }
                let chunk = resampler.process(&[&left[..need], &right[..need]], None)?;
                left.drain(..need);
                right.drain(..need);
                for (l, r) in chunk[0].iter().zip(&chunk[1]) {
                    out.push(*l);
                    out.push(*r);
                }
            },
        }
        Ok(())
    }
}

impl Capture {
    pub fn start(device_id: &str) -> Arc<Self> {
        let capture = Arc::new(Self {
            device_id: device_id.to_string(),
            fifo: Mutex::new(Fifo {
                data: VecDeque::with_capacity(16 * TARGET),
                primed: false,
                level: TARGET as f64,
                since_adjust: 0,
                stats: Stats::default(),
            }),
            gain: AtomicU32::new(1f32.to_bits()),
            running: AtomicBool::new(true),
            started: Instant::now(),
            last_sound_ms: AtomicU64::new(0),
        });
        for (name, body) in [("keepalive", Self::keep_alive as fn(&Self) -> Result<()>), ("capture", Self::record)] {
            let c = capture.clone();
            let spawned = thread::Builder::new().name(name.into()).spawn(move || c.supervise(name, body));
            if let Err(e) = spawned {
                log::error!("starting the {name} thread: {e}");
            }
        }
        capture
    }

    /// Runs one stream loop, restarting it after errors such as the device being reset after sleep.
    fn supervise(&self, what: &str, body: fn(&Self) -> Result<()>) {
        com_init();
        pro_audio_thread();
        while self.running.load(Relaxed) {
            if let Err(e) = body(self) {
                log::warn!("{what} stream stopped: {e:#}; restarting");
                thread::sleep(Duration::from_secs(1));
            }
        }
    }

    pub fn stop(&self) {
        self.running.store(false, Relaxed);
    }

    /// Factor that undoes the Windows volume, since loopback is captured after it.
    pub fn set_gain(&self, gain: f32) {
        self.gain.store(gain.to_bits(), Relaxed);
    }

    pub fn reset(&self) {
        let mut f = lock(&self.fifo);
        f.data.clear();
        f.primed = false;
    }

    pub fn stats(&self) -> Stats {
        lock(&self.fifo).stats
    }

    /// How long nothing above about -80 dBFS has played; updates wait for a quiet moment.
    pub fn quiet_for(&self) -> Duration {
        self.started.elapsed().saturating_sub(Duration::from_millis(self.last_sound_ms.load(Relaxed)))
    }

    /// Fills one packet. Waits (silence) until 30 ms are buffered, then keeps the buffer near that level:
    /// the capture clock and the send clock drift apart by a few ppm, so when the level stays off target a
    /// single frame is dropped or repeated, at most once every 50 packets (0.4 s).
    pub fn next_packet(&self, out: &mut [i16]) {
        let mut guard = lock(&self.fifo);
        let f = &mut *guard;
        out.fill(0);
        if !f.primed {
            if f.data.len() < TARGET {
                return;
            }
            let excess = f.data.len() - TARGET;
            f.data.drain(..excess);
            f.level = TARGET as f64;
            f.primed = true;
        }
        if f.data.len() < PACKET {
            f.stats.underruns += 1;
            f.primed = false;
            for (o, s) in out.iter_mut().zip(f.data.drain(..)) {
                *o = s;
            }
            return;
        }
        f.level += 0.02 * (f.data.len() as f64 - f.level);
        f.since_adjust += 1;
        if f.since_adjust >= 50 && (f.level - TARGET as f64).abs() > WINDOW as f64 {
            f.since_adjust = 0;
            if f.level > TARGET as f64 && f.data.len() >= PACKET + 2 {
                for (o, s) in out.iter_mut().zip(f.data.drain(..PACKET)) {
                    *o = s;
                }
                f.data.drain(..2);
                f.stats.dropped += 1;
                return;
            }
            if f.level < TARGET as f64 {
                for (o, s) in out[..PACKET - 2].iter_mut().zip(f.data.drain(..PACKET - 2)) {
                    *o = s;
                }
                out[PACKET - 2] = out[PACKET - 4];
                out[PACKET - 1] = out[PACKET - 3];
                f.stats.repeated += 1;
                return;
            }
        }
        for (o, s) in out.iter_mut().zip(f.data.drain(..PACKET)) {
            *o = s;
        }
    }

    fn push(&self, samples: &[i16]) {
        let mut f = lock(&self.fifo);
        f.data.extend(samples.iter().copied());
        if f.data.len() > 8 * TARGET {
            // Nobody is sending: keep only the newest audio.
            let excess = f.data.len() - TARGET;
            f.data.drain(..excess);
        }
    }

    fn keep_alive(&self) -> Result<()> {
        unsafe {
            let client: IAudioClient = devices::device(&self.device_id)?.Activate(CLSCTX_ALL, None)?;
            let format = client.GetMixFormat()?;
            let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 500_000, 0, format, None);
            CoTaskMemFree(Some(format as *const _));
            init?;
            let render: IAudioRenderClient = client.GetService()?;
            let size = client.GetBufferSize()?;
            client.Start()?;
            while self.running.load(Relaxed) {
                let free = size - client.GetCurrentPadding()?;
                if free > 0 {
                    render.GetBuffer(free)?;
                    render.ReleaseBuffer(free, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
                }
                thread::sleep(Duration::from_millis(10));
            }
            client.Stop()?;
        }
        Ok(())
    }

    fn record(&self) -> Result<()> {
        unsafe {
            let client: IAudioClient = devices::device(&self.device_id)?.Activate(CLSCTX_ALL, None)?;
            let format = client.GetMixFormat()?;
            let fmt: WAVEFORMATEX = *format;
            let (tag, channels, rate, bits) = (fmt.wFormatTag, fmt.nChannels as usize, fmt.nSamplesPerSec, fmt.wBitsPerSample);
            // WAVE_FORMAT_EXTENSIBLE keeps the sample type in the first field of SubFormat, 24 bytes in.
            let float = tag == 3 || (tag == 0xFFFE && (format as *const u8).add(24).cast::<u32>().read_unaligned() == 3);
            let init = client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, 200_000, 0, format, None);
            CoTaskMemFree(Some(format as *const _));
            init?;
            if !(float && bits == 32) && !(!float && bits == 16) {
                bail!("unsupported mix format: {bits}-bit, tag {tag:#x}");
            }
            log::info!("capturing {rate} Hz, {channels} channels, {bits}-bit {}", if float { "float" } else { "integer" });
            let capture: IAudioCaptureClient = client.GetService()?;
            let mut converter = Converter::new(rate)?;
            let (mut left, mut right, mut out) = (Vec::new(), Vec::new(), Vec::new());
            let second = channels.min(2) - 1;
            client.Start()?;
            while self.running.load(Relaxed) {
                thread::sleep(Duration::from_millis(3));
                while capture.GetNextPacketSize()? > 0 {
                    let mut data = std::ptr::null_mut();
                    let (mut frames, mut flags) = (0u32, 0u32);
                    capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
                    let gain = f32::from_bits(self.gain.load(Relaxed));
                    let n = frames as usize;
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        left.resize(left.len() + n, 0.0);
                        right.resize(right.len() + n, 0.0);
                    } else if float {
                        for f in std::slice::from_raw_parts(data as *const f32, n * channels).chunks_exact(channels) {
                            left.push(f[0] * gain);
                            right.push(f[second] * gain);
                        }
                    } else {
                        for f in std::slice::from_raw_parts(data as *const i16, n * channels).chunks_exact(channels) {
                            left.push(f32::from(f[0]) / 32768.0 * gain);
                            right.push(f32::from(f[second]) / 32768.0 * gain);
                        }
                    }
                    capture.ReleaseBuffer(frames)?;
                }
                converter.process(&mut left, &mut right, &mut out)?;
                if !out.is_empty() {
                    let pcm: Vec<i16> = out.iter().map(|&x| to_i16(x)).collect();
                    if pcm.iter().any(|s| s.unsigned_abs() > 3) {
                        self.last_sound_ms.store(self.started.elapsed().as_millis() as u64, Relaxed);
                    }
                    self.push(&pcm);
                    out.clear();
                }
            }
            client.Stop()?;
        }
        Ok(())
    }
}
