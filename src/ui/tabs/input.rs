use crate::domain::models::{Category, InputMode, TrackKind};
use crate::ui::app::{AppTab, CapybaraApp};
use crate::worker::WorkerCommand;
use eframe::egui;
use std::path::PathBuf;

pub fn show(app: &mut CapybaraApp, ui: &mut egui::Ui) {
    ui.heading("1. Entrada & Inspeção de Mídia");
    ui.add_space(6.0);

    ui.horizontal(|ui| {
        ui.label("Modo de entrada:");
        ui.radio_value(
            &mut app.input_mode,
            InputMode::VideoFiles,
            "Vídeo(s) Pronto(s) (.mkv / .mp4)",
        );
        ui.radio_value(
            &mut app.input_mode,
            InputMode::RawDisc,
            "Disco Raw (Blu-ray / DVD)",
        );
        ui.radio_value(
            &mut app.input_mode,
            InputMode::ExtractedFolder,
            "Pasta já Extraída",
        );
    });

    ui.horizontal(|ui| {
        ui.label("Categoria:");
        let changed_m = ui
            .radio_value(&mut app.meta.category, Category::Movie, "Filme")
            .changed();
        let changed_s = ui
            .radio_value(
                &mut app.meta.category,
                Category::Series,
                "Série - Temporada Completa",
            )
            .changed();
        if changed_m || changed_s {
            app.refresh_auto_release_name();
        }
    });

    ui.add_space(8.0);

    match app.input_mode {
        InputMode::VideoFiles => show_video_mode(app, ui),
        InputMode::RawDisc => show_raw_disc_mode(app, ui),
        InputMode::ExtractedFolder => show_folder_mode(app, ui),
    }

    ui.add_space(12.0);
    let can_next = !app.inspected_videos.is_empty()
        || app.folder_scan.is_some()
        || app.inspected_disc.is_some();
    ui.add_enabled_ui(can_next && !app.busy, |ui| {
        if ui.button("Avançar para 2. TMDB & Metadados ->").clicked() {
            app.active_tab = AppTab::Metadata;
        }
    });
}

fn show_video_mode(app: &mut CapybaraApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        if ui
            .add_enabled(!app.busy, egui::Button::new("Selecionar Vídeo(s) (.mkv/.mp4)"))
            .clicked()
        {
            let picked = rfd::FileDialog::new()
                .add_filter("Vídeos", &["mkv", "mp4"])
                .pick_files();
            if let Some(files) = picked {
                if !files.is_empty() {
                    app.busy = true;
                    let _ = app.worker.tx.send(WorkerCommand::InspectVideos {
                        cfg: app.config.clone(),
                        paths: files,
                    });
                }
            }
        }

        if ui
            .add_enabled(
                !app.busy,
                egui::Button::new("Selecionar Pasta de Temporada (Série)"),
            )
            .clicked()
        {
            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                let mut files: Vec<PathBuf> = Vec::new();
                if let Ok(rd) = std::fs::read_dir(&folder) {
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
                if files.is_empty() {
                    app.status_banner = Some((
                        false,
                        "Nenhum arquivo .mkv ou .mp4 encontrado na pasta selecionada.".into(),
                    ));
                } else {
                    app.meta.category = Category::Series;
                    app.busy = true;
                    let _ = app.worker.tx.send(WorkerCommand::InspectVideos {
                        cfg: app.config.clone(),
                        paths: files,
                    });
                }
            }
        }
    });

    ui.add_space(10.0);

    if app.inspected_videos.is_empty() {
        ui.colored_label(
            egui::Color32::GRAY,
            "Selecione um arquivo .mkv/.mp4 (Filme) ou vários episódios / pasta da temporada (Série) para inspecionar faixas.",
        );
        return;
    }

    let mut sync_audio_meta = false;

    for (vid_idx, video) in app.inspected_videos.iter_mut().enumerate() {
        let fname = video
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        let header = format!(
            "#{} {}  |  FPS: {}  |  Capítulos: {}{}",
            vid_idx + 1,
            fname,
            video.fps.as_deref().unwrap_or("?"),
            if video.has_chapters { "Sim" } else { "Não" },
            video
                .embedded_tmdb_id
                .as_deref()
                .map(|id| format!("  |  TMDB Tag: {id}"))
                .unwrap_or_default()
        );

        egui::CollapsingHeader::new(header)
            .default_open(vid_idx == 0)
            .show(ui, |ui| {
                ui.label(egui::RichText::new("Faixas de Áudio (selecione exatamente 1):").strong());
                for t in video.tracks.iter_mut().filter(|t| t.kind == TrackKind::Audio) {
                    let allowed = t.allowed_audio_codec.is_some();
                    let label = format!(
                        "ID {} | {} | Idioma: {} | Canais: {} | Bitrate: {} kbps{}{}",
                        t.id,
                        t.allowed_audio_codec
                            .map(|c| format!("{} (.{})", c.title_label(), c.pure_extension()))
                            .unwrap_or_else(|| format!("{} [PROIBIDO]", t.codec_raw)),
                        t.language,
                        t.channels.as_deref().unwrap_or("?"),
                        t.bitrate_kbps
                            .map(|b| b.to_string())
                            .unwrap_or_else(|| "?".into()),
                        if t.name.is_empty() {
                            String::new()
                        } else {
                            format!(" | \"{}\"", t.name)
                        },
                        t.delay_ms
                            .map(|d| format!(" | DELAY: {d} ms"))
                            .unwrap_or_default()
                    );

                    ui.add_enabled_ui(allowed, |ui| {
                        if ui.checkbox(&mut t.selected, label).changed() && vid_idx == 0 {
                            sync_audio_meta = true;
                        }
                    });
                }

                let has_subs = video.tracks.iter().any(|t| t.kind == TrackKind::Subtitle);
                if has_subs {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(
                            "Legendas:",
                        )
                        .strong(),
                    );
                    for t in video
                        .tracks
                        .iter_mut()
                        .filter(|t| t.kind == TrackKind::Subtitle)
                    {
                        let label = format!(
                            "ID {} | {} | Idioma: {}{}{}",
                            t.id,
                            t.codec_raw,
                            t.language,
                            if t.forced { " [FORCED]" } else { "" },
                            if t.name.is_empty() {
                                String::new()
                            } else {
                                format!(" | \"{}\"", t.name)
                            }
                        );
                        ui.checkbox(&mut t.selected, label);
                    }
                }
            });
    }

    if sync_audio_meta {
        if let Some(first) = app.inspected_videos.first() {
            if let Some(audio) = first
                .tracks
                .iter()
                .find(|t| t.kind == TrackKind::Audio && t.selected)
            {
                if let Some(c) = audio.allowed_audio_codec {
                    app.meta.audio_codec = c;
                }
                if let Some(kbps) = audio.bitrate_kbps {
                    app.meta.bitrate_kbps = kbps.to_string();
                }
                if let Some(ch) = &audio.channels {
                    app.meta.channels = ch.clone();
                }
                app.refresh_auto_release_name();
            }
        }
    }
}

