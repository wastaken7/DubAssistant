//! Verificação de nova versão publicada nos Releases do GitHub.
//!
//! Faz uma única requisição à API pública do GitHub (sem autenticação) e apenas
//! informa se existe uma versão mais nova; nada é baixado ou instalado.

use serde::Deserialize;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

const LATEST_RELEASE_API: &str =
    "https://api.github.com/repos/wastaken7/DubAssistant/releases/latest";
pub const RELEASES_PAGE: &str = "https://github.com/wastaken7/DubAssistant/releases/latest";
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone)]
pub struct UpdateInfo {
    /// Tag da release mais recente (ex: "v0.2.0").
    pub tag: String,
    /// Página da release no GitHub.
    pub url: String,
}

#[derive(Deserialize)]
struct LatestRelease {
    tag_name: String,
    html_url: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// Inicia a verificação em uma thread separada (não bloqueia a UI nem o worker).
/// O receptor entrega `Some(info)` somente quando há versão mais nova; qualquer
/// falha de rede/parse é silenciosa e resulta em `None`.
pub fn spawn_update_check() -> Receiver<Option<UpdateInfo>> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(check_latest().ok().flatten());
    });
    rx
}

fn check_latest() -> anyhow::Result<Option<UpdateInfo>> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let release: LatestRelease = rt.block_on(async {
        let client = reqwest::Client::builder()
            .user_agent(concat!("DubAssistant/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(10))
            .build()?;
        let resp = client
            .get(LATEST_RELEASE_API)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await?
            .error_for_status()?;
        anyhow::Ok(resp.json::<LatestRelease>().await?)
    })?;

    if release.draft || release.prerelease || !is_newer(&release.tag_name, CURRENT_VERSION) {
        return Ok(None);
    }
    // Só aceita links para o próprio repositório.
    let url = if release
        .html_url
        .starts_with("https://github.com/wastaken7/DubAssistant/")
    {
        release.html_url
    } else {
        RELEASES_PAGE.to_string()
    };
    Ok(Some(UpdateInfo {
        tag: release.tag_name,
        url,
    }))
}

/// Converte "v1.2.3", "1.2.3-beta" ou "1.2.3+build" em (1, 2, 3).
fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches(['v', 'V']);
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// `true` somente se `latest` for estritamente maior que `current`.
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("v0.1.4"), Some((0, 1, 4)));
        assert_eq!(parse_version("1.10.0"), Some((1, 10, 0)));
        assert_eq!(parse_version("v2.0.0-rc.1"), Some((2, 0, 0)));
        assert_eq!(parse_version("1.2.3+abc"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2"), None);
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("latest"), None);
    }

    #[test]
    fn compares_versions() {
        assert!(is_newer("v0.1.5", "0.1.4"));
        assert!(is_newer("v0.2.0", "0.1.9"));
        assert!(is_newer("v0.1.10", "0.1.9"));
        assert!(is_newer("v1.0.0", "0.99.99"));
        assert!(!is_newer("v0.1.4", "0.1.4"));
        assert!(!is_newer("v0.1.3", "0.1.4"));
        assert!(!is_newer("garbage", "0.1.4"));
    }
}
