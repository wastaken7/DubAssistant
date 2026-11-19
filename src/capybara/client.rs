use crate::capybara::cookies::load_cookie_header;
use crate::config::AppConfig;
use crate::domain::models::{Category, UploadMetadata};
use crate::torrent::bencode::validate_and_extract_infohash;
use anyhow::{bail, Context, Result};
use regex::Regex;
use reqwest::cookie::Jar;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, REFERER, USER_AGENT};
use reqwest::{multipart, Client};
use scraper::{Html, Selector};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use url::Url;

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
pub struct SessionInfo {
    pub csrf_token: String,
    pub announce_url: Option<String>,
    pub username_hint: Option<String>,
}

#[derive(Debug, Clone)]
pub struct UploadResult {
    pub torrent_id: Option<String>,
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
    user_agent: String,
    api_key: String,
}

impl CapybaraClient {
    pub fn from_config(cfg: &AppConfig) -> Result<Self> {
        let cookie_header = load_cookie_header(cfg)?;
        let ua = if cfg.user_agent.trim().is_empty() {
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36".to_string()
        } else {
            cfg.user_agent.trim().to_string()
        };

        // Carrega os cookies num Jar compartilhado para que qualquer `Set-Cookie` retornado
        // pelo GET /dublagens/upload (ex: rotação de laravel_session / XSRF-TOKEN via remember_web)
        // seja aplicado automaticamente no POST /dublagens seguinte.
        let jar = Arc::new(Jar::default());
        let base_url = Url::parse(BASE_URL)?;
        for pair in cookie_header.split(';') {
            let trimmed = pair.trim();
            if !trimmed.is_empty() && trimmed.contains('=') {
                jar.add_cookie_str(
                    &format!("{trimmed}; Domain=capybarabr.com; Path=/"),
                    &base_url,
                );
            }
        }

        let http = Client::builder()
            .cookie_provider(jar)
            .timeout(Duration::from_secs(60))
            .redirect(reqwest::redirect::Policy::limited(10))
            .build()?;

        Ok(Self {
            http,
            user_agent: ua,
            api_key: cfg.capybara_api_key.trim().to_string(),
        })
    }

    fn web_headers(&self) -> Result<HeaderMap> {
        let mut h = HeaderMap::new();
        h.insert(
            USER_AGENT,
            HeaderValue::from_str(&self.user_agent).context("User-Agent inválido")?,
        );
        h.insert(
            ACCEPT,
            HeaderValue::from_static("text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8"),
        );
        Ok(h)
    }

    /// Testa autenticação via cookies acessando `/dublagens/upload`, extrai `_token` CSRF e `announce` URL pessoal.
    pub async fn test_session(&self) -> Result<SessionInfo> {
        let url = format!("{BASE_URL}/dublagens/upload");
        let resp = self
            .http
            .get(&url)
            .headers(self.web_headers()?)
            .send()
            .await
            .context("Erro de rede ao acessar https://capybarabr.com/dublagens/upload")?;

        let status = resp.status();
        let final_url = resp.url().to_string();
        let body = resp.text().await?;

        if status.as_u16() == 403 || status.as_u16() == 503 || body.contains("Just a moment...") {
            bail!(
                "Bloqueado pelo Cloudflare (HTTP {}). Exporte cookies recentes incluindo `cf_clearance` e use o mesmo User-Agent do seu Chrome.",
                status
            );
        }

        if final_url.contains("/login") || body.contains("Faça login para acessar") {
            bail!("Sessão não autenticada (redirecionado para /login). Atualize seus cookies.");
        }

        if !status.is_success() {
            bail!("Falha ao acessar /dublagens/upload (HTTP {status}).");
        }

        let csrf_token = extract_csrf_token(&body).ok_or_else(|| {
            anyhow::anyhow!("Autenticado, mas não foi possível encontrar o token CSRF `_token` em /dublagens/upload.")
        })?;

        let announce_url = extract_announce_url(&body);
        let username_hint = extract_username_hint(&body);

        Ok(SessionInfo {
            csrf_token,
            announce_url,
            username_hint,
        })
    }

