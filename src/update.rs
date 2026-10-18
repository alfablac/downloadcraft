//! Manual update check against GitHub releases.
// ponytail: manual check only, no silent startup check / self-replace in v1.
// Add startup check when a settings toggle exists; replacing the running
// binary needs a platform-specific swap helper, so updates download as a
// normal task and the user runs the installer.

use serde::Deserialize;

pub const LATEST_URL: &str = "https://api.github.com/repos/alfablac/webcraft/releases/latest";

#[derive(Debug, Clone, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CheckOutcome {
    Checking,
    UpToDate {
        latest: String,
    },
    Available {
        latest: String,
        asset: Option<(String, String)>,
    },
    Failed(String),
}

/// Numeric semver-ish compare: strips a leading `v`, compares dot parts
/// numerically (missing parts = 0, non-numeric suffixes ignored).
pub fn is_newer(latest: &str, current: &str) -> bool {
    fn parts(s: &str) -> Vec<u64> {
        s.trim()
            .trim_start_matches(['v', 'V'])
            .split('.')
            .map(|p| {
                p.chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect::<String>()
                    .parse()
                    .unwrap_or(0)
            })
            .collect()
    }
    let (a, b) = (parts(latest), parts(current));
    for i in 0..a.len().max(b.len()) {
        let (x, y) = (
            a.get(i).copied().unwrap_or(0),
            b.get(i).copied().unwrap_or(0),
        );
        if x != y {
            return x > y;
        }
    }
    false
}

/// Pick best asset for `os`/`arch` (`std::env::consts::{OS, ARCH}` values).
/// Windows: setup `.exe` > `.exe` > `.msi`; macOS: `.dmg` > `.pkg`;
/// Linux: `.AppImage` > `.deb` > `.rpm`. Assets naming a different arch
/// are skipped; within a tier, an explicit arch match wins.
pub fn pick_asset(assets: &[Asset], os: &str, arch: &str) -> Option<(String, String)> {
    fn tier(name: &str, os: &str) -> Option<u8> {
        match os {
            "windows" if name.ends_with(".exe") => {
                Some(if name.contains("setup") { 0 } else { 1 })
            }
            "windows" if name.ends_with(".msi") => Some(2),
            "macos" if name.ends_with(".dmg") => Some(0),
            "macos" if name.ends_with(".pkg") => Some(1),
            "linux" if name.ends_with(".appimage") => Some(0),
            "linux" if name.ends_with(".deb") => Some(1),
            "linux" if name.ends_with(".rpm") => Some(2),
            _ => None,
        }
    }
    let arch_tokens: &[&str] = match arch {
        "aarch64" => &["aarch64", "arm64"],
        "x86" => &["x86", "i686", "win32", "32bit"],
        _ => &["x86_64", "amd64", "x64", "win64", "64bit"],
    };
    let foreign: &[&str] = if arch == "aarch64" {
        &["x86_64", "amd64", "x64", "win64", "i686"]
    } else {
        &["aarch64", "arm64"]
    };
    let mut best: Option<(u8, bool, usize, &Asset)> = None;
    for (i, a) in assets.iter().enumerate() {
        let name = a.name.to_ascii_lowercase();
        let Some(t) = tier(&name, os) else { continue };
        if foreign.iter().any(|t| name.contains(t)) {
            continue;
        }
        let arch_match = arch_tokens.iter().any(|t| name.contains(t));
        let key = (t, !arch_match, i);
        if best.is_none_or(|b| key < (b.0, b.1, b.2)) {
            best = Some((t, !arch_match, i, a));
        }
    }
    best.map(|(_, _, _, a)| (a.name.clone(), a.browser_download_url.clone()))
}

pub fn evaluate(release: &Release, current: &str, os: &str, arch: &str) -> CheckOutcome {
    if is_newer(&release.tag_name, current) {
        CheckOutcome::Available {
            latest: release.tag_name.clone(),
            asset: pick_asset(&release.assets, os, arch),
        }
    } else {
        CheckOutcome::UpToDate {
            latest: release.tag_name.clone(),
        }
    }
}

pub async fn fetch_latest() -> Result<Release, String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("webcraft/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client
        .get(LATEST_URL)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("GitHub HTTP {}", resp.status()));
    }
    // reqwest built without `json` feature: parse body via serde_json.
    let text = resp.text().await.map_err(|e| e.to_string())?;
    serde_json::from_str(&text).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newer_compares_numeric_parts() {
        assert!(is_newer("v1.2.3", "1.2.2"));
        assert!(is_newer("1.10.0", "1.9.9"));
        assert!(is_newer("v2", "1.9.9"));
        assert!(is_newer("1.0.1", "1.0"));
        assert!(!is_newer("v1.2.3", "1.2.3"));
        assert!(!is_newer("1.2", "1.2.0"));
        assert!(!is_newer("v0.9.0", "1.0.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }

    fn fake_release() -> Release {
        serde_json::from_str(
            r#"{
              "tag_name": "v9.9.9",
              "assets": [
                {"name": "webcraft-portable.exe", "browser_download_url": "https://x/portable.exe"},
                {"name": "webcraft-setup-x64.exe", "browser_download_url": "https://x/setup.exe"},
                {"name": "webcraft-arm64.dmg", "browser_download_url": "https://x/arm.dmg"},
                {"name": "webcraft-x86_64.dmg", "browser_download_url": "https://x/mac.dmg"},
                {"name": "webcraft-x86_64.deb", "browser_download_url": "https://x/app.deb"},
                {"name": "webcraft-x86_64.AppImage", "browser_download_url": "https://x/app.AppImage"}
              ]
            }"#,
        )
        .unwrap()
    }

    #[test]
    fn pick_asset_prefers_platform_installer() {
        let r = fake_release();
        assert_eq!(
            pick_asset(&r.assets, "windows", "x86_64"),
            Some(("webcraft-setup-x64.exe".into(), "https://x/setup.exe".into()))
        );
        assert_eq!(
            pick_asset(&r.assets, "macos", "x86_64").map(|a| a.0),
            Some("webcraft-x86_64.dmg".into())
        );
        assert_eq!(
            pick_asset(&r.assets, "macos", "aarch64").map(|a| a.0),
            Some("webcraft-arm64.dmg".into())
        );
        assert_eq!(
            pick_asset(&r.assets, "linux", "x86_64").map(|a| a.0),
            Some("webcraft-x86_64.AppImage".into())
        );
        assert_eq!(pick_asset(&r.assets, "freebsd", "x86_64"), None);
        assert_eq!(pick_asset(&[], "windows", "x86_64"), None);
    }

    #[test]
    fn evaluate_reports_available_and_up_to_date() {
        let r = fake_release();
        assert!(matches!(
            evaluate(&r, "0.5.0", "linux", "x86_64"),
            CheckOutcome::Available { .. }
        ));
        assert!(matches!(
            evaluate(&r, "9.9.9", "linux", "x86_64"),
            CheckOutcome::UpToDate { .. }
        ));
    }
}
