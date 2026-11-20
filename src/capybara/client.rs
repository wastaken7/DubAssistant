use crate::config::AppConfig;
use crate::domain::models::{Category, UploadMetadata};
use crate::torrent::bencode::validate_and_extract_infohash;
use anyhow::{bail, Context, Result};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, USER_AGENT};
use reqwest::{multipart, Client};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

const BASE_URL: &str = "https://capybarabr.com";
const GITHUB_URL: &str = "https://github.com/wastaken7/DubAssistant";

/// Anexa automaticamente a assinatura oficial do DubAssistant ao final da descrição BBCode
/// apenas no envio HTTP (sem exibir ou permitir edição na GUI).
pub fn build_description_with_signature(user_description: &str) -> String {
    let version = env!("CARGO_PKG_VERSION");
    let signature = format!(
        "[right][url={GITHUB_URL}][size=4]Compartilhado com DubAssistant {version}[/size][/url][/right]"
    );
    let trimmed = user_description.trim();
    if trimmed.is_empty() {
        signature
    } else {
        format!("{trimmed}\n\n{signature}")
    }
}

#[derive(Debug, Clone)]
pub struct UploadResult {
    pub torrent_id: Option<String>,
    pub download_link: Option<String>,
    pub final_url: String,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct DownloadedTorrent {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
    pub infohash: String,
    pub source_used: String,
}

#[derive(Debug, Clone)]
pub struct DupeEntry {
    pub id: Option<String>,
    pub title: String,
    pub details: String,
    pub url: Option<String>,
    pub likely_same_release: bool,
}

pub struct CapybaraClient {
    http: Client,
    api_key: String,
}

impl CapybaraClient {
    pub fn from_config(cfg: &AppConfig) -> Result<Self> {
        let api_key = cfg.capybara_api_key.trim().to_string();
        if api_key.is_empty() {
            bail!("API Key do CapybaraBR não configurada. Adicione nas Configurações (ou via `config-set --capybara-api-key`).");
        }

        let http = Client::builder()
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()?;

        Ok(Self { http, api_key })
    }

