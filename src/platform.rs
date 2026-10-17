//! Single platform module: all OS differences live here.
use std::path::{Path, PathBuf};
use std::process::Command;

pub const HOST_NAME: &str = "com.idmcraft.host";
pub const PROTOCOL: &str = "idmcraft";
pub const FIREFOX_EXT_ID: &str = "idmcraft@idmcraft.local";
pub const CHROME_EXT_ID_PLACEHOLDER: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

pub fn config_dir() -> PathBuf { directories::ProjectDirs::from("", "", "idmcraft").map(|d| d.config_dir().to_path_buf()).unwrap_or_else(|| PathBuf::from(".idmcraft")) }
pub fn data_dir() -> PathBuf { directories::ProjectDirs::from("", "", "idmcraft").map(|d| d.data_dir().to_path_buf()).unwrap_or_else(|| PathBuf::from(".idmcraft")) }
pub fn cache_dir() -> PathBuf { directories::ProjectDirs::from("", "", "idmcraft").map(|d| d.cache_dir().to_path_buf()).unwrap_or_else(|| PathBuf::from(".idmcraft-cache")) }
pub fn default_download_dir() -> PathBuf { directories::UserDirs::new().and_then(|u| u.download_dir().map(|p| p.to_path_buf())).unwrap_or_else(|| std::env::home_dir().unwrap_or_else(|| PathBuf::from(".")).join("Downloads")) }
pub fn state_path() -> PathBuf { config_dir().join("state.json") }
pub fn inbox_dir() -> PathBuf { config_dir().join("inbox") }
pub fn ensure_dirs() -> std::io::Result<()> { for d in [config_dir(), data_dir(), cache_dir(), inbox_dir()] { std::fs::create_dir_all(d)?; } Ok(()) }

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Browser { Chrome, Edge, Brave, Chromium, Vivaldi, Firefox }

pub fn all_browsers() -> [Browser; 6] { [Browser::Chrome, Browser::Edge, Browser::Brave, Browser::Chromium, Browser::Vivaldi, Browser::Firefox] }

