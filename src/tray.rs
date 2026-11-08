//! The notification-area icon and its menu, in plain Win32.

use crate::devices;
use crate::discovery::Speaker;
use crate::engine::{self, lock, Command, Shared, State, LATENCIES_MS};
use crate::{i18n, system, update};
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, POINT, TRUE, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, GetDC, ReleaseDC, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIIF_INFO, NIIF_NOSOUND, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetCursorPos, GetMessageW, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, TrackPopupMenu, TranslateMessage, HICON, HMENU, ICONINFO, MENU_ITEM_FLAGS, MF_CHECKED,
    MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, TPM_NONOTIFY, TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE,
    WM_APP, WM_CLOSE, WM_CONTEXTMENU, WM_NULL, WNDCLASSW, WS_OVERLAPPED,
};

const WM_TRAY: u32 = WM_APP + 1;
const WM_REFRESH: u32 = WM_APP + 2;
const NIN_KEYSELECT: u32 = NIN_SELECT | 1;
const ICON_ID: u32 = 1;
const ID_CONNECT: usize = 1;
const ID_DISCONNECT: usize = 2;
const ID_AUTOSTART: usize = 3;
const ID_FIREWALL: usize = 4;
const ID_LOG: usize = 5;
const ID_QUIT: usize = 6;
const ID_AUTO_UPDATE: usize = 7;
const ID_INSTALL_UPDATE: usize = 8;
const ID_SPEAKER: usize = 100;
const ID_LATENCY: usize = 200;
const ID_CAPTURE: usize = 300;

