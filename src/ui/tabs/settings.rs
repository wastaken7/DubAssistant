use crate::config::{save_config, AppConfig, ImageHostType, TorrentClientType};
use crate::ui::app::CapybaraApp;
use crate::worker::WorkerCommand;
use eframe::egui;

pub fn show(app: &mut CapybaraApp, ui: &mut egui::Ui) {
    ui.heading("Configurações");
    ui.add_space(6.0);

    // 1. Autenticação CapybaraBR (100% API Oficial)
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(egui::RichText::new("CapybaraBR — API Oficial").strong());
        egui::Grid::new("capy_cfg_grid")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("CapybaraBR API Key:");
                ui.add(
                    egui::TextEdit::singleline(&mut app.config.capybara_api_key)
                        .password(true)
                        .desired_width(420.0)
                        .hint_text("Configurações -> Chaves de API no site"),
                );
                ui.end_row();

                ui.label("Announce URL:");
                ui.add(
                    egui::TextEdit::singleline(&mut app.config.announce_url)
                        .desired_width(500.0)
                        .hint_text("https://capybarabr.com/announce/<passkey>"),
                );
                ui.end_row();
            });

        ui.horizontal(|ui| {
            if ui
                .add_enabled(!app.busy, egui::Button::new("Testar API Key"))
                .clicked()
            {
                let _ = save_config(&app.config);
                app.busy = true;
                let _ = app.worker.tx.send(WorkerCommand::TestCapybaraApiKey {
                    cfg: app.config.clone(),
                });
            }
            if let Some(status) = &app.session_status {
                match status {
                    Ok(m) => {
                        ui.colored_label(egui::Color32::from_rgb(80, 200, 120), m);
                    }
                    Err(e) => {
                        ui.colored_label(egui::Color32::from_rgb(240, 100, 100), e);
                    }
                }
            }
        });
    });

    ui.add_space(8.0);

    // 2. TMDB
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(egui::RichText::new("TMDB API").strong());
        ui.horizontal(|ui| {
            ui.label("API Key v3 ou Bearer Token:");
            ui.add(
                egui::TextEdit::singleline(&mut app.config.tmdb_api_key)
                    .password(true)
                    .desired_width(400.0),
            );
        });
    });

    ui.add_space(8.0);

    // 3. Cliente BitTorrent para injeção pós-upload
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(egui::RichText::new("Injeção no Cliente BitTorrent Pós-Upload").strong());
        let tc = &mut app.config.torrent_client;

        egui::ComboBox::from_id_salt("client_type_combo")
            .selected_text(tc.client_type.label())
            .show_ui(ui, |ui| {
                for ct in TorrentClientType::ALL {
                    ui.selectable_value(&mut tc.client_type, ct, ct.label());
                }
            });

        if tc.client_type == TorrentClientType::WatchFolder {
            ui.horizontal(|ui| {
                ui.label("Pasta Watch Folder:");
                ui.add(egui::TextEdit::singleline(&mut tc.watch_folder).desired_width(400.0));
                if ui.button("Escolher...").clicked() {
                    if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                        tc.watch_folder = dir.to_string_lossy().to_string();
                    }
                }
            });
        } else if tc.client_type != TorrentClientType::None {
            egui::Grid::new("tc_grid")
                .num_columns(2)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label("URL / Host:");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut tc.url).desired_width(260.0));
                        ui.label("Porta:");
                        ui.add(egui::DragValue::new(&mut tc.port).range(0..=65535));
                    });
                    ui.end_row();

                    if tc.client_type == TorrentClientType::QBittorrent {
                        ui.label("qui Proxy URL (opcional):");
                        ui.add(
                            egui::TextEdit::singleline(&mut tc.qui_proxy_url)
                                .desired_width(360.0)
                                .hint_text("Se preenchido, usa proxy qui em vez de login WebUI"),
                        );
                        ui.end_row();
                    }

                    ui.label("Usuário / Senha:");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut tc.username).desired_width(140.0));
                        ui.add(
                            egui::TextEdit::singleline(&mut tc.password)
                                .password(true)
                                .desired_width(160.0),
                        );
                    });
                    ui.end_row();

                    ui.label("Categoria / Tags:");
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(&mut tc.category).desired_width(140.0));
                        ui.label("Tags:");
                        ui.add(egui::TextEdit::singleline(&mut tc.tags).desired_width(160.0));
                    });
                    ui.end_row();

                    ui.label("Opções:");
                    ui.horizontal(|ui| {
                        ui.checkbox(&mut tc.skip_checking, "Skip Checking / SeedMode");
                        ui.checkbox(&mut tc.paused_on_add, "Adicionar Pausado");
                        ui.checkbox(&mut tc.allow_invalid_certs, "Ignorar SSL inválido");
                        ui.label("Delay (s):");
                        ui.add(egui::DragValue::new(&mut tc.inject_delay_secs).range(0..=60));
                    });
                    ui.end_row();

                    ui.label("Mapeamento Local -> Remoto (opcional):");
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut tc.local_path_prefix)
                                .desired_width(180.0)
                                .hint_text(r"Ex: D:\Dublagens"),
                        );
                        ui.label("->");
                        ui.add(
                            egui::TextEdit::singleline(&mut tc.remote_path_prefix)
                                .desired_width(180.0)
                                .hint_text("Ex: /downloads/dublagens"),
                        );
                    });
                    ui.end_row();
                });
        }

        if tc.client_type != TorrentClientType::None {
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!app.busy, egui::Button::new("Testar Conexão do Cliente"))
                    .clicked()
                {
                    let _ = save_config(&app.config);
                    app.busy = true;
                    let _ = app.worker.tx.send(WorkerCommand::TestTorrentClient {
                        client_cfg: app.config.torrent_client.clone(),
                    });
                }
                if let Some(st) = &app.client_status {
                    match st {
                        Ok(m) => {
                            ui.colored_label(egui::Color32::from_rgb(80, 200, 120), m);
                        }
                        Err(e) => {
                            ui.colored_label(egui::Color32::from_rgb(240, 100, 100), e);
                        }
                    }
                }
            });
        }
    });

    ui.add_space(8.0);

    // 4. Espectrograma & Host de Imagens
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(egui::RichText::new("Espectrograma de Áudio & Host de Imagens").strong());
        let ih = &mut app.config.image_host;

        egui::Grid::new("imghost_grid")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Host de Imagens:");
                egui::ComboBox::from_id_salt("imghost_combo")
                    .selected_text(ih.host_type.label())
                    .show_ui(ui, |ui| {
                        for ht in ImageHostType::ALL {
                            ui.selectable_value(&mut ih.host_type, ht, ht.label());
                        }
                    });
                ui.end_row();

                if ih.host_type == ImageHostType::Zipline {
                    ui.label("URL do Zipline:");
                    ui.add(
                        egui::TextEdit::singleline(&mut ih.custom_url)
                            .desired_width(360.0)
                            .hint_text("https://seu-zipline/api/upload"),
                    );
                    ui.end_row();
                }

                if ih.host_type.needs_api_key() || ih.host_type == ImageHostType::Catbox {
                    let lbl = if ih.host_type == ImageHostType::Catbox {
                        "Userhash Catbox (opcional):"
                    } else {
                        "API Key do Host:"
                    };
                    ui.label(lbl);
                    ui.add(
                        egui::TextEdit::singleline(&mut ih.api_key)
                            .password(true)
                            .desired_width(360.0),
                    );
                    ui.end_row();
                }

                ui.label("Opções do Espectrograma:");
                ui.horizontal(|ui| {
                    ui.checkbox(
                        &mut ih.auto_generate_spectrogram,
                        "Gerar e hospedar automaticamente ao processar",
                    );
                    ui.label("Duração (s, 0 = completo):");
                    ui.add(
                        egui::DragValue::new(&mut ih.spectrogram_duration_secs).range(0..=3600),
                    );
                });
                ui.end_row();
            });
    });

    ui.add_space(8.0);

    // 5. Ferramentas Externas
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Ferramentas de Mídia").strong());
            if ui.small_button("Auto-detectar").clicked() {
                app.config.fill_missing_tools();
            }
        });
        egui::Grid::new("tools_grid")
            .num_columns(3)
            .spacing([8.0, 6.0])
            .show(ui, |ui| {
                tool_row(ui, "MediaInfo CLI:", &mut app.config.mediainfo_path);
                tool_row(ui, "eac3to (Blu-ray):", &mut app.config.eac3to_path);
                tool_row(ui, "mkvmerge:", &mut app.config.mkvmerge_path);
                tool_row(ui, "mkvextract:", &mut app.config.mkvextract_path);
                tool_row(ui, "ffmpeg:", &mut app.config.ffmpeg_path);
                tool_row(ui, "ffprobe:", &mut app.config.ffprobe_path);
            });
    });

    ui.add_space(10.0);
    if ui.button("Salvar Configurações").clicked() {
        match save_config(&app.config) {
            Ok(_) => {
                app.status_banner = Some((true, "Configurações salvas com sucesso.".into()));
            }
            Err(e) => {
                app.status_banner = Some((false, format!("Erro ao salvar configurações: {e}")));
            }
        }
    }
}

fn tool_row(ui: &mut egui::Ui, label: &str, value: &mut String) {
    ui.label(label);
    ui.add(egui::TextEdit::singleline(value).desired_width(380.0));
    if AppConfig::tool_exists(value) {
        ui.colored_label(egui::Color32::from_rgb(80, 200, 120), "OK");
    } else {
        ui.colored_label(egui::Color32::from_rgb(240, 100, 100), "Não encontrado");
    }
    ui.end_row();
}