    fn api_headers(&self) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.api_key))
                .context("API Key contém caracteres inválidos")?,
        );
        h.insert(ACCEPT, HeaderValue::from_static("application/json"));
        h.insert(
            USER_AGENT,
            HeaderValue::from_str(&format!("DubAssistant/{}", env!("CARGO_PKG_VERSION")))?,
        );
        Ok(h)
    }

    /// Testa a API Key do CapybaraBR.
    pub async fn test_api_key(&self) -> Result<String> {
        let resp = self
            .http
            .get(format!("{BASE_URL}/api/torrents/filter?perPage=1"))
            .headers(self.api_headers()?)
            .send()
            .await
            .context("Erro de rede ao testar API Key no CapybaraBR")?;
        let status = resp.status();
        if status.as_u16() == 401 {
            bail!("API Key inválida ou expirada (HTTP 401).");
        }
        if status.as_u16() == 403 {
            bail!("Acesso negado pela API (HTTP 403). Verifique permissões e e-mail confirmado.");
        }
        if !status.is_success() {
            bail!("API retornou HTTP {status}.");
        }
        Ok("API Key válida! Pronta para envio via /api/dubs/upload.".to_string())
    }

    /// Verifica dublagens pendentes de moderação via `/api/torrents/pending`.
    /// (Duplicatas já aprovadas no acervo também são recusadas automaticamente pelo servidor no `POST /api/dubs/upload` via HTTP 422.)
    pub async fn check_pending_dupes(
        &self,
        title_pt_br: &str,
        title_original: &str,
        tmdb_id: &str,
        codec_label: &str,
        source_label: &str,
    ) -> Result<Vec<DupeEntry>> {
        let resp = self
            .http
            .get(format!("{BASE_URL}/api/torrents/pending"))
            .headers(self.api_headers()?)
            .send()
            .await
            .context("Erro de rede ao consultar /api/torrents/pending")?;

        if !resp.status().is_success() {
            bail!("Falha ao consultar pendentes (HTTP {}).", resp.status());
        }
        let json: Value = resp.json().await?;
        Ok(parse_pending_api_dupes(
            &json,
            title_pt_br,
            title_original,
            tmdb_id,
            codec_label,
            source_label,
        ))
    }

    /// Envio oficial via API: `POST https://capybarabr.com/api/dubs/upload` (`Authorization: Bearer`).
    pub async fn upload_dublagem(
        &self,
        meta: &UploadMetadata,
        torrent_path: &Path,
    ) -> Result<UploadResult> {
        let torrent_bytes = std::fs::read(torrent_path)
            .with_context(|| format!("Falha ao ler arquivo .torrent: {}", torrent_path.display()))?;
        let torrent_filename = torrent_path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "dublagem.torrent".to_string());

        let torrent_part = multipart::Part::bytes(torrent_bytes)
            .file_name(torrent_filename)
            .mime_str("application/x-bittorrent")?;

        let mut form = multipart::Form::new()
            .part("torrent", torrent_part)
            .text("name", meta.release_name.trim().to_string())
            .text("description", build_description_with_signature(&meta.description))
            .text("category_id", meta.category.id().to_string())
            .text("tmdb_id", meta.tmdb_id.trim().to_string())
            .text("mediainfo", meta.source_mediainfo.clone())
            .text("audio_mediainfo", meta.audio_mediainfo.clone())
            .text("version", meta.version.form_value().to_string())
            .text("source", meta.source.form_value().to_string())
            .text("anon", if meta.anon { "1" } else { "0" }.to_string());

        // `season_number` só é aceito em séries; em filmes a API recusa o campo!
        if meta.category == Category::Series {
            form = form.text("season_number", meta.season_number.trim().to_string());
        }
        if !meta.source_label.trim().is_empty() {
            form = form.text("source_label", meta.source_label.trim().to_string());
        }
        if !meta.fps.trim().is_empty() {
            form = form.text("fps", meta.fps.trim().to_string());
        }
        if !meta.studio.trim().is_empty() {
            form = form.text("studio", meta.studio.trim().to_string());
        }
        if !meta.dub_year.trim().is_empty() {
            form = form.text("dub_year", meta.dub_year.trim().to_string());
        }
        if !meta.note.trim().is_empty() {
            form = form.text("note", meta.note.trim().to_string());
        }

        let resp = self
            .http
            .post(format!("{BASE_URL}/api/dubs/upload"))
            .headers(self.api_headers()?)
            .multipart(form)
            .send()
            .await
            .context("Erro de rede ao enviar para POST /api/dubs/upload")?;

        let status = resp.status().as_u16();
        let body = resp.text().await?;
        parse_api_upload_response(status, &body)
    }

    /// Baixa o `.torrent` corrigido pelo servidor usando o `download_link` retornado pela API de upload,
    /// `/api/torrents/<id>` ou `/torrents/download/<id>?confirmado=1` com `Authorization: Bearer`.
    pub async fn download_torrent_via_api(
        &self,
        torrent_id: &str,
        direct_download_link: Option<&str>,
        dest_path: &Path,
    ) -> Result<DownloadedTorrent> {
        let tid = torrent_id.trim();
        let mut errors = Vec::new();

        // Tentativa 1 (Preferencial): `download_link` retornado direto por POST /api/dubs/upload
        if let Some(link) = direct_download_link
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
        {
            let dl_url = ensure_confirmado_param(link);
            match self.try_download_with_bearer(&dl_url).await {
                Ok(bytes) => {
                    return save_validated_torrent(
                        bytes,
                        dest_path,
                        "API /api/dubs/upload (download_link)",
                    );
                }
                Err(e) => errors.push(format!("download_link: {e}")),
            }
        }

        if !tid.is_empty() {
            // Tentativa 2: API UNIT3D (GET /api/torrents/<id> -> attributes.download_link)
            match self.try_download_via_unit3d_api(tid).await {
                Ok(bytes) => {
                    return save_validated_torrent(bytes, dest_path, "API UNIT3D (/api/torrents)");
                }
                Err(e) => errors.push(format!("API /api/torrents/{tid}: {e}")),
            }

            // Tentativa 3: Endpoint direto com Bearer token + confirmado=1
            let direct_url = format!("{BASE_URL}/torrents/download/{tid}?confirmado=1");
            match self.try_download_with_bearer(&direct_url).await {
                Ok(bytes) => {
                    return save_validated_torrent(
                        bytes,
                        dest_path,
                        "API Bearer (/torrents/download)",
                    );
                }
                Err(e) => errors.push(format!("Bearer /torrents/download/{tid}: {e}")),
            }
        }

        bail!(
            "Falha ao baixar o .torrent #{tid} do CapybaraBR via API:\n- {}",
            errors.join("\n- ")
        );
    }

    async fn try_download_via_unit3d_api(&self, torrent_id: &str) -> Result<Vec<u8>> {
        let api_url = format!("{BASE_URL}/api/torrents/{torrent_id}");
        let resp = self
            .http
            .get(&api_url)
            .headers(self.api_headers()?)
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("HTTP {}", resp.status());
        }

        let json: Value = resp.json().await?;
        let download_link = json
            .pointer("/attributes/download_link")
            .or_else(|| json.pointer("/data/attributes/download_link"))
            .or_else(|| json.get("download_link"))
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("Campo `download_link` ausente na resposta JSON"))?;

        let dl_url = ensure_confirmado_param(download_link);
        self.try_download_with_bearer(&dl_url).await
    }

    async fn try_download_with_bearer(&self, url: &str) -> Result<Vec<u8>> {
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.api_key))?,
        );
        h.insert(
            USER_AGENT,
            HeaderValue::from_str(&format!("DubAssistant/{}", env!("CARGO_PKG_VERSION")))?,
        );

        let resp = self.http.get(url).headers(h).send().await?;
        if !resp.status().is_success() {
            bail!("HTTP {}", resp.status());
        }
        let bytes = resp.bytes().await?.to_vec();
        validate_and_extract_infohash(&bytes)
            .context("Resposta recebida não é um arquivo .torrent Bencode válido")?;
        Ok(bytes)
    }
}

