use crate::config::{ImageHostConfig, ImageHostType};
use anyhow::{bail, Context, Result};
use base64::Engine;
use reqwest::{multipart, Client};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct UploadedImage {
    pub img_url: String,
    pub raw_url: String,
    pub web_url: String,
    pub host_name: String,
    /// Hosts that failed before this one succeeded (fallback), as "Host: erro".
    pub failures: Vec<String>,
}

impl UploadedImage {
    pub fn to_bbcode(&self) -> String {
        format!(
            "[h2]Espectrograma de Áudio[/h2]\n[center][img]{}[/img][/center]",
            self.raw_url
        )
    }
}

pub async fn upload_image(image_path: &Path, cfg: &ImageHostConfig) -> Result<UploadedImage> {
    if !image_path.is_file() {
        bail!(
            "Imagem não encontrada para upload: {}",
            image_path.display()
        );
    }
    let bytes = std::fs::read(image_path)
        .with_context(|| format!("Falha ao ler imagem: {}", image_path.display()))?;
    // Nome neutro (hash do conteúdo) para não expor o nome da mídia aos hosts.
    let filename = hashed_filename(&bytes);

    // Some hosts (e.g. Catbox) reset connections that send no User-Agent.
    let client = Client::builder()
        .user_agent(concat!("DubAssistant/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(60))
        .build()?;

    let order = cfg.upload_order();
    let mut failures: Vec<String> = Vec::new();
    for host in order {
        let key = cfg.key_for(host);
        match upload_to(
            &client,
            host,
            bytes.clone(),
            &filename,
            key,
            &cfg.custom_url,
        )
        .await
        {
            Ok(mut up) => {
                up.failures = failures;
                return Ok(up);
            }
            Err(e) => failures.push(format!("{}: {e:#}", host_name(host))),
        }
    }
    bail!(
        "Falha ao enviar a imagem em todos os hosts tentados:
{}",
        failures.join(
            "
"
        )
    )
}

fn hashed_filename(bytes: &[u8]) -> String {
    use sha1::{Digest, Sha1};
    format!("{}.png", hex::encode(Sha1::digest(bytes)))
}

fn host_name(host: ImageHostType) -> &'static str {
    match host {
        ImageHostType::Pixhost => "Pixhost",
        ImageHostType::Catbox => "Catbox",
        ImageHostType::ImgBB => "ImgBB",
        ImageHostType::OnlyImage => "OnlyImage",
        ImageHostType::Lensdump => "Lensdump",
        ImageHostType::Zipline => "Zipline",
        ImageHostType::PtScreens => "PTScreens",
        ImageHostType::PassTheImage => "PassTheImage",
        ImageHostType::LostImg => "LostImg",
    }
}

async fn upload_to(
    client: &Client,
    host: ImageHostType,
    bytes: Vec<u8>,
    filename: &str,
    key: &str,
    custom_url: &str,
) -> Result<UploadedImage> {
    match host {
        ImageHostType::Pixhost => upload_pixhost(client, bytes, filename).await,
        ImageHostType::Catbox => upload_catbox(client, bytes, filename, key).await,
        ImageHostType::ImgBB => upload_imgbb(client, bytes, key).await,
        ImageHostType::OnlyImage => {
            upload_chevereto(
                client,
                bytes,
                filename,
                "https://onlyimage.org/api/1/upload",
                key,
                "OnlyImage",
            )
            .await
        }
        ImageHostType::Lensdump => upload_lensdump(client, bytes, key).await,
        ImageHostType::PtScreens => {
            upload_chevereto_header(
                client,
                bytes,
                filename,
                "https://ptscreens.com/api/1/upload",
                key,
                "PTScreens",
            )
            .await
        }
        ImageHostType::PassTheImage => {
            upload_chevereto_header(
                client,
                bytes,
                filename,
                "https://passtheima.ge/api/1/upload",
                key,
                "PassTheImage",
            )
            .await
        }
        ImageHostType::LostImg => upload_lostimg(client, bytes, filename, key).await,
        ImageHostType::Zipline => {
            let url = custom_url.trim();
            if url.is_empty() {
                bail!("URL do Zipline não configurada nas Configurações.");
            }
            upload_zipline(client, bytes, filename, url, key, "Zipline").await
        }
    }
}

async fn upload_pixhost(client: &Client, bytes: Vec<u8>, filename: &str) -> Result<UploadedImage> {
    let part = multipart::Part::bytes(bytes)
        .file_name(filename.to_string())
        .mime_str("image/png")?;
    let form = multipart::Form::new()
        .text("content_type", "0")
        .text("max_th_size", "420")
        .part("img", part);

    let resp = client
        .post("https://api.pixhost.to/images")
        .multipart(form)
        .send()
        .await
        .context("Erro de rede ao enviar imagem ao Pixhost")?;
    if !resp.status().is_success() {
        bail!("Pixhost retornou HTTP {}", resp.status());
    }
    let json: Value = resp.json().await?;
    let th_url = json
        .get("th_url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Resposta inválida do Pixhost (sem th_url)"))?;
    let show_url = json
        .get("show_url")
        .and_then(|v| v.as_str())
        .unwrap_or(th_url);
    let raw_url = th_url
        .replace("https://t", "https://img")
        .replace("/thumbs/", "/images/");

    Ok(UploadedImage {
        img_url: th_url.to_string(),
        raw_url,
        web_url: show_url.to_string(),
        host_name: "Pixhost".to_string(),
        failures: Vec::new(),
    })
}

