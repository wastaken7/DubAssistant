use crate::cancel::CancelHandle;
use crate::capybara::client::DupeEntry;
use crate::config::{load_config, AppConfig};
use crate::domain::models::{
    Category, DiscKind, ExtractedFolderScan, InputMode, InspectedDisc, InspectedVideo,
    PackageOutput, SourceType, TrackKind, UploadMetadata,
};
use crate::domain::rules::{
    build_official_release_name, detect_source_from_name_and_codec, extract_episode_tag,
    validate_upload_metadata,
};
use crate::imghost::UploadedImage;
use crate::media::audio_report::{remove_report, upsert_report};
use crate::media::disc::disc_query_name;
use crate::media::{detect_input_kind, DetectedInput};
use crate::tmdb::client::{guess_search_query_from_name, PosterImage, TmdbDetails, TmdbSearchItem};
use crate::ui::tabs;
use crate::update::{spawn_update_check, UpdateInfo, CURRENT_VERSION};
use crate::worker::{spawn_worker, UploadPipelineResult, WorkerCommand, WorkerEvent, WorkerHandle};
use eframe::egui;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, TryRecvError};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppTab {
    Main,
    Settings,
}

pub struct CapybaraApp {
    pub config: AppConfig,
    pub active_tab: AppTab,
    pub input_mode: InputMode,
    pub meta: UploadMetadata,

    // Estado de entrada
    pub inspected_videos: Vec<InspectedVideo>,
    pub folder_scan: Option<ExtractedFolderScan>,
    pub inspected_disc: Option<InspectedDisc>,

    // Estado TMDB + Pôster
    pub tmdb_query: String,
    pub tmdb_results: Vec<TmdbSearchItem>,
    pub tmdb_details: Option<TmdbDetails>,
    pub poster_texture: Option<egui::TextureHandle>,
    pending_poster: Option<PosterImage>,
    auto_tmdb_search: bool,

    // Estado de processamento / upload
    pub package_output: Option<PackageOutput>,
    pub spectrogram_image: Option<UploadedImage>,
    pub upload_result: Option<UploadPipelineResult>,
    pub pending_upload_after_package: bool,
    pub confirm_dupe_override: bool,
    pub progress: f32,
    pub busy: bool,
    pub logs: Vec<String>,

    // Status globais
    pub session_status: Option<Result<String, String>>,
    pub dupe_results: Option<Result<Vec<DupeEntry>, String>>,
    pub client_status: Option<Result<String, String>>,
    pub status_banner: Option<(bool, String)>,

    // Aviso de nova versão no GitHub
    update_rx: Option<Receiver<Option<UpdateInfo>>>,
    pub update_available: Option<UpdateInfo>,
    update_dismissed: bool,

    pub cancel: CancelHandle,
    pub worker: WorkerHandle,
}

impl CapybaraApp {
    pub fn new(_cc: &eframe::CreationContext<'_>, initial_paths: Vec<PathBuf>) -> Self {
        let config = load_config();
        let cancel = CancelHandle::new();
        let worker = spawn_worker(cancel.clone());
        let mut meta = UploadMetadata::default();
        meta.anon = config.default_anon;
        let mut app = Self {
            config,
            active_tab: AppTab::Main,
            input_mode: InputMode::VideoFiles,
            meta,
            inspected_videos: Vec::new(),
            folder_scan: None,
            inspected_disc: None,
            tmdb_query: String::new(),
            tmdb_results: Vec::new(),
            tmdb_details: None,
            poster_texture: None,
            pending_poster: None,
            auto_tmdb_search: false,
            package_output: None,
            spectrogram_image: None,
            upload_result: None,
            pending_upload_after_package: false,
            confirm_dupe_override: false,
            progress: 0.0,
            busy: false,
            logs: Vec::new(),
            session_status: None,
            dupe_results: None,
            client_status: None,
            status_banner: None,
            update_rx: None,
            update_available: None,
            update_dismissed: false,
            cancel,
            worker,
        };
        if app.config.check_updates {
            app.update_rx = Some(spawn_update_check());
        }
        if !initial_paths.is_empty() {
            app.load_paths(initial_paths);
        }
        app
    }

