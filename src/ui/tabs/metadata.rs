use crate::config::save_config;
use crate::domain::models::{AudioCodec, Category, DubVersion, SourceType};
use crate::domain::rules::{build_official_release_name, sanitize_windows_filename};
use crate::ui::app::{AppTab, CapybaraApp};
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
    ui.heading("2. Integração TMDB & Metadados do CapybaraBR");
    ui.add_space(6.0);

    // Bloco TMDB
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(egui::RichText::new("Busca no TMDB (título ou ID numérico)").strong());
        ui.horizontal(|ui| {
            let enter = ui
                .add(
                    egui::TextEdit::singleline(&mut app.tmdb_query)
                        .desired_width(360.0)
                        .hint_text("Ex: As Tartarugas Ninja ou 98566"),
                )
                .lost_focus()
                && ui.input(|i| i.key_pressed(egui::Key::Enter));

            if (ui
                .add_enabled(!app.busy, egui::Button::new("Buscar no TMDB"))
                .clicked()
                || enter)
                && !app.busy
            {
                app.busy = true;
                let _ = app.worker.tx.send(WorkerCommand::SearchTmdb {
                    api_key: app.config.tmdb_api_key.clone(),
                    query: app.tmdb_query.clone(),
                    category: app.meta.category,
                });
            }
        });

        if !app.tmdb_results.is_empty() {
            ui.add_space(6.0);
            egui::ScrollArea::vertical()
                .id_salt("tmdb_results_scroll")
                .max_height(140.0)
                .show(ui, |ui| {
                    let mut chosen: Option<(u64, Category)> = None;
                    for item in &app.tmdb_results {
                        ui.horizontal(|ui| {
                            if ui.button("Selecionar").clicked() {
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
        }
    });

    // Card visual com pôster e detalhes do TMDB selecionado
    if let Some(details) = &app.tmdb_details {
        ui.add_space(8.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal_top(|ui| {
                if let Some(tex) = &app.poster_texture {
                    let max_h = 165.0;
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
                                .size(17.0),
                        );
                        if !details.year.is_empty() {
                            ui.label(
                                egui::RichText::new(format!("({})", details.year))
                                    .size(15.0)
                                    .color(egui::Color32::LIGHT_GRAY),
                            );
                        }
                    });

                    if !details.title_original.is_empty()
                        && !details.title_original.eq_ignore_ascii_case(&details.title_pt_br)
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
                    ui.label(
                        egui::RichText::new(meta_badges.join("  ·  "))
                            .color(egui::Color32::from_rgb(130, 190, 255)),
                    );

                    if !details.genres.is_empty() {
                        ui.label(format!("Gêneros: {}", details.genres.join(", ")));
                    }

                    if !details.overview.trim().is_empty() {
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new(&details.overview)
                                .small()
                                .color(egui::Color32::from_gray(205)),
                        );
                    }
                });
            });
        });
    }

    ui.add_space(10.0);

    // Painel de Dupe Check no Acervo de Dublagens
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Dupe Check (Acervo de Dublagens)").strong());
            if ui
                .add_enabled(
                    !app.busy,
                    egui::Button::new("Verificar Duplicatas no CapybaraBR"),
                )
                .clicked()
            {
                app.trigger_dupe_check();
            }
        });
        match &app.dupe_results {
            None => {
                ui.colored_label(
                    egui::Color32::GRAY,
                    "Ainda não verificado (roda automaticamente ao carregar um título do TMDB).",
                );
            }
            Some(Ok(list)) if list.is_empty() => {
                ui.colored_label(
                    egui::Color32::from_rgb(80, 200, 120),
                    "✔ Nenhuma dublagem encontrada no Acervo para este título.",
                );
            }
            Some(Ok(list)) => {
                let has_exact = list.iter().any(|d| d.likely_same_release);
                let header_color = if has_exact {
                    egui::Color32::from_rgb(240, 100, 100)
                } else {
                    egui::Color32::from_rgb(240, 190, 80)
                };
                ui.colored_label(
                    header_color,
                    format!(
                        "⚠ {} dublagem(ns) já existem no Acervo para esta obra — verifique antes de enviar:",
                        list.len()
                    ),
                );
                for d in list {
                    let prefix = if d.likely_same_release {
                        "[POSSÍVEL DUPLICATA] "
                    } else {
                        "• "
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
            Some(Err(e)) => {
                ui.colored_label(
                    egui::Color32::from_rgb(240, 160, 90),
                    format!("Não foi possível verificar duplicatas: {e}"),
                );
            }
        }
    });

    ui.add_space(10.0);

    // Formulário de metadados
    let mut recalc_name = false;

    egui::Grid::new("meta_grid")
        .num_columns(2)
        .spacing([12.0, 8.0])
        .show(ui, |ui| {
            required_label(ui, "Categoria");
            ui.horizontal(|ui| {
                if ui
                    .radio_value(&mut app.meta.category, Category::Movie, Category::Movie.label())
                    .changed()
                {
                    recalc_name = true;
                }
                if ui
                    .radio_value(
                        &mut app.meta.category,
                        Category::Series,
                        Category::Series.label(),
                    )
                    .changed()
                {
                    recalc_name = true;
                }
            });
            ui.end_row();

            required_label(ui, "TMDB ID");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut app.meta.tmdb_id).desired_width(120.0));
                if ui
                    .add_enabled(!app.busy, egui::Button::new("Carregar Detalhes/Pôster"))
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
                ui.horizontal(|ui| {
                    if ui
                        .add(egui::TextEdit::singleline(&mut app.meta.season_number).desired_width(60.0))
                        .changed()
                    {
                        recalc_name = true;
                    }
                    if let Some(details) = &app.tmdb_details {
                        for s in &details.seasons {
                            if s.season_number > 0
                                && ui
                                    .small_button(format!("S{:02} ({} ep)", s.season_number, s.episode_count))
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

            ui.label("Título em PT-BR:");
            if ui
                .add(egui::TextEdit::singleline(&mut app.meta.title_pt_br).desired_width(400.0))
                .changed()
            {
                recalc_name = true;
            }
            ui.end_row();

            ui.label("Título Original:");
            if ui
                .add(egui::TextEdit::singleline(&mut app.meta.title_original).desired_width(400.0))
                .changed()
            {
                recalc_name = true;
            }
            ui.end_row();

            ui.label("Ano da Obra:");
            if ui
                .add(egui::TextEdit::singleline(&mut app.meta.year).desired_width(100.0))
                .changed()
            {
                recalc_name = true;
            }
            ui.end_row();

            ui.label("Codec / Bitrate / Canais:");
            ui.horizontal(|ui| {
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
                ui.label("Bitrate (kbps):");
                if ui
                    .add(egui::TextEdit::singleline(&mut app.meta.bitrate_kbps).desired_width(70.0))
                    .changed()
                {
                    recalc_name = true;
                }
                ui.label("Canais:");
                if ui
                    .add(egui::TextEdit::singleline(&mut app.meta.channels).desired_width(60.0))
                    .changed()
                {
                    recalc_name = true;
                }
            });
            ui.end_row();

            required_label(ui, "Versão da Dublagem");
            egui::ComboBox::from_id_salt("version_combo")
                .selected_text(app.meta.version.label())
                .show_ui(ui, |ui| {
                    for v in DubVersion::ALL {
                        ui.selectable_value(&mut app.meta.version, v, v.label());
                    }
                });
            ui.end_row();

            required_label(ui, "Fonte");
            ui.horizontal(|ui| {
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
                ui.label("Rótulo da fonte (opcional):");
                ui.add(
                    egui::TextEdit::singleline(&mut app.meta.source_label)
                        .desired_width(180.0)
                        .hint_text("Ex: Blu-ray Remux, DVD-R NTSC"),
                );
            });
            ui.end_row();

            ui.label("Estúdio / Ano da Dublagem:");
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut app.meta.studio)
                        .desired_width(200.0)
                        .hint_text("Ex: Delart, Herbert Richers"),
                );
                ui.label("Ano dub:");
                ui.add(
                    egui::TextEdit::singleline(&mut app.meta.dub_year)
                        .desired_width(70.0)
                        .hint_text("Ex: 2014"),
                );
            });
            ui.end_row();

            ui.label("FPS / Observação:");
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut app.meta.fps).desired_width(80.0));
                ui.label("Nota curta:");
                ui.add(
                    egui::TextEdit::singleline(&mut app.meta.note)
                        .desired_width(260.0)
                        .hint_text("Opcional (máx 255 chars)"),
                );
            });
            ui.end_row();

            ui.label("Anônimo:");
            if ui
                .checkbox(&mut app.meta.anon, "Enviar anonimamente")
                .changed()
            {
                app.config.default_anon = app.meta.anon;
                let _ = save_config(&app.config);
            }
            ui.end_row();
        });

    if recalc_name {
        app.refresh_auto_release_name();
    }

    ui.add_space(10.0);
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            ui.label(egui::RichText::new("Nome Oficial do Envio").strong());
            ui.colored_label(egui::Color32::from_rgb(235, 75, 75), "*");
            ui.label(egui::RichText::new(":").strong());
            ui.add_space(8.0);
            ui.checkbox(&mut app.meta.manual_name_override, "Editar manualmente");
            if ui.small_button("Regenerar Padrão").clicked() {
                app.meta.manual_name_override = false;
                app.meta.release_name = build_official_release_name(&app.meta);
            }
        });
        ui.add_enabled(
            app.meta.manual_name_override,
            egui::TextEdit::singleline(&mut app.meta.release_name).desired_width(f32::INFINITY),
        );
        ui.label(format!(
            "Pasta no Windows: {}/",
            sanitize_windows_filename(&app.meta.release_name)
        ));
    });

    ui.add_space(8.0);
    ui.label("Descrição (BBCode — informe fonte/release de sincronia e eventuais delays):");
    ui.add(
        egui::TextEdit::multiline(&mut app.meta.description)
            .desired_rows(4)
            .desired_width(f32::INFINITY),
    );

    ui.add_space(10.0);
    if ui.button("Avançar para 3. Processar & Empacotar ->").clicked() {
        app.active_tab = AppTab::Process;
    }
}