    /// Verifica se já existem dublagens cadastradas no Acervo de Dublagens (`/dublagens` e RSS de dublagens)
    /// para o título/TMDB informado.
    pub async fn check_dupes(
        &self,
        title_pt_br: &str,
        title_original: &str,
        tmdb_id: &str,
        codec_label: &str,
        source_label: &str,
    ) -> Result<Vec<DupeEntry>> {
        let search_term = if !title_pt_br.trim().is_empty() {
            title_pt_br.trim()
        } else if !title_original.trim().is_empty() {
            title_original.trim()
        } else {
            tmdb_id.trim()
        };

        if search_term.is_empty() {
            bail!("Informe um título ou TMDB ID antes de verificar duplicatas.");
        }

        let mut url = Url::parse(&format!("{BASE_URL}/dublagens"))?;
        url.query_pairs_mut().append_pair("search", search_term);

        let resp = self
            .http
            .get(url)
            .headers(self.web_headers()?)
            .send()
            .await
            .context("Erro de rede ao consultar https://capybarabr.com/dublagens")?;

        let status = resp.status();
        let final_url = resp.url().to_string();
        let body = resp.text().await?;

        if status.as_u16() == 403 || status.as_u16() == 503 || body.contains("Just a moment...") {
            bail!("Bloqueado pelo Cloudflare ao consultar /dublagens (HTTP {status}).");
        }
        if final_url.contains("/login") || body.contains("Faça login para acessar") {
            bail!("Sessão expirada ao consultar /dublagens. Renove seus cookies.");
        }
        if !status.is_success() {
            bail!("Falha ao consultar /dublagens (HTTP {status}).");
        }

        let mut found: Vec<DupeEntry> = Vec::new();

        // 1. Tenta ler o RSS pessoal das dublagens (exposto na sidebar de /dublagens) — XML estruturado e completo
        if let Some(rss_base) = extract_dublagens_rss_url(&body) {
            if let Ok(mut rss_url) = Url::parse(&rss_base) {
                rss_url.query_pairs_mut().append_pair("search", search_term);
                if let Ok(rss_resp) = self
                    .http
                    .get(rss_url)
                    .headers(self.web_headers()?)
                    .send()
                    .await
                {
                    if rss_resp.status().is_success() {
                        if let Ok(xml) = rss_resp.text().await {
                            found.extend(parse_rss_dupes(
                                &xml,
                                title_pt_br,
                                title_original,
                                tmdb_id,
                                codec_label,
                                source_label,
                            ));
                        }
                    }
                }
            }
        }

        // 2. Complementa parseando o HTML de /dublagens diretamente (links /torrents/<id> e linhas)
        let html_dupes = parse_html_dupes(
            &body,
            title_pt_br,
            title_original,
            tmdb_id,
            codec_label,
            source_label,
        );
        for item in html_dupes {
            let already = found.iter().any(|existing| {
                (item.id.is_some() && existing.id == item.id)
                    || existing.title.eq_ignore_ascii_case(&item.title)
            });
            if !already {
                found.push(item);
            }
        }

        Ok(found)
    }