/// Native-messaging manifest paths relative to a home dir. Pure: unit-tested with mocked home.
pub fn native_host_manifest_paths(home: &Path) -> Vec<(Browser, PathBuf)> {
    let mut v = Vec::new();
    #[cfg(target_os = "windows")]
    {
        // Windows uses registry pointing at manifest in install dir; path fn returns manifest file location convention.
        let base = home.join("AppData").join("Local").join("idmcraft");
        for b in all_browsers() { v.push((b, base.join(if b == Browser::Firefox { "host-firefox.json" } else { "host.json" }))); }
    }
    #[cfg(target_os = "macos")]
    {
        let s = home.join("Library").join("Application Support");
        v.push((Browser::Chrome, s.join("Google/Chrome/NativeMessagingHosts").join(format!("{HOST_NAME}.json"))));
        v.push((Browser::Edge, s.join("Microsoft Edge/NativeMessagingHosts").join(format!("{HOST_NAME}.json"))));
        v.push((Browser::Brave, s.join("BraveSoftware/Brave-Browser/NativeMessagingHosts").join(format!("{HOST_NAME}.json"))));
        v.push((Browser::Chromium, s.join("Chromium/NativeMessagingHosts").join(format!("{HOST_NAME}.json"))));
        v.push((Browser::Vivaldi, s.join("Vivaldi/NativeMessagingHosts").join(format!("{HOST_NAME}.json"))));
        v.push((Browser::Firefox, s.join("Mozilla/NativeMessagingHosts").join(format!("{HOST_NAME}.json"))));
    }
    #[cfg(target_os = "linux")]
    {
        let c = home.join(".config");
        v.push((Browser::Chrome, c.join("google-chrome/NativeMessagingHosts").join(format!("{HOST_NAME}.json"))));
        v.push((Browser::Chromium, c.join("chromium/NativeMessagingHosts").join(format!("{HOST_NAME}.json"))));
        v.push((Browser::Brave, c.join("BraveSoftware/Brave-Browser/NativeMessagingHosts").join(format!("{HOST_NAME}.json"))));
        v.push((Browser::Edge, c.join("microsoft-edge/NativeMessagingHosts").join(format!("{HOST_NAME}.json"))));
        v.push((Browser::Vivaldi, c.join("vivaldi/NativeMessagingHosts").join(format!("{HOST_NAME}.json"))));
        v.push((Browser::Firefox, home.join(".mozilla/native-messaging-hosts").join(format!("{HOST_NAME}.json"))));
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
    { for b in all_browsers() { v.push((b, home.join(format!("{HOST_NAME}.json")))); } }
    v
}

pub fn host_manifest_json(host_exe: &Path, firefox: bool) -> String {
    if firefox {
        format!("{{\n  \"name\": \"{HOST_NAME}\",\n  \"description\": \"idmcraft native host\",\n  \"type\": \"stdio\",\n  \"allowed_extensions\": [\"{FIREFOX_EXT_ID}\"],\n  \"path\": \"{}\"\n}}\n", host_exe.display().to_string().replace('\\', "\\\\"))
    } else {
        format!("{{\n  \"name\": \"{HOST_NAME}\",\n  \"description\": \"idmcraft native host\",\n  \"type\": \"stdio\",\n  \"allowed_origins\": [\"chrome-extension://{CHROME_EXT_ID_PLACEHOLDER}/\"],\n  \"path\": \"{}\"\n}}\n", host_exe.display().to_string().replace('\\', "\\\\"))
    }
}

/// Register native messaging host for all browsers, per-user, no admin.
pub fn register_native_hosts(host_exe: &Path) -> std::io::Result<Vec<PathBuf>> {
    #[cfg(not(target_os = "windows"))]
    let home = std::env::home_dir().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home"))?;
    let mut written = Vec::new();
    #[cfg(target_os = "windows")]
    {
        // Manifest lives next to host exe; registry points to it.
        let dir = host_exe.parent().map(|p| p.to_path_buf()).unwrap_or_default();
        let chrome_manifest = dir.join("host.json");
        let moz_manifest = dir.join("host-firefox.json");
        std::fs::write(&chrome_manifest, host_manifest_json(host_exe, false))?;
        std::fs::write(&moz_manifest, host_manifest_json(host_exe, true))?;
        register_windows_native_host(&chrome_manifest, &moz_manifest);
        written.push(chrome_manifest); written.push(moz_manifest);
    }
    #[cfg(not(target_os = "windows"))]
    {
        for (browser, path) in native_host_manifest_paths(&home) {
            if let Some(p) = path.parent() { std::fs::create_dir_all(p)?; }
            std::fs::write(&path, host_manifest_json(host_exe, browser == Browser::Firefox))?;
            written.push(path);
        }
    }
    Ok(written)
}

pub fn unregister_native_hosts() -> std::io::Result<()> {
    #[cfg(not(target_os = "windows"))]
    let home = std::env::home_dir().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home"))?;
    #[cfg(not(target_os = "windows"))]
    for (_, path) in native_host_manifest_paths(&home) { let _ = std::fs::remove_file(path); }
    #[cfg(target_os = "windows")]
    unregister_windows_native_host();
    Ok(())
}

#[cfg(target_os = "windows")]
fn register_windows_native_host(chrome_manifest: &Path, moz_manifest: &Path) {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for root in ["Software\\Google\\Chrome\\NativeMessagingHosts", "Software\\Microsoft\\Edge\\NativeMessagingHosts", "Software\\BraveSoftware\\Brave-Browser\\NativeMessagingHosts", "Software\\Chromium\\NativeMessagingHosts", "Software\\Vivaldi\\NativeMessagingHosts"] {
        if let Ok((k, _)) = hkcu.create_subkey(format!("{root}\\{HOST_NAME}")) { let _ = k.set_value("", &chrome_manifest.to_string_lossy().as_ref()); }
    }
    if let Ok((k, _)) = hkcu.create_subkey(format!("Software\\Mozilla\\NativeMessagingHosts\\{HOST_NAME}")) { let _ = k.set_value("", &moz_manifest.to_string_lossy().as_ref()); }
    // Protocol + autostart handled elsewhere.
    if let Ok((k, _)) = hkcu.create_subkey(format!("Software\\Classes\\{PROTOCOL}\\shell\\open\\command")) {
        if let Ok(exe) = std::env::current_exe() { let _ = k.set_value("", &format!("\"{}\" \"%1\"", exe.display()).as_str()); }
    }
    if let Ok((k, _)) = hkcu.create_subkey(format!("Software\\Classes\\{PROTOCOL}")) { let _ = k.set_value("", &"URL:idmcraft Protocol"); let _ = k.set_value("URL Protocol", &""); }
}
#[cfg(target_os = "windows")]
fn unregister_windows_native_host() {
    use winreg::{enums::HKEY_CURRENT_USER, RegKey};
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    for root in ["Software\\Google\\Chrome\\NativeMessagingHosts", "Software\\Microsoft\\Edge\\NativeMessagingHosts", "Software\\BraveSoftware\\Brave-Browser\\NativeMessagingHosts", "Software\\Chromium\\NativeMessagingHosts", "Software\\Vivaldi\\NativeMessagingHosts", "Software\\Mozilla\\NativeMessagingHosts"] {
        if let Ok(k) = hkcu.open_subkey_with_flags(root, winreg::enums::KEY_WRITE) { let _ = k.delete_subkey_all(HOST_NAME); }
    }
    if let Ok(k) = hkcu.open_subkey_with_flags("Software\\Classes", winreg::enums::KEY_WRITE) { let _ = k.delete_subkey_all(PROTOCOL); }
}

// ---------- autostart ----------
pub fn autostart_path_for(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")] { home.join("Library/LaunchAgents/com.idmcraft.plist") }
    #[cfg(target_os = "linux")] { home.join(".config/autostart/idmcraft.desktop") }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))] { home.join("idmcraft-autostart") }
}

pub fn set_autostart(enabled: bool) -> std::io::Result<()> {
    let exe = std::env::current_exe()?;
    #[cfg(target_os = "windows")]
    {
        use winreg::{enums::HKEY_CURRENT_USER, RegKey};
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        let (k, _) = hkcu.create_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run")?;
        if enabled { k.set_value("idmcraft", &format!("\"{}\" --autostart", exe.display()).as_str())?; } else { let _ = k.delete_value("idmcraft"); }
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::home_dir().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home"))?;
        let path = autostart_path_for(&home);
        if enabled {
            if let Some(p) = path.parent() { std::fs::create_dir_all(p)?; }
            std::fs::write(&path, format!("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Label</key><string>com.idmcraft</string><key>ProgramArguments</key><array><string>{}</string><string>--autostart</string></array><key>RunAtLoad</key><true/></dict></plist>\n", exe.display()))?;
        } else { let _ = std::fs::remove_file(path); }
    }
    #[cfg(target_os = "linux")]
    {
        let home = std::env::home_dir().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home"))?;
        let path = autostart_path_for(&home);
        if enabled {
            if let Some(p) = path.parent() { std::fs::create_dir_all(p)?; }
            std::fs::write(&path, format!("[Desktop Entry]\nType=Application\nName=idmcraft\nExec=\"{}\" --autostart\nX-GNOME-Autostart-enabled=true\nNoDisplay=false\n", exe.display()))?;
        } else { let _ = std::fs::remove_file(path); }
    }
    Ok(())
}

pub fn autostart_enabled() -> bool {
    #[cfg(target_os = "windows")]
    {
        use winreg::{enums::HKEY_CURRENT_USER, RegKey};
        let hkcu = RegKey::predef(HKEY_CURRENT_USER);
        return hkcu.open_subkey("Software\\Microsoft\\Windows\\CurrentVersion\\Run").and_then(|k| k.get_value::<String, _>("idmcraft")).is_ok();
    }
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        if let Some(home) = std::env::home_dir() { return autostart_path_for(&home).exists(); }
        return false;
    }
    #[allow(unreachable_code)] false
}

