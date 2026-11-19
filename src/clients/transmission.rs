use crate::config::TorrentClientConfig;
use anyhow::{bail, Context, Result};
use base64::Engine;
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

fn rpc_url(cfg: &TorrentClientConfig) -> String {
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
    if with_port.ends_with("/transmission/rpc") {
        with_port
    } else {
        format!("{with_port}/transmission/rpc")
    }
}

async fn call_rpc(cfg: &TorrentClientConfig, payload: Value) -> Result<Value> {
    let client = Client::builder()
        .danger_accept_invalid_certs(cfg.allow_invalid_certs)
        .timeout(Duration::from_secs(30))
        .build()?;
    let url = rpc_url(cfg);

    let mut session_id = String::new();
    for _ in 0..2 {
        let mut req = client.post(&url).json(&payload);
        if !cfg.username.trim().is_empty() {
            req = req.basic_auth(cfg.username.trim(), Some(&cfg.password));
        }
        if !session_id.is_empty() {
            req = req.header("X-Transmission-Session-Id", &session_id);
        }
        let resp = req
            .send()
            .await
            .context("Falha ao conectar no Transmission RPC")?;

        if resp.status().as_u16() == 409 {
            if let Some(hdr) = resp.headers().get("X-Transmission-Session-Id") {
                session_id = hdr.to_str().unwrap_or("").to_string();
                continue;
            }
        }
        if !resp.status().is_success() {
            bail!("Transmission retornou HTTP {}", resp.status());
        }
        let val: Value = resp.json().await?;
        let result_str = val.get("result").and_then(|v| v.as_str()).unwrap_or("");
        if result_str != "success" {
            bail!("Transmission RPC erro: {result_str}");
        }
        return Ok(val);
    }

    bail!("Falha no handshake X-Transmission-Session-Id com o Transmission")
}

pub async fn test_connection(cfg: &TorrentClientConfig) -> Result<String> {
    let val = call_rpc(cfg, json!({ "method": "session-get" })).await?;
    let ver = val
        .pointer("/arguments/version")
        .and_then(|v| v.as_str())
        .unwrap_or("desconhecida");
    Ok(format!("Conectado ao Transmission {ver}"))
}

pub async fn add_torrent(
    torrent_bytes: &[u8],
    save_path: &str,
    cfg: &TorrentClientConfig,
) -> Result<String> {
    let metainfo = base64::engine::general_purpose::STANDARD.encode(torrent_bytes);
    let mut args = json!({
        "metainfo": metainfo,
        "download-dir": save_path,
        "paused": cfg.paused_on_add,
    });

    if !cfg.tags.trim().is_empty() {
        let labels: Vec<String> = cfg
            .tags
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if !labels.is_empty() {
            args["labels"] = json!(labels);
        }
    }

    call_rpc(
        cfg,
        json!({
            "method": "torrent-add",
            "arguments": args
        }),
    )
    .await?;

    Ok(format!(
        "Injetado no Transmission com sucesso (download-dir: {save_path})"
    ))
}
