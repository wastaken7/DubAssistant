use crate::cancel::{run_command_cancellable, run_command_cancellable_with_stdout, CancelHandle};
use crate::config::AppConfig;
use crate::domain::models::{
    AudioCodec, Category, ExtractedFolderScan, InspectedVideo, MediaTrack, PackageOutput,
    TrackKind, UploadMetadata,
};
use crate::domain::rules::{extract_episode_tag, sanitize_windows_filename};
use crate::media::inspector::{is_portuguese_pt, run_mediainfo};
use crate::torrent::creator::create_private_torrent;
use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Processa vídeos (.mkv / .mp4): extrai áudio puro + legendas selecionadas em **uma única leitura**
/// do arquivo (sem recodificar), coloca todos os arquivos na mesma pasta raiz (sem subpasta Legendas/),
/// gera `mediainfo.txt` + `audio_mediainfo` e cria o `.torrent` privado.
pub fn process_videos_and_package<FLog, FProg>(
    cfg: &AppConfig,
    videos: &[InspectedVideo],
    meta: &UploadMetadata,
    output_base_dir: &Path,
    cancel: &CancelHandle,
    mut log: FLog,
    mut on_progress: FProg,
) -> Result<PackageOutput>
where
    FLog: FnMut(String),
    FProg: FnMut(f32),
{
    if cancel.is_cancelled() {
        bail!("Tarefa cancelada pelo usuário.");
    }
    if videos.is_empty() {
        bail!("Nenhum vídeo selecionado para extração.");
    }
    if meta.category == Category::Series && videos.len() < 2 {
        log(
            "Aviso: apenas 1 episódio selecionado em modo Série (o site exige temporada completa)."
                .into(),
        );
    }

    let safe_root_name = sanitize_windows_filename(&meta.release_name);
    let out_folder = output_base_dir.join(&safe_root_name);
    if out_folder.exists() {
        std::fs::remove_dir_all(&out_folder).with_context(|| {
            format!("Falha ao limpar pasta existente: {}", out_folder.display())
        })?;
    }
    std::fs::create_dir_all(&out_folder)?;

    let mut created_files = Vec::new();
    let mut delay_warnings = Vec::new();
    let mut first_extracted_audio: Option<PathBuf> = None;

    let extract_weight = 0.80_f32;
    let per_video = extract_weight / (videos.len() as f32);

    for (idx, video) in videos.iter().enumerate() {
        if cancel.is_cancelled() {
            bail!("Tarefa cancelada pelo usuário.");
        }

        let ep_prefix = if meta.category == Category::Series {
            video
                .episode_tag
                .clone()
                .unwrap_or_else(|| format!("S{}E{:02}", meta.season_number.trim(), idx + 1))
        } else {
            String::new()
        };

        let selected_audio: Vec<&MediaTrack> = video
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Audio && t.selected)
            .collect();

        if selected_audio.is_empty() {
            bail!(
                "Nenhuma faixa de áudio selecionada para '{}'.",
                video.path.display()
            );
        }
        if selected_audio.len() > 1 {
            bail!(
                "Selecione apenas 1 faixa de áudio por arquivo (regra: 1 torrent = 1 dublagem) em '{}'.",
                video.path.display()
            );
        }

        let audio_track = selected_audio[0];
        let codec = audio_track.allowed_audio_codec.ok_or_else(|| {
            anyhow::anyhow!(
                "Codec de áudio '{}' não é aceito pelo CapybaraBR (arquivo {}).",
                audio_track.codec_raw,
                video.path.display()
            )
        })?;

        if let Some(delay) = audio_track.delay_ms {
            if delay != 0 {
                let msg = format!(
                    "Delay detectado em {}: {} ms (informe na descrição conforme regras).",
                    video.path.file_name().unwrap_or_default().to_string_lossy(),
                    delay
                );
                log(format!("[AVISO] {msg}"));
                delay_warnings.push(msg);
            }
        }

        let audio_ext = codec.pure_extension();
        let audio_filename = if meta.category == Category::Series {
            format!("{safe_root_name} - {ep_prefix}.{audio_ext}")
        } else {
            format!("{safe_root_name}.{audio_ext}")
        };
        let audio_dest = out_folder.join(&audio_filename);

        // Prepara todas as faixas de legenda selecionadas diretamente na pasta raiz (sem subpasta)
        let selected_subs: Vec<&MediaTrack> = video
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Subtitle && t.selected)
            .collect();

        let mut sub_targets: Vec<(&MediaTrack, PathBuf)> = Vec::with_capacity(selected_subs.len());
        let mut used_names: Vec<PathBuf> = vec![audio_dest.clone()];

        for (sub_i, sub) in selected_subs.iter().enumerate() {
            let sub_ext = subtitle_extension(&sub.codec_raw);
            let tag = if sub.forced || sub.name.to_ascii_lowercase().contains("forced") {
                "Forced"
            } else {
                "Full"
            };
            let lang_label = if is_portuguese_pt(sub) {
                "PT-PT".to_string()
            } else if sub.language.to_ascii_lowercase().starts_with("pt")
                || sub.language.eq_ignore_ascii_case("por")
            {
                "PT-BR".to_string()
            } else {
                sub.language.to_ascii_uppercase()
            };
            let base_sub_name = if meta.category == Category::Series {
                format!("{ep_prefix} - {lang_label} ({tag})")
            } else {
                format!("{lang_label} ({tag})")
            };
            let mut sub_dest = out_folder.join(format!("{base_sub_name}.{sub_ext}"));
            if used_names.contains(&sub_dest) || sub_dest.exists() {
                sub_dest = out_folder.join(format!("{base_sub_name} #{}.{sub_ext}", sub_i + 1));
            }
            used_names.push(sub_dest.clone());
            sub_targets.push((*sub, sub_dest));
        }

        log(format!(
            "[{}/{}] Extraindo: áudio ({}) + {} legenda(s) -> {}",
            idx + 1,
            videos.len(),
            codec.title_label(),
            sub_targets.len(),
            audio_filename
        ));

        let video_base = (idx as f32) * per_video;
        extract_tracks_single_pass(
            cfg,
            video,
            audio_track,
            &audio_dest,
            &sub_targets,
            cancel,
            |p| on_progress((video_base + p * per_video).clamp(0.0, extract_weight)),
        )?;

        if first_extracted_audio.is_none() {
            first_extracted_audio = Some(audio_dest.clone());
        }
        created_files.push(audio_dest);
        for (_, sub_dest) in sub_targets {
            if sub_dest.exists() {
                created_files.push(sub_dest);
            }
        }

        // Capítulos (apenas metadados leves no MKV, rápido e isolado para não falhar áudio)
        if video.is_mkv && video.has_chapters && meta.category == Category::Movie && idx == 0 {
            let chap_dest = out_folder.join("chapters.txt");
            log("Extraindo capítulos -> chapters.txt".into());
            if let Ok(true) = extract_mkv_chapters(cfg, &video.path, &chap_dest, cancel) {
                created_files.push(chap_dest);
            }
        }

        on_progress(((idx + 1) as f32 * per_video).min(extract_weight));
    }

    if cancel.is_cancelled() {
        bail!("Tarefa cancelada pelo usuário.");
    }

    // Gravar mediainfo.txt da fonte (1º vídeo)
    let source_mediainfo = if !meta.source_mediainfo.trim().is_empty() {
        meta.source_mediainfo.clone()
    } else {
        videos[0].raw_mediainfo.clone()
    };
    if !source_mediainfo.trim().is_empty() {
        let mi_path = out_folder.join("mediainfo.txt");
        std::fs::write(&mi_path, &source_mediainfo)?;
        created_files.push(mi_path);
        log("Salvo mediainfo.txt da fonte.".into());
    }

    // Gerar MediaInfo do áudio extraído
    let audio_mediainfo = if let Some(audio_path) = &first_extracted_audio {
        log("Gerando MediaInfo do áudio extraído...".into());
        run_mediainfo(cfg, audio_path, cancel)?
    } else {
        String::new()
    };
    on_progress(0.85);

    // Criar .torrent privado
    let torrent_path = output_base_dir.join(format!("{safe_root_name}.torrent"));
    log(format!(
        "Gerando .torrent privado em {}...",
        torrent_path.display()
    ));
    let created =
        create_private_torrent(&out_folder, &torrent_path, &cfg.announce_url, cancel, |p| {
            on_progress(0.85 + p * 0.15)
        })?;
    log(format!(
        ".torrent criado! InfoHash: {} ({} bytes de conteúdo)",
        created.infohash, created.total_size
    ));

    Ok(PackageOutput {
        output_folder: out_folder,
        torrent_path: created.torrent_path,
        infohash: created.infohash,
        first_audio_path: first_extracted_audio,
        source_mediainfo,
        audio_mediainfo,
        created_files,
        delay_warnings,
        audio_report: None,
        measured_bitrate_kbps: None,
        corrected_release_name: None,
    })
}

