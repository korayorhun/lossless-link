//! UI text: English, or Turkish when Windows' display language is Turkish.

use std::sync::OnceLock;
use windows::Win32::Globalization::GetUserDefaultUILanguage;

fn turkish() -> bool {
    static TURKISH: OnceLock<bool> = OnceLock::new();
    // LANG_TURKISH is 0x1F, in the low 10 bits of the LANGID.
    *TURKISH.get_or_init(|| unsafe { GetUserDefaultUILanguage() } & 0x3FF == 0x1F)
}

macro_rules! text {
    ($($name:ident: $en:literal, $tr:literal;)*) => {
        $(pub fn $name() -> &'static str { if turkish() { $tr } else { $en } })*
    };
}

text! {
    starting: "Starting…", "Başlıyor…";
    paused: "Disconnected. Choose “Connect now” to resume.", "Bağlantı kesildi. Devam etmek için “Şimdi bağlan”ı seçin.";
    no_speaker: "Choose a speaker in the menu.", "Menüden bir hoparlör seçin.";
    no_capture: "Choose an unused output to capture in the menu.", "Menüden kullanılmayan bir ses çıkışı seçin.";
    firewall: "Windows Firewall is blocking the speaker's replies. Choose “Allow through Windows Firewall” in the menu.",
        "Windows Güvenlik Duvarı hoparlörün cevaplarını engelliyor. Menüden “Güvenlik duvarında izin ver”i seçin.";
    menu_connect: "Connect now", "Şimdi bağlan";
    menu_disconnect: "Disconnect", "Bağlantıyı kes";
    menu_speaker: "Speaker", "Hoparlör";
    menu_searching: "Searching…", "Aranıyor…";
    menu_latency: "Latency (lower it if the sound lags behind the picture)", "Gecikme (ses görüntüden geride kalırsa azaltın)";
    menu_capture: "Capture from (an output with nothing connected)", "Yakalanacak çıkış (hiçbir şey bağlı olmayan)";
    menu_autostart: "Start with Windows", "Windows ile başlat";
    menu_firewall: "Allow through Windows Firewall…", "Güvenlik duvarında izin ver…";
    menu_log: "Open log", "Günlüğü aç";
    menu_auto_update: "Automatic updates", "Otomatik güncelle";
    menu_quit: "Quit", "Kapat";
}

pub fn install_update(tag: &str) -> String {
    if turkish() { format!("Güncellemeyi şimdi yükle ({tag})") } else { format!("Install update {tag} now") }
}

pub fn updated(version: &str) -> String {
    if turkish() { format!("{version} sürümüne güncellendi") } else { format!("Updated to version {version}") }
}

pub fn connecting(speaker: &str) -> String {
    if turkish() { format!("{speaker} hoparlörüne bağlanıyor…") } else { format!("Connecting to {speaker}…") }
}

pub fn connected(speaker: &str, latency_ms: u32) -> String {
    if turkish() {
        format!("{speaker} hoparlörüne bağlı: kayıpsız 16 bit/44,1 kHz, {latency_ms} ms gecikme")
    } else {
        format!("Connected to {speaker}: lossless 16-bit/44.1 kHz, {latency_ms} ms latency")
    }
}

pub fn speaker_off(speaker: &str) -> String {
    if turkish() {
        format!("{speaker} kapalı (bekleme modunda); açılınca bağlanılacak")
    } else {
        format!("{speaker} is off (standby); will connect when it is switched on")
    }
}

pub fn busy(speaker: &str, service: Option<&str>) -> String {
    let service = service.unwrap_or("?");
    if turkish() {
        format!("{speaker} başka bir kaynağı çalıyor ({service}); bitince bağlanılacak")
    } else {
        format!("{speaker} is playing another source ({service}); will connect when it stops")
    }
}

pub fn taken_over(speaker: &str) -> String {
    if turkish() {
        format!("{speaker} başka bir kaynağa geçti; geri dönmek için “Şimdi bağlan”ı seçin")
    } else {
        format!("{speaker} switched to another source; choose “Connect now” to take it back")
    }
}

pub fn unreachable(speaker: &str) -> String {
    if turkish() { format!("{speaker} ağda bulunamadı") } else { format!("{speaker} can't be found on the network") }
}

pub fn connect_failed(speaker: &str) -> String {
    if turkish() {
        format!("{speaker} hoparlörüne bağlanılamadı; tekrar denenecek")
    } else {
        format!("Couldn't connect to {speaker}; will retry")
    }
}