    pub fn push_log(&mut self, line: impl Into<String>) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        self.logs.push(format!("[{ts}] {}", line.into()));
    }

    pub fn reset_for_new_upload(&mut self) {
        if self.busy {
            return;
        }
        let mut meta = UploadMetadata::default();
        meta.anon = self.config.default_anon;
        self.meta = meta;
        self.input_mode = InputMode::VideoFiles;
        self.inspected_videos.clear();
        self.folder_scan = None;
        self.inspected_disc = None;
        self.tmdb_query.clear();
        self.tmdb_results.clear();
        self.tmdb_details = None;
        self.poster_texture = None;
        self.pending_poster = None;
        self.auto_tmdb_search = false;
        self.package_output = None;
        self.spectrogram_image = None;
        self.upload_result = None;
        self.pending_upload_after_package = false;
        self.confirm_dupe_override = false;
        self.dupe_results = None;
        self.progress = 0.0;
        self.status_banner = Some((true, "Pronto para um novo upload.".into()));
    }

    pub fn has_loaded_input(&self) -> bool {
        !self.inspected_videos.is_empty()
            || self.folder_scan.is_some()
            || self.inspected_disc.is_some()
    }

    pub fn is_input_ready_for_packaging(&self) -> bool {
        match self.input_mode {
            InputMode::VideoFiles => !self.inspected_videos.is_empty(),
            InputMode::ExtractedFolder => self.folder_scan.is_some(),
            InputMode::RawDisc => self
                .inspected_disc
                .as_ref()
                .map(|d| d.titles.iter().any(|t| t.selected))
                .unwrap_or(false),
        }
    }

    pub fn has_exact_dupe(&self) -> bool {
        matches!(
            &self.dupe_results,
            Some(Ok(list)) if list.iter().any(|d| d.likely_same_release)
        )
    }

    pub fn load_paths(&mut self, paths: Vec<PathBuf>) {
        if self.busy || paths.is_empty() {
            return;
        }
        match detect_input_kind(&paths, false) {
            Ok(detected) => {
                self.reset_for_new_upload();
                self.active_tab = AppTab::Main;
                self.busy = true;
                match detected {
                    DetectedInput::Videos(files) => {
                        self.input_mode = InputMode::VideoFiles;
                        if files.len() > 1 {
                            self.meta.category = Category::Series;
                        }
                        self.status_banner =
                            Some((true, format!("Inspecionando {} vídeo(s)...", files.len())));
                        let _ = self.worker.tx.send(WorkerCommand::InspectVideos {
                            cfg: self.config.clone(),
                            paths: files,
                        });
                    }
                    DetectedInput::RawDisc(path) => {
                        self.input_mode = InputMode::RawDisc;
                        self.status_banner = Some((
                            true,
                            "Analisando disco raw (BDMV / VIDEO_TS / ISO)...".into(),
                        ));
                        let _ = self.worker.tx.send(WorkerCommand::InspectRawDisc {
                            cfg: self.config.clone(),
                            path,
                        });
                    }
                    DetectedInput::ExtractedFolder(folder) => {
                        self.input_mode = InputMode::ExtractedFolder;
                        self.status_banner = Some((true, "Escaneando pasta já extraída...".into()));
                        let _ = self.worker.tx.send(WorkerCommand::ScanExtractedFolder {
                            cfg: self.config.clone(),
                            folder,
                        });
                    }
                }
            }
            Err(e) => {
                let msg = format!("{e:#}");
                self.push_log(format!("[ERRO ENTRADA] {msg}"));
                self.status_banner = Some((false, msg));
            }
        }
    }

    pub fn cancel_current_task(&mut self) {
        if self.busy {
            self.pending_upload_after_package = false;
            self.auto_tmdb_search = false;
            self.push_log("Cancelando tarefa em andamento...".to_string());
            self.cancel.cancel();
            self.status_banner = Some((false, "Cancelando tarefa em andamento...".into()));
        }
    }

    pub fn refresh_auto_release_name(&mut self) {
        if !self.meta.manual_name_override {
            self.meta.release_name = build_official_release_name(&self.meta);
        }
    }

    pub fn trigger_tmdb_search(&mut self, auto: bool) {
        if self.busy || self.tmdb_query.trim().is_empty() {
            return;
        }
        if self.config.tmdb_api_key.trim().is_empty() {
            if !auto {
                self.status_banner = Some((
                    false,
                    "Configure a chave do TMDB na aba Configurações para buscar.".into(),
                ));
            }
            return;
        }
        self.busy = true;
        self.auto_tmdb_search = auto;
        let _ = self.worker.tx.send(WorkerCommand::SearchTmdb {
            api_key: self.config.tmdb_api_key.clone(),
            query: self.tmdb_query.clone(),
            category: self.meta.category,
        });
    }

    pub fn trigger_dupe_check(&mut self) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.dupe_results = None;
        self.confirm_dupe_override = false;
        let _ = self.worker.tx.send(WorkerCommand::CheckDupes {
            cfg: self.config.clone(),
            title_pt_br: self.meta.title_pt_br.clone(),
            title_original: self.meta.title_original.clone(),
            tmdb_id: self.meta.tmdb_id.clone(),
            codec_label: self.meta.audio_codec.title_label().to_string(),
            source_label: self.meta.source.title_label().to_string(),
        });
    }

    /// Lista pendências antes de disparar "Processar e Enviar" (ou "Enviar Dublagem").
    pub fn pre_upload_errors(&self) -> Vec<String> {
        let mut errors = Vec::new();

        if !self.is_input_ready_for_packaging() {
            errors.push("Selecione um arquivo, pasta ou disco de entrada.".to_string());
        }
        if self.config.announce_url.trim().is_empty() {
            errors.push("Announce URL não configurada (aba Configurações).".to_string());
        }
        if self.config.capybara_api_key.trim().is_empty() {
            errors.push("API Key do CapybaraBR não configurada (aba Configurações).".to_string());
        }

        if self.package_output.is_some() {
            errors.extend(validate_upload_metadata(&self.meta));
        } else {
            if self.meta.release_name.trim().is_empty() {
                errors.push("O nome do envio está vazio.".to_string());
            } else if self.meta.release_name.chars().count() > 255 {
                errors.push("O nome do envio excede o limite de 255 caracteres.".to_string());
            }
            if self.meta.tmdb_id.trim().is_empty()
                || !self.meta.tmdb_id.trim().chars().all(|c| c.is_ascii_digit())
            {
                errors.push("TMDB ID é obrigatório e deve conter apenas números.".to_string());
            }
            if self.meta.category == Category::Series
                && (self.meta.season_number.trim().is_empty()
                    || !self
                        .meta
                        .season_number
                        .trim()
                        .chars()
                        .all(|c| c.is_ascii_digit()))
            {
                errors.push("Número da temporada é obrigatório para Séries.".to_string());
            }
            if self.meta.source_mediainfo.trim().is_empty() {
                errors.push("MediaInfo do vídeo fonte é obrigatório.".to_string());
            }
        }

        errors
    }

    pub fn start_packaging(&mut self, auto_upload_after: bool) {
        if self.busy || !self.is_input_ready_for_packaging() {
            return;
        }
        self.busy = true;
        self.progress = 0.0;
        self.package_output = None;
        self.upload_result = None;
        self.pending_upload_after_package = auto_upload_after;
        self.status_banner = Some((
            true,
            if auto_upload_after {
                "Processando mídia e preparando envio automático...".into()
            } else {
                "Processando mídia e gerando .torrent privado...".into()
            },
        ));

        let out_dir = PathBuf::from(self.config.default_output_dir.trim());
        match self.input_mode {
            InputMode::VideoFiles => {
                let _ = self.worker.tx.send(WorkerCommand::PackageVideos {
                    cfg: self.config.clone(),
                    videos: self.inspected_videos.clone(),
                    meta: self.meta.clone(),
                    output_dir: out_dir,
                });
            }
            InputMode::ExtractedFolder => {
                if let Some(scan) = self.folder_scan.clone() {
                    let _ = self.worker.tx.send(WorkerCommand::PackageExtractedFolder {
                        cfg: self.config.clone(),
                        scan,
                        meta: self.meta.clone(),
                        output_dir: out_dir,
                    });
                }
            }
            InputMode::RawDisc => {
                if let Some(disc) = self.inspected_disc.clone() {
                    let _ = self.worker.tx.send(WorkerCommand::PackageRawDisc {
                        cfg: self.config.clone(),
                        disc,
                        meta: self.meta.clone(),
                        output_dir: out_dir,
                    });
                }
            }
        }
    }

    pub fn start_upload(&mut self) {
        if self.busy {
            return;
        }
        let errors = validate_upload_metadata(&self.meta);
        if !errors.is_empty() {
            let msg = format!("Validação falhou antes do upload: {}", errors.join(" | "));
            self.push_log(format!("[ERRO VALIDAÇÃO] {msg}"));
            self.status_banner = Some((false, msg));
            return;
        }
        let Some(pkg) = &self.package_output else {
            return;
        };
        self.busy = true;
        self.upload_result = None;
        self.status_banner = Some((true, "Enviando dublagem via API do CapybaraBR...".into()));
        let _ = self.worker.tx.send(WorkerCommand::UploadAndInject {
            cfg: self.config.clone(),
            meta: self.meta.clone(),
            output_folder: pkg.output_folder.clone(),
            torrent_path: pkg.torrent_path.clone(),
            auto_download_and_inject: self.config.auto_download_and_inject,
        });
    }

    fn poll_update_check(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.update_rx else { return };
        match rx.try_recv() {
            Ok(result) => {
                self.update_rx = None;
                if let Some(info) = result {
                    self.push_log(format!(
                        "Nova versão disponível: {} (atual: v{CURRENT_VERSION}) — {}",
                        info.tag, info.url
                    ));
                    self.update_available = Some(info);
                }
            }
            Err(TryRecvError::Empty) => {
                // Garante que a UI acorde para exibir o aviso mesmo sem interação.
                ctx.request_repaint_after(std::time::Duration::from_millis(500));
            }
            Err(TryRecvError::Disconnected) => self.update_rx = None,
        }
    }

    fn poll_worker_events(&mut self) {
        while let Ok(evt) = self.worker.rx.try_recv() {
            match evt {
                WorkerEvent::Log(msg) => self.push_log(msg),
                WorkerEvent::Progress(p) => self.progress = p,
                WorkerEvent::VideosInspected(res) => {
                    self.busy = false;
                    match res {
                        Ok(videos) => {
                            let mut auto_fetch_tmdb: Option<(u64, Category)> = None;
                            if let Some(first) = videos.first() {
                                if videos.len() > 1 {
                                    self.meta.category = Category::Series;
                                }
                                if let Some((season, _, _)) = extract_episode_tag(&first.path) {
                                    self.meta.category = Category::Series;
                                    self.meta.season_number = season.to_string();
                                }
                                if let Some(fps) = &first.fps {
                                    self.meta.fps = fps.clone();
                                }
                                if let Some(audio) = first
                                    .tracks
                                    .iter()
                                    .find(|t| t.kind == TrackKind::Audio && t.selected)
                                {
                                    if let Some(c) = audio.allowed_audio_codec {
                                        self.meta.audio_codec = c;
                                    }
                                    if let Some(kbps) = audio.bitrate_kbps {
                                        self.meta.bitrate_kbps = kbps.to_string();
                                    }
                                    if let Some(ch) = &audio.channels {
                                        self.meta.channels = ch.clone();
                                    }
                                }
                                let path_hint = first.path.to_string_lossy();
                                if let Some(detected_src) = detect_source_from_name_and_codec(
                                    &path_hint,
                                    Some(self.meta.audio_codec),
                                ) {
                                    self.meta.source = detected_src;
                                }
                                self.meta.source_mediainfo = first.raw_mediainfo.clone();

                                // Aproveita automaticamente a tag TMDB embutida no MKV/MP4 se existir!
                                if let Some(tmdb_id_str) = &first.embedded_tmdb_id {
                                    if let Some(cat) = first.embedded_tmdb_category {
                                        self.meta.category = cat;
                                    }
                                    self.meta.tmdb_id = tmdb_id_str.clone();
                                    self.tmdb_query = tmdb_id_str.clone();
                                    self.push_log(format!(
                                        "Tag TMDB embutida detectada no arquivo: {} ({})",
                                        tmdb_id_str,
                                        self.meta.category.label()
                                    ));
                                    if !self.config.tmdb_api_key.trim().is_empty() {
                                        if let Ok(id_num) = tmdb_id_str.parse::<u64>() {
                                            auto_fetch_tmdb = Some((id_num, self.meta.category));
                                        }
                                    }
                                } else if let Some(stem) = first.path.file_stem() {
                                    self.tmdb_query =
                                        guess_search_query_from_name(&stem.to_string_lossy());
                                }
                            }
                            self.push_log(format!(
                                "Inspeção concluída: {} vídeo(s) carregado(s).",
                                videos.len()
                            ));
                            self.inspected_videos = videos;
                            self.folder_scan = None;
                            self.inspected_disc = None;
                            self.package_output = None;
                            self.refresh_auto_release_name();

                            if let Some((id_num, cat)) = auto_fetch_tmdb {
                                self.busy = true;
                                self.status_banner = Some((
                                    true,
                                    format!(
                                        "Vídeo inspecionado! TMDB #{id_num} detectado na tag — carregando detalhes e pôster..."
                                    ),
                                ));
                                let _ = self.worker.tx.send(WorkerCommand::FetchTmdbDetails {
                                    api_key: self.config.tmdb_api_key.clone(),
                                    id: id_num,
                                    category: cat,
                                });
                            } else if !self.config.tmdb_api_key.trim().is_empty()
                                && !self.tmdb_query.trim().is_empty()
                            {
                                self.status_banner = Some((
                                    true,
                                    format!(
                                        "Vídeo inspecionado! Buscando '{}' no TMDB...",
                                        self.tmdb_query
                                    ),
                                ));
                                self.trigger_tmdb_search(true);
                            } else {
                                self.status_banner =
                                    Some((true, "Vídeo(s) inspecionado(s) com sucesso.".into()));
                            }
                        }
                        Err(e) => {
                            self.push_log(format!("[ERRO] {e}"));
                            self.status_banner = Some((false, e));
                        }
                    }
                }
                WorkerEvent::FolderScanned(res) => {
                    self.busy = false;
                    match res {
                        Ok(scan) => {
                            if scan.audio_files.len() > 1 {
                                self.meta.category = Category::Series;
                            }
                            if let Some(first_audio) = scan.audio_files.first() {
                                if let Some((season, _, _)) = extract_episode_tag(first_audio) {
                                    self.meta.category = Category::Series;
                                    self.meta.season_number = season.to_string();
                                }
                            }
                            if let Some(c) = scan.detected_codec {
                                self.meta.audio_codec = c;
                            }
                            if let Some(detected_src) = detect_source_from_name_and_codec(
                                &scan.root_dir.to_string_lossy(),
                                scan.detected_codec.or(Some(self.meta.audio_codec)),
                            ) {
                                self.meta.source = detected_src;
                            }
                            if let Some(kbps) = scan.detected_bitrate_kbps {
                                self.meta.bitrate_kbps = kbps.to_string();
                            }
                            if let Some(ch) = &scan.detected_channels {
                                self.meta.channels = ch.clone();
                            }
                            if let Some(fps) = &scan.detected_fps {
                                self.meta.fps = fps.clone();
                            }
                            if !scan.source_mediainfo.is_empty() {
                                self.meta.source_mediainfo = scan.source_mediainfo.clone();
                            }
                            if !scan.audio_mediainfo.is_empty() {
                                self.meta.audio_mediainfo = scan.audio_mediainfo.clone();
                            }
                            if let Some(name) = scan.root_dir.file_name() {
                                self.tmdb_query =
                                    guess_search_query_from_name(&name.to_string_lossy());
                            }
                            self.push_log(format!(
                                "Pasta escaneada: {} áudio(s), {} legenda(s), {} proibido(s).",
                                scan.audio_files.len(),
                                scan.subtitle_files.len(),
                                scan.forbidden_files.len()
                            ));
                            self.folder_scan = Some(scan);
                            self.inspected_videos.clear();
                            self.inspected_disc = None;
                            self.package_output = None;
                            self.refresh_auto_release_name();

                            if !self.config.tmdb_api_key.trim().is_empty()
                                && !self.tmdb_query.trim().is_empty()
                            {
                                self.status_banner = Some((
                                    true,
                                    format!(
                                        "Pasta analisada! Buscando '{}' no TMDB...",
                                        self.tmdb_query
                                    ),
                                ));
                                self.trigger_tmdb_search(true);
                            } else {
                                self.status_banner =
                                    Some((true, "Pasta extraída analisada com sucesso.".into()));
                            }
                        }
                        Err(e) => {
                            self.push_log(format!("[ERRO] {e}"));
                            self.status_banner = Some((false, e));
                        }
                    }
                }
                WorkerEvent::DiscInspected(res) => {
                    self.busy = false;
                    match res {
                        Ok(disc) => {
                            self.meta.source = match disc.kind {
                                DiscKind::BluRay => SourceType::Bluray,
                                DiscKind::Dvd => SourceType::Dvd,
                            };
                            if let Some(sel) = disc.titles.iter().find(|t| t.selected) {
                                if let Some(fps) = &sel.fps {
                                    self.meta.fps = fps.clone();
                                }
                                if let Some(audio) = sel
                                    .tracks
                                    .iter()
                                    .find(|t| t.kind == TrackKind::Audio && t.selected)
                                {
                                    if let Some(c) = audio.allowed_audio_codec {
                                        self.meta.audio_codec = c;
                                    }
                                    if let Some(kbps) = audio.bitrate_kbps {
                                        self.meta.bitrate_kbps = kbps.to_string();
                                    }
                                    if let Some(ch) = &audio.channels {
                                        self.meta.channels = ch.clone();
                                    }
                                }
                            }
                            if !disc.raw_mediainfo.is_empty() {
                                self.meta.source_mediainfo = disc.raw_mediainfo.clone();
                            }
                            if let Some(name) = disc_query_name(&disc.root_dir) {
                                self.tmdb_query = guess_search_query_from_name(&name);
                            }
                            self.push_log(format!(
                                "Disco {} analisado: {} playlist(s)/título(s) encontrado(s).",
                                disc.kind.label(),
                                disc.titles.len()
                            ));
                            self.inspected_disc = Some(disc);
                            self.inspected_videos.clear();
                            self.folder_scan = None;
                            self.package_output = None;
                            self.refresh_auto_release_name();

                            if !self.config.tmdb_api_key.trim().is_empty()
                                && !self.tmdb_query.trim().is_empty()
                            {
                                self.status_banner = Some((
                                    true,
                                    format!(
                                        "Disco analisado! Buscando '{}' no TMDB...",
                                        self.tmdb_query
                                    ),
                                ));
                                self.trigger_tmdb_search(true);
                            } else {
                                self.status_banner =
                                    Some((true, "Disco raw analisado com sucesso.".into()));
                            }
                        }
                        Err(e) => {
                            self.push_log(format!("[ERRO DISCO] {e}"));
                            self.status_banner = Some((false, e));
                        }
                    }
                }
                WorkerEvent::TmdbSearchResults(res) => {
                    self.busy = false;
                    let was_auto = self.auto_tmdb_search;
                    self.auto_tmdb_search = false;
                    match res {
                        Ok(items) => {
                            self.push_log(format!(
                                "TMDB: {} resultado(s) encontrado(s).",
                                items.len()
                            ));
                            let auto_pick = if was_auto {
                                items.first().map(|it| (it.id, it.category))
                            } else if items.len() == 1 {
                                Some((items[0].id, items[0].category))
                            } else {
                                None
                            };
                            self.tmdb_results = items;

                            if let Some((id, cat)) = auto_pick {
                                self.busy = true;
                                self.status_banner =
                                    Some((true, format!("Carregando detalhes do TMDB #{id}...")));
                                let _ = self.worker.tx.send(WorkerCommand::FetchTmdbDetails {
                                    api_key: self.config.tmdb_api_key.clone(),
                                    id,
                                    category: cat,
                                });
                            } else if self.tmdb_results.is_empty() {
                                self.status_banner = Some((
                                    false,
                                    "Nenhum resultado encontrado no TMDB. Ajuste o termo de busca."
                                        .into(),
                                ));
                            }
                        }
                        Err(e) => {
                            self.push_log(format!("[ERRO TMDB] {e}"));
                            self.status_banner = Some((false, e));
                        }
                    }
                }
                WorkerEvent::TmdbDetailsLoaded(res) => {
                    self.busy = false;
                    match res {
                        Ok(mut details) => {
                            self.meta.tmdb_id = details.id.to_string();
                            self.meta.title_pt_br = details.title_pt_br.clone();
                            self.meta.title_original = details.title_original.clone();
                            self.meta.year = details.year.clone();
                            self.meta.category = details.category;
                            if self.meta.category == Category::Series {
                                if let Ok(snum) = self.meta.season_number.trim().parse::<u32>() {
                                    if let Some(s) =
                                        details.seasons.iter().find(|s| s.season_number == snum)
                                    {
                                        if !s.year.is_empty() {
                                            self.meta.year = s.year.clone();
                                        }
                                    }
                                }
                            }
                            self.pending_poster = details.poster.take();
                            if self.pending_poster.is_none() {
                                self.poster_texture = None;
                            }
                            self.tmdb_details = Some(details);
                            self.dupe_results = None;
                            self.confirm_dupe_override = false;
                            self.refresh_auto_release_name();
                            self.status_banner = Some((
                                true,
                                format!(
                                    "TMDB #{} carregado: {} ({}).",
                                    self.meta.tmdb_id, self.meta.title_pt_br, self.meta.year
                                ),
                            ));
                            if !self.config.capybara_api_key.trim().is_empty() {
                                self.trigger_dupe_check();
                            }
                        }
                        Err(e) => {
                            self.push_log(format!("[ERRO TMDB] {e}"));
                            self.status_banner = Some((false, e));
                        }
                    }
                }
                WorkerEvent::PackageFinished(res) => {
                    self.busy = false;
                    match res {
                        Ok(out) => {
                            if let Some(kbps) = &out.measured_bitrate_kbps {
                                self.meta.bitrate_kbps = kbps.clone();
                            }
                            if let Some(name) = &out.corrected_release_name {
                                self.meta.release_name = name.clone();
                            }
                            if !out.source_mediainfo.is_empty() {
                                self.meta.source_mediainfo = out.source_mediainfo.clone();
                            }
                            if !out.audio_mediainfo.is_empty() {
                                self.meta.audio_mediainfo = out.audio_mediainfo.clone();
                            }
                            if !out.delay_warnings.is_empty()
                                && !self.meta.description.contains("Delay")
                            {
                                let warn_block = out.delay_warnings.join("\n");
                                if !self.meta.description.is_empty() {
                                    self.meta.description.push_str("\n\n");
                                }
                                self.meta.description.push_str(&warn_block);
                            }
                            // Substitui o relatório anterior (mesmo editado); sem relatório novo
                            // (opção desligada ou análise falhou), remove o antigo para não
                            // enviar medições de outro pacote.
                            match &out.audio_report {
                                Some(report) => upsert_report(&mut self.meta.description, report),
                                None => remove_report(&mut self.meta.description),
                            }
                            let auto_spec_audio =
                                if self.config.image_host.auto_generate_spectrogram {
                                    out.first_audio_path.clone()
                                } else {
                                    None
                                };
                            self.package_output = Some(out);
                            self.spectrogram_image = None;

                            if let Some(audio_path) = auto_spec_audio {
                                self.busy = true;
                                self.status_banner = Some((
                                    true,
                                    "Pacote pronto! Gerando e hospedando espectrograma...".into(),
                                ));
                                let _ = self.worker.tx.send(WorkerCommand::GenerateSpectrogram {
                                    cfg: self.config.clone(),
                                    audio_path,
                                    channels_hint: self.meta.channels.clone(),
                                });
                            } else if self.pending_upload_after_package {
                                self.pending_upload_after_package = false;
                                self.start_upload();
                            } else {
                                self.status_banner = Some((
                                    true,
                                    "Extração/organização e .torrent privado concluídos!".into(),
                                ));
                            }
                        }
                        Err(e) => {
                            self.pending_upload_after_package = false;
                            self.progress = 0.0;
                            self.push_log(format!("[ERRO PROCESSAMENTO] {e}"));
                            self.status_banner = Some((false, e));
                        }
                    }
                }
                WorkerEvent::SpectrogramFinished(res) => {
                    self.busy = false;
                    match res {
                        Ok(uploaded) => {
                            let bbcode = uploaded.to_bbcode();
                            if !self.meta.description.contains(&uploaded.raw_url) {
                                if !self.meta.description.trim().is_empty() {
                                    self.meta.description.push_str("\n\n");
                                }
                                self.meta.description.push_str(&bbcode);
                            }
                            let msg = format!(
                                "Espectrograma hospedado em {} e adicionado à descrição!",
                                uploaded.host_name
                            );
                            self.spectrogram_image = Some(uploaded);
                            self.status_banner = Some((true, msg));

                            if self.pending_upload_after_package {
                                self.pending_upload_after_package = false;
                                self.start_upload();
                            }
                        }
                        Err(e) => {
                            self.pending_upload_after_package = false;
                            self.push_log(format!("[ERRO ESPECTROGRAMA] {e}"));
                            self.status_banner = Some((false, e));
                        }
                    }
                }
                WorkerEvent::ApiKeyTested(res) => {
                    self.busy = false;
                    match res {
                        Ok(msg) => {
                            self.push_log(msg.clone());
                            self.session_status = Some(Ok(msg.clone()));
                            self.status_banner = Some((true, msg));
                        }
                        Err(e) => {
                            self.push_log(format!("[ERRO API KEY] {e}"));
                            self.session_status = Some(Err(e.clone()));
                            self.status_banner = Some((false, e));
                        }
                    }
                }
                WorkerEvent::DupesChecked(res) => {
                    self.busy = false;
                    match &res {
                        Ok(list) if list.is_empty() => {
                            self.push_log(
                                "Dupe Check: nenhuma dublagem pendente encontrada para este título.",
                            );
                        }
                        Ok(list) => {
                            self.push_log(format!(
                                "Dupe Check: {} dublagem(ns) pendente(s) encontrada(s) para esta obra.",
                                list.len()
                            ));
                        }
                        Err(e) => {
                            self.push_log(format!("[AVISO DUPE CHECK] {e}"));
                        }
                    }
                    self.dupe_results = Some(res);
                }
                WorkerEvent::TorrentClientTested(res) => {
                    self.busy = false;
                    match res {
                        Ok(msg) => {
                            self.push_log(msg.clone());
                            self.client_status = Some(Ok(msg.clone()));
                            self.status_banner = Some((true, msg));
                        }
                        Err(e) => {
                            self.push_log(format!("[ERRO CLIENTE] {e}"));
                            self.client_status = Some(Err(e.clone()));
                            self.status_banner = Some((false, e));
                        }
                    }
                }
                WorkerEvent::UploadFinished(res) => {
                    self.busy = false;
                    self.pending_upload_after_package = false;
                    match res {
                        Ok(pipeline) => {
                            let msg = pipeline.upload.message.clone();
                            self.upload_result = Some(pipeline);
                            self.status_banner = Some((true, msg));
                        }
                        Err(e) => {
                            self.push_log(format!("[ERRO UPLOAD] {e}"));
                            self.status_banner = Some((false, e));
                        }
                    }
                }
            }
        }
    }
}