pub fn register_protocol() -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let home = std::env::home_dir().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no home"))?;
        let dir = home.join(".local/share/applications");
        std::fs::create_dir_all(&dir)?;
        let exe = std::env::current_exe()?;
        std::fs::write(dir.join("idmcraft.desktop"), format!("[Desktop Entry]\nType=Application\nName=idmcraft\nExec=\"{}\" %u\nMimeType=x-scheme-handler/{PROTOCOL};\nNoDisplay=true\n", exe.display()))?;
        let _ = Command::new("xdg-mime").args(["default", "idmcraft.desktop", &format!("x-scheme-handler/{PROTOCOL}")]).status();
    }
    #[cfg(target_os = "windows")]
    { let exe = std::env::current_exe()?; let _ = exe; register_windows_native_host(&state_path(), &state_path()); }
    // macOS: URL scheme declared in app bundle Info.plist (see packaging/idmcraft.app Info.plist template in docs).
    Ok(())
}

pub fn open_path(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")] { Command::new("cmd").args(["/C", "start", "", &path.to_string_lossy()]).spawn()?; }
    #[cfg(target_os = "macos")] { Command::new("open").arg(path).spawn()?; }
    #[cfg(target_os = "linux")] { Command::new("xdg-open").arg(path).spawn()?; }
    Ok(())
}
pub fn reveal_path(path: &Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")] { Command::new("explorer").arg(format!("/select,{}", path.display())).spawn()?; }
    #[cfg(target_os = "macos")] { Command::new("open").args(["-R"]).arg(path).spawn()?; }
    #[cfg(target_os = "linux")] { Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn()?; }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PowerAction { Shutdown, Sleep }
/// Build power command without executing. Unit-tested per OS construction.
pub fn power_command(action: PowerAction) -> (String, Vec<String>) {
    #[cfg(target_os = "windows")] { match action { PowerAction::Shutdown => ("shutdown.exe".into(), vec!["/s".into(), "/t".into(), "0".into()]), PowerAction::Sleep => ("rundll32.exe".into(), vec!["powrprof.dll,SetSuspendState".into(), "0".into(), "1".into(), "0".into()]) } }
    #[cfg(target_os = "macos")] { match action { PowerAction::Shutdown => ("osascript".into(), vec!["-e".into(), "tell application \"System Events\" to shut down".into()]), PowerAction::Sleep => ("pmset".into(), vec!["sleepnow".into()]) } }
    #[cfg(target_os = "linux")] { match action { PowerAction::Shutdown => ("systemctl".into(), vec!["poweroff".into()]), PowerAction::Sleep => ("systemctl".into(), vec!["suspend".into()]) } }
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))] { ("".into(), vec![]) }
}
pub fn execute_power(action: PowerAction) -> std::io::Result<()> { let (p, a) = power_command(action); Command::new(p).args(a).spawn()?; Ok(()) }

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn manifest_paths_mock_home() {
        let paths = native_host_manifest_paths(Path::new("/mockhome"));
        assert_eq!(paths.len(), 6);
        for (_, p) in &paths { let s = p.to_string_lossy().replace('\\', "/"); assert!(s.contains("/mockhome/") || s.contains("mockhome"), "{s}"); assert!(s.contains(HOST_NAME) || s.contains("host"), "{s}"); }
    }
    #[test] fn autostart_path_mock() { let p = autostart_path_for(Path::new("/h")); assert!(!p.to_string_lossy().is_empty()); }
    #[test] fn power_cmd_nonempty() { let (p, _) = power_command(PowerAction::Shutdown); assert!(!p.is_empty()); }
    #[test] fn manifest_json_content() { let j = host_manifest_json(Path::new("/tmp/host"), false); assert!(j.contains(HOST_NAME)); let jf = host_manifest_json(Path::new("/tmp/host"), true); assert!(jf.contains(FIREFOX_EXT_ID)); }
}