async fn upload_catbox(
    client: &Client,
    bytes: Vec<u8>,
    filename: &str,
    key: &str,
) -> Result<UploadedImage> {
    let part = multipart::Part::bytes(bytes)
        .file_name(filename.to_string())
        .mime_str("image/png")?;
    let mut form = multipart::Form::new()
        .text("reqtype", "fileupload")
        .part("fileToUpload", part);
    if !key.trim().is_empty() {
        form = form.text("userhash", key.trim().to_string());
    }

    let resp = client
        .post("https://catbox.moe/user/api.php")
        .multipart(form)
        .send()
        .await
        .context("Erro de rede ao enviar imagem ao Catbox")?;
    let status = resp.status();
    let url = resp.text().await?.trim().to_string();
    if !status.is_success() || !url.starts_with("https://files.catbox.moe/") {
        bail!("Catbox falhou (HTTP {status}): {url}");
    }

    Ok(UploadedImage {
        img_url: url.clone(),
        raw_url: url.clone(),
        web_url: url,
        host_name: "Catbox".to_string(),
        failures: Vec::new(),
    })
}

async fn upload_imgbb(client: &Client, bytes: Vec<u8>, key: &str) -> Result<UploadedImage> {
    let key = key.trim();
    if key.is_empty() {
        bail!("API Key do ImgBB não configurada nas Configurações.");
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let resp = client
        .post("https://api.imgbb.com/1/upload")
        .form(&[("key", key), ("image", encoded.as_str())])
        .send()
        .await
        .context("Erro de rede ao enviar imagem ao ImgBB")?;
    let status = resp.status();
    let json: Value = resp.json().await?;
    if !status.is_success() || json.get("success").and_then(|v| v.as_bool()) != Some(true) {
        bail!("ImgBB falhou (HTTP {status}): {json}");
    }

    let raw_url = json
        .pointer("/data/image/url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Resposta ImgBB sem URL"))?
        .to_string();
    let img_url = json
        .pointer("/data/medium/url")
        .or_else(|| json.pointer("/data/thumb/url"))
        .and_then(|v| v.as_str())
        .unwrap_or(&raw_url)
        .to_string();
    let web_url = json
        .pointer("/data/url_viewer")
        .and_then(|v| v.as_str())
        .unwrap_or(&raw_url)
        .to_string();

    Ok(UploadedImage {
        img_url,
        raw_url,
        web_url,
        host_name: "ImgBB".to_string(),
        failures: Vec::new(),
    })
}