/// Organiza uma pasta já extraída (Modo B) colocando todos os arquivos válidos na mesma pasta raiz
/// e gera o `.torrent` privado.
pub fn organize_extracted_folder_and_package<FLog, FProg>(
    cfg: &AppConfig,
    scan: &ExtractedFolderScan,
    meta: &UploadMetadata,
    output_base_dir: &Path,
    cancel: &CancelHandle,
    mut log: FLog,
    mut on_progress: FProg,
) -> Result<PackageOutput>
where
    FLog: FnMut(String),
    FProg: FnMut(f32),
{
    if cancel.is_cancelled() {
        bail!("Tarefa cancelada pelo usuário.");
    }
    if scan.audio_files.is_empty() {
        bail!("Nenhum arquivo de áudio puro permitido encontrado na pasta selecionada.");
    }
    if !scan.forbidden_files.is_empty() {
        log(format!(
            "[AVISO] {} arquivo(s) proibido(s) detectado(s) na pasta original serão IGNORADOS na organização.",
            scan.forbidden_files.len()
        ));
    }

    let safe_root_name = sanitize_windows_filename(&meta.release_name);
    let out_folder = output_base_dir.join(&safe_root_name);
    if out_folder.exists() {
        std::fs::remove_dir_all(&out_folder)?;
    }
    std::fs::create_dir_all(&out_folder)?;

    let mut created_files = Vec::new();
    let mut first_audio_dest: Option<PathBuf> = None;

    for (idx, src_audio) in scan.audio_files.iter().enumerate() {
        if cancel.is_cancelled() {
            bail!("Tarefa cancelada pelo usuário.");
        }
        let ext = src_audio
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_else(|| meta.audio_codec.pure_extension().to_string());
        let normalized_ext = AudioCodec::from_extension(&ext)
            .map(|c| c.pure_extension().to_string())
            .unwrap_or(ext);

        let dest_name = if meta.category == Category::Series || scan.audio_files.len() > 1 {
            let ep_tag = extract_episode_tag(src_audio)
                .map(|(_, _, tag)| tag)
                .unwrap_or_else(|| format!("S{}E{:02}", meta.season_number.trim(), idx + 1));
            format!("{safe_root_name} - {ep_tag}.{normalized_ext}")
        } else {
            format!("{safe_root_name}.{normalized_ext}")
        };
        let dest = out_folder.join(&dest_name);
        log(format!("Copiando áudio -> {dest_name}"));
        std::fs::copy(src_audio, &dest)?;
        if first_audio_dest.is_none() {
            first_audio_dest = Some(dest.clone());
        }
        created_files.push(dest);
        on_progress(((idx + 1) as f32 / (scan.audio_files.len() as f32 + 1.0)) * 0.6);
    }

    // Legendas diretamente na pasta raiz (sem subpasta Legendas/)
    for sub in &scan.subtitle_files {
        if cancel.is_cancelled() {
            bail!("Tarefa cancelada pelo usuário.");
        }
        let fname = sub.file_name().context("Nome de legenda inválido")?;
        let dest = out_folder.join(fname);
        log(format!("Copiando legenda -> {}", fname.to_string_lossy()));
        std::fs::copy(sub, &dest)?;
        created_files.push(dest);
    }

    for chap in &scan.chapter_files {
        if cancel.is_cancelled() {
            bail!("Tarefa cancelada pelo usuário.");
        }
        let fname = chap.file_name().context("Nome de capítulo inválido")?;
        let dest = out_folder.join(fname);
        log(format!("Copiando capítulos -> {}", fname.to_string_lossy()));
        std::fs::copy(chap, &dest)?;
        created_files.push(dest);
    }

    let source_mediainfo = if !meta.source_mediainfo.trim().is_empty() {
        meta.source_mediainfo.clone()
    } else {
        scan.source_mediainfo.clone()
    };
    if !source_mediainfo.trim().is_empty() {
        let mi_path = out_folder.join("mediainfo.txt");
        std::fs::write(&mi_path, &source_mediainfo)?;
        created_files.push(mi_path);
        log("Salvo mediainfo.txt.".into());
    }

    let audio_mediainfo = if let Some(first) = &first_audio_dest {
        log("Gerando MediaInfo do áudio...".into());
        run_mediainfo(cfg, first, cancel)?
    } else {
        scan.audio_mediainfo.clone()
    };
    on_progress(0.75);

    let torrent_path = output_base_dir.join(format!("{safe_root_name}.torrent"));
    log(format!(
        "Gerando .torrent privado em {}...",
        torrent_path.display()
    ));
    let created =
        create_private_torrent(&out_folder, &torrent_path, &cfg.announce_url, cancel, |p| {
            on_progress(0.75 + p * 0.25)
        })?;
    log(format!(".torrent criado! InfoHash: {}", created.infohash));

    Ok(PackageOutput {
        output_folder: out_folder,
        torrent_path: created.torrent_path,
        infohash: created.infohash,
        first_audio_path: first_audio_dest,
        source_mediainfo,
        audio_mediainfo,
        created_files,
        delay_warnings: Vec::new(),
        audio_report: None,
        measured_bitrate_kbps: None,
        corrected_release_name: None,
    })
}

