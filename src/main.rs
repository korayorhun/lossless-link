//! Lossless Link: plays everything Windows plays on an AirPlay speaker, losslessly and with a short latency.
//!
//! Windows plays into an output with nothing connected to it (a digital S/PDIF output, say). That output is
//! captured with WASAPI loopback, resampled to 44.1 kHz and sent to the speaker as uncompressed ALAC over
//! AirPlay 1 (RAOP), with a latency the sender chooses. The Windows volume keys set the speaker's own volume.

#![cfg_attr(not(test), windows_subsystem = "windows")]

mod alac;
mod capture;
mod config;
mod devices;
mod discovery;
mod engine;
mod i18n;
mod kef;
mod raop;
mod system;
mod tray;

use std::net::UdpSocket;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::thread;
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Threading::{GetCurrentProcess, SetPriorityClass, ABOVE_NORMAL_PRIORITY_CLASS};

/// A second start sends "quit" here, so one shortcut both starts and stops the program.
const CONTROL: &str = "127.0.0.1:47824";

fn main() {
    let control = match UdpSocket::bind(CONTROL) {
        Ok(socket) => socket,
        Err(_) => {
            if let Ok(socket) = UdpSocket::bind("127.0.0.1:0") {
                let _ = socket.send_to(b"quit", CONTROL);
            }
            return;
        }
    };
    system::init_logging();
    std::panic::set_hook(Box::new(|info| log::error!("panic: {info}")));
    log::info!("Lossless Link {} starting", env!("CARGO_PKG_VERSION"));
    devices::com_init();
    unsafe {
        timeBeginPeriod(1); // 1 ms sleep granularity for even packet pacing
        let _ = SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS);
    }

    let shared = engine::Shared::new(config::Config::load());
    discovery::start(shared.speakers.clone());
    let (commands, receiver) = mpsc::channel();
    let engine = {
        let shared = shared.clone();
        thread::spawn(move || engine::run(shared, receiver))
    };
    {
        let shared = shared.clone();
        thread::spawn(move || engine::volume_loop(shared));
    }
    {
        let (shared, commands) = (shared.clone(), commands.clone());
        thread::spawn(move || {
            let mut buf = [0u8; 16];
            while let Ok((n, _)) = control.recv_from(&mut buf) {
                if &buf[..n] == b"quit" {
                    let _ = commands.send(engine::Command::Quit);
                    tray::close(&shared);
                    break;
                }
            }
        });
    }
    if let Err(e) = tray::run(shared.clone(), commands.clone()) {
        log::error!("tray: {e:#}");
        let _ = commands.send(engine::Command::Quit);
    }
    shared.stopping.store(true, Relaxed);
    let _ = engine.join();
    unsafe {
        timeEndPeriod(1);
    }
    log::info!("stopped");
}