    /// Envia a dublagem via `POST https://capybarabr.com/dublagens` (`multipart/form-data`).
    pub async fn upload_dublagem(
        &self,
        meta: &UploadMetadata,
        torrent_path: &Path,
    ) -> Result<UploadResult> {
        // 1. Obtém _token CSRF fresco
        let session = self.test_session().await?;

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
            .text("_token", session.csrf_token)
            .part("torrent", torrent_part)
            .text("name", meta.release_name.trim().to_string())
            .text("mediainfo", meta.source_mediainfo.clone())
            .text("audio_mediainfo", meta.audio_mediainfo.clone())
            .text("category_id", meta.category.id().to_string())
            .text("tmdb_id", meta.tmdb_id.trim().to_string())
            .text("version", meta.version.form_value().to_string())
            .text("studio", meta.studio.trim().to_string())
            .text("dub_year", meta.dub_year.trim().to_string())
            .text("source", meta.source.form_value().to_string())
            .text("source_label", meta.source_label.trim().to_string())
            .text("fps", meta.fps.trim().to_string())
            .text("note", meta.note.trim().to_string())
            .text("description", build_description_with_signature(&meta.description))
            .text("anon", if meta.anon { "1" } else { "0" }.to_string());

        // `season_number` só deve ser enviado quando categoria == Séries ("15")
        if meta.category == Category::Series {
            form = form.text("season_number", meta.season_number.trim().to_string());
        }

        let mut headers = self.web_headers()?;
        headers.insert(
            REFERER,
            HeaderValue::from_static("https://capybarabr.com/dublagens/upload"),
        );
        headers.insert(
            reqwest::header::ORIGIN,
            HeaderValue::from_static("https://capybarabr.com"),
        );

        let resp = self
            .http
            .post(format!("{BASE_URL}/dublagens"))
            .headers(headers)
            .multipart(form)
            .send()
            .await
            .context("Erro de rede ao enviar formulário para /dublagens")?;

        let status = resp.status();
        let final_url = resp.url().to_string();
        let body = resp.text().await?;

        if status.as_u16() == 419 {
            bail!("Sessão/CSRF expirado (HTTP 419). Renove seus cookies e tente novamente.");
        }
        if status.as_u16() == 403 || status.as_u16() == 503 || body.contains("Just a moment...") {
            bail!(
                "Bloqueado pelo Cloudflare (HTTP {status}). Exporte cookies recentes (com `cf_clearance`) e confira o User-Agent."
            );
        }

        if final_url.contains("/login") {
            bail!("Sessão expirou durante o upload (redirecionado para /login).");
        }

        // Sucesso típico do UNIT3D/Laravel: redireciona para /torrents/<id> ou /dublagens
        if let Some(tid) = extract_torrent_id_from_url(&final_url) {
            return Ok(UploadResult {
                torrent_id: Some(tid.clone()),
                final_url,
                message: format!("Dublagem enviada com sucesso! Torrent ID: {tid}"),
            });
        }

        // Verifica se retornou erros de validação do Laravel
        let validation_errors = extract_laravel_errors(&body);
        if !validation_errors.is_empty() {
            bail!("Erro de validação do CapybaraBR:\n- {}", validation_errors.join("\n- "));
        }

        if final_url.contains("/dublagens/upload") || status.is_client_error() || status.is_server_error() {
            bail!(
                "Upload não concluído (HTTP {status}, URL final: {final_url}). Verifique os campos obrigatórios."
            );
        }

        // Caso tenha redirecionado para /dublagens ou página de sucesso sem ID explícito na URL
        let fallback_id = extract_torrent_id_from_html(&body);
        Ok(UploadResult {
            torrent_id: fallback_id,
            final_url,
            message: "Dublagem enviada com sucesso! (Aguardando moderação no acervo)".to_string(),
        })
    }

    /// Baixa o `.torrent` corrigido pelo servidor usando a API Key do CapybaraBR (`Authorization: Bearer`),
    /// com fallback automático para download autenticado por cookies caso dublagens pendentes não estejam na API.
    pub async fn download_torrent_via_api(
        &self,
        torrent_id: &str,
        dest_path: &Path,
    ) -> Result<DownloadedTorrent> {
        let tid = torrent_id.trim();
        if tid.is_empty() {
            bail!("Torrent ID vazio para download.");
        }

        let mut errors = Vec::new();

        // Tentativa 1: API oficial UNIT3D (GET /api/torrents/<id> -> attributes.download_link)
        if !self.api_key.is_empty() {
            match self.try_download_via_unit3d_api(tid).await {
                Ok(bytes) => {
                    return save_validated_torrent(bytes, dest_path, "API UNIT3D (/api/torrents)");
                }
                Err(e) => errors.push(format!("API /api/torrents/{tid}: {e}")),
            }

            // Tentativa 2: Endpoint direto com Bearer token + confirmado=1
            let direct_url = format!("{BASE_URL}/torrents/download/{tid}?confirmado=1");
            match self.try_download_with_bearer(&direct_url).await {
                Ok(bytes) => {
                    return save_validated_torrent(bytes, dest_path, "API Bearer (/torrents/download)");
                }
                Err(e) => errors.push(format!("Bearer /torrents/download/{tid}: {e}")),
            }
        } else {
            errors.push("API Key do CapybaraBR não configurada".to_string());
        }

        // Tentativa 3 (Fallback): Download autenticado via cookies de sessão
        let web_url = format!("{BASE_URL}/torrents/download/{tid}?confirmado=1");
        match self.try_download_with_cookies(&web_url).await {
            Ok(bytes) => {
                save_validated_torrent(bytes, dest_path, "Cookies de sessão (fallback)")
            }
            Err(e) => {
                errors.push(format!("Cookies fallback: {e}"));
                bail!(
                    "Falha ao baixar o .torrent #{tid} do CapybaraBR:\n- {}",
                    errors.join("\n- ")
                );
            }
        }
    }