/// Extrai áudio + todas as legendas selecionadas em UMA ÚNICA LEITURA do arquivo fonte:
/// - MKV: `mkvextract <file> --gui-mode tracks <audio_id>:<audio_out> <sub1_id>:<sub1_out> ...`
/// - MP4 / fallback: `ffmpeg -y -v error -i <file> -map 0:<a_id> -c:a copy <a_out> -map 0:<s1_id> -c:s copy <s1_out> ...`
fn extract_tracks_single_pass<FProg>(
    cfg: &AppConfig,
    video: &InspectedVideo,
    audio_track: &MediaTrack,
    audio_dest: &Path,
    sub_targets: &[(&MediaTrack, PathBuf)],
    cancel: &CancelHandle,
    mut on_progress: FProg,
) -> Result<()>
where
    FProg: FnMut(f32),
{
    if video.is_mkv && !cfg.mkvextract_path.trim().is_empty() {
        let mut cmd = Command::new(cfg.mkvextract_path.trim());
        cmd.arg(&video.path).arg("--gui-mode").arg("tracks");
        cmd.arg(format!(
            "{}:{}",
            audio_track.id,
            audio_dest.to_string_lossy()
        ));
        for (sub, dest) in sub_targets {
            cmd.arg(format!("{}:{}", sub.id, dest.to_string_lossy()));
        }

        let out = run_command_cancellable_with_stdout(&mut cmd, cancel, |line| {
            if let Some(p) = parse_mkvextract_progress(line) {
                on_progress(p);
            }
        })
        .context("Falha ao executar mkvextract")?;
        if !out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            bail!("mkvextract falhou: {stdout} {stderr}");
        }
        return Ok(());
    }

    // Fallback / MP4: ffmpeg com múltiplos outputs no mesmo comando (1 única leitura)
    let ffmpeg = cfg.ffmpeg_path.trim();
    if ffmpeg.is_empty() {
        bail!(
            "ffmpeg não configurado para extrair faixas de {}",
            video.path.display()
        );
    }
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-y", "-v", "error", "-i"]).arg(&video.path);

    // Output do áudio
    cmd.args([
        "-map",
        &format!("0:{}", audio_track.id),
        "-vn",
        "-sn",
        "-dn",
        "-c:a",
        "copy",
    ])
    .arg(audio_dest);

    // Outputs das legendas na mesma passada
    for (sub, dest) in sub_targets {
        cmd.args([
            "-map",
            &format!("0:{}", sub.id),
            "-vn",
            "-an",
            "-dn",
            "-c:s",
            "copy",
        ])
        .arg(dest);
    }

    let out = run_command_cancellable(&mut cmd, cancel).context("Falha ao executar ffmpeg")?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        bail!("ffmpeg falhou ao extrair faixas: {stderr}");
    }
    Ok(())
}

