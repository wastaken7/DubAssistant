use crate::cancel::CancelHandle;
use crate::capybara::client::DupeEntry;
use crate::capybara::cookies::load_cookie_header;
use crate::config::{load_config, save_config, AppConfig};
use crate::domain::models::{
    Category, DiscKind, ExtractedFolderScan, InputMode, InspectedDisc, InspectedVideo,
    PackageOutput, SourceType, TrackKind, UploadMetadata,
};
use crate::domain::rules::{build_official_release_name, detect_source_from_name_and_codec};
use crate::imghost::UploadedImage;
use crate::tmdb::client::{guess_search_query_from_name, PosterImage, TmdbDetails, TmdbSearchItem};
use crate::ui::tabs;
use crate::worker::{
    spawn_worker, UploadPipelineResult, WorkerCommand, WorkerEvent, WorkerHandle,
};
use eframe::egui;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppTab {
    Input,
    Metadata,
    Process,
    Upload,
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

    // Estado de processamento / upload
    pub package_output: Option<PackageOutput>,
    pub spectrogram_image: Option<UploadedImage>,
    pub upload_result: Option<UploadPipelineResult>,
    pub progress: f32,
    pub busy: bool,
    pub logs: Vec<String>,

    // Status globais
    pub session_status: Option<Result<String, String>>,
    pub dupe_results: Option<Result<Vec<DupeEntry>, String>>,
    pub client_status: Option<Result<String, String>>,
    pub status_banner: Option<(bool, String)>,

    pub cancel: CancelHandle,
    pub worker: WorkerHandle,
}

impl CapybaraApp {
    pub fn new(_cc: &eframe::CreationContext<'_>) -> Self {
        let config = load_config();
        let cancel = CancelHandle::new();
        let worker = spawn_worker(cancel.clone());
        let mut meta = UploadMetadata::default();
        meta.anon = config.default_anon;
        Self {
            config,
            active_tab: AppTab::Input,
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
            package_output: None,
            spectrogram_image: None,
            upload_result: None,
            progress: 0.0,
            busy: false,
            logs: Vec::new(),
            session_status: None,
            dupe_results: None,
            client_status: None,
            status_banner: None,
            cancel,
            worker,
        }
    }

    pub fn push_log(&mut self, line: impl Into<String>) {
        let ts = chrono::Local::now().format("%H:%M:%S");
        self.logs.push(format!("[{ts}] {}", line.into()));
    }