struct App {
    shared: Arc<Shared>,
    commands: Sender<Command>,
    icons: HashMap<State, HICON>,
    taskbar_created: u32,
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

/// Asks the tray thread to show the current status.
pub fn refresh(shared: &Shared) {
    post(shared, WM_REFRESH);
}

/// Closes the tray from another thread (a second start of the program asks for this).
pub fn close(shared: &Shared) {
    post(shared, WM_CLOSE);
}

fn post(shared: &Shared, message: u32) {
    let hwnd = shared.hwnd.load(Relaxed);
    if hwnd != 0 {
        unsafe {
            let _ = PostMessageW(HWND(hwnd as *mut _), message, WPARAM(0), LPARAM(0));
        }
    }
}

pub fn run(shared: Arc<Shared>, commands: Sender<Command>) -> anyhow::Result<()> {
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = w!("LosslessLinkTray");
        let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: instance.into(), lpszClassName: class, ..Default::default() };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(WINDOW_EX_STYLE::default(), class, w!("Lossless Link"), WS_OVERLAPPED, 0, 0, 0, 0, None, None, instance, None)?;
        let icons = [State::Starting, State::Connected, State::Off, State::Busy, State::Error, State::Paused]
            .into_iter()
            .map(|s| (s, make_icon(color(s))))
            .collect();
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        APP.with(|a| *a.borrow_mut() = Some(App { shared: shared.clone(), commands, icons, taskbar_created }));
        shared.hwnd.store(hwnd.0 as isize, Relaxed);
        add_icon(hwnd);
        update(hwnd);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        shared.hwnd.store(0, Relaxed);
        let nid = base_data(hwnd);
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    }
    Ok(())
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let taskbar_created = APP.with(|a| a.borrow().as_ref().map_or(0, |app| app.taskbar_created));
    match msg {
        WM_TRAY => {
            let event = (lparam.0 as u32) & 0xFFFF;
            if event == WM_CONTEXTMENU || event == NIN_SELECT || event == NIN_KEYSELECT {
                show_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_REFRESH => {
            update(hwnd);
            LRESULT(0)
        }
        WM_CLOSE => {
            quit();
            LRESULT(0)
        }
        _ if msg != 0 && msg == taskbar_created => {
            // Explorer restarted: the icon is gone and has to be added again.
            add_icon(hwnd);
            update(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn base_data(hwnd: HWND) -> NOTIFYICONDATAW {
    NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: ICON_ID,
        ..Default::default()
    }
}

fn copy_wide(dst: &mut [u16], text: &str) {
    let (mut n, room) = (0, dst.len() - 1);
    for (d, s) in dst.iter_mut().take(room).zip(text.encode_utf16()) {
        *d = s;
        n += 1;
    }
    dst[n] = 0;
}

unsafe fn add_icon(hwnd: HWND) {
    let mut nid = base_data(hwnd);
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    nid.uCallbackMessage = WM_TRAY;
    nid.hIcon = APP.with(|a| a.borrow().as_ref().map(|app| app.icons[&State::Starting])).unwrap_or_default();
    copy_wide(&mut nid.szTip, "Lossless Link");
    let _ = Shell_NotifyIconW(NIM_ADD, &nid);
    nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
}

unsafe fn update(hwnd: HWND) {
    let Some((shared, icons)) = APP.with(|a| a.borrow().as_ref().map(|app| (app.shared.clone(), app.icons.clone()))) else {
        return;
    };
    let (state, text) = lock(&shared.status).clone();
    let balloon = lock(&shared.balloon).take();
    let mut nid = base_data(hwnd);
    nid.uFlags = NIF_ICON | NIF_TIP | NIF_SHOWTIP;
    nid.hIcon = icons[&state];
    copy_wide(&mut nid.szTip, &format!("Lossless Link\n{text}"));
    if let Some(message) = balloon {
        nid.uFlags |= NIF_INFO;
        copy_wide(&mut nid.szInfoTitle, "Lossless Link");
        copy_wide(&mut nid.szInfo, &message);
        nid.dwInfoFlags = NIIF_INFO | NIIF_NOSOUND;
    }
    let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
}

unsafe fn append(menu: HMENU, flags: MENU_ITEM_FLAGS, id: usize, text: &str) {
    let _ = AppendMenuW(menu, flags, id, &HSTRING::from(text));
}

unsafe fn submenu(menu: HMENU, sub: HMENU, text: &str) {
    let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, &HSTRING::from(text));
}

fn checked(on: bool) -> MENU_ITEM_FLAGS {
    if on {
        MF_STRING | MF_CHECKED
    } else {
        MF_STRING
    }
}

unsafe fn show_menu(hwnd: HWND) {
    // Copy what the menu needs: the window procedure runs again while the menu is open.
    let Some((shared, commands)) = APP.with(|a| a.borrow().as_ref().map(|app| (app.shared.clone(), app.commands.clone())))
    else {
        return;
    };
    let (state, text) = lock(&shared.status).clone();
    let config = lock(&shared.config).clone();
    let mut speakers: Vec<Speaker> = lock(&shared.speakers).clone();
    if let Some(current) = &config.speaker {
        if !speakers.iter().any(|s| s.name == current.name) {
            speakers.insert(0, Speaker { name: current.name.clone(), host: current.host.clone(), port: current.port, model: String::new() });
        }
    }
    let outputs = devices::outputs().unwrap_or_default();

    let Ok(menu) = CreatePopupMenu() else { return };
    append(menu, MF_STRING | MF_GRAYED, 0, &text);
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    if state == State::Connected {
        append(menu, MF_STRING, ID_DISCONNECT, i18n::menu_disconnect());
    } else {
        append(menu, MF_STRING, ID_CONNECT, i18n::menu_connect());
    }
    if let Ok(sub) = CreatePopupMenu() {
        if speakers.is_empty() {
            append(sub, MF_STRING | MF_GRAYED, 0, i18n::menu_searching());
        }
        for (i, s) in speakers.iter().enumerate() {
            let label = if s.model.is_empty() { s.name.clone() } else { format!("{} ({})", s.name, s.model) };
            append(sub, checked(config.speaker.as_ref().is_some_and(|c| c.name == s.name)), ID_SPEAKER + i, &label);
        }
        submenu(menu, sub, i18n::menu_speaker());
    }
    if let Ok(sub) = CreatePopupMenu() {
        for (i, ms) in LATENCIES_MS.iter().enumerate() {
            append(sub, checked(config.latency_ms == *ms), ID_LATENCY + i, &format!("{ms} ms"));
        }
        submenu(menu, sub, i18n::menu_latency());
    }
    if let Ok(sub) = CreatePopupMenu() {
        for (i, o) in outputs.iter().enumerate() {
            append(sub, checked(config.capture_device.as_deref() == Some(o.id.as_str())), ID_CAPTURE + i, &o.name);
        }
        submenu(menu, sub, i18n::menu_capture());
    }
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    if let Some((tag, _)) = lock(&shared.update_ready).as_ref() {
        append(menu, MF_STRING, ID_INSTALL_UPDATE, &i18n::install_update(tag));
    }
    append(menu, checked(config.auto_update), ID_AUTO_UPDATE, i18n::menu_auto_update());
    append(menu, checked(system::autostart_enabled()), ID_AUTOSTART, i18n::menu_autostart());
    append(menu, MF_STRING, ID_FIREWALL, i18n::menu_firewall());
    append(menu, MF_STRING, ID_LOG, i18n::menu_log());
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    append(menu, MF_STRING | MF_GRAYED, 0, &format!("Lossless Link {}", update::current()));
    append(menu, MF_STRING, ID_QUIT, i18n::menu_quit());

    let mut point = POINT::default();
    let _ = GetCursorPos(&mut point);
    let _ = SetForegroundWindow(hwnd);
    let chosen = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, point.x, point.y, 0, hwnd, None).0 as usize;
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);

    let command = match chosen {
        ID_CONNECT => Some(Command::Connect),
        ID_DISCONNECT => Some(Command::Disconnect),
        ID_AUTOSTART => {
            system::set_autostart(!system::autostart_enabled());
            None
        }
        ID_AUTO_UPDATE => {
            let mut config = lock(&shared.config);
            config.auto_update = !config.auto_update;
            config.save();
            None
        }
        ID_INSTALL_UPDATE => {
            engine::restart_for_update(&shared);
            None
        }
        ID_FIREWALL => {
            system::allow_through_firewall();
            None
        }
        ID_LOG => {
            system::open_log();
            None
        }
        ID_QUIT => {
            quit();
            None
        }
        id if (ID_SPEAKER..ID_SPEAKER + speakers.len()).contains(&id) => Some(Command::SelectSpeaker(speakers[id - ID_SPEAKER].clone())),
        id if (ID_LATENCY..ID_LATENCY + LATENCIES_MS.len()).contains(&id) => Some(Command::SetLatency(LATENCIES_MS[id - ID_LATENCY])),
        id if (ID_CAPTURE..ID_CAPTURE + outputs.len()).contains(&id) => Some(Command::SetCapture(outputs[id - ID_CAPTURE].id.clone())),
        _ => None,
    };
    if let Some(command) = command {
        let _ = commands.send(command);
    }
}

fn quit() {
    if let Some(commands) = APP.with(|a| a.borrow().as_ref().map(|app| app.commands.clone())) {
        let _ = commands.send(Command::Quit);
    }
    unsafe { PostQuitMessage(0) };
}

fn color(state: State) -> [u8; 3] {
    match state {
        State::Connected => [46, 125, 50],
        State::Busy => [21, 101, 192],
        State::Off => [110, 110, 110],
        State::Paused => [96, 125, 139],
        State::Starting | State::Error => [239, 108, 0],
    }
}

/// A speaker glyph on a coloured disc, drawn at 32 px with 4x4 supersampling.
fn icon_pixels(rgb: [u8; 3]) -> Vec<u32> {
    const SIZE: usize = 32;
    let body = [(14.0, 26.0), (24.0, 26.0), (34.0, 16.0), (34.0, 48.0), (24.0, 38.0), (14.0, 38.0)];
    let in_body = |x: f32, y: f32| {
        let mut inside = false;
        let mut j = body.len() - 1;
        for i in 0..body.len() {
            let ((xi, yi), (xj, yj)) = (body[i], body[j]);
            if (yi > y) != (yj > y) && x < (xj - xi) * (y - yi) / (yj - yi) + xi {
                inside = !inside;
            }
            j = i;
        }
        inside
    };
    let in_wave = |x: f32, y: f32| {
        let (dx, dy) = (x - 34.0, y - 32.0);
        let r = (dx * dx + dy * dy).sqrt();
        dx > 0.0 && dy.atan2(dx).abs() < 0.87 && [11.0f32, 19.0].iter().any(|&w| (r - w).abs() < 2.2)
    };
    let mut pixels = vec![0u32; SIZE * SIZE];
    for py in 0..SIZE {
        for px in 0..SIZE {
            let (mut r, mut g, mut b, mut hits) = (0u32, 0u32, 0u32, 0u32);
            for sy in 0..4 {
                for sx in 0..4 {
                    // 64-unit design space
                    let x = (px as f32 + (sx as f32 + 0.5) / 4.0) * 2.0;
                    let y = (py as f32 + (sy as f32 + 0.5) / 4.0) * 2.0;
                    if (x - 32.0).powi(2) + (y - 32.0).powi(2) > 31.0f32.powi(2) {
                        continue;
                    }
                    let c = if in_body(x, y) || in_wave(x, y) { [255, 255, 255] } else { rgb };
                    r += u32::from(c[0]);
                    g += u32::from(c[1]);
                    b += u32::from(c[2]);
                    hits += 1;
                }
            }
            if hits > 0 {
                let alpha = hits * 255 / 16;
                pixels[py * SIZE + px] = (alpha << 24) | ((r / hits) << 16) | ((g / hits) << 8) | (b / hits);
            }
        }
    }
    pixels
}

unsafe fn make_icon(rgb: [u8; 3]) -> HICON {
    const SIZE: i32 = 32;
    let pixels = icon_pixels(rgb);
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: SIZE,
            biHeight: -SIZE, // top-down rows
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let dc = GetDC(HWND::default());
    let mut bits = std::ptr::null_mut();
    let color = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, HANDLE::default(), 0);
    ReleaseDC(HWND::default(), dc);
    let Ok(color) = color else { return HICON::default() };
    std::ptr::copy_nonoverlapping(pixels.as_ptr(), bits.cast::<u32>(), pixels.len());
    let mask = CreateBitmap(SIZE, SIZE, 1, 1, None);
    let icon = CreateIconIndirect(&ICONINFO { fIcon: TRUE, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color });
    let _ = DeleteObject(color);
    let _ = DeleteObject(mask);
    icon.unwrap_or_default()
}