    async fn try_download_via_unit3d_api(&self, torrent_id: &str) -> Result<Vec<u8>> {
        let api_url = format!("{BASE_URL}/api/torrents/{torrent_id}");
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.api_key))?,
        );
        h.insert(ACCEPT, HeaderValue::from_static("application/json"));
        h.insert(USER_AGENT, HeaderValue::from_str(&self.user_agent)?);

        let resp = self.http.get(&api_url).headers(h.clone()).send().await?;
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

        let dl_url = if download_link.contains("confirmado=") {
            download_link.to_string()
        } else if download_link.contains('?') {
            format!("{download_link}&confirmado=1")
        } else {
            format!("{download_link}?confirmado=1")
        };

        self.try_download_with_bearer(&dl_url).await
    }

    async fn try_download_with_bearer(&self, url: &str) -> Result<Vec<u8>> {
        let mut h = HeaderMap::new();
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {}", self.api_key))?,
        );
        h.insert(USER_AGENT, HeaderValue::from_str(&self.user_agent)?);

        let resp = self.http.get(url).headers(h).send().await?;
        if !resp.status().is_success() {
            bail!("HTTP {}", resp.status());
        }
        let bytes = resp.bytes().await?.to_vec();
        validate_and_extract_infohash(&bytes)
            .context("Resposta recebida não é um arquivo .torrent Bencode válido")?;
        Ok(bytes)
    }

    async fn try_download_with_cookies(&self, url: &str) -> Result<Vec<u8>> {
        let resp = self
            .http
            .get(url)
            .headers(self.web_headers()?)
            .send()
            .await?;
        if !resp.status().is_success() {
            bail!("HTTP {}", resp.status());
        }
        let bytes = resp.bytes().await?.to_vec();
        validate_and_extract_infohash(&bytes)
            .context("Resposta recebida via cookies não é um .torrent válido")?;
        Ok(bytes)
    }
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

fn extract_csrf_token(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    if let Ok(sel) = Selector::parse(r#"input[name="_token"]"#) {
        if let Some(el) = doc.select(&sel).next() {
            if let Some(val) = el.value().attr("value") {
                if !val.trim().is_empty() {
                    return Some(val.trim().to_string());
                }
            }
        }
    }
    if let Ok(sel) = Selector::parse(r#"meta[name="csrf-token"]"#) {
        if let Some(el) = doc.select(&sel).next() {
            if let Some(val) = el.value().attr("content") {
                if !val.trim().is_empty() {
                    return Some(val.trim().to_string());
                }
            }
        }
    }
    None
}

fn extract_announce_url(html: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"https://capybarabr\.com/announce/[A-Za-z0-9]+").unwrap());
    re.find(html).map(|m| m.as_str().to_string())
}

fn extract_username_hint(html: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"href="https://capybarabr\.com/users/([^"/]+)"#).unwrap());
    re.captures(html).map(|c| c[1].to_string())
}

fn extract_torrent_id_from_url(url: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"/torrents/(\d+)").unwrap());
    re.captures(url).map(|c| c[1].to_string())
}

fn extract_torrent_id_from_html(html: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"/torrents/download/(\d+)").unwrap());
    re.captures(html).map(|c| c[1].to_string())
}

fn extract_laravel_errors(html: &str) -> Vec<String> {
    let doc = Html::parse_document(html);
    let mut errs = Vec::new();
    for css in [".alert-danger li", ".alert-danger", ".invalid-feedback", ".text-danger"] {
        if let Ok(sel) = Selector::parse(css) {
            for el in doc.select(&sel) {
                let txt = el
                    .text()
                    .collect::<Vec<_>>()
                    .join(" ")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                if !txt.is_empty() && txt.len() < 300 && !errs.contains(&txt) {
                    errs.push(txt);
                }
            }
        }
    }
    errs
}

fn extract_dublagens_rss_url(html: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE
        .get_or_init(|| Regex::new(r"https://capybarabr\.com/rss/dublagens\.[A-Za-z0-9_-]+").unwrap());
    re.find(html).map(|m| m.as_str().to_string())
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
    let codec_ok = !codec_label.trim().is_empty() && hay.contains(&codec_label.to_ascii_lowercase());
    let src_ok = !source_label.trim().is_empty() && hay.contains(&source_label.to_ascii_lowercase());
    codec_ok && src_ok
}

