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
}

impl UploadedImage {
    pub fn to_bbcode(&self) -> String {
        format!(
            "[center][b]Espectrograma de Áudio[/b]\n[url={}][img]{}[/img][/url][/center]",
            self.web_url, self.img_url
        )
    }
}

pub async fn upload_image(image_path: &Path, cfg: &ImageHostConfig) -> Result<UploadedImage> {
    if !image_path.is_file() {
        bail!("Imagem não encontrada para upload: {}", image_path.display());
    }
    let bytes = std::fs::read(image_path)
        .with_context(|| format!("Falha ao ler imagem: {}", image_path.display()))?;
    let filename = image_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "spectrogram.png".to_string());

    let client = Client::builder().timeout(Duration::from_secs(60)).build()?;

    match cfg.host_type {
        ImageHostType::Pixhost => upload_pixhost(&client, bytes, &filename).await,
        ImageHostType::Catbox => upload_catbox(&client, bytes, &filename, cfg).await,
        ImageHostType::ImgBB => upload_imgbb(&client, bytes, cfg).await,
        ImageHostType::OnlyImage => {
            upload_chevereto(
                &client,
                bytes,
                &filename,
                "https://onlyimage.org/api/1/upload",
                &cfg.api_key,
                "OnlyImage",
            )
            .await
        }
        ImageHostType::Lensdump => upload_lensdump(&client, bytes, cfg).await,
        ImageHostType::Zipline => {
            let url = cfg.custom_url.trim();
            if url.is_empty() {
                bail!("URL do Zipline não configurada nas Configurações.");
            }
            upload_zipline(&client, bytes, &filename, url, &cfg.api_key, "Zipline").await
        }
    }
}

async fn upload_pixhost(
    client: &Client,
    bytes: Vec<u8>,
    filename: &str,
) -> Result<UploadedImage> {
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
    })
}

async fn upload_catbox(
    client: &Client,
    bytes: Vec<u8>,
    filename: &str,
    cfg: &ImageHostConfig,
) -> Result<UploadedImage> {
    let part = multipart::Part::bytes(bytes)
        .file_name(filename.to_string())
        .mime_str("image/png")?;
    let mut form = multipart::Form::new()
        .text("reqtype", "fileupload")
        .part("fileToUpload", part);
    if !cfg.api_key.trim().is_empty() {
        form = form.text("userhash", cfg.api_key.trim().to_string());
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
    })
}

async fn upload_imgbb(
    client: &Client,
    bytes: Vec<u8>,
    cfg: &ImageHostConfig,
) -> Result<UploadedImage> {
    let key = cfg.api_key.trim();
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
    })
}

async fn upload_lensdump(
    client: &Client,
    bytes: Vec<u8>,
    cfg: &ImageHostConfig,
) -> Result<UploadedImage> {
    let key = cfg.api_key.trim();
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
    })
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
    })
}
