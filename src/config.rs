use anyhow::{Context, Result};
use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TorrentClientType {
    None,
    QBittorrent,
    Transmission,
    Deluge,
    WatchFolder,
}

impl TorrentClientType {
    pub const ALL: [TorrentClientType; 5] = [
        TorrentClientType::None,
        TorrentClientType::QBittorrent,
        TorrentClientType::Transmission,
        TorrentClientType::Deluge,
        TorrentClientType::WatchFolder,
    ];

    pub fn label(self) -> &'static str {
        match self {
            TorrentClientType::None => "Nenhum (somente salvar .torrent)",
            TorrentClientType::QBittorrent => "qBittorrent (WebUI / qui)",
            TorrentClientType::Transmission => "Transmission (RPC)",
            TorrentClientType::Deluge => "Deluge (WebUI JSON-RPC)",
            TorrentClientType::WatchFolder => "Watch Folder (copiar .torrent)",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TorrentClientConfig {
    pub client_type: TorrentClientType,
    pub url: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub api_key: String,
    pub qui_proxy_url: String,
    pub allow_invalid_certs: bool,
    pub category: String,
    pub tags: String,
    pub skip_checking: bool,
    pub paused_on_add: bool,
    pub auto_tmm: bool,
    pub inject_delay_secs: u64,
    pub local_path_prefix: String,
    pub remote_path_prefix: String,
    pub watch_folder: String,
}

impl Default for TorrentClientConfig {
    fn default() -> Self {
        Self {
            client_type: TorrentClientType::None,
            url: "http://127.0.0.1".to_string(),
            port: 8080,
            username: "admin".to_string(),
            password: String::new(),
            api_key: String::new(),
            qui_proxy_url: String::new(),
            allow_invalid_certs: false,
            category: "dublagens".to_string(),
            tags: "CAPYBARABR".to_string(),
            skip_checking: true,
            paused_on_add: false,
            auto_tmm: false,
            inject_delay_secs: 3,
            local_path_prefix: String::new(),
            remote_path_prefix: String::new(),
            watch_folder: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageHostType {
    Pixhost,
    Catbox,
    ImgBB,
    OnlyImage,
    Lensdump,
    Zipline,
    PtScreens,
    PassTheImage,
    LostImg,
}

impl ImageHostType {
    pub const ALL: [ImageHostType; 9] = [
        ImageHostType::Pixhost,
        ImageHostType::Catbox,
        ImageHostType::ImgBB,
        ImageHostType::OnlyImage,
        ImageHostType::Lensdump,
        ImageHostType::PtScreens,
        ImageHostType::PassTheImage,
        ImageHostType::LostImg,
        ImageHostType::Zipline,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ImageHostType::Pixhost => "Pixhost (sem API Key)",
            ImageHostType::Catbox => "Catbox (sem API Key)",
            ImageHostType::ImgBB => "ImgBB (API Key)",
            ImageHostType::OnlyImage => "OnlyImage (API Key)",
            ImageHostType::Lensdump => "Lensdump (API Key)",
            ImageHostType::Zipline => "Zipline (URL + API Key)",
            ImageHostType::PtScreens => "PTScreens (API Key)",
            ImageHostType::PassTheImage => "PassTheImage (API Key)",
            ImageHostType::LostImg => "LostImg (API Key)",
        }
    }

    pub fn needs_api_key(self) -> bool {
        !matches!(self, ImageHostType::Pixhost | ImageHostType::Catbox)
    }

    /// Stable identifier used as the key in `ImageHostConfig::api_keys`.
    /// Never change an existing value: it is persisted in config.json.
    pub fn id(self) -> &'static str {
        match self {
            ImageHostType::Pixhost => "pixhost",
            ImageHostType::Catbox => "catbox",
            ImageHostType::ImgBB => "imgbb",
            ImageHostType::OnlyImage => "onlyimage",
            ImageHostType::Lensdump => "lensdump",
            ImageHostType::Zipline => "zipline",
            ImageHostType::PtScreens => "ptscreens",
            ImageHostType::PassTheImage => "passtheimage",
            ImageHostType::LostImg => "lostimg",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ImageHostConfig {
    /// Host tentado primeiro; os demais configurados servem de fallback.
    pub host_type: ImageHostType,
    /// Campo legado (uma única chave para o host selecionado). Lido apenas para
    /// migração em `migrate_legacy_key`; nunca é gravado de volta.
    #[serde(skip_serializing)]
    pub api_key: String,
    /// API key / userhash por host, indexado por `ImageHostType::id()`.
    pub api_keys: BTreeMap<String, String>,
    pub custom_url: String,
    pub auto_generate_spectrogram: bool,
    pub spectrogram_duration_secs: u32,
}

impl Default for ImageHostConfig {
    fn default() -> Self {
        Self {
            host_type: ImageHostType::Pixhost,
            api_key: String::new(),
            api_keys: BTreeMap::new(),
            custom_url: String::new(),
            auto_generate_spectrogram: false,
            spectrogram_duration_secs: 0,
        }
    }
}

impl ImageHostConfig {
    pub fn key_for(&self, host: ImageHostType) -> &str {
        self.api_keys.get(host.id()).map(|s| s.trim()).unwrap_or("")
    }

    pub fn set_key(&mut self, host: ImageHostType, key: impl Into<String>) {
        self.api_keys.insert(host.id().to_string(), key.into());
    }

    /// Whether `host` has everything it needs to attempt an upload.
    pub fn is_usable(&self, host: ImageHostType) -> bool {
        match host {
            ImageHostType::Pixhost | ImageHostType::Catbox => true,
            ImageHostType::Zipline => {
                !self.custom_url.trim().is_empty() && !self.key_for(host).is_empty()
            }
            _ => !self.key_for(host).is_empty(),
        }
    }

    /// Upload order: the selected host first, then every other usable host
    /// in `ImageHostType::ALL` order as fallbacks.
    pub fn upload_order(&self) -> Vec<ImageHostType> {
        let mut order = vec![self.host_type];
        order.extend(
            ImageHostType::ALL
                .into_iter()
                .filter(|h| *h != self.host_type && self.is_usable(*h)),
        );
        order
    }

    /// Moves the legacy single `api_key` to the host that was selected when it
    /// was saved, without overwriting a per-host key that already exists.
    pub fn migrate_legacy_key(&mut self) {
        let legacy = std::mem::take(&mut self.api_key);
        if !legacy.trim().is_empty() && self.key_for(self.host_type).is_empty() {
            self.set_key(self.host_type, legacy);
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    pub capybara_api_key: String,
    pub announce_url: String,
    pub tmdb_api_key: String,
    pub ffmpeg_path: String,
    pub ffprobe_path: String,
    pub mkvmerge_path: String,
    pub mkvextract_path: String,
    pub mediainfo_path: String,
    pub eac3to_path: String,
    pub default_output_dir: String,
    pub default_anon: bool,
    pub auto_download_and_inject: bool,
    /// Anexa à descrição um relatório de análise do áudio (loudness, canais, banda, clipping, silêncios).
    pub audio_report: bool,
    /// Verifica ao iniciar se há uma versão mais nova nos Releases do GitHub.
    pub check_updates: bool,
    pub torrent_client: TorrentClientConfig,
    pub image_host: ImageHostConfig,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            capybara_api_key: String::new(),
            announce_url: String::new(),
            tmdb_api_key: String::new(),
            ffmpeg_path: detect_binary(&["ffmpeg"]),
            ffprobe_path: detect_binary(&["ffprobe"]),
            mkvmerge_path: detect_binary(&["mkvmerge", r"C:\Program Files\MKVToolNix\mkvmerge.exe"]),
            mkvextract_path: detect_binary(&["mkvextract", r"C:\Program Files\MKVToolNix\mkvextract.exe"]),
            mediainfo_path: detect_binary(&["mediainfo", "MediaInfo"]),
            eac3to_path: detect_binary(&["eac3to"]),
            default_output_dir: default_output_directory(),
            default_anon: false,
            auto_download_and_inject: true,
            audio_report: true,
            check_updates: true,
            torrent_client: TorrentClientConfig::default(),
            image_host: ImageHostConfig::default(),
        }
    }
}

fn default_output_directory() -> String {
    if let Some(proj) = ProjectDirs::from("com", "CapybaraBR", "DubAssistant") {
        let dir = proj.data_local_dir().join("output");
        return dir.to_string_lossy().to_string();
    }
    "output".to_string()
}

pub fn config_file_path() -> Result<PathBuf> {
    let proj = ProjectDirs::from("com", "CapybaraBR", "DubAssistant")
        .context("Não foi possível resolver o diretório de configuração do usuário")?;
    let dir = proj.config_dir();
    std::fs::create_dir_all(dir)?;
    Ok(dir.join("config.json"))
}

pub fn load_config() -> AppConfig {
    let Ok(path) = config_file_path() else {
        return AppConfig::default();
    };
    // Migra automaticamente config antiga de CapybaraDubUploader caso ainda não exista em DubAssistant
    if !path.exists() {
        if let Some(legacy) = ProjectDirs::from("com", "CapybaraBR", "CapybaraDubUploader") {
            let legacy_file = legacy.config_dir().join("config.json");
            if legacy_file.exists() {
                let _ = std::fs::copy(&legacy_file, &path);
            }
        }
    }
    if !path.exists() {
        return AppConfig::default();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => {
            let mut cfg: AppConfig = serde_json::from_str(&content).unwrap_or_default();
            cfg.fill_missing_tools();
            cfg.image_host.migrate_legacy_key();
            cfg
        }
        Err(_) => AppConfig::default(),
    }
}

pub fn save_config(cfg: &AppConfig) -> Result<()> {
    let path = config_file_path()?;
    let json = serde_json::to_string_pretty(cfg)?;
    std::fs::write(path, json)?;
    Ok(())
}

impl AppConfig {
    pub fn fill_missing_tools(&mut self) {
        if self.ffmpeg_path.trim().is_empty() {
            self.ffmpeg_path = detect_binary(&["ffmpeg"]);
        }
        if self.ffprobe_path.trim().is_empty() {
            self.ffprobe_path = detect_binary(&["ffprobe"]);
        }
        if self.mkvmerge_path.trim().is_empty() {
            self.mkvmerge_path = detect_binary(&["mkvmerge", r"C:\Program Files\MKVToolNix\mkvmerge.exe"]);
        }
        if self.mkvextract_path.trim().is_empty() {
            self.mkvextract_path = detect_binary(&["mkvextract", r"C:\Program Files\MKVToolNix\mkvextract.exe"]);
        }
        if self.mediainfo_path.trim().is_empty() {
            self.mediainfo_path = detect_binary(&["mediainfo", "MediaInfo"]);
        }
        if self.eac3to_path.trim().is_empty() {
            self.eac3to_path = detect_binary(&["eac3to"]);
        }
    }

    pub fn tool_exists(bin: &str) -> bool {
        let trimmed = bin.trim();
        if trimmed.is_empty() {
            return false;
        }
        if Path::new(trimmed).exists() {
            return true;
        }
        which_in_path(trimmed).is_some()
    }
}

pub fn detect_binary(candidates: &[&str]) -> String {
    for candidate in candidates {
        if Path::new(candidate).exists() {
            return (*candidate).to_string();
        }
        if let Some(found) = which_in_path(candidate) {
            return found;
        }
    }
    String::new()
}

/// Busca um executável diretamente na variável de ambiente `PATH` em Rust puro
/// (sem spawnar `where.exe`/`which`, evitando piscar janelas de terminal no Windows).
fn which_in_path(cmd: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;

    #[cfg(target_os = "windows")]
    let exts: Vec<String> = {
        let has_ext = Path::new(cmd).extension().is_some();
        if has_ext {
            vec![String::new()]
        } else {
            let pathext = std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".to_string());
            pathext
                .split(';')
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_ascii_lowercase())
                .collect()
        }
    };

    for dir in std::env::split_paths(&path_var) {
        #[cfg(target_os = "windows")]
        {
            for ext in &exts {
                let candidate = if ext.is_empty() {
                    dir.join(cmd)
                } else {
                    dir.join(format!("{cmd}{ext}"))
                };
                if candidate.is_file() {
                    return Some(candidate.to_string_lossy().to_string());
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let candidate = dir.join(cmd);
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod image_host_tests {
    use super::*;

    #[test]
    fn legacy_key_migrates_to_selected_host_and_is_not_saved() {
        let json = r#"{"host_type":"ImgBB","api_key":"abc","custom_url":""}"#;
        let mut ih: ImageHostConfig = serde_json::from_str(json).unwrap();
        ih.migrate_legacy_key();
        assert_eq!(ih.key_for(ImageHostType::ImgBB), "abc");
        assert!(ih.api_key.is_empty());
        let out = serde_json::to_string(&ih).unwrap();
        assert!(!out.contains("\"api_key\""));
        assert!(out.contains("\"imgbb\":\"abc\""));
    }

    #[test]
    fn legacy_key_does_not_overwrite_existing_per_host_key() {
        let json = r#"{"host_type":"ImgBB","api_key":"old","api_keys":{"imgbb":"new"}}"#;
        let mut ih: ImageHostConfig = serde_json::from_str(json).unwrap();
        ih.migrate_legacy_key();
        assert_eq!(ih.key_for(ImageHostType::ImgBB), "new");
    }

    #[test]
    fn upload_order_selected_first_then_usable_fallbacks() {
        let mut ih = ImageHostConfig {
            host_type: ImageHostType::PtScreens,
            ..Default::default()
        };
        ih.set_key(ImageHostType::PtScreens, "k1");
        ih.set_key(ImageHostType::LostImg, "k2");
        ih.set_key(ImageHostType::ImgBB, "  ");
        ih.set_key(ImageHostType::Zipline, "k3"); // sem URL -> inutilizável
        assert_eq!(
            ih.upload_order(),
            vec![
                ImageHostType::PtScreens,
                ImageHostType::Pixhost,
                ImageHostType::Catbox,
                ImageHostType::LostImg,
            ]
        );
    }

    #[test]
    fn selected_host_is_tried_even_without_key() {
        let ih = ImageHostConfig {
            host_type: ImageHostType::ImgBB,
            ..Default::default()
        };
        assert_eq!(ih.upload_order()[0], ImageHostType::ImgBB);
    }
}