pub fn parse_rss_dupes(
    xml: &str,
    title_pt_br: &str,
    title_original: &str,
    tmdb_id: &str,
    codec_label: &str,
    source_label: &str,
) -> Vec<DupeEntry> {
    static RE_ITEM: OnceLock<Regex> = OnceLock::new();
    static RE_TITLE: OnceLock<Regex> = OnceLock::new();
    static RE_LINK: OnceLock<Regex> = OnceLock::new();
    static RE_DESC: OnceLock<Regex> = OnceLock::new();

    let re_item = RE_ITEM.get_or_init(|| Regex::new(r"(?is)<item>(.*?)</item>").unwrap());
    let re_title = RE_TITLE
        .get_or_init(|| Regex::new(r"(?is)<title>(?:<!\[CDATA\[)?(.*?)(?:\]\]>)?</title>").unwrap());
    let re_link = RE_LINK
        .get_or_init(|| Regex::new(r"(?is)<link>(?:<!\[CDATA\[)?(.*?)(?:\]\]>)?</link>").unwrap());
    let re_desc = RE_DESC.get_or_init(|| {
        Regex::new(r"(?is)<description>(?:<!\[CDATA\[)?(.*?)(?:\]\]>)?</description>").unwrap()
    });

    let mut out = Vec::new();
    for cap in re_item.captures_iter(xml) {
        let block = &cap[1];
        let title = re_title
            .captures(block)
            .map(|c| c[1].trim().to_string())
            .unwrap_or_default();
        if title.is_empty() {
            continue;
        }
        let link = re_link.captures(block).map(|c| c[1].trim().to_string());
        let desc = re_desc
            .captures(block)
            .map(|c| c[1].trim().to_string())
            .unwrap_or_default();
        let combined = format!("{title} {desc}");

        if !matches_title_or_tmdb(&combined, title_pt_br, title_original, tmdb_id) {
            continue;
        }

        let id = link
            .as_deref()
            .and_then(|u| extract_torrent_id_from_url(u).or_else(|| extract_torrent_id_from_html(u)));
        let likely_same_release = is_likely_same(&combined, codec_label, source_label);

        out.push(DupeEntry {
            id,
            title,
            details: desc,
            url: link,
            likely_same_release,
        });
    }
    out
}

pub fn parse_html_dupes(
    html: &str,
    title_pt_br: &str,
    title_original: &str,
    tmdb_id: &str,
    codec_label: &str,
    source_label: &str,
) -> Vec<DupeEntry> {
    let doc = Html::parse_document(html);
    let mut out = Vec::new();

    if let Ok(sel) = Selector::parse(r#"a[href*="/torrents/"]"#) {
        for el in doc.select(&sel) {
            let Some(href) = el.value().attr("href") else {
                continue;
            };
            let id = extract_torrent_id_from_url(href)
                .or_else(|| extract_torrent_id_from_html(href));
            let text = el
                .text()
                .collect::<Vec<_>>()
                .join(" ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            if text.len() < 4 || text.eq_ignore_ascii_case("download") {
                continue;
            }
            if !matches_title_or_tmdb(&text, title_pt_br, title_original, tmdb_id) {
                continue;
            }
            let full_url = if href.starts_with("http") {
                href.to_string()
            } else {
                format!("{BASE_URL}{href}")
            };
            let likely_same_release = is_likely_same(&text, codec_label, source_label);
            out.push(DupeEntry {
                id,
                title: text,
                details: String::new(),
                url: Some(full_url),
                likely_same_release,
            });
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_description_signature() {
        let with_body = build_description_with_signature("Fonte: Blu-ray Remux");
        assert!(with_body.starts_with("Fonte: Blu-ray Remux\n\n[right][url=https://github.com/wastaken7/DubAssistant]"));
        assert!(with_body.contains("Compartilhado com DubAssistant"));

        let empty = build_description_with_signature("   ");
        assert!(empty.starts_with("[right][url=https://github.com/wastaken7/DubAssistant]"));
    }

    #[test]
    fn test_parse_rss_dupes() {
        let xml = r#"
        <rss><channel>
            <item>
                <title><![CDATA[As Tartarugas Ninja (Teenage Mutant Ninja Turtles) 2014 AC3 640 5.1 Blu-ray]]></title>
                <link>https://capybarabr.com/torrents/102369</link>
                <description>Cinema - Delart</description>
            </item>
            <item>
                <title>Outro Filme 2020 EAC3 640 5.1 WEB-DL</title>
                <link>https://capybarabr.com/torrents/102370</link>
                <description>Streaming</description>
            </item>
        </channel></rss>
        "#;
        let dupes = parse_rss_dupes(xml, "As Tartarugas Ninja", "Teenage Mutant Ninja Turtles", "98566", "AC3", "Blu-ray");
        assert_eq!(dupes.len(), 1);
        assert_eq!(dupes[0].id.as_deref(), Some("102369"));
        assert!(dupes[0].likely_same_release);
    }
}