fn show_folder_mode(app: &mut CapybaraApp, ui: &mut egui::Ui) {
    if ui
        .add_enabled(!app.busy, egui::Button::new("Selecionar Pasta já Extraída"))
        .clicked()
    {
        if let Some(folder) = rfd::FileDialog::new().pick_folder() {
            app.busy = true;
            let _ = app.worker.tx.send(WorkerCommand::ScanExtractedFolder {
                cfg: app.config.clone(),
                folder,
            });
        }
    }

    ui.add_space(10.0);

    let Some(scan) = &app.folder_scan else {
        ui.colored_label(
            egui::Color32::GRAY,
            "Selecione uma pasta contendo faixas de áudio puras (.ac3, .eac3, .dts, .dtshd, .thd, .flac, .wav, .aac), legendas, capítulos e mediainfo.txt.",
        );
        return;
    };

    ui.label(format!("Pasta: {}", scan.root_dir.display()));
    ui.separator();

    ui.label(
        egui::RichText::new(format!("Áudios permitidos ({}):", scan.audio_files.len())).strong(),
    );
    for a in &scan.audio_files {
        ui.label(format!("  • {}", a.file_name().unwrap_or_default().to_string_lossy()));
    }

    if !scan.subtitle_files.is_empty() {
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(format!(
                "Legendas ({}) -> ficarão na mesma pasta raiz:",
                scan.subtitle_files.len()
            ))
            .strong(),
        );
        for s in &scan.subtitle_files {
            ui.label(format!("  • {}", s.file_name().unwrap_or_default().to_string_lossy()));
        }
    }

    if !scan.chapter_files.is_empty() {
        ui.add_space(4.0);
        ui.label(
            egui::RichText::new(format!("Capítulos ({}):", scan.chapter_files.len())).strong(),
        );
        for c in &scan.chapter_files {
            ui.label(format!("  • {}", c.file_name().unwrap_or_default().to_string_lossy()));
        }
    }

    ui.add_space(4.0);
    if let Some(mi) = &scan.mediainfo_file {
        ui.colored_label(
            egui::Color32::from_rgb(80, 200, 120),
            format!("MediaInfo da fonte encontrado: {}", mi.file_name().unwrap_or_default().to_string_lossy()),
        );
    } else {
        ui.colored_label(
            egui::Color32::from_rgb(240, 190, 80),
            "Aviso: mediainfo.txt da fonte não encontrado na pasta. Cole o MediaInfo da fonte na aba 2 ou 4.",
        );
    }

    if !scan.forbidden_files.is_empty() {
        ui.add_space(6.0);
        ui.colored_label(
            egui::Color32::from_rgb(240, 100, 100),
            format!(
                "Arquivos proibidos detectados ({}) — serão ignorados na pasta final:",
                scan.forbidden_files.len()
            ),
        );
        for f in &scan.forbidden_files {
            ui.colored_label(
                egui::Color32::from_rgb(240, 140, 140),
                format!("  ✖ {}", f.file_name().unwrap_or_default().to_string_lossy()),
            );
        }
    }
}

