use crate::config::save_config;
use crate::domain::models::{AudioCodec, Category, DubVersion, InputMode, SourceType, TrackKind};
use crate::domain::rules::{build_official_release_name, sanitize_windows_filename};
use crate::media::fps::fps_with_detail;
use crate::ui::app::CapybaraApp;
use crate::worker::WorkerCommand;
use eframe::egui;

fn required_label(ui: &mut egui::Ui, text: &str) {
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 2.0;
        ui.label(text);
        ui.colored_label(egui::Color32::from_rgb(235, 75, 75), "*");
        ui.label(":");
    });
}

pub fn show(app: &mut CapybaraApp, ui: &mut egui::Ui) {
    show_input_section(app, ui);

    if app.has_loaded_input() {
        ui.add_space(10.0);
        show_metadata_section(app, ui);

        ui.add_space(10.0);
        show_process_and_upload_section(app, ui);
    }

    if !app.logs.is_empty() {
        ui.add_space(10.0);
        let has_err = matches!(app.status_banner, Some((false, _)));
        egui::CollapsingHeader::new(format!("Log de Execução ({})", app.logs.len()))
            .id_salt("main_logs_header")
            .default_open(app.busy || has_err)
            .show(ui, |ui| {
                egui::ScrollArea::vertical()
                    .id_salt("main_log_scroll")
                    .max_height(200.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &app.logs {
                            ui.monospace(line);
                        }
                    });
            });
    }
}

fn show_input_section(app: &mut CapybaraApp, ui: &mut egui::Ui) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("1. Mídia de Entrada").strong().size(15.0));
            ui.add_space(8.0);

            if ui
                .add_enabled(
                    !app.busy,
                    egui::Button::new("Selecionar Vídeo(s) / .ISO"),
                )
                .clicked()
            {
                let picked = rfd::FileDialog::new()
                    .add_filter("Vídeos e ISO", &["mkv", "mp4", "iso"])
                    .pick_files();
                if let Some(files) = picked {
                    app.load_paths(files);
                }
            }

            if ui
                .add_enabled(
                    !app.busy,
                    egui::Button::new("Selecionar Pasta (Série / Disco / Extraída)"),
                )
                .clicked()
            {
                if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                    app.load_paths(vec![folder]);
                }
            }

            ui.colored_label(
                egui::Color32::GRAY,
                "ou arraste arquivo(s)/pasta em qualquer lugar da janela",
            );
        });

        if !app.has_loaded_input() {
            ui.add_space(16.0);
            ui.vertical_centered(|ui| {
                ui.label(
                    egui::RichText::new(
                        "Arraste um vídeo (.mkv/.mp4), pasta de temporada, disco raw (BDMV/VIDEO_TS/.ISO) ou pasta já extraída",
                    )
                    .size(14.0)
                    .color(egui::Color32::LIGHT_GRAY),
                );
                ui.add_space(4.0);
                ui.colored_label(
                    egui::Color32::GRAY,
                    "O tipo de entrada, faixas PT-BR, TMDB e Dupe Check são detectados automaticamente.",
                );
            });
            ui.add_space(16.0);
            return;
        }

        ui.add_space(6.0);
        match app.input_mode {
            InputMode::VideoFiles => show_video_summary_and_tracks(app, ui),
            InputMode::RawDisc => show_raw_disc_summary_and_tracks(app, ui),
            InputMode::ExtractedFolder => show_folder_summary(app, ui),
        }
    });
}

