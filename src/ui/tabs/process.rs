use crate::config::save_config;
use crate::domain::models::InputMode;
use crate::ui::app::{AppTab, CapybaraApp};
use crate::worker::WorkerCommand;
use eframe::egui;
use std::path::PathBuf;

pub fn show(app: &mut CapybaraApp, ui: &mut egui::Ui) {
    ui.heading("3. Processar, Organizar & Gerar .torrent Privado");
    ui.add_space(6.0);

    ui.horizontal(|ui| {
        ui.label("Pasta base de saída:");
        let edited = ui
            .add(
                egui::TextEdit::singleline(&mut app.config.default_output_dir)
                    .desired_width(420.0),
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
    });

    if app.config.announce_url.trim().is_empty() {
        ui.add_space(6.0);
        ui.colored_label(
            egui::Color32::from_rgb(240, 190, 80),
            "Aviso: URL de announce ainda não preenchida. Informe sua Announce URL pessoal na aba Configurações.",
        );
    }

    ui.add_space(10.0);

    let ready = match app.input_mode {
        InputMode::VideoFiles => !app.inspected_videos.is_empty(),
        InputMode::ExtractedFolder => app.folder_scan.is_some(),
        InputMode::RawDisc => app
            .inspected_disc
            .as_ref()
            .map(|d| d.titles.iter().any(|t| t.selected))
            .unwrap_or(false),
    } && !app.meta.release_name.trim().is_empty()
        && !app.config.announce_url.trim().is_empty();

    ui.horizontal(|ui| {
        if ui
            .add_enabled(
                ready && !app.busy,
                egui::Button::new("Extrair / Organizar Pasta e Gerar .torrent Privado"),
            )
            .clicked()
        {
            app.busy = true;
            app.progress = 0.0;
            app.package_output = None;
            let out_dir = PathBuf::from(app.config.default_output_dir.trim());
            match app.input_mode {
                InputMode::VideoFiles => {
                    let _ = app.worker.tx.send(WorkerCommand::PackageVideos {
                        cfg: app.config.clone(),
                        videos: app.inspected_videos.clone(),
                        meta: app.meta.clone(),
                        output_dir: out_dir,
                    });
                }
                InputMode::ExtractedFolder => {
                    if let Some(scan) = app.folder_scan.clone() {
                        let _ = app.worker.tx.send(WorkerCommand::PackageExtractedFolder {
                            cfg: app.config.clone(),
                            scan,
                            meta: app.meta.clone(),
                            output_dir: out_dir,
                        });
                    }
                }
                InputMode::RawDisc => {
                    if let Some(disc) = app.inspected_disc.clone() {
                        let _ = app.worker.tx.send(WorkerCommand::PackageRawDisc {
                            cfg: app.config.clone(),
                            disc,
                            meta: app.meta.clone(),
                            output_dir: out_dir,
                        });
                    }
                }
            }
        }

        if app.busy {
            let cancel_btn = egui::Button::new(
                egui::RichText::new("Cancelar Tarefa").color(egui::Color32::WHITE),
            )
            .fill(egui::Color32::from_rgb(190, 50, 50));
            if ui.add(cancel_btn).clicked() {
                app.cancel_current_task();
            }
        }
    });

    ui.add_space(8.0);
    ui.add(egui::ProgressBar::new(app.progress).show_percentage());

    if let Some(out) = &app.package_output {
        ui.add_space(10.0);
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.colored_label(
                egui::Color32::from_rgb(80, 200, 120),
                "Pacote pronto para upload!",
            );
            ui.label(format!("Pasta gerada: {}", out.output_folder.display()));
            ui.label(format!(".torrent: {}", out.torrent_path.display()));
            ui.label(format!("InfoHash: {}", out.infohash));
            ui.label(format!("Arquivos gerados ({}):", out.created_files.len()));
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

            let audio_for_spec = out.first_audio_path.clone();
            ui.horizontal(|ui| {
                if ui.button("Abrir Pasta").clicked() {
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
                if let Some(audio_path) = audio_for_spec {
                    if ui
                        .add_enabled(
                            !app.busy,
                            egui::Button::new("Gerar & Hospedar Espectrograma"),
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
                if ui.button("Avançar para 4. Upload ->").clicked() {
                    app.active_tab = AppTab::Upload;
                }
            });
        });
    }

    ui.add_space(10.0);
    ui.label(egui::RichText::new("Log de Execução:").strong());
    egui::ScrollArea::vertical()
        .id_salt("process_log_scroll")
        .max_height(260.0)
        .stick_to_bottom(true)
        .show(ui, |ui| {
            for line in &app.logs {
                ui.monospace(line);
            }
        });
}