    pub fn cancel_current_task(&mut self) {
        if self.busy {
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

    pub fn trigger_dupe_check(&mut self) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.dupe_results = None;
        let _ = self.worker.tx.send(WorkerCommand::CheckDupes {
            cfg: self.config.clone(),
            title_pt_br: self.meta.title_pt_br.clone(),
            title_original: self.meta.title_original.clone(),
            tmdb_id: self.meta.tmdb_id.clone(),
            codec_label: self.meta.audio_codec.title_label().to_string(),
            source_label: self.meta.source.title_label().to_string(),
        });
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
                            self.status_banner =
                                Some((true, "Pasta extraída analisada com sucesso.".into()));
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
                            let name_for_query = if disc
                                .root_dir
                                .file_name()
                                .map(|n| n.to_string_lossy().eq_ignore_ascii_case("VIDEO_TS"))
                                .unwrap_or(false)
                            {
                                disc.root_dir
                                    .parent()
                                    .and_then(|p| p.file_name())
                                    .map(|n| n.to_string_lossy().to_string())
                            } else {
                                disc.root_dir
                                    .file_name()
                                    .map(|n| n.to_string_lossy().to_string())
                            };
                            if let Some(name) = name_for_query {
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
                            self.status_banner =
                                Some((true, "Disco raw analisado com sucesso.".into()));
                        }
                        Err(e) => {
                            self.push_log(format!("[ERRO DISCO] {e}"));
                            self.status_banner = Some((false, e));
                        }
                    }
                }
                WorkerEvent::TmdbSearchResults(res) => {
                    self.busy = false;
                    match res {
                        Ok(items) => {
                            self.push_log(format!(
                                "TMDB: {} resultado(s) encontrado(s).",
                                items.len()
                            ));
                            self.tmdb_results = items;
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
                            self.pending_poster = details.poster.take();
                            if self.pending_poster.is_none() {
                                self.poster_texture = None;
                            }
                            self.tmdb_details = Some(details);
                            self.dupe_results = None;
                            self.refresh_auto_release_name();
                            self.status_banner = Some((
                                true,
                                format!(
                                    "TMDB #{} carregado: {} ({}).",
                                    self.meta.tmdb_id, self.meta.title_pt_br, self.meta.year
                                ),
                            ));
                            if load_cookie_header(&self.config).is_ok()
                                || !self.config.capybara_api_key.trim().is_empty()
                            {
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
                            let auto_spec_audio = if self.config.image_host.auto_generate_spectrogram {
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
                            } else {
                                self.status_banner = Some((
                                    true,
                                    "Extração/organização e .torrent privado concluídos!".into(),
                                ));
                            }
                        }
                        Err(e) => {
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
                            if !self.meta.description.contains(&uploaded.img_url) {
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
                        }
                        Err(e) => {
                            self.push_log(format!("[ERRO ESPECTROGRAMA] {e}"));
                            self.status_banner = Some((false, e));
                        }
                    }
                }
                WorkerEvent::SessionTested(res) => {
                    self.busy = false;
                    match res {
                        Ok(info) => {
                            if let Some(ann) = &info.announce_url {
                                self.config.announce_url = ann.clone();
                                let _ = save_config(&self.config);
                            }
                            let who = info
                                .username_hint
                                .clone()
                                .unwrap_or_else(|| "autenticado".into());
                            let msg = format!(
                                "Sessão OK ({who}) — CSRF obtido{}",
                                if info.announce_url.is_some() {
                                    " + Announce URL capturada!"
                                } else {
                                    ""
                                }
                            );
                            self.push_log(msg.clone());
                            self.session_status = Some(Ok(msg.clone()));
                            self.status_banner = Some((true, msg));
                        }
                        Err(e) => {
                            self.push_log(format!("[ERRO SESSÃO] {e}"));
                            self.session_status = Some(Err(e.clone()));
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
                                "Dupe Check: nenhuma dublagem existente encontrada no Acervo.",
                            );
                        }
                        Ok(list) => {
                            self.push_log(format!(
                                "Dupe Check: {} dublagem(ns) encontrada(s) no Acervo para esta obra.",
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

        // Carrega textura do pôster TMDB na GPU quando chegar do worker
        if let Some(poster) = self.pending_poster.take() {
            let color_img =
                egui::ColorImage::from_rgba_unmultiplied([poster.width, poster.height], &poster.rgba);
            self.poster_texture = Some(ctx.load_texture(
                "tmdb_poster",
                color_img,
                egui::TextureOptions::LINEAR,
            ));
        }

        if self.busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(80));
        }

        // Barra superior de navegação
        egui::TopBottomPanel::top("top_nav").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("DubAssistant");
                ui.separator();
                ui.selectable_value(&mut self.active_tab, AppTab::Input, "1. Entrada");
                ui.selectable_value(
                    &mut self.active_tab,
                    AppTab::Metadata,
                    "2. TMDB & Metadados",
                );
                ui.selectable_value(&mut self.active_tab, AppTab::Process, "3. Processar");
                ui.selectable_value(&mut self.active_tab, AppTab::Upload, "4. Upload");
                ui.selectable_value(&mut self.active_tab, AppTab::Settings, "Configurações");

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
                        ui.colored_label(egui::Color32::from_rgb(80, 200, 120), "Sessão OK");
                    } else {
                        ui.colored_label(egui::Color32::GRAY, "Sessão não testada");
                    }
                });
            });
            ui.add_space(4.0);
        });

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
                    ui.label("Pronto.");
                }
            });
        });

        // Conteúdo principal da aba ativa
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| match self.active_tab {
                AppTab::Input => tabs::input::show(self, ui),
                AppTab::Metadata => tabs::metadata::show(self, ui),
                AppTab::Process => tabs::process::show(self, ui),
                AppTab::Upload => tabs::upload::show(self, ui),
                AppTab::Settings => tabs::settings::show(self, ui),
            });
        });
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Garante que nenhum processo filho (mkvextract/ffmpeg/etc.) fique órfão ao fechar o app
        self.cancel.cancel();
    }
}