fn show_video_summary_and_tracks(app: &mut CapybaraApp, ui: &mut egui::Ui) {
    let vid_count = app.inspected_videos.len();
    let first_name = app
        .inspected_videos
        .first()
        .and_then(|v| v.path.file_name())
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let has_selected_audio = app
        .inspected_videos
        .first()
        .map(|v| {
            v.tracks
                .iter()
                .any(|t| t.kind == TrackKind::Audio && t.selected)
        })
        .unwrap_or(false);
    let audio_summary = app
        .inspected_videos
        .first()
        .and_then(|v| {
            v.tracks
                .iter()
                .find(|t| t.kind == TrackKind::Audio && t.selected)
        })
        .map(|t| {
            format!(
                "{} {} {} ({})",
                t.allowed_audio_codec
                    .map(|c| c.title_label())
                    .unwrap_or(&t.codec_raw),
                t.bitrate_kbps
                    .map(|b| format!("{b} kbps"))
                    .unwrap_or_default(),
                t.channels.as_deref().unwrap_or(""),
                t.language
            )
        })
        .unwrap_or_else(|| "Nenhuma faixa de áudio selecionada!".to_string());

    let summary_header = if vid_count == 1 {
        format!("Vídeo: {first_name}  ·  Áudio: {audio_summary}")
    } else {
        format!("{vid_count} vídeos (1º: {first_name})  ·  Áudio: {audio_summary}")
    };

    let mut sync_audio_meta = false;

    egui::CollapsingHeader::new(summary_header)
        .id_salt("video_tracks_collapsing")
        .default_open(!has_selected_audio)
        .show(ui, |ui| {
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
                    fps_with_detail(video.fps.as_deref(), video.fps_detail.as_deref()),
                    if video.has_chapters { "Sim" } else { "Não" },
                    video
                        .embedded_tmdb_id
                        .as_deref()
                        .map(|id| format!("  |  TMDB Tag: {id}"))
                        .unwrap_or_default()
                );

                egui::CollapsingHeader::new(header)
                    .id_salt(format!("vid_item_{vid_idx}"))
                    .default_open(vid_idx == 0)
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("Faixas de Áudio (selecione exatamente 1):")
                                .strong(),
                        );
                        for t in video.tracks.iter_mut().filter(|t| t.kind == TrackKind::Audio) {
                            let allowed = t.allowed_audio_codec.is_some();
                            let label = format!(
                                "ID {} | {} | Idioma: {} | Canais: {} | Bitrate: {} kbps{}{}",
                                t.id,
                                t.allowed_audio_codec
                                    .map(|c| {
                                        format!("{} (.{})", c.title_label(), c.pure_extension())
                                    })
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
                            ui.label(egui::RichText::new("Legendas:").strong());
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
        });

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

fn show_raw_disc_summary_and_tracks(app: &mut CapybaraApp, ui: &mut egui::Ui) {
    let Some(disc) = &mut app.inspected_disc else {
        return;
    };

    let sel_count = disc.titles.iter().filter(|t| t.selected).count();
    let header = format!(
        "Disco {}: {}  ·  {} playlist(s)/título(s) selecionado(s) de {}",
        disc.kind.label(),
        disc.root_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
        sel_count,
        disc.titles.len()
    );

    let mut sync_audio_meta = false;

    egui::CollapsingHeader::new(header)
        .id_salt("disc_tracks_collapsing")
        .default_open(sel_count == 0 || disc.titles.len() > 1)
        .show(ui, |ui| {
            ui.label(
                egui::RichText::new(
                    "Marque 1 playlist/título para Filme, ou múltiplos para Série (episódios na ordem):",
                )
                .small()
                .color(egui::Color32::LIGHT_GRAY),
            );
            for title in &mut disc.titles {
                ui.horizontal(|ui| {
                    if ui.checkbox(&mut title.selected, "").changed() {
                        sync_audio_meta = true;
                    }
                    let t_header = format!(
                        "{}  |  FPS: {}  |  Capítulos: {}",
                        title.label,
                        fps_with_detail(title.fps.as_deref(), title.fps_detail.as_deref()),
                        if title.has_chapters { "Sim" } else { "Não" }
                    );
                    egui::CollapsingHeader::new(t_header)
                        .id_salt(format!("disc_title_{}", title.index))
                        .default_open(title.selected)
                        .show(ui, |ui| {
                            ui.label(
                                egui::RichText::new("Faixas de Áudio (selecione exatamente 1):")
                                    .strong(),
                            );
                            for t in title.tracks.iter_mut().filter(|t| t.kind == TrackKind::Audio)
                            {
                                let allowed = t.allowed_audio_codec.is_some();
                                let label = format!(
                                    "ID {} | {} | Idioma: {} | Canais: {}",
                                    t.id,
                                    t.allowed_audio_codec
                                        .map(|c| {
                                            format!("{} (.{})", c.title_label(), c.pure_extension())
                                        })
                                        .unwrap_or_else(|| format!("{} [PROIBIDO]", t.codec_raw)),
                                    t.language,
                                    t.channels.as_deref().unwrap_or("?"),
                                );
                                ui.add_enabled_ui(allowed, |ui| {
                                    if ui.checkbox(&mut t.selected, label).changed()
                                        && title.selected
                                    {
                                        sync_audio_meta = true;
                                    }
                                });
                            }

                            let has_subs =
                                title.tracks.iter().any(|t| t.kind == TrackKind::Subtitle);
                            if has_subs && disc.kind == crate::domain::models::DiscKind::BluRay {
                                ui.add_space(4.0);
                                ui.label(egui::RichText::new("Legendas PGS:").strong());
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
        });

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

fn show_folder_summary(app: &mut CapybaraApp, ui: &mut egui::Ui) {
    let Some(scan) = &app.folder_scan else {
        return;
    };

    let header = format!(
        "Pasta Extraída: {}  ·  {} áudio(s), {} legenda(s), {} capítulo(s)",
        scan.root_dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy(),
        scan.audio_files.len(),
        scan.subtitle_files.len(),
        scan.chapter_files.len()
    );

    let needs_attention = scan.mediainfo_file.is_none() || !scan.forbidden_files.is_empty();

    egui::CollapsingHeader::new(header)
        .id_salt("extracted_folder_collapsing")
        .default_open(needs_attention)
        .show(ui, |ui| {
            ui.label(format!("Caminho: {}", scan.root_dir.display()));
            for a in &scan.audio_files {
                ui.label(format!(
                    "  • Áudio: {}",
                    a.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
            for s in &scan.subtitle_files {
                ui.label(format!(
                    "  • Legenda: {}",
                    s.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
            for c in &scan.chapter_files {
                ui.label(format!(
                    "  • Capítulos: {}",
                    c.file_name().unwrap_or_default().to_string_lossy()
                ));
            }
            if let Some(mi) = &scan.mediainfo_file {
                ui.colored_label(
                    egui::Color32::from_rgb(80, 200, 120),
                    format!(
                        "✔ MediaInfo da fonte encontrado: {}",
                        mi.file_name().unwrap_or_default().to_string_lossy()
                    ),
                );
            } else {
                ui.colored_label(
                    egui::Color32::from_rgb(240, 190, 80),
                    "⚠ mediainfo.txt da fonte não encontrado na pasta — cole o MediaInfo da fonte abaixo.",
                );
            }
            if !scan.forbidden_files.is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(240, 100, 100),
                    format!(
                        "Arquivos proibidos detectados ({}) — serão ignorados:",
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
        });
}

fn show_metadata_section(app: &mut CapybaraApp, ui: &mut egui::Ui) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.label(
                egui::RichText::new("2. TMDB, Dupe Check & Metadados")
                    .strong()
                    .size(15.0),
            );
            ui.add_space(10.0);

            let enter = ui
                .add(
                    egui::TextEdit::singleline(&mut app.tmdb_query)
                        .desired_width(300.0)
                        .hint_text("Título ou ID do TMDB"),
                )
                .lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter));

            if (ui
                .add_enabled(!app.busy, egui::Button::new("Buscar TMDB"))
                .clicked()
                || enter)
                && !app.busy
            {
                app.trigger_tmdb_search(false);
            }
        });

        // Lista de resultados TMDB (aberta se ainda não selecionou; recolhida se já auto-selecionou)
        if !app.tmdb_results.is_empty()
            && (app.tmdb_details.is_none() || app.tmdb_results.len() > 1)
        {
            ui.add_space(4.0);
            let label = if app.tmdb_details.is_some() {
                format!(
                    "Trocar resultado TMDB ({} encontrado(s))",
                    app.tmdb_results.len()
                )
            } else {
                format!(
                    "Resultados TMDB ({} encontrado(s))",
                    app.tmdb_results.len()
                )
            };
            egui::CollapsingHeader::new(label)
                .id_salt("tmdb_results_header")
                .default_open(app.tmdb_details.is_none())
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("tmdb_results_scroll")
                        .max_height(130.0)
                        .show(ui, |ui| {
                            let mut chosen: Option<(u64, Category)> = None;
                            for item in &app.tmdb_results {
                                let is_current = app.meta.tmdb_id == item.id.to_string();
                                ui.horizontal(|ui| {
                                    let btn_text = if is_current { "Atual" } else { "Selecionar" };
                                    if ui
                                        .add_enabled(!app.busy && !is_current, egui::Button::new(btn_text))
                                        .clicked()
                                    {
                                        chosen = Some((item.id, item.category));
                                    }
                                    ui.label(format!(
                                        "[{}] {} ({}) — {}",
                                        item.id, item.title_pt_br, item.title_original, item.year
                                    ));
                                });
                            }
                            if let Some((id, cat)) = chosen {
                                app.busy = true;
                                let _ = app.worker.tx.send(WorkerCommand::FetchTmdbDetails {
                                    api_key: app.config.tmdb_api_key.clone(),
                                    id,
                                    category: cat,
                                });
                            }
                        });
                });
        }

        // Card TMDB (pôster + detalhes) + Dupe Check lado a lado / compacto
        if let Some(details) = &app.tmdb_details {
            ui.add_space(6.0);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    if let Some(tex) = &app.poster_texture {
                        let max_h = 135.0;
                        let sz = tex.size_vec2();
                        let scale = if sz.y > 0.0 { max_h / sz.y } else { 1.0 };
                        let display_size = egui::vec2(sz.x * scale, max_h);
                        ui.add(egui::Image::new((tex.id(), display_size)).rounding(4.0));
                        ui.add_space(10.0);
                    }

                    ui.vertical(|ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                egui::RichText::new(&details.title_pt_br)
                                    .strong()
                                    .size(16.0),
                            );
                            if !details.year.is_empty() {
                                ui.label(
                                    egui::RichText::new(format!("({})", details.year))
                                        .size(14.0)
                                        .color(egui::Color32::LIGHT_GRAY),
                                );
                            }
                        });

                        if !details.title_original.is_empty()
                            && !details
                                .title_original
                                .eq_ignore_ascii_case(&details.title_pt_br)
                        {
                            ui.label(
                                egui::RichText::new(format!(
                                    "Título original: {}",
                                    details.title_original
                                ))
                                .italics()
                                .color(egui::Color32::LIGHT_GRAY),
                            );
                        }

                        let mut meta_badges = vec![
                            format!("TMDB #{}", details.id),
                            details.category.label().to_string(),
                        ];
                        if details.vote_average > 0.0 {
                            meta_badges.push(format!("Nota: {:.1}/10", details.vote_average));
                        }
                        if !details.runtime_label.is_empty() {
                            meta_badges.push(details.runtime_label.clone());
                        }
                        if !details.genres.is_empty() {
                            meta_badges.push(details.genres.join(", "));
                        }
                        ui.label(
                            egui::RichText::new(meta_badges.join("  ·  "))
                                .color(egui::Color32::from_rgb(130, 190, 255)),
                        );

                        if !details.overview.trim().is_empty() {
                            ui.add_space(3.0);
                            ui.label(
                                egui::RichText::new(&details.overview)
                                    .small()
                                    .color(egui::Color32::from_gray(200)),
                            );
                        }
                    });
                });
            });
        }

        // Dupe Check compacto
        ui.add_space(6.0);
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new("Dupe Check:").strong());
            match &app.dupe_results {
                None => {
                    ui.colored_label(
                        egui::Color32::GRAY,
                        "Pendente (executado automaticamente após carregar TMDB).",
                    );
                }
                Some(Ok(list)) if list.is_empty() => {
                    ui.colored_label(
                        egui::Color32::from_rgb(80, 200, 120),
                        "✔ Nenhuma dublagem pendente encontrada para este título.",
                    );
                }
                Some(Ok(list)) => {
                    let has_exact = list.iter().any(|d| d.likely_same_release);
                    let col = if has_exact {
                        egui::Color32::from_rgb(240, 100, 100)
                    } else {
                        egui::Color32::from_rgb(240, 190, 80)
                    };
                    ui.colored_label(
                        col,
                        format!(
                            "⚠ {} dublagem(ns) encontrada(s) para esta obra:",
                            list.len()
                        ),
                    );
                }
                Some(Err(e)) => {
                    ui.colored_label(
                        egui::Color32::from_rgb(240, 160, 90),
                        format!("Aviso: {e}"),
                    );
                }
            }
            if ui
                .add_enabled(!app.busy, egui::Button::new("Verificar Duplicatas"))
                .clicked()
            {
                app.trigger_dupe_check();
            }
        });

        if let Some(Ok(list)) = &app.dupe_results {
            if !list.is_empty() {
                for d in list {
                    let prefix = if d.likely_same_release {
                        "  [POSSÍVEL DUPLICATA] "
                    } else {
                        "  • "
                    };
                    let color = if d.likely_same_release {
                        egui::Color32::from_rgb(240, 110, 110)
                    } else {
                        egui::Color32::from_rgb(235, 200, 110)
                    };
                    let line = if d.details.is_empty() {
                        format!("{prefix}{}", d.title)
                    } else {
                        format!("{prefix}{} — {}", d.title, d.details)
                    };
                    ui.horizontal_wrapped(|ui| {
                        ui.colored_label(color, line);
                        if let Some(url) = &d.url {
                            ui.hyperlink_to("Abrir no site", url);
                        }
                    });
                }
            }
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(6.0);

        // Formulário de metadados
        let mut recalc_name = false;

        egui::Grid::new("unified_meta_grid")
            .num_columns(2)
            .spacing([12.0, 6.0])
            .show(ui, |ui| {
                required_label(ui, "Categoria / TMDB ID");
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .radio_value(&mut app.meta.category, Category::Movie, "Filme")
                        .changed()
                    {
                        recalc_name = true;
                    }
                    if ui
                        .radio_value(&mut app.meta.category, Category::Series, "Série")
                        .changed()
                    {
                        recalc_name = true;
                    }
                    ui.separator();
                    ui.label("TMDB ID:");
                    ui.add(egui::TextEdit::singleline(&mut app.meta.tmdb_id).desired_width(90.0));
                    if ui
                        .add_enabled(!app.busy, egui::Button::new("Carregar"))
                        .clicked()
                    {
                        if let Ok(id_num) = app.meta.tmdb_id.trim().parse::<u64>() {
                            app.busy = true;
                            let _ = app.worker.tx.send(WorkerCommand::FetchTmdbDetails {
                                api_key: app.config.tmdb_api_key.clone(),
                                id: id_num,
                                category: app.meta.category,
                            });
                        }
                    }
                });
                ui.end_row();

                if app.meta.category == Category::Series {
                    required_label(ui, "Temporada");
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .add(
                                egui::TextEdit::singleline(&mut app.meta.season_number)
                                    .desired_width(50.0),
                            )
                            .changed()
                        {
                            recalc_name = true;
                        }
                        if let Some(details) = &app.tmdb_details {
                            for s in &details.seasons {
                                if s.season_number > 0
                                    && ui
                                        .small_button(format!(
                                            "S{:02} ({} ep)",
                                            s.season_number, s.episode_count
                                        ))
                                        .clicked()
                                {
                                    app.meta.season_number = s.season_number.to_string();
                                    if !s.year.is_empty() {
                                        app.meta.year = s.year.clone();
                                    }
                                    recalc_name = true;
                                }
                            }
                        }
                    });
                    ui.end_row();
                }

                ui.label("Título PT-BR / Original / Ano:");
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut app.meta.title_pt_br)
                                .desired_width(250.0)
                                .hint_text("Título PT-BR"),
                        )
                        .changed()
                    {
                        recalc_name = true;
                    }
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut app.meta.title_original)
                                .desired_width(230.0)
                                .hint_text("Título Original"),
                        )
                        .changed()
                    {
                        recalc_name = true;
                    }
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut app.meta.year)
                                .desired_width(60.0)
                                .hint_text("Ano"),
                        )
                        .changed()
                    {
                        recalc_name = true;
                    }
                });
                ui.end_row();

                ui.label("Áudio / FPS:");
                ui.horizontal_wrapped(|ui| {
                    egui::ComboBox::from_id_salt("codec_combo")
                        .selected_text(app.meta.audio_codec.title_label())
                        .show_ui(ui, |ui| {
                            for c in AudioCodec::ALL {
                                if ui
                                    .selectable_value(&mut app.meta.audio_codec, c, c.title_label())
                                    .changed()
                                {
                                    recalc_name = true;
                                }
                            }
                        });
                    ui.label("Bitrate:");
                    if ui
                        .add(
                            egui::TextEdit::singleline(&mut app.meta.bitrate_kbps)
                                .desired_width(60.0),
                        )
                        .changed()
                    {
                        recalc_name = true;
                    }
                    ui.label("Canais:");
                    if ui
                        .add(egui::TextEdit::singleline(&mut app.meta.channels).desired_width(50.0))
                        .changed()
                    {
                        recalc_name = true;
                    }
                    ui.label("FPS:");
                    ui.add(egui::TextEdit::singleline(&mut app.meta.fps).desired_width(65.0));
                });
                ui.end_row();

                required_label(ui, "Versão / Fonte");
                ui.horizontal_wrapped(|ui| {
                    egui::ComboBox::from_id_salt("version_combo")
                        .selected_text(app.meta.version.label())
                        .show_ui(ui, |ui| {
                            for v in DubVersion::ALL {
                                ui.selectable_value(&mut app.meta.version, v, v.label());
                            }
                        });
                    egui::ComboBox::from_id_salt("source_combo")
                        .selected_text(app.meta.source.title_label())
                        .show_ui(ui, |ui| {
                            for s in SourceType::ALL {
                                if ui
                                    .selectable_value(&mut app.meta.source, s, s.title_label())
                                    .changed()
                                {
                                    recalc_name = true;
                                }
                            }
                        });
                    ui.add(
                        egui::TextEdit::singleline(&mut app.meta.source_label)
                            .desired_width(180.0)
                            .hint_text("Rótulo (ex: Blu-ray Remux)"),
                    );
                });
                ui.end_row();

                ui.label("Estúdio / Ano Dub / Nota:");
                ui.horizontal_wrapped(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut app.meta.studio)
                            .desired_width(160.0)
                            .hint_text("Estúdio (ex: Delart)"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut app.meta.dub_year)
                            .desired_width(60.0)
                            .hint_text("Ano dub"),
                    );
                    ui.add(
                        egui::TextEdit::singleline(&mut app.meta.note)
                            .desired_width(210.0)
                            .hint_text("Nota curta (opcional)"),
                    );
                    if ui
                        .checkbox(&mut app.meta.anon, "Anônimo")
                        .changed()
                    {
                        app.config.default_anon = app.meta.anon;
                        let _ = save_config(&app.config);
                    }
                });
                ui.end_row();
            });

        if recalc_name {
            app.refresh_auto_release_name();
        }

        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.label(egui::RichText::new("Nome Oficial do Envio").strong());
            ui.colored_label(egui::Color32::from_rgb(235, 75, 75), "*");
            ui.label(egui::RichText::new(":").strong());
            ui.add_space(8.0);
            ui.checkbox(&mut app.meta.manual_name_override, "Editar manualmente");
            if ui.small_button("Regenerar").clicked() {
                app.meta.manual_name_override = false;
                app.meta.release_name = build_official_release_name(&app.meta);
            }
            ui.add_space(8.0);
            ui.colored_label(
                egui::Color32::GRAY,
                format!(
                    "Pasta: {}/",
                    sanitize_windows_filename(&app.meta.release_name)
                ),
            );
        });
        ui.add_enabled(
            app.meta.manual_name_override,
            egui::TextEdit::singleline(&mut app.meta.release_name).desired_width(f32::INFINITY),
        );

        ui.add_space(6.0);
        ui.label("Descrição (BBCode — fonte/release de sincronia, delays, espectrograma):");
        ui.add(
            egui::TextEdit::multiline(&mut app.meta.description)
                .desired_rows(3)
                .desired_width(f32::INFINITY),
        );

        ui.add_space(4.0);
        let missing_src_mi = app.meta.source_mediainfo.trim().is_empty();
        egui::CollapsingHeader::new(if missing_src_mi {
            "MediaInfo da Fonte / Áudio (⚠ MediaInfo da fonte ausente)"
        } else {
            "Visualizar / Editar MediaInfo (Fonte & Áudio)"
        })
        .id_salt("mediainfo_collapsing")
        .default_open(missing_src_mi)
        .show(ui, |ui| {
            ui.label(egui::RichText::new("MediaInfo da Fonte:").strong());
            ui.add(
                egui::TextEdit::multiline(&mut app.meta.source_mediainfo)
                    .desired_rows(5)
                    .desired_width(f32::INFINITY)
                    .font(egui::TextStyle::Monospace),
            );
            ui.add_space(4.0);
            ui.label(egui::RichText::new("MediaInfo do Áudio Extraído:").strong());
            ui.add(
                egui::TextEdit::multiline(&mut app.meta.audio_mediainfo)
                    .desired_rows(5)
                    .desired_width(f32::INFINITY)
                    .font(egui::TextStyle::Monospace),
            );
        });
    });
}