fn ensure_confirmado_param(link: &str) -> String {
    if link.contains("confirmado=") {
        link.to_string()
    } else if link.contains('?') {
        format!("{link}&confirmado=1")
    } else {
        format!("{link}?confirmado=1")
    }
}

/// Interpreta a resposta JSON de `POST https://capybarabr.com/api/dubs/upload`
/// (HTTP 200 sucesso, 422 validação/duplicata, 403 permissão, 401 token inválido).
pub fn parse_api_upload_response(status: u16, body: &str) -> Result<UploadResult> {
    if status == 401 {
        let msg = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|j| {
                j.get("message")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| "Token da API ausente ou inválido.".to_string());
        bail!("Autenticação API falhou (HTTP 401): {msg}");
    }
    if status == 403 {
        let msg = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|j| {
                j.get("message")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| {
                "Sem permissão de envio ao acervo ou e-mail não confirmado.".to_string()
            });
        bail!("Acesso negado pela API (HTTP 403): {msg}");
    }

    let json: Value = serde_json::from_str(body).with_context(|| {
        format!(
            "Resposta inesperada (não-JSON) da API (HTTP {status}): {}",
            &body[..body.len().min(300)]
        )
    })?;

    if status == 422 || json.get("success").and_then(|v| v.as_bool()) == Some(false) {
        let mut details = Vec::new();
        if let Some(obj) = json.get("data").and_then(|v| v.as_object()) {
            for (field, val) in obj {
                if let Some(arr) = val.as_array() {
                    for item in arr {
                        if let Some(s) = item.as_str() {
                            details.push(format!("{field}: {s}"));
                        }
                    }
                } else if let Some(s) = val.as_str() {
                    details.push(format!("{field}: {s}"));
                }
            }
        }
        let base_msg = json
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Erro de validação na API.");
        if details.is_empty() {
            bail!("Erro da API (HTTP {status}): {base_msg}");
        }
        bail!(
            "Erro da API (HTTP {status}) — {base_msg}:\n- {}",
            details.join("\n- ")
        );
    }

    if !(200..300).contains(&status) {
        bail!("API retornou HTTP {status}: {body}");
    }

    let torrent_id = json.pointer("/data/torrent_id").and_then(|v| {
        v.as_u64()
            .map(|n| n.to_string())
            .or_else(|| v.as_str().map(|s| s.to_string()))
    });
    let download_link = json
        .pointer("/data/download_link")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let status_str = json
        .pointer("/data/status")
        .and_then(|v| v.as_str())
        .unwrap_or("pending");
    let api_msg = json
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or("Dublagem enviada com sucesso via API!");

    let final_url = match &torrent_id {
        Some(tid) => format!("{BASE_URL}/torrents/{tid}"),
        None => format!("{BASE_URL}/dublagens"),
    };

    let message = match &torrent_id {
        Some(tid) => format!("{api_msg} (Torrent #{tid}, status: {status_str})"),
        None => format!("{api_msg} (status: {status_str})"),
    };

    Ok(UploadResult {
        torrent_id,
        download_link,
        final_url,
        message,
    })
}

