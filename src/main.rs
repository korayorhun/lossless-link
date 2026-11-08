//! Lossless Link: plays everything Windows plays on an AirPlay speaker, losslessly and with a short latency.
//!
//! Windows plays into an output with nothing connected to it (a digital S/PDIF output, say). That output is
//! captured with WASAPI loopback and sent to the speaker as uncompressed ALAC over AirPlay 1 (RAOP), with a
//! latency the sender chooses. The Windows volume keys set the speaker's own volume.

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
mod update;

use std::net::UdpSocket;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Media::{timeBeginPeriod, timeEndPeriod};
use windows::Win32::System::Threading::{GetCurrentProcess, SetPriorityClass, ABOVE_NORMAL_PRIORITY_CLASS};

/// A second start sends "quit" here, so one shortcut both starts and stops the program.
const CONTROL: &str = "127.0.0.1:47824";
/// Passed to the new version after an update; it waits for the old one to exit instead of stopping it.
const UPDATED: &str = "--updated";

fn bind_control(wait: bool) -> Option<UdpSocket> {
    let deadline = Instant::now() + Duration::from_secs(if wait { 15 } else { 0 });
    loop {
        if let Ok(socket) = UdpSocket::bind(CONTROL) {
            return Some(socket);
        }
        if Instant::now() >= deadline {
            return None;
        }
        thread::sleep(Duration::from_millis(250));
    }
}

fn main() {
    let updated = std::env::args().any(|a| a == UPDATED);
    let exe = std::env::current_exe();
    let Some(control) = bind_control(updated) else {
        if !updated {
            if let Ok(socket) = UdpSocket::bind("127.0.0.1:0") {
                let _ = socket.send_to(b"quit", CONTROL);
            }
        }
        return;
    };
    system::init_logging();
    std::panic::set_hook(Box::new(|info| log::error!("panic: {info}")));
    log::info!("Lossless Link {} starting", update::current());
    devices::com_init();
    unsafe {
        timeBeginPeriod(1); // 1 ms sleep granularity for even packet pacing
        let _ = SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS);
    }

    let shared = engine::Shared::new(config::Config::load());
    if updated {
        shared.notify(i18n::updated(update::current()));
    }
    discovery::start(shared.speakers.clone());
    let (commands, receiver) = mpsc::channel();
    let engine = {
        let shared = shared.clone();
        thread::spawn(move || engine::run(shared, receiver))
    };
    for task in [engine::volume_loop as fn(_), engine::updater_loop] {
        let shared = shared.clone();
        thread::spawn(move || task(shared));
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

    if shared.restart_for_update.load(Relaxed) {
        if let (Some((tag, staged)), Ok(exe)) = (engine::lock(&shared.update_ready).take(), exe) {
            match update::install(&staged) {
                Ok(()) => {
                    log::info!("installed {tag}; starting it");
                    if let Err(e) = std::process::Command::new(&exe).arg(UPDATED).spawn() {
                        log::error!("starting the new version: {e}");
                    }
                }
                Err(e) => log::error!("installing {tag}: {e:#}"),
            }
        }
    }
    log::info!("stopped");
}