fn extract_mkv_chapters(
    cfg: &AppConfig,
    video_path: &Path,
    dest: &Path,
    cancel: &CancelHandle,
) -> Result<bool> {
    let bin = cfg.mkvextract_path.trim();
    if bin.is_empty() {
        return Ok(false);
    }
    let mut cmd = Command::new(bin);
    cmd.arg(video_path).args(["chapters", "-s"]);
    let out = run_command_cancellable(&mut cmd, cancel)
        .context("Falha ao extrair capítulos via mkvextract")?;
    if !out.status.success() || out.stdout.is_empty() {
        return Ok(false);
    }
    std::fs::write(dest, &out.stdout)?;
    Ok(true)
}

fn subtitle_extension(codec_raw: &str) -> &'static str {
    let up = codec_raw.to_ascii_uppercase();
    if up.contains("PGS") || up.contains("HDMV") {
        "sup"
    } else if up.contains("ASS") || up.contains("SSA") {
        "ass"
    } else if up.contains("VOBSUB") || up.contains("DVD") {
        "sub"
    } else if up.contains("WEBVTT") || up.contains("VTT") {
        "vtt"
    } else {
        "srt"
    }
}

/// Faz parse das linhas de progresso emitidas pelo `mkvextract`:
/// - Modo GUI (`--gui-mode`): `#GUI#progress 42%`
/// - Modo padrão (localizado): `Progress: 42%` / `Progresso: 42%`
fn parse_mkvextract_progress(line: &str) -> Option<f32> {
    let trimmed = line.trim();
    if let Some(rest) = trimmed.strip_prefix("#GUI#progress") {
        let pct_str = rest.trim().strip_suffix('%')?.trim();
        let pct: f32 = pct_str.parse().ok()?;
        return Some((pct / 100.0).clamp(0.0, 1.0));
    }
    if let Some(colon_idx) = trimmed.find(':') {
        let label = trimmed[..colon_idx].trim().to_ascii_lowercase();
        if label.contains("progress") {
            let pct_str = trimmed[colon_idx + 1..].trim().strip_suffix('%')?.trim();
            let pct: f32 = pct_str.parse().ok()?;
            return Some((pct / 100.0).clamp(0.0, 1.0));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::parse_mkvextract_progress;

    #[test]
    fn parses_gui_mode_progress() {
        assert_eq!(parse_mkvextract_progress("#GUI#progress 0%"), Some(0.0));
        assert_eq!(parse_mkvextract_progress("#GUI#progress 42%"), Some(0.42));
        assert_eq!(
            parse_mkvextract_progress("  #GUI#progress 100%  "),
            Some(1.0)
        );
    }

    #[test]
    fn parses_localized_progress_fallback() {
        assert_eq!(parse_mkvextract_progress("Progress: 25%"), Some(0.25));
        assert_eq!(parse_mkvextract_progress("Progresso: 73%"), Some(0.73));
        assert_eq!(parse_mkvextract_progress("Extracting track 1"), None);
    }
}