fn show_raw_disc_mode(app: &mut CapybaraApp, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                !app.busy,
                egui::Button::new("Selecionar Pasta do Disco (BDMV / VIDEO_TS)"),
            )
            .clicked()
        {
            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                app.busy = true;
                let _ = app.worker.tx.send(WorkerCommand::InspectRawDisc {
                    cfg: app.config.clone(),
                    path: folder,
                });
            }
        }

        if ui
            .add_enabled(
                !app.busy,
                egui::Button::new("Selecionar Imagem .ISO (DVD)"),
            )
            .clicked()
        {
            if let Some(iso) = rfd::FileDialog::new()
                .add_filter("Imagem de Disco ISO", &["iso"])
                .pick_file()
            {
                app.busy = true;
                let _ = app.worker.tx.send(WorkerCommand::InspectRawDisc {
                    cfg: app.config.clone(),
                    path: iso,
                });
            }
        }
    });

    ui.add_space(10.0);

    let Some(disc) = &mut app.inspected_disc else {
        ui.colored_label(
            egui::Color32::GRAY,
            "Selecione uma pasta de Blu-ray (contendo BDMV) ou DVD (contendo VIDEO_TS / arquivo .ISO) para listar playlists/títulos e faixas.",
        );
        return;
    };

    ui.label(format!(
        "Disco detectado: {}  |  Origem: {}",
        disc.kind.label(),
        disc.root_dir.display()
    ));
    ui.label(
        egui::RichText::new(
            "Marque 1 playlist/título para Filme, ou múltiplos para Série (episódios na ordem):",
        )
        .small()
        .color(egui::Color32::LIGHT_GRAY),
    );
    ui.separator();

    let mut sync_audio_meta = false;

    for title in &mut disc.titles {
        ui.horizontal(|ui| {
            if ui.checkbox(&mut title.selected, "").changed() {
                sync_audio_meta = true;
            }
            let header = format!(
                "{}  |  FPS: {}  |  Capítulos: {}",
                title.label,
                title.fps.as_deref().unwrap_or("?"),
                if title.has_chapters { "Sim" } else { "Não" }
            );
            egui::CollapsingHeader::new(header)
                .id_salt(format!("disc_title_{}", title.index))
                .default_open(title.selected)
                .show(ui, |ui| {
                    ui.label(
                        egui::RichText::new("Faixas de Áudio (selecione exatamente 1):").strong(),
                    );
                    for t in title.tracks.iter_mut().filter(|t| t.kind == TrackKind::Audio) {
                        let allowed = t.allowed_audio_codec.is_some();
                        let label = format!(
                            "ID {} | {} | Idioma: {} | Canais: {}",
                            t.id,
                            t.allowed_audio_codec
                                .map(|c| format!("{} (.{})", c.title_label(), c.pure_extension()))
                                .unwrap_or_else(|| format!("{} [PROIBIDO]", t.codec_raw)),
                            t.language,
                            t.channels.as_deref().unwrap_or("?"),
                        );
                        ui.add_enabled_ui(allowed, |ui| {
                            if ui.checkbox(&mut t.selected, label).changed() && title.selected {
                                sync_audio_meta = true;
                            }
                        });
                    }

                    let has_subs = title.tracks.iter().any(|t| t.kind == TrackKind::Subtitle);
                    if has_subs && disc.kind == crate::domain::models::DiscKind::BluRay {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new("Legendas PGS:")
                                .strong(),
                        );
                        for t in title
                            .tracks
                            .iter_mut()
                            .filter(|t| t.kind == TrackKind::Subtitle)
                        {
                            let label = format!(
                                "ID {} | {} | Idioma: {}",
                                t.id, t.codec_raw, t.language
                            );
                            ui.checkbox(&mut t.selected, label);
                        }
                    }
                });
        });
    }

    if sync_audio_meta {
        let sel_count = disc.titles.iter().filter(|t| t.selected).count();
        if sel_count > 1 {
            app.meta.category = Category::Series;
        }
        if let Some(first_sel) = disc.titles.iter().find(|t| t.selected) {
            if let Some(fps) = &first_sel.fps {
                app.meta.fps = fps.clone();
            }
            if let Some(audio) = first_sel
                .tracks
                .iter()
                .find(|t| t.kind == TrackKind::Audio && t.selected)
            {
                if let Some(c) = audio.allowed_audio_codec {
                    app.meta.audio_codec = c;
                }
                if let Some(kbps) = audio.bitrate_kbps {
                    app.meta.bitrate_kbps = kbps.to_string();
                }
                if let Some(ch) = &audio.channels {
                    app.meta.channels = ch.clone();
                }
            }
        }
        app.refresh_auto_release_name();
    }
}
