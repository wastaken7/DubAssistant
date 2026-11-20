use crate::domain::rules::validate_upload_metadata;
use crate::ui::app::CapybaraApp;
use crate::worker::WorkerCommand;
use eframe::egui;

pub fn show(app: &mut CapybaraApp, ui: &mut egui::Ui) {
    ui.heading("4. Revisão Final, Upload & Injeção no Cliente");
    ui.add_space(6.0);

    let errors = validate_upload_metadata(&app.meta);
    let has_pkg = app.package_output.is_some();

    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(egui::RichText::new("Checklist de Validação").strong());
        if has_pkg {
            ui.colored_label(
                egui::Color32::from_rgb(80, 200, 120),
                "✔ Pasta organizada e .torrent privado gerados",
            );
        } else {
            ui.colored_label(
                egui::Color32::from_rgb(240, 100, 100),
                "✖ Execute a etapa 3 (Processar) primeiro",
            );
        }

        if errors.is_empty() {
            ui.colored_label(
                egui::Color32::from_rgb(80, 200, 120),
                "✔ Todos os campos obrigatórios do formulário estão preenchidos",
            );
        } else {
            for e in &errors {
                ui.colored_label(egui::Color32::from_rgb(240, 100, 100), format!("✖ {e}"));
            }
        }

        ui.horizontal(|ui| {
            match &app.dupe_results {
                None => {
                    ui.colored_label(
                        egui::Color32::from_rgb(240, 190, 80),
                        "○ Dupe check ainda não executado",
                    );
                }
                Some(Ok(list)) if list.is_empty() => {
                    ui.colored_label(
                        egui::Color32::from_rgb(80, 200, 120),
                        "✔ Dupe check: nenhuma duplicata encontrada no Acervo",
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
                            "⚠ Dupe check: {} dublagem(ns) já existem para esta obra",
                            list.len()
                        ),
                    );
                }
                Some(Err(e)) => {
                    ui.colored_label(
                        egui::Color32::from_rgb(240, 160, 90),
                        format!("⚠ Dupe check falhou: {e}"),
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
            for d in list {
                let col = if d.likely_same_release {
                    egui::Color32::from_rgb(240, 110, 110)
                } else {
                    egui::Color32::from_rgb(235, 200, 110)
                };
                ui.colored_label(col, format!("   • {}", d.title));
            }
        }
    });

    ui.add_space(8.0);

    egui::CollapsingHeader::new("Visualizar / Editar MediaInfo da Fonte")
        .default_open(app.meta.source_mediainfo.trim().is_empty())
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut app.meta.source_mediainfo)
                    .desired_rows(6)
                    .desired_width(f32::INFINITY)
                    .font(egui::TextStyle::Monospace),
            );
        });

    egui::CollapsingHeader::new("Visualizar / Editar MediaInfo do Áudio Extraído")
        .default_open(app.meta.audio_mediainfo.trim().is_empty())
        .show(ui, |ui| {
            ui.add(
                egui::TextEdit::multiline(&mut app.meta.audio_mediainfo)
                    .desired_rows(6)
                    .desired_width(f32::INFINITY)
                    .font(egui::TextStyle::Monospace),
            );
        });

    ui.add_space(10.0);
    ui.checkbox(
        &mut app.config.auto_download_and_inject,
        format!(
            "Baixar .torrent corrigido via API e injetar automaticamente no cliente ({})",
            app.config.torrent_client.client_type.label()
        ),
    );

    ui.add_space(8.0);
    let upload_btn_label = if !app.config.capybara_api_key.trim().is_empty() {
        "Enviar Dublagem via API Oficial (/api/dubs/upload)"
    } else {
        "Enviar Dublagem via Cookies (/dublagens)"
    };
    let can_upload = has_pkg && errors.is_empty() && !app.busy;
    if ui
        .add_enabled(can_upload, egui::Button::new(upload_btn_label))
        .clicked()
    {
        if let Some(pkg) = &app.package_output {
            app.busy = true;
            app.upload_result = None;
            let _ = app.worker.tx.send(WorkerCommand::UploadAndInject {
                cfg: app.config.clone(),
                meta: app.meta.clone(),
                output_folder: pkg.output_folder.clone(),
                torrent_path: pkg.torrent_path.clone(),
                auto_download_and_inject: app.config.auto_download_and_inject,
            });
        }
    }

    if let Some(res) = &app.upload_result {
        ui.add_space(12.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.colored_label(
                egui::Color32::from_rgb(80, 200, 120),
                &res.upload.message,
            );
            ui.label(format!("URL final: {}", res.upload.final_url));
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
        });
    }
}
