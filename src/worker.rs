use crate::cancel::CancelHandle;
use crate::capybara::client::{CapybaraClient, DupeEntry, UploadResult};
use crate::clients::{inject_torrent, test_connection};
use crate::config::{AppConfig, TorrentClientConfig};
use crate::domain::models::{
    Category, ExtractedFolderScan, InspectedDisc, InspectedVideo, PackageOutput, UploadMetadata,
};
use crate::imghost::{upload_image, UploadedImage};
use crate::media::audio_report::attach_package_report;
use crate::media::disc::{inspect_raw_disc, process_raw_disc_and_package};
use crate::media::extractor::{organize_extracted_folder_and_package, process_videos_and_package};
use crate::media::inspector::{inspect_videos, scan_extracted_folder};
use crate::media::spectrogram::{generate_audio_spectrogram, spectrogram_scope_label};
use crate::tmdb::client::{TmdbClient, TmdbDetails, TmdbSearchItem};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};

#[derive(Debug)]
pub enum WorkerCommand {
    InspectVideos {
        cfg: AppConfig,
        paths: Vec<PathBuf>,
    },
    ScanExtractedFolder {
        cfg: AppConfig,
        folder: PathBuf,
    },
    InspectRawDisc {
        cfg: AppConfig,
        path: PathBuf,
    },
    SearchTmdb {
        api_key: String,
        query: String,
        category: Category,
    },
    FetchTmdbDetails {
        api_key: String,
        id: u64,
        category: Category,
    },
    PackageVideos {
        cfg: AppConfig,
        videos: Vec<InspectedVideo>,
        meta: UploadMetadata,
        output_dir: PathBuf,
    },
    PackageExtractedFolder {
        cfg: AppConfig,
        scan: ExtractedFolderScan,
        meta: UploadMetadata,
        output_dir: PathBuf,
    },
    PackageRawDisc {
        cfg: AppConfig,
        disc: InspectedDisc,
        meta: UploadMetadata,
        output_dir: PathBuf,
    },
    GenerateSpectrogram {
        cfg: AppConfig,
        audio_path: PathBuf,
        channels_hint: String,
    },
    TestCapybaraApiKey {
        cfg: AppConfig,
    },
    CheckDupes {
        cfg: AppConfig,
        title_pt_br: String,
        title_original: String,
        tmdb_id: String,
        codec_label: String,
        source_label: String,
    },
    TestTorrentClient {
        client_cfg: TorrentClientConfig,
    },
    UploadAndInject {
        cfg: AppConfig,
        meta: UploadMetadata,
        output_folder: PathBuf,
        torrent_path: PathBuf,
        auto_download_and_inject: bool,
    },
}

#[derive(Debug, Clone)]
pub struct UploadPipelineResult {
    pub upload: UploadResult,
    pub downloaded_torrent_path: Option<PathBuf>,
    pub download_source: Option<String>,
    pub injection_status: Option<String>,
}

#[derive(Debug)]
pub enum WorkerEvent {
    Log(String),
    Progress(f32),
    VideosInspected(Result<Vec<InspectedVideo>, String>),
    FolderScanned(Result<ExtractedFolderScan, String>),
    DiscInspected(Result<InspectedDisc, String>),
    TmdbSearchResults(Result<Vec<TmdbSearchItem>, String>),
    TmdbDetailsLoaded(Result<TmdbDetails, String>),
    PackageFinished(Result<PackageOutput, String>),
    SpectrogramFinished(Result<UploadedImage, String>),
    ApiKeyTested(Result<String, String>),
    DupesChecked(Result<Vec<DupeEntry>, String>),
    TorrentClientTested(Result<String, String>),
    UploadFinished(Result<UploadPipelineResult, String>),
}

pub struct WorkerHandle {
    pub tx: Sender<WorkerCommand>,
    pub rx: Receiver<WorkerEvent>,
}

fn run_async_cancellable<T, F>(
    rt: &tokio::runtime::Runtime,
    cancel: &CancelHandle,
    fut: F,
) -> Result<T, String>
where
    T: Send + 'static,
    F: std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
{
    if cancel.is_cancelled() {
        return Err("Tarefa cancelada pelo usuário.".to_string());
    }
    let handle = rt.spawn(fut);
    cancel.set_async_abort(Some(handle.abort_handle()));
    let joined = rt.block_on(handle);
    cancel.set_async_abort(None);

    match joined {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(format!("{e:#}")),
        Err(join_err) if join_err.is_cancelled() => {
            Err("Tarefa cancelada pelo usuário.".to_string())
        }
        Err(join_err) => Err(format!("Falha interna na tarefa: {join_err}")),
    }
}

