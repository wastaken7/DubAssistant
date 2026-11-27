use crate::config::TorrentClientConfig;
use anyhow::{bail, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use reqwest::{multipart, Client, StatusCode};
use serde_json::Value;
use std::time::Duration;

fn normalized_api_key(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let stripped = if trimmed.len() >= 7 && trimmed[..7].eq_ignore_ascii_case("bearer ") {
        trimmed[7..].trim()
    } else {
        trimmed
    };
    if stripped.is_empty() {
        None
    } else {
        Some(stripped)
    }
}

fn uses_api_key(cfg: &TorrentClientConfig) -> bool {
    cfg.qui_proxy_url.trim().is_empty() && normalized_api_key(&cfg.api_key).is_some()
}

fn build_http(cfg: &TorrentClientConfig) -> Result<Client> {
    let mut builder = Client::builder()
        .cookie_store(true)
        .danger_accept_invalid_certs(cfg.allow_invalid_certs)
        .timeout(Duration::from_secs(30));

    if cfg.qui_proxy_url.trim().is_empty() {
        if let Some(key) = normalized_api_key(&cfg.api_key) {
            let mut headers = HeaderMap::new();
            let val = HeaderValue::from_str(&format!("Bearer {key}")).context(
                "API Key do qBittorrent contém caracteres inválidos para cabeçalho HTTP",
            )?;
            headers.insert(AUTHORIZATION, val);
            builder = builder.default_headers(headers);
        }
    }

    Ok(builder.build()?)
}

fn base_url(cfg: &TorrentClientConfig) -> String {
    let qui = cfg.qui_proxy_url.trim().trim_end_matches('/');
    if !qui.is_empty() {
        return qui.to_string();
    }
    let raw = cfg.url.trim().trim_end_matches('/');
    let with_scheme = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    if cfg.port > 0 && !has_explicit_port(&with_scheme) {
        format!("{with_scheme}:{}", cfg.port)
    } else {
        with_scheme
    }
}

fn has_explicit_port(url_str: &str) -> bool {
    url::Url::parse(url_str)
        .ok()
        .and_then(|u| u.port())
        .is_some()
}

fn is_login_response_ok(status: reqwest::StatusCode, body: &str) -> bool {
    if status == reqwest::StatusCode::NO_CONTENT {
        return true;
    }
    let trimmed = body.trim();
    status.is_success() && (trimmed == "Ok." || trimmed.is_empty())
}

async fn login_if_direct(client: &Client, base: &str, cfg: &TorrentClientConfig) -> Result<()> {
    if !cfg.qui_proxy_url.trim().is_empty() || uses_api_key(cfg) {
        return Ok(());
    }
    if cfg.username.trim().is_empty() && cfg.password.is_empty() {
        return Ok(());
    }
    let resp = client
        .post(format!("{base}/api/v2/auth/login"))
        .form(&[
            ("username", cfg.username.as_str()),
            ("password", cfg.password.as_str()),
        ])
        .send()
        .await
        .context("Falha ao conectar no qBittorrent WebUI")?;

    let status = resp.status();
    let body = resp.text().await?;
    if !is_login_response_ok(status, &body) {
        bail!(
            "Login no qBittorrent falhou (HTTP {status}): {}",
            body.trim()
        );
    }
    Ok(())
}

pub async fn test_connection(cfg: &TorrentClientConfig) -> Result<String> {
    let client = build_http(cfg)?;
    let base = base_url(cfg);
    login_if_direct(&client, &base, cfg).await?;

    let resp = client
        .get(format!("{base}/api/v2/app/version"))
        .send()
        .await
        .context("Falha ao consultar versão do qBittorrent")?;
    if !resp.status().is_success() {
        let status = resp.status();
        if uses_api_key(cfg)
            && (status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN)
        {
            bail!(
                "Autenticação por API Key no qBittorrent falhou (HTTP {status}). Verifique a chave nas configurações da WebUI."
            );
        }
        bail!("qBittorrent retornou HTTP {status}");
    }
    let version = resp.text().await?;
    let via = if !cfg.qui_proxy_url.trim().is_empty() {
        "proxy qui"
    } else if uses_api_key(cfg) {
        "API Key"
    } else {
        "WebUI direto"
    };
    Ok(format!("Conectado ao qBittorrent {version} ({via})"))
}

pub async fn add_torrent(
    torrent_bytes: &[u8],
    torrent_filename: &str,
    infohash: &str,
    save_path: &str,
    cfg: &TorrentClientConfig,
) -> Result<String> {
    let client = build_http(cfg)?;
    let base = base_url(cfg);
    login_if_direct(&client, &base, cfg).await?;

    let skip_str = if cfg.skip_checking { "true" } else { "false" };
    let paused_str = if cfg.paused_on_add { "true" } else { "false" };
    let auto_tmm_str = if cfg.auto_tmm { "true" } else { "false" };

    let file_part = multipart::Part::bytes(torrent_bytes.to_vec())
        .file_name(torrent_filename.to_string())
        .mime_str("application/x-bittorrent")?;

    let mut form = multipart::Form::new()
        .part("torrents", file_part)
        .text("savepath", save_path.to_string())
        .text("autoTMM", auto_tmm_str.to_string())
        // qBittorrent 5.3+ renomeou skip_checking para seedMode; enviamos ambos como no Upload-Assistant
        .text("seedMode", skip_str.to_string())
        .text("skip_checking", skip_str.to_string())
        .text("paused", paused_str.to_string())
        .text("stopped", paused_str.to_string())
        .text("contentLayout", "Original".to_string());

    if !cfg.category.trim().is_empty() {
        form = form.text("category", cfg.category.trim().to_string());
    }
    if !cfg.tags.trim().is_empty() {
        form = form.text("tags", cfg.tags.trim().to_string());
    }

    let resp = client
        .post(format!("{base}/api/v2/torrents/add"))
        .multipart(form)
        .send()
        .await
        .context("Erro ao enviar torrent para qBittorrent")?;

    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() || text.trim().eq_ignore_ascii_case("Fails.") {
        bail!("qBittorrent recusou o torrent (HTTP {status}): {text}");
    }

    // Verifica rapidamente se o torrent apareceu no cliente
    tokio::time::sleep(Duration::from_millis(800)).await;
    if let Ok(info_resp) = client
        .get(format!("{base}/api/v2/torrents/info"))
        .query(&[("hashes", infohash)])
        .send()
        .await
    {
        if let Ok(arr) = info_resp.json::<Vec<Value>>().await {
            if let Some(first) = arr.first() {
                let state = first
                    .get("state")
                    .and_then(|v| v.as_str())
                    .unwrap_or("desconhecido");
                if state == "error" || state == "missingFiles" {
                    bail!("Torrent adicionado no qBittorrent, mas entrou em estado '{state}'. Verifique o save_path ({save_path}).");
                }
                return Ok(format!(
                    "Injetado no qBittorrent com sucesso (estado: {state}, savepath: {save_path})"
                ));
            }
        }
    }

    Ok(format!(
        "Injetado no qBittorrent com sucesso (savepath: {save_path})"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    #[test]
    fn login_response_accepts_legacy_200_ok_and_v53_204() {
        assert!(is_login_response_ok(StatusCode::OK, "Ok."));
        assert!(is_login_response_ok(StatusCode::OK, "Ok.\n"));
        assert!(is_login_response_ok(StatusCode::NO_CONTENT, ""));
        assert!(is_login_response_ok(StatusCode::OK, ""));
    }

    #[test]
    fn login_response_rejects_failures() {
        assert!(!is_login_response_ok(StatusCode::OK, "Fails."));
        assert!(!is_login_response_ok(
            StatusCode::UNAUTHORIZED,
            "Unauthorized"
        ));
        assert!(!is_login_response_ok(StatusCode::FORBIDDEN, "Banned"));
    }

    #[test]
    fn api_key_normalization() {
        assert_eq!(normalized_api_key(""), None);
        assert_eq!(normalized_api_key("   "), None);
        assert_eq!(normalized_api_key("Bearer   "), None);
        assert_eq!(
            normalized_api_key("qbt_1234567890abcdef1234567890ab"),
            Some("qbt_1234567890abcdef1234567890ab")
        );
        assert_eq!(
            normalized_api_key("  Bearer qbt_1234567890abcdef1234567890ab  "),
            Some("qbt_1234567890abcdef1234567890ab")
        );
    }

    #[tokio::test]
    #[ignore]
    async fn live_test_connection_from_env() {
        let mut cfg = TorrentClientConfig::default();
        cfg.url = std::env::var("QBT_TEST_URL").unwrap_or_else(|_| "http://localhost".into());
        cfg.port = std::env::var("QBT_TEST_PORT")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(8080);
        cfg.username = std::env::var("QBT_TEST_USER").unwrap_or_else(|_| "admin".into());
        cfg.password = std::env::var("QBT_TEST_PASS").unwrap_or_default();
        cfg.api_key = std::env::var("QBT_TEST_API_KEY").unwrap_or_default();

        let msg = test_connection(&cfg)
            .await
            .expect("test_connection deve suceder");
        println!("{msg}");
        assert!(msg.contains("Conectado ao qBittorrent"));
    }
}