async fn upload_lensdump(client: &Client, bytes: Vec<u8>, key: &str) -> Result<UploadedImage> {
    let key = key.trim();
    if key.is_empty() {
        bail!("API Key do Lensdump não configurada nas Configurações.");
    }
    let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
    let resp = client
        .post("https://lensdump.com/api/1/upload")
        .header("X-API-Key", key)
        .form(&[("image", encoded.as_str())])
        .send()
        .await
        .context("Erro de rede ao enviar imagem ao Lensdump")?;
    let json: Value = resp.json().await?;
    if json.get("status_code").and_then(|v| v.as_i64()) != Some(200) {
        bail!("Lensdump falhou: {json}");
    }
    let url = json
        .pointer("/data/image/url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Resposta Lensdump sem URL"))?
        .to_string();
    let web_url = json
        .pointer("/data/url_viewer")
        .and_then(|v| v.as_str())
        .unwrap_or(&url)
        .to_string();

    Ok(UploadedImage {
        img_url: url.clone(),
        raw_url: url,
        web_url,
        host_name: "Lensdump".to_string(),
        failures: Vec::new(),
    })
}

async fn upload_chevereto(
    client: &Client,
    bytes: Vec<u8>,
    filename: &str,
    endpoint: &str,
    api_key: &str,
    host_name: &str,
) -> Result<UploadedImage> {
    let key = api_key.trim();
    if key.is_empty() {
        bail!("API Key do {host_name} não configurada nas Configurações.");
    }
    let part = multipart::Part::bytes(bytes)
        .file_name(filename.to_string())
        .mime_str("image/png")?;
    let form = multipart::Form::new()
        .text("key", key.to_string())
        .text("format", "json")
        .part("source", part);

    let resp = client
        .post(endpoint)
        .multipart(form)
        .send()
        .await
        .with_context(|| format!("Erro de rede ao enviar imagem ao {host_name}"))?;
    let json: Value = resp.json().await?;
    let raw_url = json
        .pointer("/image/url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("{host_name} retornou resposta inválida: {json}"))?
        .to_string();
    let img_url = json
        .pointer("/image/medium/url")
        .or_else(|| json.pointer("/image/thumb/url"))
        .and_then(|v| v.as_str())
        .unwrap_or(&raw_url)
        .to_string();
    let web_url = json
        .pointer("/image/url_viewer")
        .and_then(|v| v.as_str())
        .unwrap_or(&raw_url)
        .to_string();

    Ok(UploadedImage {
        img_url,
        raw_url,
        web_url,
        host_name: host_name.to_string(),
        failures: Vec::new(),
    })
}

/// Chevereto-based hosts that authenticate via the `X-API-Key` header
/// (PTScreens, PassTheImage).
async fn upload_chevereto_header(
    client: &Client,
    bytes: Vec<u8>,
    filename: &str,
    endpoint: &str,
    api_key: &str,
    host_name: &str,
) -> Result<UploadedImage> {
    let key = api_key.trim();
    if key.is_empty() {
        bail!("API Key do {host_name} não configurada nas Configurações.");
    }
    let part = multipart::Part::bytes(bytes)
        .file_name(filename.to_string())
        .mime_str("image/png")?;
    let form = multipart::Form::new().part("source", part);

    let resp = client
        .post(endpoint)
        .header("X-API-Key", key)
        .multipart(form)
        .send()
        .await
        .with_context(|| format!("Erro de rede ao enviar imagem ao {host_name}"))?;
    let status = resp.status();
    let text = resp.text().await?;
    let json: Value = serde_json::from_str(&text).map_err(|_| {
        anyhow::anyhow!(
            "{host_name} retornou resposta não-JSON (HTTP {status}): {}",
            truncate(&text, 200)
        )
    })?;
    if !status.is_success() {
        let msg = json
            .pointer("/error/message")
            .and_then(|v| v.as_str())
            .unwrap_or("erro desconhecido");
        bail!("{host_name} falhou (HTTP {status}): {msg}");
    }
    let raw_url = json
        .pointer("/image/url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("{host_name} retornou resposta inválida: {json}"))?
        .to_string();
    let img_url = json
        .pointer("/image/medium/url")
        .and_then(|v| v.as_str())
        .unwrap_or(&raw_url)
        .to_string();
    let web_url = json
        .pointer("/image/url_viewer")
        .and_then(|v| v.as_str())
        .unwrap_or(&raw_url)
        .to_string();

    Ok(UploadedImage {
        img_url,
        raw_url,
        web_url,
        host_name: host_name.to_string(),
        failures: Vec::new(),
    })
}

async fn upload_lostimg(
    client: &Client,
    bytes: Vec<u8>,
    filename: &str,
    key: &str,
) -> Result<UploadedImage> {
    let key = key.trim();
    if key.is_empty() {
        bail!("API Key do LostImg não configurada nas Configurações.");
    }
    let part = multipart::Part::bytes(bytes)
        .file_name(filename.to_string())
        .mime_str("image/png")?;
    let form = multipart::Form::new().part("file[]", part);

    let resp = client
        .post("https://lostimg.cc/api/v1/images")
        .bearer_auth(key)
        .multipart(form)
        .send()
        .await
        .context("Erro de rede ao enviar imagem ao LostImg")?;
    let status = resp.status();
    let text = resp.text().await?;
    let json: Value = serde_json::from_str(&text).map_err(|_| {
        anyhow::anyhow!(
            "LostImg retornou resposta não-JSON (HTTP {status}): {}",
            truncate(&text, 200)
        )
    })?;
    if !status.is_success() {
        bail!("LostImg falhou (HTTP {status}): {json}");
    }
    let url = json
        .get("url")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("Resposta LostImg sem URL: {json}"))?
        .to_string();

    Ok(UploadedImage {
        img_url: url.clone(),
        raw_url: url.clone(),
        web_url: url,
        host_name: "LostImg".to_string(),
        failures: Vec::new(),
    })
}

fn truncate(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

async fn upload_zipline(
    client: &Client,
    bytes: Vec<u8>,
    filename: &str,
    endpoint: &str,
    api_key: &str,
    host_name: &str,
) -> Result<UploadedImage> {
    let key = api_key.trim();
    if key.is_empty() {
        bail!("API Key do {host_name} não configurada nas Configurações.");
    }
    let part = multipart::Part::bytes(bytes)
        .file_name(filename.to_string())
        .mime_str("image/png")?;
    let form = multipart::Form::new().part("file", part);

    let resp = client
        .post(endpoint)
        .header("Authorization", key)
        .multipart(form)
        .send()
        .await
        .with_context(|| format!("Erro de rede ao enviar imagem ao {host_name}"))?;
    if !resp.status().is_success() {
        bail!("{host_name} retornou HTTP {}", resp.status());
    }
    let json: Value = resp.json().await?;
    let first = json
        .get("files")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .ok_or_else(|| anyhow::anyhow!("{host_name} não retornou URL em `files`"))?;
    let url = first
        .get("url")
        .and_then(|v| v.as_str())
        .or_else(|| first.as_str())
        .ok_or_else(|| anyhow::anyhow!("URL inválida retornada pelo {host_name}"))?
        .to_string();

    Ok(UploadedImage {
        img_url: url.clone(),
        raw_url: url.clone(),
        web_url: url,
        host_name: host_name.to_string(),
        failures: Vec::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashed_filename_is_content_sha1() {
        assert_eq!(
            hashed_filename(b"abc"),
            "a9993e364706816aba3e25717850c26c9cd0d89d.png"
        );
    }
}