pub fn spawn_worker(cancel: CancelHandle) -> WorkerHandle {
    let (cmd_tx, cmd_rx) = mpsc::channel::<WorkerCommand>();
    let (evt_tx, evt_rx) = mpsc::channel::<WorkerEvent>();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Falha ao inicializar Tokio runtime");

        while let Ok(cmd) = cmd_rx.recv() {
            cancel.reset();
            let tx = evt_tx.clone();
            match cmd {
                WorkerCommand::InspectVideos { cfg, paths } => {
                    let _ = tx.send(WorkerEvent::Log(format!(
                        "Inspecionando {} arquivo(s) de vídeo...",
                        paths.len()
                    )));
                    let res = inspect_videos(&cfg, &paths, &cancel).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(WorkerEvent::VideosInspected(res));
                }
                WorkerCommand::ScanExtractedFolder { cfg, folder } => {
                    let _ = tx.send(WorkerEvent::Log(format!(
                        "Escaneando pasta já extraída: {}",
                        folder.display()
                    )));
                    let res =
                        scan_extracted_folder(&cfg, &folder, &cancel).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(WorkerEvent::FolderScanned(res));
                }
                WorkerCommand::InspectRawDisc { cfg, path } => {
                    let _ = tx.send(WorkerEvent::Log(format!(
                        "Analisando disco raw: {}",
                        path.display()
                    )));
                    let res = inspect_raw_disc(&cfg, &path, &cancel).map_err(|e| format!("{e:#}"));
                    let _ = tx.send(WorkerEvent::DiscInspected(res));
                }
                WorkerCommand::SearchTmdb {
                    api_key,
                    query,
                    category,
                } => {
                    let res = run_async_cancellable(&rt, &cancel, async move {
                        let client = TmdbClient::new(&api_key)?;
                        client.search(&query, category).await
                    });
                    let _ = tx.send(WorkerEvent::TmdbSearchResults(res));
                }
                WorkerCommand::FetchTmdbDetails {
                    api_key,
                    id,
                    category,
                } => {
                    let res = run_async_cancellable(&rt, &cancel, async move {
                        let client = TmdbClient::new(&api_key)?;
                        client.get_details(id, category).await
                    });
                    let _ = tx.send(WorkerEvent::TmdbDetailsLoaded(res));
                }
                WorkerCommand::PackageVideos {
                    cfg,
                    videos,
                    meta,
                    output_dir,
                } => {
                    let tx_log = tx.clone();
                    let tx_prog = tx.clone();
                    let res = process_videos_and_package(
                        &cfg,
                        &videos,
                        &meta,
                        &output_dir,
                        &cancel,
                        move |m| {
                            let _ = tx_log.send(WorkerEvent::Log(m));
                        },
                        move |p| {
                            let _ = tx_prog.send(WorkerEvent::Progress(p));
                        },
                    )
                    .and_then(|mut pkg| {
                        let tx_a = tx.clone();
                        attach_package_report(
                            &cfg,
                            &mut pkg,
                            &meta.fps,
                            &videos.iter().map(|v| v.path.clone()).collect::<Vec<_>>(),
                            &cancel,
                            move |m| {
                                let _ = tx_a.send(WorkerEvent::Log(m));
                            },
                        )?;
                        Ok(pkg)
                    })
                    .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(WorkerEvent::PackageFinished(res));
                }
                WorkerCommand::PackageExtractedFolder {
                    cfg,
                    scan,
                    meta,
                    output_dir,
                } => {
                    let tx_log = tx.clone();
                    let tx_prog = tx.clone();
                    let res = organize_extracted_folder_and_package(
                        &cfg,
                        &scan,
                        &meta,
                        &output_dir,
                        &cancel,
                        move |m| {
                            let _ = tx_log.send(WorkerEvent::Log(m));
                        },
                        move |p| {
                            let _ = tx_prog.send(WorkerEvent::Progress(p));
                        },
                    )
                    .and_then(|mut pkg| {
                        let tx_a = tx.clone();
                        attach_package_report(
                            &cfg,
                            &mut pkg,
                            &meta.fps,
                            &Vec::new(),
                            &cancel,
                            move |m| {
                                let _ = tx_a.send(WorkerEvent::Log(m));
                            },
                        )?;
                        Ok(pkg)
                    })
                    .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(WorkerEvent::PackageFinished(res));
                }
                WorkerCommand::PackageRawDisc {
                    cfg,
                    disc,
                    meta,
                    output_dir,
                } => {
                    let tx_log = tx.clone();
                    let tx_prog = tx.clone();
                    let res = process_raw_disc_and_package(
                        &cfg,
                        &disc,
                        &meta,
                        &output_dir,
                        &cancel,
                        move |m| {
                            let _ = tx_log.send(WorkerEvent::Log(m));
                        },
                        move |p| {
                            let _ = tx_prog.send(WorkerEvent::Progress(p));
                        },
                    )
                    .and_then(|mut pkg| {
                        let tx_a = tx.clone();
                        attach_package_report(
                            &cfg,
                            &mut pkg,
                            &meta.fps,
                            &Vec::new(),
                            &cancel,
                            move |m| {
                                let _ = tx_a.send(WorkerEvent::Log(m));
                            },
                        )?;
                        Ok(pkg)
                    })
                    .map_err(|e| format!("{e:#}"));
                    let _ = tx.send(WorkerEvent::PackageFinished(res));
                }
                WorkerCommand::GenerateSpectrogram {
                    cfg,
                    audio_path,
                    channels_hint,
                } => {
                    let tx_log = tx.clone();
                    let cancel_clone = cancel.clone();
                    let res = run_async_cancellable(&rt, &cancel, async move {
                        let _ = tx_log.send(WorkerEvent::Log(format!(
                            "Gerando espectrograma via ffmpeg ({})...",
                            spectrogram_scope_label(cfg.image_host.spectrogram_duration_secs)
                        )));
                        let png_path = tokio::task::spawn_blocking({
                            let cfg_b = cfg.clone();
                            let audio_b = audio_path.clone();
                            let ch_b = channels_hint.clone();
                            let cancel_b = cancel_clone.clone();
                            move || {
                                generate_audio_spectrogram(
                                    &cfg_b,
                                    &audio_b,
                                    &ch_b,
                                    cfg_b.image_host.spectrogram_duration_secs,
                                    &cancel_b,
                                )
                            }
                        })
                        .await??;

                        let _ = tx_log.send(WorkerEvent::Log(format!(
                            "Enviando espectrograma para {}...",
                            cfg.image_host.host_type.label()
                        )));
                        let uploaded = upload_image(&png_path, &cfg.image_host).await?;
                        for f in &uploaded.failures {
                            let _ = tx_log.send(WorkerEvent::Log(format!(
                                "Falha no host de imagens, usando fallback — {f}"
                            )));
                        }
                        let _ = tx_log.send(WorkerEvent::Log(format!(
                            "Espectrograma hospedado em {}: {}",
                            uploaded.host_name, uploaded.raw_url
                        )));
                        Ok(uploaded)
                    });
                    let _ = tx.send(WorkerEvent::SpectrogramFinished(res));
                }
                WorkerCommand::TestCapybaraApiKey { cfg } => {
                    let res = run_async_cancellable(&rt, &cancel, async move {
                        let client = CapybaraClient::from_config(&cfg)?;
                        client.test_api_key().await
                    });
                    let _ = tx.send(WorkerEvent::ApiKeyTested(res));
                }
                WorkerCommand::CheckDupes {
                    cfg,
                    title_pt_br,
                    title_original,
                    tmdb_id,
                    codec_label,
                    source_label,
                } => {
                    let res = run_async_cancellable(&rt, &cancel, async move {
                        let client = CapybaraClient::from_config(&cfg)?;
                        client
                            .check_pending_dupes(
                                &title_pt_br,
                                &title_original,
                                &tmdb_id,
                                &codec_label,
                                &source_label,
                            )
                            .await
                    });
                    let _ = tx.send(WorkerEvent::DupesChecked(res));
                }
                WorkerCommand::TestTorrentClient { client_cfg } => {
                    let res = run_async_cancellable(&rt, &cancel, async move {
                        test_connection(&client_cfg).await
                    });
                    let _ = tx.send(WorkerEvent::TorrentClientTested(res));
                }
                WorkerCommand::UploadAndInject {
                    cfg,
                    meta,
                    output_folder,
                    torrent_path,
                    auto_download_and_inject,
                } => {
                    let tx_log = tx.clone();
                    let res = run_async_cancellable(&rt, &cancel, async move {
                        let client = CapybaraClient::from_config(&cfg)?;
                        let _ = tx_log.send(WorkerEvent::Log(
                            "Enviando dublagem via POST https://capybarabr.com/api/dubs/upload..."
                                .to_string(),
                        ));
                        let upload = client.upload_dublagem(&meta, &torrent_path).await?;
                        let _ = tx_log.send(WorkerEvent::Log(upload.message.clone()));

                        let mut downloaded_torrent_path = None;
                        let mut download_source = None;
                        let mut injection_status = None;

                        if auto_download_and_inject {
                            let tid_str = upload.torrent_id.as_deref().unwrap_or("");
                            if !tid_str.is_empty() || upload.download_link.is_some() {
                                let _ = tx_log.send(WorkerEvent::Log(format!(
                                    "Baixando .torrent #{tid_str} corrigido via API do CapybaraBR..."
                                )));
                                let dest_torrent = torrent_path.with_file_name(format!(
                                    "[CAPYBARABR]_{}.torrent",
                                    if tid_str.is_empty() { "dub" } else { tid_str }
                                ));
                                match client
                                    .download_torrent_via_api(
                                        tid_str,
                                        upload.download_link.as_deref(),
                                        &dest_torrent,
                                    )
                                    .await
                                {
                                    Ok(dl) => {
                                        let _ = tx_log.send(WorkerEvent::Log(format!(
                                            ".torrent baixado ({}) | InfoHash: {}",
                                            dl.source_used, dl.infohash
                                        )));
                                        downloaded_torrent_path = Some(dl.path.clone());
                                        download_source = Some(dl.source_used.clone());

                                        let parent_dir =
                                            output_folder.parent().unwrap_or(&output_folder);
                                        let fname = dl
                                            .path
                                            .file_name()
                                            .map(|s| s.to_string_lossy().to_string())
                                            .unwrap_or_else(|| format!("{tid_str}.torrent"));

                                        let _ = tx_log.send(WorkerEvent::Log(format!(
                                            "Injetando .torrent no cliente ({}) com delay de {}s...",
                                            cfg.torrent_client.client_type.label(),
                                            cfg.torrent_client.inject_delay_secs
                                        )));
                                        match inject_torrent(
                                            &dl.bytes,
                                            &fname,
                                            &dl.infohash,
                                            parent_dir,
                                            &cfg.torrent_client,
                                        )
                                        .await
                                        {
                                            Ok(msg) => {
                                                let _ = tx_log.send(WorkerEvent::Log(msg.clone()));
                                                injection_status = Some(msg);
                                            }
                                            Err(e) => {
                                                let err_msg = format!("Erro na injeção: {e:#}");
                                                let _ = tx_log.send(WorkerEvent::Log(format!(
                                                    "[AVISO] {err_msg}"
                                                )));
                                                injection_status = Some(err_msg);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        let err_msg =
                                            format!("Falha ao baixar .torrent via API: {e:#}");
                                        let _ = tx_log
                                            .send(WorkerEvent::Log(format!("[AVISO] {err_msg}")));
                                        injection_status = Some(err_msg);
                                    }
                                }
                            } else {
                                let msg = "Upload aceito, mas não foi possível detectar o ID numérico do torrent na resposta para download automático.".to_string();
                                let _ = tx_log.send(WorkerEvent::Log(format!("[AVISO] {msg}")));
                                injection_status = Some(msg);
                            }
                        }

                        Ok(UploadPipelineResult {
                            upload,
                            downloaded_torrent_path,
                            download_source,
                            injection_status,
                        })
                    });

                    let _ = tx.send(WorkerEvent::UploadFinished(res));
                }
            }
        }
    });

    WorkerHandle {
        tx: cmd_tx,
        rx: evt_rx,
    }
}