fn show_process_and_upload_section(app: &mut CapybaraApp, ui: &mut egui::Ui) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(
            egui::RichText::new("3. Processar & Enviar")
                .strong()
                .size(15.0),
        );
        ui.add_space(4.0);

        // Opções rápidas em uma linha
        ui.horizontal_wrapped(|ui| {
            ui.label("Saída:");
            let edited = ui
                .add(
                    egui::TextEdit::singleline(&mut app.config.default_output_dir)
                        .desired_width(300.0),
                )
                .lost_focus();
            if edited {
                let _ = save_config(&app.config);
            }
            if ui.button("Escolher...").clicked() {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    app.config.default_output_dir = dir.to_string_lossy().to_string();
                    let _ = save_config(&app.config);
                }
            }
            ui.separator();
            if ui
                .checkbox(
                    &mut app.config.auto_download_and_inject,
                    format!(
                        "Injetar no cliente ({})",
                        app.config.torrent_client.client_type.label()
                    ),
                )
                .changed()
            {
                let _ = save_config(&app.config);
            }
            if ui
                .checkbox(
                    &mut app.config.image_host.auto_generate_spectrogram,
                    "Gerar espectrograma",
                )
                .changed()
            {
                let _ = save_config(&app.config);
            }
        });

        ui.add_space(6.0);

        let pre_errors = app.pre_upload_errors();
        let has_exact_dupe = app.has_exact_dupe();

        if pre_errors.is_empty() {
            ui.colored_label(
                egui::Color32::from_rgb(80, 200, 120),
                if app.package_output.is_some() {
                    "✔ Pacote e metadados prontos para envio."
                } else {
                    "✔ Tudo pronto — clique abaixo para extrair, gerar .torrent e enviar automaticamente."
                },
            );
        } else {
            for e in &pre_errors {
                ui.colored_label(egui::Color32::from_rgb(240, 100, 100), format!("✖ {e}"));
            }
        }

        if has_exact_dupe {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                ui.checkbox(
                    &mut app.confirm_dupe_override,
                    egui::RichText::new(
                        "Confirmar envio mesmo com possível duplicata encontrada no Dupe Check",
                    )
                    .color(egui::Color32::from_rgb(240, 190, 80)),
                );
            });
        }

        ui.add_space(8.0);

        let can_primary_upload =
            pre_errors.is_empty() && (!has_exact_dupe || app.confirm_dupe_override) && !app.busy;
        let can_package_only = app.is_input_ready_for_packaging()
            && !app.meta.release_name.trim().is_empty()
            && !app.config.announce_url.trim().is_empty()
            && !app.busy;

        ui.horizontal_wrapped(|ui| {
            if app.package_output.is_none() {
                let primary_btn = egui::Button::new(
                    egui::RichText::new("Processar e Enviar")
                        .strong()
                        .size(15.0)
                        .color(egui::Color32::WHITE),
                )
                .fill(if can_primary_upload {
                    egui::Color32::from_rgb(38, 138, 78)
                } else {
                    egui::Color32::from_gray(70)
                })
                .min_size(egui::vec2(180.0, 32.0));

                if ui.add_enabled(can_primary_upload, primary_btn).clicked() {
                    app.start_packaging(true);
                }

                if ui
                    .add_enabled(
                        can_package_only,
                        egui::Button::new("Apenas Processar (.torrent)")
                            .min_size(egui::vec2(0.0, 32.0)),
                    )
                    .clicked()
                {
                    app.start_packaging(false);
                }
            } else {
                let upload_btn = egui::Button::new(
                    egui::RichText::new("Enviar Dublagem")
                        .strong()
                        .size(15.0)
                        .color(egui::Color32::WHITE),
                )
                .fill(if can_primary_upload {
                    egui::Color32::from_rgb(38, 138, 78)
                } else {
                    egui::Color32::from_gray(70)
                })
                .min_size(egui::vec2(180.0, 32.0));

                if ui.add_enabled(can_primary_upload, upload_btn).clicked() {
                    app.start_upload();
                }

                if ui
                    .add_enabled(
                        can_package_only,
                        egui::Button::new("Reprocessar").min_size(egui::vec2(0.0, 32.0)),
                    )
                    .clicked()
                {
                    app.start_packaging(false);
                }

                if let Some(out) = app.package_output.clone() {
                    if ui
                        .add(egui::Button::new("Abrir Pasta").min_size(egui::vec2(0.0, 32.0)))
                        .clicked()
                    {
                        #[cfg(target_os = "windows")]
                        let _ = std::process::Command::new("explorer")
                            .arg(&out.output_folder)
                            .spawn();
                        #[cfg(target_os = "macos")]
                        let _ = std::process::Command::new("open")
                            .arg(&out.output_folder)
                            .spawn();
                        #[cfg(target_os = "linux")]
                        let _ = std::process::Command::new("xdg-open")
                            .arg(&out.output_folder)
                            .spawn();
                    }

                    if let Some(audio_path) = out.first_audio_path {
                        if ui
                            .add_enabled(
                                !app.busy,
                                egui::Button::new("Gerar Espectrograma")
                                    .min_size(egui::vec2(0.0, 32.0)),
                            )
                            .clicked()
                        {
                            app.busy = true;
                            let _ = app.worker.tx.send(WorkerCommand::GenerateSpectrogram {
                                cfg: app.config.clone(),
                                audio_path,
                                channels_hint: app.meta.channels.clone(),
                            });
                        }
                    }
                }
            }

            if app.busy {
                let cancel_btn = egui::Button::new(
                    egui::RichText::new("Cancelar Tarefa").color(egui::Color32::WHITE),
                )
                .fill(egui::Color32::from_rgb(190, 50, 50))
                .min_size(egui::vec2(0.0, 32.0));
                if ui.add(cancel_btn).clicked() {
                    app.cancel_current_task();
                }
            }
        });

        if app.busy || app.progress > 0.0 {
            ui.add_space(6.0);
            ui.add(egui::ProgressBar::new(app.progress).show_percentage());
        }

        // Resumo do pacote gerado
        if let Some(out) = &app.package_output {
            ui.add_space(8.0);
            egui::CollapsingHeader::new(format!(
                "Pacote gerado ({} arquivo(s) · InfoHash: {})",
                out.created_files.len(),
                out.infohash
            ))
            .id_salt("pkg_summary_collapsing")
            .default_open(app.upload_result.is_none())
            .show(ui, |ui| {
                ui.label(format!("Pasta: {}", out.output_folder.display()));
                ui.label(format!(".torrent: {}", out.torrent_path.display()));
                for f in &out.created_files {
                    let rel = f
                        .strip_prefix(&out.output_folder)
                        .unwrap_or(f)
                        .to_string_lossy();
                    ui.monospace(format!("  • {rel}"));
                }
                if let Some(spec) = &app.spectrogram_image {
                    ui.colored_label(
                        egui::Color32::from_rgb(130, 190, 255),
                        format!("Espectrograma ({}): {}", spec.host_name, spec.raw_url),
                    );
                }
            });
        }

        // Resultado final do upload
        let mut reset_requested = false;
        if let Some(res) = &app.upload_result {
            ui.add_space(10.0);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.colored_label(
                    egui::Color32::from_rgb(80, 200, 120),
                    egui::RichText::new(&res.upload.message).strong(),
                );
                if !res.upload.final_url.trim().is_empty() {
                    ui.horizontal(|ui| {
                        ui.label("URL final:");
                        ui.hyperlink_to(&res.upload.final_url, &res.upload.final_url);
                    });
                }
                if let Some(dl_path) = &res.downloaded_torrent_path {
                    ui.label(format!(
                        ".torrent baixado ({}): {}",
                        res.download_source.as_deref().unwrap_or("API"),
                        dl_path.display()
                    ));
                }
                if let Some(inj) = &res.injection_status {
                    ui.label(format!("Cliente BitTorrent: {inj}"));
                }
                ui.add_space(4.0);
                if ui.button("Iniciar Novo Upload").clicked() {
                    reset_requested = true;
                }
            });
        }
        if reset_requested {
            app.reset_for_new_upload();
        }
    });
}