fn matches_title_or_tmdb(
    haystack: &str,
    title_pt_br: &str,
    title_original: &str,
    tmdb_id: &str,
) -> bool {
    let hay = haystack.to_ascii_lowercase();
    let check = |needle: &str| {
        let n = needle.trim().to_ascii_lowercase();
        !n.is_empty() && hay.contains(&n)
    };
    check(title_pt_br)
        || check(title_original)
        || (!tmdb_id.trim().is_empty() && hay.contains(tmdb_id.trim()))
}

fn is_likely_same(haystack: &str, codec_label: &str, source_label: &str) -> bool {
    let hay = haystack.to_ascii_lowercase();
    let codec_ok =
        !codec_label.trim().is_empty() && hay.contains(&codec_label.to_ascii_lowercase());
    let src_ok =
        !source_label.trim().is_empty() && hay.contains(&source_label.to_ascii_lowercase());
    codec_ok && src_ok
}

pub fn parse_pending_api_dupes(
    json: &Value,
    title_pt_br: &str,
    title_original: &str,
    tmdb_id: &str,
    codec_label: &str,
    source_label: &str,
) -> Vec<DupeEntry> {
    let mut out = Vec::new();
    let Some(items) = json.get("data").and_then(|v| v.as_array()) else {
        return out;
    };
    for item in items {
        let cat = item.get("category").and_then(|v| v.as_str()).unwrap_or("");
        let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
        if name.is_empty() || !cat.to_ascii_lowercase().contains("dublag") {
            continue;
        }
        let item_tmdb = item
            .get("tmdb_id")
            .and_then(|v| {
                v.as_u64()
                    .map(|n| n.to_string())
                    .or_else(|| v.as_str().map(|s| s.to_string()))
            })
            .unwrap_or_default();

        let tmdb_match = !tmdb_id.trim().is_empty() && item_tmdb == tmdb_id.trim();
        let title_match = matches_title_or_tmdb(name, title_pt_br, title_original, tmdb_id);
        if !tmdb_match && !title_match {
            continue;
        }
        let id = item.get("id").and_then(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .or_else(|| v.as_u64().map(|n| n.to_string()))
        });
        let url = id.as_ref().map(|tid| format!("{BASE_URL}/torrents/{tid}"));
        let likely_same_release = is_likely_same(name, codec_label, source_label);
        out.push(DupeEntry {
            id,
            title: name.to_string(),
            details: "Pendente de moderação".to_string(),
            url,
            likely_same_release,
        });
    }
    out
}

fn save_validated_torrent(
    bytes: Vec<u8>,
    dest_path: &Path,
    source_used: &str,
) -> Result<DownloadedTorrent> {
    let infohash = validate_and_extract_infohash(&bytes)?;
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp_path = dest_path.with_extension("torrent.tmp");
    std::fs::write(&tmp_path, &bytes)?;
    std::fs::rename(&tmp_path, dest_path)?;

    Ok(DownloadedTorrent {
        path: dest_path.to_path_buf(),
        bytes,
        infohash,
        source_used: source_used.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_description_signature() {
        let with_body = build_description_with_signature("Fonte: Blu-ray Remux");
        assert!(with_body
            .starts_with("Fonte: Blu-ray Remux\n\n[right][url=https://github.com/wastaken7/DubAssistant]"));
        assert!(with_body.contains("Compartilhado com DubAssistant"));

        let empty = build_description_with_signature("   ");
        assert!(empty.starts_with("[right][url=https://github.com/wastaken7/DubAssistant]"));
    }

    #[test]
    fn test_parse_api_upload_success_and_422_duplicate() {
        let ok_body = r#"{"success":true,"data":{"torrent_id":123,"status":"pending","download_link":"https://capybarabr.com/torrents/download/123.abc"},"message":"Dublagem enviada."}"#;
        let res = parse_api_upload_response(200, ok_body).unwrap();
        assert_eq!(res.torrent_id.as_deref(), Some("123"));
        assert_eq!(
            res.download_link.as_deref(),
            Some("https://capybarabr.com/torrents/download/123.abc")
        );

        let dup_body = r#"{"success":false,"message":"Validation Error.","data":{"duplicate":["Já existe uma dublagem idêntica no acervo."]}}"#;
        let err = parse_api_upload_response(422, dup_body).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("duplicate"));
        assert!(msg.contains("idêntica"));
    }
}
