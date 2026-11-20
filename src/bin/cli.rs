use anyhow::{bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use dub_assistant::cancel::CancelHandle;
use dub_assistant::capybara::client::CapybaraClient;
use dub_assistant::clients::{inject_torrent, test_connection};
use dub_assistant::config::{
    config_file_path, load_config, save_config, AppConfig, ImageHostType, TorrentClientType,
};
use dub_assistant::domain::models::{
    AudioCodec, Category, DiscKind, DubVersion, SourceType, TrackKind, UploadMetadata,
};
use dub_assistant::domain::rules::{
    build_official_release_name, detect_source_from_name_and_codec, validate_upload_metadata,
};
use dub_assistant::imghost::upload_image;
use dub_assistant::media::disc::{inspect_raw_disc, process_raw_disc_and_package};
use dub_assistant::media::extractor::{
    organize_extracted_folder_and_package, process_videos_and_package,
};
use dub_assistant::media::inspector::{
    inspect_videos, sanitize_mediainfo_paths, scan_extracted_folder,
};
use dub_assistant::media::spectrogram::generate_audio_spectrogram;
use dub_assistant::tmdb::client::{guess_search_query_from_name, TmdbClient};
use std::path::{Path, PathBuf};

#[derive(Parser, Debug)]
#[command(
    name = "dubassistant-cli",
    version,
    about = "DubAssistant CLI — Extração, organização, dupe-check, espectrograma, upload e injeção de dublagens no CapybaraBR via terminal"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Testa a API Key do CapybaraBR
    TestApi {
        /// API Key específica apenas para este teste
        #[arg(long)]
        api_key: Option<String>,
    },
    /// Testa a conexão com o cliente BitTorrent configurado
    TestClient,
    /// Mostra o caminho do config.json e o status das ferramentas detectadas
    ConfigInfo,
    /// Configura API Key, Announce URL, TMDB, host de imagens e cliente BitTorrent direto pelo terminal
    ConfigSet(ConfigSetArgs),
    /// Inspeciona um vídeo (.mkv/.mp4), pasta de temporada, disco raw (BDMV/VIDEO_TS/.ISO) ou pasta extraída
    Inspect {
        /// Arquivo(s) ou pasta de entrada
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
        /// Força leitura como pasta já extraída
        #[arg(long)]
        extracted: bool,
    },
    /// Verifica duplicatas no Acervo de Dublagens do CapybaraBR
    DupeCheck {
        /// Título em PT-BR (ou termo de busca)
        #[arg(long)]
        title: String,
        /// Título original (opcional)
        #[arg(long, default_value = "")]
        original: String,
        /// TMDB ID (opcional)
        #[arg(long, default_value = "")]
        tmdb: String,
        /// Codec para destacar possível duplicata exata (ex: AC3, EAC3, DTS-HD MA)
        #[arg(long, default_value = "")]
        codec: String,
        /// Fonte para destacar possível duplicata exata (ex: Blu-ray, WEB-DL, DVD)
        #[arg(long, default_value = "")]
        source: String,
    },
    /// Processa, empacota (.torrent privado), faz upload para o CapybaraBR e injeta no cliente BitTorrent
    Upload(UploadArgs),
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum CliCategory {
    Movie,
    Series,
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum CliVersion {
    Cinema,
    Tv,
    Redub,
    Streaming,
    HomeVideo,
    Other,
}

impl From<CliVersion> for DubVersion {
    fn from(v: CliVersion) -> Self {
        match v {
            CliVersion::Cinema => DubVersion::Cinema,
            CliVersion::Tv => DubVersion::Tv,
            CliVersion::Redub => DubVersion::Redub,
            CliVersion::Streaming => DubVersion::Streaming,
            CliVersion::HomeVideo => DubVersion::HomeVideo,
            CliVersion::Other => DubVersion::Other,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum CliSource {
    Bluray,
    Web,
    Dvd,
    Tv,
}

impl From<CliSource> for SourceType {
    fn from(s: CliSource) -> Self {
        match s {
            CliSource::Bluray => SourceType::Bluray,
            CliSource::Web => SourceType::Web,
            CliSource::Dvd => SourceType::Dvd,
            CliSource::Tv => SourceType::Tv,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum CliCodec {
    Ac3,
    Eac3,
    Dts,
    Dtshd,
    Truehd,
    Aac,
    Flac,
    Pcm,
}

impl From<CliCodec> for AudioCodec {
    fn from(c: CliCodec) -> Self {
        match c {
            CliCodec::Ac3 => AudioCodec::Ac3,
            CliCodec::Eac3 => AudioCodec::Eac3,
            CliCodec::Dts => AudioCodec::Dts,
            CliCodec::Dtshd => AudioCodec::DtsHdMa,
            CliCodec::Truehd => AudioCodec::TrueHd,
            CliCodec::Aac => AudioCodec::Aac,
            CliCodec::Flac => AudioCodec::Flac,
            CliCodec::Pcm => AudioCodec::Pcm,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum CliImageHost {
    Pixhost,
    Catbox,
    Imgbb,
    Onlyimage,
    Lensdump,
    Zipline,
}

impl From<CliImageHost> for ImageHostType {
    fn from(h: CliImageHost) -> Self {
        match h {
            CliImageHost::Pixhost => ImageHostType::Pixhost,
            CliImageHost::Catbox => ImageHostType::Catbox,
            CliImageHost::Imgbb => ImageHostType::ImgBB,
            CliImageHost::Onlyimage => ImageHostType::OnlyImage,
            CliImageHost::Lensdump => ImageHostType::Lensdump,
            CliImageHost::Zipline => ImageHostType::Zipline,
        }
    }
}

#[derive(ValueEnum, Clone, Copy, Debug)]
enum CliTorrentClient {
    None,
    Qbittorrent,
    Transmission,
    Deluge,
    Watch,
}

impl From<CliTorrentClient> for TorrentClientType {
    fn from(c: CliTorrentClient) -> Self {
        match c {
            CliTorrentClient::None => TorrentClientType::None,
            CliTorrentClient::Qbittorrent => TorrentClientType::QBittorrent,
            CliTorrentClient::Transmission => TorrentClientType::Transmission,
            CliTorrentClient::Deluge => TorrentClientType::Deluge,
            CliTorrentClient::Watch => TorrentClientType::WatchFolder,
        }
    }
}

#[derive(Args, Debug)]
struct ConfigSetArgs {
    /// API Key do CapybaraBR (para upload via /api/dubs/upload e download do .torrent)
    #[arg(long)]
    capybara_api_key: Option<String>,
    /// Announce URL pessoal do CapybaraBR
    #[arg(long)]
    announce: Option<String>,
    /// API Key v3 ou Bearer Read Access Token do TMDB
    #[arg(long)]
    tmdb_api_key: Option<String>,
    /// Pasta padrão de saída
    #[arg(long)]
    output: Option<PathBuf>,
    /// Define padrão de envio anônimo (true/false)
    #[arg(long)]
    default_anon: Option<bool>,
    /// Host de imagens padrão para espectrogramas
    #[arg(long, value_enum)]
    imghost: Option<CliImageHost>,
    /// API Key / Userhash do host de imagens
    #[arg(long)]
    imghost_key: Option<String>,
    /// URL customizada do Zipline
    #[arg(long)]
    zipline_url: Option<String>,
    /// Gerar espectrograma automaticamente (true/false)
    #[arg(long)]
    auto_spectrogram: Option<bool>,
    /// Duração do espectrograma em segundos (padrão 600)
    #[arg(long)]
    spectrogram_duration: Option<u32>,
    /// Cliente BitTorrent para injeção pós-upload
    #[arg(long, value_enum)]
    client: Option<CliTorrentClient>,
    /// URL/Host do cliente BitTorrent (ex: http://127.0.0.1)
    #[arg(long)]
    client_url: Option<String>,
    /// Porta do cliente BitTorrent (ex: 8080)
    #[arg(long)]
    client_port: Option<u16>,
    /// Usuário do cliente BitTorrent
    #[arg(long)]
    client_user: Option<String>,
    /// Senha do cliente BitTorrent
    #[arg(long)]
    client_pass: Option<String>,
    /// URL do proxy qui (para qBittorrent)
    #[arg(long)]
    qui_proxy_url: Option<String>,
    /// Categoria padrão no cliente BitTorrent
    #[arg(long)]
    client_category: Option<String>,
    /// Tags padrão no cliente BitTorrent
    #[arg(long)]
    client_tags: Option<String>,
    /// Delay em segundos antes da injeção
    #[arg(long)]
    inject_delay: Option<u64>,
    /// Pasta Watch Folder (quando --client watch)
    #[arg(long)]
    watch_folder: Option<PathBuf>,
    /// Mapeamento local -> remoto (prefixo local)
    #[arg(long)]
    local_path_prefix: Option<String>,
    /// Mapeamento local -> remoto (prefixo remoto)
    #[arg(long)]
    remote_path_prefix: Option<String>,
}

#[derive(Args, Debug)]
struct UploadArgs {
    /// Arquivo(s) de vídeo (.mkv/.mp4), pasta de temporada, pasta de disco raw (BDMV/VIDEO_TS/.ISO) ou pasta já extraída
    #[arg(required = true)]
    inputs: Vec<PathBuf>,

    /// Trata a pasta informada como já extraída
    #[arg(long)]
    extracted: bool,

    // --- Identificação / TMDB ---
    /// TMDB ID numérico (opcional se o MKV já possuir tag TMDB ou busca automática)
    #[arg(short = 't', long)]
    tmdb: Option<u64>,

    /// Categoria (auto-detectada se omitida)
    #[arg(short = 'c', long, value_enum)]
    category: Option<CliCategory>,

    /// Número da temporada (para séries, padrão 1)
    #[arg(short = 'S', long, default_value_t = 1)]
    season: u32,

    /// Sobrescreve o título em PT-BR (útil sem TMDB ou para ajuste manual)
    #[arg(long)]
    title_pt: Option<String>,

    /// Sobrescreve o título original
    #[arg(long)]
    title_original: Option<String>,

    /// Sobrescreve o ano da obra (4 dígitos)
    #[arg(long)]
    year: Option<String>,

    // --- Metadados da Dublagem (formulário CapybaraBR) ---
    /// Versão da dublagem
    #[arg(short = 'v', long, alias = "dub-version", value_enum, default_value_t = CliVersion::Cinema)]
    version: CliVersion,

    /// Fonte da mídia (auto-detectada pelo nome/codec/disco se omitida)
    #[arg(short = 's', long, alias = "fonte", value_enum)]
    source: Option<CliSource>,

    /// Rótulo detalhado da fonte (ex: "Blu-ray Remux", "DVD-R NTSC", "VHS")
    #[arg(long, alias = "fonte-label", default_value = "")]
    source_label: String,

    /// Estúdio de dublagem (ex: "Delart", "Herbert Richers")
    #[arg(long, alias = "estudio", default_value = "")]
    studio: String,

    /// Ano da dublagem (4 dígitos, ex: 2014)
    #[arg(long, alias = "ano-dub", default_value = "")]
    dub_year: String,

    /// Sobrescreve o codec exibido no nome oficial
    #[arg(long, value_enum)]
    codec: Option<CliCodec>,

    /// Sobrescreve o bitrate em kbps no nome oficial (ex: 640, 192)
    #[arg(long)]
    bitrate: Option<String>,

    /// Sobrescreve os canais no nome oficial (ex: 2.0, 5.1, 7.1)
    #[arg(long)]
    channels: Option<String>,

    /// FPS manual (sobrescreve detecção automática, ex: 23.976)
    #[arg(long)]
    fps: Option<String>,

    /// Delay de sincronia em ms (ex: 120 ou -80) — anexado automaticamente à descrição
    #[arg(long, allow_hyphen_values = true)]
    delay: Option<i64>,

    /// Release/mídia usada como referência de sincronia (adiciona `FONTE: ...` na descrição)
    #[arg(long, alias = "sync-source")]
    release_source: Option<String>,

    /// Nota curta (máx 255 chars)
    #[arg(long, default_value = "")]
    note: String,

    /// Descrição em BBCode (texto direto)
    #[arg(short = 'd', long, default_value = "")]
    description: String,

    /// Arquivo de texto contendo a descrição em BBCode
    #[arg(long)]
    description_file: Option<PathBuf>,

    /// Sobrescreve manualmente o nome oficial do envio
    #[arg(short = 'n', long)]
    name: Option<String>,

    /// Arquivo mediainfo.txt da fonte (útil no modo --extracted se a pasta não tiver mediainfo.txt)
    #[arg(long)]
    mediainfo_file: Option<PathBuf>,

    /// Arquivo de MediaInfo do áudio extraído (sobrescreve geração automática)
    #[arg(long)]
    audio_mediainfo_file: Option<PathBuf>,

    // --- Seleção de Faixas / Discos ---
    /// ID explícito da faixa de áudio a extrair (sobrescreve auto-seleção PT-BR)
    #[arg(long)]
    audio_track: Option<u32>,

    /// IDs das legendas separados por vírgula (ex: "3,4") ou "none" para nenhuma
    #[arg(long)]
    sub_tracks: Option<String>,

    /// Para discos raw (Blu-ray/DVD): índice(s) da playlist/título separados por vírgula (ex: "1" ou "1,2,3")
    #[arg(long)]
    playlist: Option<String>,

    /// Não extrair chapters.txt mesmo que o arquivo/disco possua capítulos
    #[arg(long)]
    no_chapters: bool,

    // --- Espectrograma & Host de Imagens ---
    /// Gera espectrograma de áudio via ffmpeg, faz upload no host configurado e anexa na descrição
    #[arg(long, alias = "audio-spectrogram")]
    spectrogram: bool,

    /// Duração do espectrograma em segundos (sobrescreve config.json)
    #[arg(long)]
    spectrogram_duration: Option<u32>,

    /// Sobrescreve o host de imagens para esta execução
    #[arg(long, value_enum)]
    imghost: Option<CliImageHost>,

    /// Sobrescreve a API key / userhash do host de imagens para esta execução
    #[arg(long)]
    imghost_key: Option<String>,

    // --- Saída, Auth & Cliente BitTorrent ---
    /// Pasta base de saída (sobrescreve config.json)
    #[arg(short = 'o', long)]
    output: Option<PathBuf>,

    /// Enviar anonimamente
    #[arg(short = 'a', long, conflicts_with = "no_anon")]
    anon: bool,

    /// Não enviar anonimamente (sobrescreve default_anon)
    #[arg(long, conflicts_with = "anon")]
    no_anon: bool,

    /// API Key do CapybaraBR específica para esta execução
    #[arg(long)]
    api_key: Option<String>,

    /// Announce URL específica para esta execução
    #[arg(long)]
    announce: Option<String>,

    /// Sobrescreve o cliente BitTorrent para injeção pós-upload
    #[arg(long, value_enum)]
    client: Option<CliTorrentClient>,

    /// Sobrescreve a categoria no cliente BitTorrent
    #[arg(long)]
    client_category: Option<String>,

    /// Sobrescreve as tags no cliente BitTorrent
    #[arg(long)]
    client_tags: Option<String>,

    /// Sobrescreve o delay de injeção em segundos
    #[arg(long)]
    inject_delay: Option<u64>,

    /// Adiciona o torrent pausado no cliente BitTorrent
    #[arg(long)]
    paused: bool,

    /// Desativa skip_checking / seedMode (força re-check no cliente)
    #[arg(long)]
    no_skip_checking: bool,

    // --- Controle de Execução ---
    /// Pula verificação de duplicatas no Acervo
    #[arg(long)]
    skip_dupe_check: bool,

    /// Prossegue mesmo se o dupe check encontrar possível duplicata exata
    #[arg(long)]
    force: bool,

    /// Executa todo o processamento e gera o .torrent, mas NÃO envia ao CapybaraBR
    #[arg(long, alias = "debug")]
    dry_run: bool,

    /// Não baixa/injeta o .torrent no cliente BitTorrent após o upload
    #[arg(long)]
    no_inject: bool,
}

#[derive(Debug)]
enum DetectedInput {
    Videos(Vec<PathBuf>),
    RawDisc(PathBuf),
    ExtractedFolder(PathBuf),
}

fn detect_input_kind(inputs: &[PathBuf], force_extracted: bool) -> Result<DetectedInput> {
    if inputs.is_empty() {
        bail!("Informe pelo menos um arquivo ou pasta de entrada.");
    }

    if force_extracted {
        if inputs.len() != 1 || !inputs[0].is_dir() {
            bail!("--extracted exige exatamente uma pasta válida.");
        }
        return Ok(DetectedInput::ExtractedFolder(inputs[0].clone()));
    }

    if inputs.len() == 1 {
        let p = &inputs[0];
        if p.is_file() {
            let ext = p
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if ext == "iso" {
                return Ok(DetectedInput::RawDisc(p.clone()));
            }
            return Ok(DetectedInput::Videos(vec![p.clone()]));
        }
        if p.is_dir() {
            if is_raw_disc_dir(p) {
                return Ok(DetectedInput::RawDisc(p.clone()));
            }
            let videos = collect_videos_in_dir(p);
            if !videos.is_empty() {
                return Ok(DetectedInput::Videos(videos));
            }
            return Ok(DetectedInput::ExtractedFolder(p.clone()));
        }
        bail!("Caminho não encontrado: {}", p.display());
    }

    Ok(DetectedInput::Videos(inputs.to_vec()))
}

fn is_raw_disc_dir(p: &Path) -> bool {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_uppercase())
        .unwrap_or_default();
    p.join("BDMV").is_dir()
        || p.join("VIDEO_TS").is_dir()
        || p.join("VIDEO_TS.IFO").is_file()
        || name == "BDMV"
        || name == "VIDEO_TS"
}

fn collect_videos_in_dir(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_file() {
                if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                    let low = ext.to_ascii_lowercase();
                    if low == "mkv" || low == "mp4" {
                        files.push(p);
                    }
                }
            }
        }
    }
    files.sort();
    files
}

fn parse_id_list(raw: &str) -> Result<Vec<u32>> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("none") || trimmed.is_empty() {
        return Ok(Vec::new());
    }
    let mut ids = Vec::new();
    for part in trimmed.split(',') {
        let n: u32 = part
            .trim()
            .parse()
            .with_context(|| format!("ID inválido: '{part}'"))?;
        ids.push(n);
    }
    Ok(ids)
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let cancel = CancelHandle::new();

    // Captura Ctrl+C para encerrar imediatamente processos filhos (mkvextract/ffmpeg/eac3to/mediainfo)
    let cancel_sig = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\n[Ctrl+C] Cancelando tarefa e encerrando processos filhos...");
            cancel_sig.cancel();
        }
    });

    let mut cfg = load_config();

    match cli.command {
        Commands::ConfigInfo => {
            let path = config_file_path()?;
            println!("Arquivo de configuração: {}", path.display());
            println!("Pasta de saída padrão:   {}", cfg.default_output_dir);
            println!("Anônimo padrão:          {}", cfg.default_anon);
            println!("MediaInfo CLI:           {}", tool_status(&cfg.mediainfo_path));
            println!("mkvmerge:                {}", tool_status(&cfg.mkvmerge_path));
            println!("mkvextract:              {}", tool_status(&cfg.mkvextract_path));
            println!("ffmpeg:                  {}", tool_status(&cfg.ffmpeg_path));
            println!("ffprobe:                 {}", tool_status(&cfg.ffprobe_path));
            println!("eac3to (Blu-ray):        {}", tool_status(&cfg.eac3to_path));
            println!(
                "Cliente BitTorrent:      {}",
                cfg.torrent_client.client_type.label()
            );
            println!(
                "Host de Imagens:         {}",
                cfg.image_host.host_type.label()
            );
            Ok(())
        }
        Commands::ConfigSet(a) => {
            apply_config_set(&mut cfg, a)?;
            save_config(&cfg)?;
            println!("Configurações salvas em {}", config_file_path()?.display());
            Ok(())
        }
        Commands::TestApi { api_key } => {
            if let Some(k) = api_key {
                cfg.capybara_api_key = k;
            }
            let client = CapybaraClient::from_config(&cfg)?;
            let msg = client.test_api_key().await?;
            println!("{msg}");
            Ok(())
        }
        Commands::TestClient => {
            let msg = test_connection(&cfg.torrent_client).await?;
            println!("{msg}");
            Ok(())
        }
        Commands::Inspect { inputs, extracted } => {
            run_inspect(&cfg, &inputs, extracted, &cancel)?;
            Ok(())
        }
        Commands::DupeCheck {
            title,
            original,
            tmdb,
            codec,
            source,
        } => {
            let client = CapybaraClient::from_config(&cfg)?;
            let dupes = client
                .check_pending_dupes(&title, &original, &tmdb, &codec, &source)
                .await?;
            if dupes.is_empty() {
                println!("Nenhuma dublagem existente encontrada no Acervo.");
            } else {
                println!("Encontradas {} dublagem(ns) no Acervo:", dupes.len());
                for d in dupes {
                    let tag = if d.likely_same_release {
                        "[POSSÍVEL DUPLICATA]"
                    } else {
                        "[MESMA OBRA]"
                    };
                    println!("  {tag} {} {}", d.title, d.url.as_deref().unwrap_or(""));
                }
            }
            Ok(())
        }
        Commands::Upload(args) => run_upload(&mut cfg, args, &cancel).await,
    }
}

fn apply_config_set(cfg: &mut AppConfig, a: ConfigSetArgs) -> Result<()> {
    if let Some(k) = a.capybara_api_key {
        cfg.capybara_api_key = k;
    }
    if let Some(ann) = a.announce {
        cfg.announce_url = ann;
    }
    if let Some(k) = a.tmdb_api_key {
        cfg.tmdb_api_key = k;
    }
    if let Some(o) = a.output {
        cfg.default_output_dir = o.to_string_lossy().to_string();
    }
    if let Some(anon) = a.default_anon {
        cfg.default_anon = anon;
    }
    if let Some(h) = a.imghost {
        cfg.image_host.host_type = h.into();
    }
    if let Some(k) = a.imghost_key {
        cfg.image_host.api_key = k;
    }
    if let Some(u) = a.zipline_url {
        cfg.image_host.custom_url = u;
    }
    if let Some(v) = a.auto_spectrogram {
        cfg.image_host.auto_generate_spectrogram = v;
    }
    if let Some(d) = a.spectrogram_duration {
        cfg.image_host.spectrogram_duration_secs = d;
    }
    if let Some(c) = a.client {
        cfg.torrent_client.client_type = c.into();
    }
    if let Some(u) = a.client_url {
        cfg.torrent_client.url = u;
    }
    if let Some(p) = a.client_port {
        cfg.torrent_client.port = p;
    }
    if let Some(u) = a.client_user {
        cfg.torrent_client.username = u;
    }
    if let Some(p) = a.client_pass {
        cfg.torrent_client.password = p;
    }
    if let Some(q) = a.qui_proxy_url {
        cfg.torrent_client.qui_proxy_url = q;
    }
    if let Some(c) = a.client_category {
        cfg.torrent_client.category = c;
    }
    if let Some(t) = a.client_tags {
        cfg.torrent_client.tags = t;
    }
    if let Some(d) = a.inject_delay {
        cfg.torrent_client.inject_delay_secs = d;
    }
    if let Some(w) = a.watch_folder {
        cfg.torrent_client.watch_folder = w.to_string_lossy().to_string();
    }
    if let Some(l) = a.local_path_prefix {
        cfg.torrent_client.local_path_prefix = l;
    }
    if let Some(r) = a.remote_path_prefix {
        cfg.torrent_client.remote_path_prefix = r;
    }
    Ok(())
}

fn tool_status(path: &str) -> String {
    if AppConfig::tool_exists(path) {
        format!("OK ({path})")
    } else {
        "NÃO ENCONTRADO".to_string()
    }
}

fn run_inspect(
    cfg: &AppConfig,
    inputs: &[PathBuf],
    force_extracted: bool,
    cancel: &CancelHandle,
) -> Result<()> {
    match detect_input_kind(inputs, force_extracted)? {
        DetectedInput::Videos(paths) => {
            let videos = inspect_videos(cfg, &paths, cancel)?;
            for (i, v) in videos.iter().enumerate() {
                println!(
                    "\n[Vídeo #{}] {} | FPS: {} | Capítulos: {}{}",
                    i + 1,
                    v.path.display(),
                    v.fps.as_deref().unwrap_or("?"),
                    if v.has_chapters { "Sim" } else { "Não" },
                    v.embedded_tmdb_id
                        .as_deref()
                        .map(|id| format!(" | TMDB Tag: {id}"))
                        .unwrap_or_default()
                );
                for t in &v.tracks {
                    if t.kind == TrackKind::Video {
                        continue;
                    }
                    let mark = if t.selected { "[*]" } else { "[ ]" };
                    let kind = if t.kind == TrackKind::Audio {
                        "Áudio"
                    } else {
                        "Legenda"
                    };
                    println!(
                        "  {mark} ID {:>2} | {:<7} | {} | lang={} | ch={} | kbps={} | \"{}\"",
                        t.id,
                        kind,
                        t.allowed_audio_codec
                            .map(|c| format!("{} (.{})", c.title_label(), c.pure_extension()))
                            .unwrap_or_else(|| t.codec_raw.clone()),
                        t.language,
                        t.channels.as_deref().unwrap_or("-"),
                        t.bitrate_kbps
                            .map(|b| b.to_string())
                            .unwrap_or_else(|| "-".into()),
                        t.name
                    );
                }
            }
        }
        DetectedInput::RawDisc(path) => {
            let disc = inspect_raw_disc(cfg, &path, cancel)?;
            println!(
                "Disco detectado: {} ({})",
                disc.kind.label(),
                disc.root_dir.display()
            );
            for t in &disc.titles {
                let mark = if t.selected { "[*]" } else { "[ ]" };
                println!(
                    "\n{mark} {} | FPS: {} | Capítulos: {}",
                    t.label,
                    t.fps.as_deref().unwrap_or("?"),
                    if t.has_chapters { "Sim" } else { "Não" }
                );
                for tr in &t.tracks {
                    if tr.kind == TrackKind::Video {
                        continue;
                    }
                    let tm = if tr.selected { "[*]" } else { "[ ]" };
                    let kind = if tr.kind == TrackKind::Audio {
                        "Áudio"
                    } else {
                        "Legenda"
                    };
                    println!(
                        "    {tm} ID {:>2} | {:<7} | {} | lang={} | ch={}",
                        tr.id,
                        kind,
                        tr.allowed_audio_codec
                            .map(|c| format!("{} (.{})", c.title_label(), c.pure_extension()))
                            .unwrap_or_else(|| tr.codec_raw.clone()),
                        tr.language,
                        tr.channels.as_deref().unwrap_or("-"),
                    );
                }
            }
        }
        DetectedInput::ExtractedFolder(dir) => {
            let scan = scan_extracted_folder(cfg, &dir, cancel)?;
            println!("Pasta extraída: {}", scan.root_dir.display());
            println!("  Áudios permitidos: {}", scan.audio_files.len());
            for a in &scan.audio_files {
                println!("    - {}", a.display());
            }
            println!("  Legendas:          {}", scan.subtitle_files.len());
            println!("  Capítulos:         {}", scan.chapter_files.len());
            println!(
                "  MediaInfo fonte:   {}",
                if scan.mediainfo_file.is_some() {
                    "Sim"
                } else {
                    "Não"
                }
            );
            if !scan.forbidden_files.is_empty() {
                println!(
                    "  Arquivos proibidos (serão ignorados): {}",
                    scan.forbidden_files.len()
                );
            }
        }
    }
    Ok(())
}

async fn run_upload(cfg: &mut AppConfig, args: UploadArgs, cancel: &CancelHandle) -> Result<()> {
    // Aplica overrides temporários de config para esta execução
    if let Some(k) = &args.api_key {
        cfg.capybara_api_key = k.clone();
    }
    if let Some(ann) = &args.announce {
        cfg.announce_url = ann.clone();
    }
    if let Some(h) = args.imghost {
        cfg.image_host.host_type = h.into();
    }
    if let Some(k) = &args.imghost_key {
        cfg.image_host.api_key = k.clone();
    }
    if let Some(d) = args.spectrogram_duration {
        cfg.image_host.spectrogram_duration_secs = d;
    }
    if let Some(c) = args.client {
        cfg.torrent_client.client_type = c.into();
    }
    if let Some(cat) = &args.client_category {
        cfg.torrent_client.category = cat.clone();
    }
    if let Some(tags) = &args.client_tags {
        cfg.torrent_client.tags = tags.clone();
    }
    if let Some(d) = args.inject_delay {
        cfg.torrent_client.inject_delay_secs = d;
    }
    if args.paused {
        cfg.torrent_client.paused_on_add = true;
    }
    if args.no_skip_checking {
        cfg.torrent_client.skip_checking = false;
    }

    let detected = detect_input_kind(&args.inputs, args.extracted)?;
    let mut meta = UploadMetadata::default();
    meta.version = args.version.into();
    meta.source_label = args.source_label.clone();
    meta.studio = args.studio.clone();
    meta.dub_year = args.dub_year.clone();
    meta.note = args.note.clone();
    meta.season_number = args.season.to_string();
    meta.anon = if args.anon {
        true
    } else if args.no_anon {
        false
    } else {
        cfg.default_anon
    };

    if let Some(desc_file) = &args.description_file {
        meta.description = std::fs::read_to_string(desc_file)
            .with_context(|| format!("Falha ao ler --description-file: {}", desc_file.display()))?;
    } else {
        meta.description = args.description.clone();
    }

    if let Some(rel_src) = &args.release_source {
        if !rel_src.trim().is_empty() {
            let line = format!("FONTE: {}", rel_src.trim());
            if !meta.description.trim().is_empty() {
                meta.description.push_str("\n\n");
            }
            meta.description.push_str(&line);
        }
    }
    if let Some(delay) = args.delay {
        if delay != 0 {
            let line = format!("Delay na origem: {delay} ms");
            if !meta.description.trim().is_empty() {
                meta.description.push_str("\n\n");
            }
            meta.description.push_str(&line);
        }
    }

    // 1. Inspeção de mídia
    let mut videos_opt = None;
    let mut disc_opt = None;
    let mut scan_opt = None;
    let mut search_hint = String::new();
    let mut embedded_tmdb: Option<(Option<Category>, u64)> = None;

    match detected {
        DetectedInput::Videos(paths) => {
            println!("[1/5] Inspecionando {} vídeo(s)...", paths.len());
            let mut videos = inspect_videos(cfg, &paths, cancel)?;
            apply_cli_track_overrides(&mut videos, args.audio_track, args.sub_tracks.as_deref())?;
            if args.no_chapters {
                for v in &mut videos {
                    v.has_chapters = false;
                }
            }

            if videos.len() > 1 {
                meta.category = Category::Series;
            }
            if let Some(first) = videos.first() {
                if let Some(fps) = &first.fps {
                    meta.fps = fps.clone();
                }
                if let Some(audio) = first
                    .tracks
                    .iter()
                    .find(|t| t.kind == TrackKind::Audio && t.selected)
                {
                    if let Some(c) = audio.allowed_audio_codec {
                        meta.audio_codec = c;
                    }
                    if let Some(kbps) = audio.bitrate_kbps {
                        meta.bitrate_kbps = kbps.to_string();
                    }
                    if let Some(ch) = &audio.channels {
                        meta.channels = ch.clone();
                    }
                }
                if let Some(src) = detect_source_from_name_and_codec(
                    &first.path.to_string_lossy(),
                    Some(meta.audio_codec),
                ) {
                    meta.source = src;
                }
                meta.source_mediainfo = first.raw_mediainfo.clone();
                if let Some(id_str) = &first.embedded_tmdb_id {
                    if let Ok(id_num) = id_str.parse::<u64>() {
                        embedded_tmdb = Some((first.embedded_tmdb_category, id_num));
                    }
                }
                if let Some(stem) = first.path.file_stem() {
                    search_hint = guess_search_query_from_name(&stem.to_string_lossy());
                }
            }
            videos_opt = Some(videos);
        }
        DetectedInput::RawDisc(path) => {
            println!("[1/5] Analisando disco raw: {}...", path.display());
            let mut disc = inspect_raw_disc(cfg, &path, cancel)?;
            if let Some(pl_raw) = &args.playlist {
                let wanted = parse_id_list(pl_raw)?;
                for t in &mut disc.titles {
                    t.selected = wanted.contains(&t.index);
                }
            }
            if args.no_chapters {
                for t in &mut disc.titles {
                    t.chapters_track_id = None;
                    t.chapters_text = None;
                }
            }
            if args.audio_track.is_some() || args.sub_tracks.is_some() {
                let wanted_subs = match args.sub_tracks.as_deref() {
                    Some(s) => Some(parse_id_list(s)?),
                    None => None,
                };
                for t in disc.titles.iter_mut().filter(|t| t.selected) {
                    if let Some(aid) = args.audio_track {
                        for tr in t.tracks.iter_mut().filter(|tr| tr.kind == TrackKind::Audio) {
                            tr.selected = tr.id == aid;
                        }
                    }
                    if let Some(ref sids) = wanted_subs {
                        for tr in t
                            .tracks
                            .iter_mut()
                            .filter(|tr| tr.kind == TrackKind::Subtitle)
                        {
                            tr.selected = sids.contains(&tr.id);
                        }
                    }
                }
            }

            let sel_count = disc.titles.iter().filter(|t| t.selected).count();
            if sel_count == 0 {
                bail!("Nenhuma playlist/título selecionado no disco.");
            }
            if sel_count > 1 {
                meta.category = Category::Series;
            }
            meta.source = match disc.kind {
                DiscKind::BluRay => SourceType::Bluray,
                DiscKind::Dvd => SourceType::Dvd,
            };
            if let Some(first) = disc.titles.iter().find(|t| t.selected) {
                if let Some(fps) = &first.fps {
                    meta.fps = fps.clone();
                }
                if let Some(audio) = first
                    .tracks
                    .iter()
                    .find(|t| t.kind == TrackKind::Audio && t.selected)
                {
                    if let Some(c) = audio.allowed_audio_codec {
                        meta.audio_codec = c;
                    }
                    if let Some(kbps) = audio.bitrate_kbps {
                        meta.bitrate_kbps = kbps.to_string();
                    }
                    if let Some(ch) = &audio.channels {
                        meta.channels = ch.clone();
                    }
                }
            }
            meta.source_mediainfo = disc.raw_mediainfo.clone();
            if let Some(name) = disc.root_dir.file_name() {
                search_hint = guess_search_query_from_name(&name.to_string_lossy());
            }
            disc_opt = Some(disc);
        }
        DetectedInput::ExtractedFolder(dir) => {
            println!("[1/5] Escaneando pasta extraída: {}...", dir.display());
            let scan = scan_extracted_folder(cfg, &dir, cancel)?;
            if scan.audio_files.len() > 1 {
                meta.category = Category::Series;
            }
            if let Some(c) = scan.detected_codec {
                meta.audio_codec = c;
            }
            if let Some(kbps) = scan.detected_bitrate_kbps {
                meta.bitrate_kbps = kbps.to_string();
            }
            if let Some(ch) = &scan.detected_channels {
                meta.channels = ch.clone();
            }
            if let Some(fps) = &scan.detected_fps {
                meta.fps = fps.clone();
            }
            if let Some(src) = detect_source_from_name_and_codec(
                &scan.root_dir.to_string_lossy(),
                Some(meta.audio_codec),
            ) {
                meta.source = src;
            }
            meta.source_mediainfo = scan.source_mediainfo.clone();
            meta.audio_mediainfo = scan.audio_mediainfo.clone();
            if let Some(name) = scan.root_dir.file_name() {
                search_hint = guess_search_query_from_name(&name.to_string_lossy());
            }
            scan_opt = Some(scan);
        }
    }

    // Overrides explícitos da CLI
    if let Some(cat) = args.category {
        meta.category = match cat {
            CliCategory::Movie => Category::Movie,
            CliCategory::Series => Category::Series,
        };
    } else if let Some((Some(emb_cat), _)) = embedded_tmdb {
        meta.category = emb_cat;
    }
    if let Some(src) = args.source {
        meta.source = src.into();
    }
    if let Some(c) = args.codec {
        meta.audio_codec = c.into();
    }
    if let Some(b) = args.bitrate {
        meta.bitrate_kbps = b;
    }
    if let Some(ch) = args.channels {
        meta.channels = ch;
    }
    if let Some(fps) = args.fps {
        meta.fps = fps;
    }
    if let Some(mi_file) = &args.mediainfo_file {
        let raw = std::fs::read_to_string(mi_file)
            .with_context(|| format!("Falha ao ler --mediainfo-file: {}", mi_file.display()))?;
        meta.source_mediainfo = sanitize_mediainfo_paths(&raw);
    }
    if let Some(ami_file) = &args.audio_mediainfo_file {
        let raw = std::fs::read_to_string(ami_file)
            .with_context(|| format!("Falha ao ler --audio-mediainfo-file: {}", ami_file.display()))?;
        meta.audio_mediainfo = sanitize_mediainfo_paths(&raw);
    }

    // 2. TMDB (ou overrides manuais completos se --title-pt + --tmdb forem passados sem chave TMDB)
    let tmdb_id_resolved = args.tmdb.or_else(|| embedded_tmdb.map(|(_, id)| id));
    if !cfg.tmdb_api_key.trim().is_empty() {
        let tmdb_client = TmdbClient::new(&cfg.tmdb_api_key)?;
        let details = if let Some(id) = tmdb_id_resolved {
            println!("[2/5] Carregando metadados do TMDB #{id}...");
            tmdb_client.get_details(id, meta.category).await?
        } else {
            println!("[2/5] Buscando no TMDB por '{search_hint}'...");
            let results = tmdb_client.search(&search_hint, meta.category).await?;
            let first = results.first().ok_or_else(|| {
                anyhow::anyhow!(
                    "Nenhum resultado no TMDB para '{search_hint}'. Informe --tmdb <ID>."
                )
            })?;
            println!(
                "  -> Selecionado: [{}] {} ({}) — {}",
                first.id, first.title_pt_br, first.title_original, first.year
            );
            tmdb_client.get_details(first.id, meta.category).await?
        };
        meta.tmdb_id = details.id.to_string();
        meta.title_pt_br = details.title_pt_br.clone();
        meta.title_original = details.title_original.clone();
        meta.year = details.year.clone();
        if meta.category == Category::Series {
            if let Some(s) = details
                .seasons
                .iter()
                .find(|s| s.season_number == args.season)
            {
                if !s.year.is_empty() {
                    meta.year = s.year.clone();
                }
            }
        }
    } else if let Some(id) = tmdb_id_resolved {
        meta.tmdb_id = id.to_string();
    } else {
        bail!("Chave TMDB não configurada e --tmdb não informado. Configure via `dubassistant-cli config-set --tmdb-api-key ...` ou informe --tmdb e --title-pt.");
    }

    if let Some(t) = args.title_pt {
        meta.title_pt_br = t;
    }
    if let Some(t) = args.title_original {
        meta.title_original = t;
    }
    if let Some(y) = args.year {
        meta.year = y;
    }

    meta.release_name = match args.name {
        Some(custom) => custom,
        None => build_official_release_name(&meta),
    };
    println!("  -> Nome oficial: {}", meta.release_name);

    // 3. Cliente CapybaraBR + Dupe Check de pendentes (a API também recusa dupes no envio via HTTP 422)
    let capy_client = if !args.dry_run || !args.skip_dupe_check {
        Some(CapybaraClient::from_config(cfg)?)
    } else {
        None
    };

    if !args.skip_dupe_check {
        if let Some(client) = &capy_client {
            println!("[3/5] Verificando pendentes via API...");
            match client
                .check_pending_dupes(
                    &meta.title_pt_br,
                    &meta.title_original,
                    &meta.tmdb_id,
                    meta.audio_codec.title_label(),
                    meta.source.title_label(),
                )
                .await
            {
                Ok(dupes) if dupes.is_empty() => {
                    println!("  -> OK: nenhuma duplicata encontrada.");
                }
                Ok(dupes) => {
                    let has_exact = dupes.iter().any(|d| d.likely_same_release);
                    for d in &dupes {
                        let tag = if d.likely_same_release {
                            "[POSSÍVEL DUPLICATA]"
                        } else {
                            "[MESMA OBRA]"
                        };
                        println!("  {tag} {}", d.title);
                    }
                    if has_exact && !args.force {
                        bail!("Possível duplicata encontrada no Acervo. Revise acima ou use --force para continuar.");
                    }
                }
                Err(e) => {
                    eprintln!("  [AVISO] Dupe check falhou: {e}");
                }
            }
        }
    }

    // 4. Extração / Organização + .torrent privado
    let out_dir = args
        .output
        .unwrap_or_else(|| PathBuf::from(cfg.default_output_dir.trim()));
    println!(
        "[4/5] Processando arquivos e gerando .torrent privado em {}...",
        out_dir.display()
    );

    let pkg = if let Some(videos) = videos_opt {
        process_videos_and_package(
            cfg,
            &videos,
            &meta,
            &out_dir,
            cancel,
            |m| println!("  {m}"),
            |_| {},
        )?
    } else if let Some(disc) = disc_opt {
        process_raw_disc_and_package(
            cfg,
            &disc,
            &meta,
            &out_dir,
            cancel,
            |m| println!("  {m}"),
            |_| {},
        )?
    } else if let Some(scan) = scan_opt {
        organize_extracted_folder_and_package(
            cfg,
            &scan,
            &meta,
            &out_dir,
            cancel,
            |m| println!("  {m}"),
            |_| {},
        )?
    } else {
        bail!("Estado interno inválido: nenhuma entrada carregada.");
    };

    if args.mediainfo_file.is_none() && !pkg.source_mediainfo.is_empty() {
        meta.source_mediainfo = pkg.source_mediainfo.clone();
    }
    if args.audio_mediainfo_file.is_none() && !pkg.audio_mediainfo.is_empty() {
        meta.audio_mediainfo = pkg.audio_mediainfo.clone();
    }
    if !pkg.delay_warnings.is_empty() {
        let warn_block = pkg.delay_warnings.join("\n");
        if !meta.description.trim().is_empty() {
            meta.description.push_str("\n\n");
        }
        meta.description.push_str(&warn_block);
    }

    // Espectrograma opcional
    if args.spectrogram || cfg.image_host.auto_generate_spectrogram {
        if let Some(audio_path) = &pkg.first_audio_path {
            println!(
                "  Gerando espectrograma via ffmpeg ({}s)...",
                cfg.image_host.spectrogram_duration_secs
            );
            let png = generate_audio_spectrogram(
                cfg,
                audio_path,
                &meta.channels,
                cfg.image_host.spectrogram_duration_secs,
                cancel,
            )?;
            println!(
                "  Enviando espectrograma para {}...",
                cfg.image_host.host_type.label()
            );
            let uploaded = upload_image(&png, &cfg.image_host).await?;
            println!("  Espectrograma hospedado: {}", uploaded.raw_url);
            if !meta.description.trim().is_empty() {
                meta.description.push_str("\n\n");
            }
            meta.description.push_str(&uploaded.to_bbcode());
        }
    }

    let val_errors = validate_upload_metadata(&meta);
    if !val_errors.is_empty() {
        bail!(
            "Validação falhou antes do envio:\n- {}",
            val_errors.join("\n- ")
        );
    }

    if args.dry_run {
        println!("\n[DRY-RUN] Concluído sem enviar ao site!");
        println!("  Nome:     {}", meta.release_name);
        println!("  Pasta:    {}", pkg.output_folder.display());
        println!("  Torrent:  {}", pkg.torrent_path.display());
        println!("  InfoHash: {}", pkg.infohash);
        return Ok(());
    }

    // 5. Upload + Download via API + Injeção no Cliente BitTorrent
    let client = capy_client.unwrap_or(CapybaraClient::from_config(cfg)?);
    println!("[5/5] Enviando via API oficial (POST /api/dubs/upload)...");
    let upload = client.upload_dublagem(&meta, &pkg.torrent_path).await?;
    println!("  {}", upload.message);
    println!("  URL: {}", upload.final_url);

    if !args.no_inject && cfg.auto_download_and_inject {
        let tid_str = upload.torrent_id.as_deref().unwrap_or("");
        if !tid_str.is_empty() || upload.download_link.is_some() {
            println!("  Baixando .torrent #{tid_str} corrigido via API...");
            let dest_torrent = pkg.torrent_path.with_file_name(format!(
                "[CAPYBARABR]_{}.torrent",
                if tid_str.is_empty() { "dub" } else { tid_str }
            ));
            let dl = client
                .download_torrent_via_api(tid_str, upload.download_link.as_deref(), &dest_torrent)
                .await?;
            println!(
                "  .torrent baixado ({}) | InfoHash: {}",
                dl.source_used, dl.infohash
            );
            let parent_dir = pkg.output_folder.parent().unwrap_or(&pkg.output_folder);
            let fname = dl
                .path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("{tid_str}.torrent"));
            let inj = inject_torrent(
                &dl.bytes,
                &fname,
                &dl.infohash,
                parent_dir,
                &cfg.torrent_client,
            )
            .await?;
            println!("  Cliente BitTorrent: {inj}");
        }
    }

    Ok(())
}

fn apply_cli_track_overrides(
    videos: &mut [dub_assistant::domain::models::InspectedVideo],
    audio_track: Option<u32>,
    sub_tracks: Option<&str>,
) -> Result<()> {
    let wanted_subs = match sub_tracks {
        Some(s) => Some(parse_id_list(s)?),
        None => None,
    };
    for v in videos.iter_mut() {
        if let Some(aid) = audio_track {
            for t in v.tracks.iter_mut().filter(|t| t.kind == TrackKind::Audio) {
                t.selected = t.id == aid;
            }
        }
        if let Some(ref sids) = wanted_subs {
            for t in v.tracks.iter_mut().filter(|t| t.kind == TrackKind::Subtitle) {
                t.selected = sids.contains(&t.id);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cli_parse_upload_args_full() {
        let cli = Cli::try_parse_from([
            "dubassistant-cli",
            "upload",
            "filme.mkv",
            "-t",
            "1368337",
            "-v",
            "streaming",
            "-s",
            "web",
            "--estudio",
            "Delart",
            "--ano-dub",
            "2026",
            "--fonte-label",
            "AMZN WEB-DL",
            "--release-source",
            "The.Odyssey.2026.1080p.AMZN.WEB-DL-BiOMA",
            "--delay",
            "-120",
            "--codec",
            "eac3",
            "--bitrate",
            "640",
            "--channels",
            "5.1",
            "--imghost",
            "pixhost",
            "--spectrogram",
            "--client",
            "qbittorrent",
            "--client-category",
            "dublagens",
            "-a",
            "--debug",
        ])
        .unwrap();

        match cli.command {
            Commands::Upload(u) => {
                assert_eq!(u.tmdb, Some(1368337));
                assert_eq!(u.studio, "Delart");
                assert_eq!(u.dub_year, "2026");
                assert_eq!(u.source_label, "AMZN WEB-DL");
                assert_eq!(u.delay, Some(-120));
                assert!(u.spectrogram);
                assert!(u.anon);
                assert!(u.dry_run);
            }
            _ => panic!("Esperado subcomando upload"),
        }
    }

    #[test]
    fn test_cli_parse_config_set() {
        let cli = Cli::try_parse_from([
            "dubassistant-cli",
            "config-set",
            "--capybara-api-key",
            "cbr_key",
            "--announce",
            "https://capybarabr.com/announce/dummy",
            "--tmdb-api-key",
            "abc123",
            "--client",
            "qbittorrent",
            "--client-port",
            "8080",
        ])
        .unwrap();

        assert!(matches!(cli.command, Commands::ConfigSet(_)));
    }
}
