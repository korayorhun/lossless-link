//! Self-update from GitHub Releases. The newest tag comes from the redirect of /releases/latest rather than
//! the GitHub API, whose anonymous limit is easily hit behind shared VPN addresses. A download is only used when
//! it matches the SHA-256 published with the release.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

const REPO: &str = "korayorhun/lossless-link";
const ASSET: &str = "LosslessLink.exe";

pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// "v0.2.0" -> (0, 2, 0); anything unparsable counts as 0.
fn parse(version: &str) -> (u32, u32, u32) {
    let mut parts = version.trim_start_matches('v').split('.').map(|p| p.parse().unwrap_or(0));
    (parts.next().unwrap_or(0), parts.next().unwrap_or(0), parts.next().unwrap_or(0))
}

pub fn is_newer(tag: &str) -> bool {
    parse(tag) > parse(current())
}

pub fn latest_tag() -> Result<String> {
    let agent = ureq::AgentBuilder::new().redirects(0).timeout(Duration::from_secs(20)).build();
    let response = agent.get(&format!("https://github.com/{REPO}/releases/latest")).call()?;
    let location = response.header("location").context("GitHub did not redirect /releases/latest")?;
    let tag = location.rsplit('/').next().unwrap_or_default();
    if !tag.starts_with('v') {
        bail!("unexpected release address {location}");
    }
    Ok(tag.to_string())
}

fn download(url: &str, limit: u64) -> Result<Vec<u8>> {
    let response = ureq::get(url).timeout(Duration::from_secs(120)).call()?;
    let mut bytes = Vec::new();
    response.into_reader().take(limit).read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn beside_exe(extension: &str) -> Result<PathBuf> {
    Ok(std::env::current_exe()?.with_extension(extension))
}

/// Downloads a release next to the running executable, checked against its published SHA-256.
pub fn download_verified(tag: &str) -> Result<PathBuf> {
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/{ASSET}");
    let exe = download(&url, 50 * 1024 * 1024)?;
    let sums = String::from_utf8(download(&format!("{url}.sha256"), 4096)?)?;
    let expected = sums.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    let actual: String = Sha256::digest(&exe).iter().map(|b| format!("{b:02x}")).collect();
    if expected.len() != 64 || expected != actual {
        bail!("{tag} does not match its published SHA-256");
    }
    if !exe.starts_with(b"MZ") {
        bail!("{tag} is not a Windows executable");
    }
    let staged = beside_exe("new.exe")?;
    std::fs::write(&staged, exe)?;
    Ok(staged)
}

/// Puts the staged executable in place of the running one, which can be renamed but not overwritten.
pub fn install(staged: &Path) -> Result<()> {
    let current = std::env::current_exe()?;
    let old = beside_exe("old.exe")?;
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&current, &old).context("moving the running executable aside")?;
    if let Err(e) = std::fs::rename(staged, &current) {
        let _ = std::fs::rename(&old, &current);
        return Err(e).context("putting the new executable in place");
    }
    Ok(())
}

/// Removes what an earlier update left behind.
pub fn clean_up() {
    for path in [beside_exe("old.exe"), beside_exe("new.exe")].into_iter().flatten() {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn versions_compare_numerically() {
        assert!(super::parse("v0.10.0") > super::parse("0.9.9"));
        assert!(super::parse("v1.0.0") > super::parse("0.99.0"));
        assert_eq!(super::parse("v0.2.0"), (0, 2, 0));
    }
}