impl eframe::App for CapybaraApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Se o usuário pediu para fechar a janela, cancela imediatamente qualquer tarefa ativa
        if ctx.input(|i| i.viewport().close_requested()) {
            self.cancel.cancel();
        }

        self.poll_worker_events();
        self.poll_update_check(ctx);

        // Suporte nativo a Drag & Drop em qualquer lugar da janela
        let dropped_paths: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped_paths.is_empty() && !self.busy {
            self.load_paths(dropped_paths);
        }

        // Carrega textura do pôster TMDB na GPU quando chegar do worker
        if let Some(poster) = self.pending_poster.take() {
            let color_img = egui::ColorImage::from_rgba_unmultiplied(
                [poster.width, poster.height],
                &poster.rgba,
            );
            self.poster_texture =
                Some(ctx.load_texture("tmdb_poster", color_img, egui::TextureOptions::LINEAR));
        }

        if self.busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        }

        // Barra superior de navegação (apenas Upload e Configurações)
        egui::TopBottomPanel::top("top_nav").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("DubAssistant");
                ui.separator();
                ui.selectable_value(&mut self.active_tab, AppTab::Main, "Upload");
                ui.selectable_value(&mut self.active_tab, AppTab::Settings, "Configurações");

                if self.has_loaded_input() && !self.busy {
                    ui.separator();
                    if ui.button("Novo Upload").clicked() {
                        self.reset_for_new_upload();
                    }
                }

                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if self.busy {
                        let cancel_btn = egui::Button::new(
                            egui::RichText::new("Cancelar").color(egui::Color32::WHITE),
                        )
                        .fill(egui::Color32::from_rgb(190, 50, 50));
                        if ui.add(cancel_btn).clicked() {
                            self.cancel_current_task();
                        }
                        ui.spinner();
                        ui.label("Processando...");
                    } else if let Some(Ok(_)) = &self.session_status {
                        ui.colored_label(egui::Color32::from_rgb(80, 200, 120), "API OK");
                    } else if !self.config.capybara_api_key.trim().is_empty() {
                        ui.colored_label(egui::Color32::GRAY, "API configurada");
                    } else {
                        ui.colored_label(egui::Color32::from_rgb(240, 190, 80), "API Key ausente");
                    }
                });
            });
            ui.add_space(4.0);
        });

        // Aviso de nova versão disponível
        if let (Some(info), false) = (&self.update_available, self.update_dismissed) {
            let mut dismiss = false;
            egui::TopBottomPanel::top("update_banner")
                .frame(
                    egui::Frame::side_top_panel(&ctx.style())
                        .fill(egui::Color32::from_rgb(30, 70, 110)),
                )
                .show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        ui.colored_label(
                            egui::Color32::WHITE,
                            format!(
                                "Nova versão disponível: {} (atual: v{CURRENT_VERSION})",
                                info.tag
                            ),
                        );
                        ui.hyperlink_to("Baixar no GitHub", &info.url);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .small_button("Fechar")
                                .on_hover_text("Dispensar aviso")
                                .clicked()
                            {
                                dismiss = true;
                            }
                        });
                    });
                });
            if dismiss {
                self.update_dismissed = true;
            }
        }

        // Banner de status no rodapé
        egui::TopBottomPanel::bottom("bottom_status").show(ctx, |ui| {
            ui.horizontal(|ui| {
                if let Some((ok, msg)) = &self.status_banner {
                    let color = if *ok {
                        egui::Color32::from_rgb(80, 200, 120)
                    } else {
                        egui::Color32::from_rgb(240, 100, 100)
                    };
                    ui.colored_label(color, msg);
                } else {
                    ui.label("Pronto. Arraste um arquivo/pasta ou selecione abaixo.");
                }
            });
        });

        // Conteúdo principal da aba ativa
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match self.active_tab {
                AppTab::Main => tabs::main_view::show(self, ui),
                AppTab::Settings => tabs::settings::show(self, ui),
            });
        });

        // Overlay visual quando o usuário arrasta arquivos sobre a janela
        let hovering_files = ctx.input(|i| !i.raw.hovered_files.is_empty());
        if hovering_files && !self.busy {
            let screen_rect = ctx.screen_rect();
            let painter = ctx.layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::new("drop_overlay"),
            ));
            painter.rect_filled(
                screen_rect,
                0.0,
                egui::Color32::from_rgba_unmultiplied(20, 40, 65, 200),
            );
            painter.rect_stroke(
                screen_rect.shrink(16.0),
                12.0,
                egui::Stroke::new(3.0_f32, egui::Color32::from_rgb(100, 180, 255)),
            );
            painter.text(
                screen_rect.center(),
                egui::Align2::CENTER_CENTER,
                "Solte o vídeo, pasta de temporada, disco raw ou pasta extraída aqui",
                egui::FontId::proportional(22.0),
                egui::Color32::WHITE,
            );
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Garante que nenhum processo filho (mkvextract/ffmpeg/etc.) fique órfão ao fechar o app
        self.cancel.cancel();
    }
}
