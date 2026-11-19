use crate::config::TorrentClientConfig;
use anyhow::{bail, Context, Result};
use base64::Engine;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

fn json_url(cfg: &TorrentClientConfig) -> String {
    let raw = cfg.url.trim().trim_end_matches('/');
    let with_scheme = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        format!("http://{raw}")
    };
    let with_port = if cfg.port > 0
        && url::Url::parse(&with_scheme)
            .ok()
            .and_then(|u| u.port())
            .is_none()
    {
        format!("{with_scheme}:{}", cfg.port)
    } else {
        with_scheme
    };
    if with_port.ends_with("/json") {
        with_port
    } else {
        format!("{with_port}/json")
    }
}

async fn login(client: &Client, url: &str, cfg: &TorrentClientConfig) -> Result<()> {
    let resp = client
        .post(url)
        .json(&json!({
            "id": 1,
            "method": "auth.login",
            "params": [cfg.password]
        }))
        .send()
        .await
        .context("Falha ao conectar no Deluge WebUI")?;
    let val: Value = resp.json().await?;
    if val.get("result").and_then(|v| v.as_bool()) != Some(true) {
        bail!("Autenticação no Deluge WebUI falhou. Verifique a senha.");
    }
    Ok(())
}

pub async fn test_connection(cfg: &TorrentClientConfig) -> Result<String> {
    let client = Client::builder()
        .cookie_store(true)
        .danger_accept_invalid_certs(cfg.allow_invalid_certs)
        .timeout(Duration::from_secs(30))
        .build()?;
    let url = json_url(cfg);
    login(&client, &url, cfg).await?;

    let resp = client
        .post(&url)
        .json(&json!({
            "id": 2,
            "method": "daemon.info",
            "params": []
        }))
        .send()
        .await?;
    let val: Value = resp.json().await?;
    let ver = val.get("result").and_then(|v| v.as_str()).unwrap_or("WebUI");
    Ok(format!("Conectado ao Deluge {ver}"))
}

pub async fn add_torrent(
    torrent_bytes: &[u8],
    torrent_filename: &str,
    save_path: &str,
    cfg: &TorrentClientConfig,
) -> Result<String> {
    let client = Client::builder()
        .cookie_store(true)
        .danger_accept_invalid_certs(cfg.allow_invalid_certs)
        .timeout(Duration::from_secs(30))
        .build()?;
    let url = json_url(cfg);
    login(&client, &url, cfg).await?;

    let b64 = base64::engine::general_purpose::STANDARD.encode(torrent_bytes);
    let options = json!({
        "download_location": save_path,
        "add_paused": cfg.paused_on_add,
        "seed_mode": cfg.skip_checking,
    });

    let resp = client
        .post(&url)
        .json(&json!({
            "id": 3,
            "method": "core.add_torrent_file",
            "params": [torrent_filename, b64, options]
        }))
        .send()
        .await
        .context("Falha ao enviar torrent ao Deluge")?;
    let val: Value = resp.json().await?;
    if let Some(err) = val.get("error").filter(|v| !v.is_null()) {
        bail!("Deluge retornou erro: {err}");
    }

    if let Some(tid) = val.get("result").and_then(|v| v.as_str()) {
        if !cfg.category.trim().is_empty() {
            let _ = client
                .post(&url)
                .json(&json!({
                    "id": 4,
                    "method": "label.set_torrent",
                    "params": [tid, cfg.category.trim()]
                }))
                .send()
                .await;
        }
    }

    Ok(format!(
        "Injetado no Deluge com sucesso (download_location: {save_path})"
    ))
}
