//! Logging, start-with-Windows, the firewall rule and opening files.

use crate::config::app_dir;
use std::path::PathBuf;
use std::sync::OnceLock;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOWNORMAL};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("Lossless Link");

static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();

pub fn init_logging() {
    let dir = app_dir("LOCALAPPDATA");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("lossless-link.log");
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > 1_000_000) {
        let _ = std::fs::rename(&path, dir.join("lossless-link.old.log"));
    }
    if let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let mut config = simplelog::ConfigBuilder::new();
        let _ = config.set_time_offset_to_local();
        config.set_time_format_rfc3339();
        let _ = simplelog::WriteLogger::init(log::LevelFilter::Info, config.build(), file);
    }
    let _ = LOG_PATH.set(path);
}

pub fn open_log() {
    if let Some(path) = LOG_PATH.get() {
        open(&HSTRING::from(path.as_os_str()), None, SW_SHOWNORMAL.0);
    }
}

fn open(file: &HSTRING, args: Option<&HSTRING>, show: i32) {
    let args = args.map_or(PCWSTR::null(), |a| PCWSTR(a.as_ptr()));
    let verb = if show == SW_HIDE.0 { w!("runas") } else { w!("open") };
    unsafe {
        ShellExecuteW(HWND::default(), verb, file, args, PCWSTR::null(), windows::Win32::UI::WindowsAndMessaging::SHOW_WINDOW_CMD(show));
    }
}

pub fn autostart_enabled() -> bool {
    unsafe { RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, None).is_ok() }
}

pub fn set_autostart(on: bool) {
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            let value: Vec<u16> = format!("\"{}\"", exe.display()).encode_utf16().chain(Some(0)).collect();
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(value.as_ptr().cast()),
                (value.len() * 2) as u32,
            );
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
        }
    }
}

/// Adds an inbound UDP rule for this program (local subnet only), with one UAC prompt. The speaker sends
/// timing requests from a port that changes every session, so they can't be let in any other way.
pub fn allow_through_firewall() {
    let Ok(exe) = std::env::current_exe() else { return };
    let args = format!(
        "/c netsh advfirewall firewall delete rule name=\"Lossless Link\" & netsh advfirewall firewall add rule \
         name=\"Lossless Link\" dir=in action=allow program=\"{}\" protocol=UDP remoteip=localsubnet enable=yes",
        exe.display()
    );
    open(&HSTRING::from("cmd.exe"), Some(&HSTRING::from(args)), SW_HIDE.0);
}
