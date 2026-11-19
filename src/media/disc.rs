use crate::cancel::{run_command_cancellable, CancelHandle};
use crate::config::AppConfig;
use crate::domain::models::{
    Category, DiscKind, DiscTitle, InspectedDisc, MediaTrack, PackageOutput, TrackKind,
    UploadMetadata,
};
use crate::domain::rules::{classify_audio_codec, sanitize_windows_filename};
use crate::media::inspector::{
    format_channel_count, is_portuguese, is_portuguese_pt, parse_mediainfo_summary, run_mediainfo,
};
use crate::torrent::creator::create_private_torrent;
use anyhow::{bail, Context, Result};
use regex::Regex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Detecta automaticamente se o caminho é um disco Blu-ray (`BDMV`) ou DVD (`VIDEO_TS` / `.iso`)
/// e lista todas as playlists / títulos com suas faixas de áudio e legenda.
pub fn inspect_raw_disc(
    cfg: &AppConfig,
    input_path: &Path,
    cancel: &CancelHandle,
) -> Result<InspectedDisc> {
    if cancel.is_cancelled() {
        bail!("Tarefa cancelada pelo usuário.");
    }

    if let Some(bd_root) = resolve_bluray_root(input_path) {
        return inspect_bluray_disc(cfg, &bd_root, cancel);
    }

    if let Some(dvd_target) = resolve_dvd_target(input_path) {
        return inspect_dvd_disc(cfg, &dvd_target, cancel);
    }

    bail!(
        "Não foi reconhecida uma estrutura de Blu-ray (BDMV) nem de DVD (VIDEO_TS / .iso) em '{}'.",
        input_path.display()
    );
}

fn resolve_bluray_root(path: &Path) -> Option<PathBuf> {
    if !path.is_dir() {
        return None;
    }
    if path.join("BDMV").is_dir() {
        return Some(path.to_path_buf());
    }
    if path
        .file_name()
        .map(|n| n.to_string_lossy().eq_ignore_ascii_case("BDMV"))
        .unwrap_or(false)
    {
        return path.parent().map(|p| p.to_path_buf());
    }
    None
}

fn resolve_dvd_target(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if ext == "iso" {
            return Some(path.to_path_buf());
        }
        return None;
    }
    if path.is_dir() {
        if path.join("VIDEO_TS").is_dir() {
            return Some(path.join("VIDEO_TS"));
        }
        if path
            .file_name()
            .map(|n| n.to_string_lossy().eq_ignore_ascii_case("VIDEO_TS"))
            .unwrap_or(false)
            || path.join("VIDEO_TS.IFO").is_file()
        {
            return Some(path.to_path_buf());
        }
    }
    None
}

fn inspect_bluray_disc(
    cfg: &AppConfig,
    bd_root: &Path,
    cancel: &CancelHandle,
) -> Result<InspectedDisc> {
    let eac3to = cfg.eac3to_path.trim();
    if eac3to.is_empty() {
        bail!("eac3to não configurado nas Configurações. Ele é necessário para listar e extrair playlists de Blu-ray raw.");
    }

    let mut cmd = Command::new(eac3to);
    cmd.arg(bd_root);
    let out = run_command_cancellable(&mut cmd, cancel)
        .with_context(|| format!("Falha ao executar eac3to ({eac3to})"))?;

    let combined = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let mut titles = parse_eac3to_disc_output(&combined);
    if titles.is_empty() {
        bail!("Nenhuma playlist Blu-ray encontrada pelo eac3to em '{}'.", bd_root.display());
    }

    // Pré-seleciona a playlist principal (mais longa) e auto-seleciona faixas PT-BR
    if let Some((best_idx, _)) = titles
        .iter()
        .enumerate()
        .max_by_key(|(_, t)| t.duration_secs)
    {
        titles[best_idx].selected = true;
    }
    for title in &mut titles {
        auto_select_disc_tracks(&mut title.tracks);
    }

    // MediaInfo da playlist principal (.mpls) para preencher mediainfo.txt + enriquecer FPS/bitrate/canais
    let mediainfo_target = titles
        .iter()
        .find(|t| t.selected)
        .and_then(|t| t.mpls_name.as_ref())
        .map(|mpls| bd_root.join("BDMV").join("PLAYLIST").join(mpls))
        .filter(|p| p.is_file());

    let raw_mediainfo = match &mediainfo_target {
        Some(p) => run_mediainfo(cfg, p, cancel).unwrap_or_default(),
        None => String::new(),
    };

    if !raw_mediainfo.is_empty() {
        let mi_sum = parse_mediainfo_summary(&raw_mediainfo);
        if let Some(sel) = titles.iter_mut().find(|t| t.selected) {
            if sel.fps.is_none() {
                sel.fps = mi_sum.fps;
            }
        }
    }

    Ok(InspectedDisc {
        root_dir: bd_root.to_path_buf(),
        kind: DiscKind::BluRay,
        titles,
        mediainfo_target,
        raw_mediainfo,
    })
}

/// Faz parsing da saída padrão do `eac3to <pasta_bluray>` com playlists e bullets de faixas.
pub fn parse_eac3to_disc_output(output: &str) -> Vec<DiscTitle> {
    static RE_HEADER: OnceLock<Regex> = OnceLock::new();
    static RE_LANG: OnceLock<Regex> = OnceLock::new();
    static RE_CH: OnceLock<Regex> = OnceLock::new();

    let re_header = RE_HEADER.get_or_init(|| {
        Regex::new(r"^\s*(\d+)\)\s+(\d{5}\.mpls),.*?(\d+:\d{2}:\d{2})\s*$").unwrap()
    });
    let re_lang = RE_LANG.get_or_init(|| Regex::new(r"\[([a-zA-Z\-]{2,8})\]").unwrap());
    let re_ch = RE_CH.get_or_init(|| Regex::new(r"(\d\.\d)\s*channels?").unwrap());

    let mut titles: Vec<DiscTitle> = Vec::new();
    let mut current: Option<DiscTitle> = None;
    let mut track_counter: u32 = 0;

    for raw_line in output.lines() {
        let line = raw_line.trim_end();
        if let Some(caps) = re_header.captures(line) {
            if let Some(t) = current.take() {
                titles.push(t);
            }
            let index: u32 = caps[1].parse().unwrap_or(1);
            let mpls = caps[2].to_string();
            let dur_str = caps[3].to_string();
            let dur_secs = parse_hms_to_secs(&dur_str);
            track_counter = 0;

            current = Some(DiscTitle {
                index,
                label: format!("{index}) {mpls} — {dur_str}"),
                mpls_name: Some(mpls),
                duration_str: dur_str,
                duration_secs: dur_secs,
                fps: None,
                has_chapters: false,
                chapters_track_id: None,
                chapters_text: None,
                tracks: Vec::new(),
                selected: false,
            });
            continue;
        }

        let trimmed = line.trim();
        if let (Some(title), Some(bullet)) = (current.as_mut(), trimmed.strip_prefix("- ")) {
            track_counter += 1;
            let low = bullet.to_ascii_lowercase();

            if low.starts_with("chapters") {
                title.has_chapters = true;
                title.chapters_track_id = Some(track_counter);
                continue;
            }

            if low.starts_with("h264")
                || low.starts_with("h265")
                || low.starts_with("hevc")
                || low.starts_with("vc-1")
                || low.starts_with("mpeg2")
                || low.starts_with("avc")
            {
                if low.contains("24/1.001") {
                    title.fps = Some("23.976".to_string());
                } else if low.contains("p24") {
                    title.fps = Some("24".to_string());
                } else if low.contains("p25") || low.contains("i50") {
                    title.fps = Some("25".to_string());
                } else if low.contains("30/1.001") || low.contains("60/1.001") {
                    title.fps = Some("29.97".to_string());
                }
                title.tracks.push(MediaTrack {
                    id: track_counter,
                    kind: TrackKind::Video,
                    codec_raw: bullet.to_string(),
                    allowed_audio_codec: None,
                    language: "und".to_string(),
                    name: String::new(),
                    channels: None,
                    bitrate_kbps: None,
                    delay_ms: None,
                    forced: false,
                    default_track: false,
                    selected: false,
                });
                continue;
            }

            let language = re_lang
                .captures(bullet)
                .map(|c| c[1].to_string())
                .unwrap_or_else(|| "und".to_string());

            if low.starts_with("subtitle") {
                title.tracks.push(MediaTrack {
                    id: track_counter,
                    kind: TrackKind::Subtitle,
                    codec_raw: "PGS".to_string(),
                    allowed_audio_codec: None,
                    language,
                    name: bullet.to_string(),
                    channels: None,
                    bitrate_kbps: None,
                    delay_ms: None,
                    forced: low.contains("forced"),
                    default_track: false,
                    selected: false,
                });
                continue;
            }

            // Faixa de áudio
            let allowed_audio_codec = classify_audio_codec(bullet, None);
            let channels = re_ch
                .captures(bullet)
                .map(|c| c[1].to_string())
                .or_else(|| {
                    if low.contains("stereo") || low.contains("2.0") {
                        Some("2.0".to_string())
                    } else if low.contains("mono") || low.contains("1.0") {
                        Some("1.0".to_string())
                    } else if low.contains("multi-channel") {
                        Some("5.1".to_string())
                    } else {
                        None
                    }
                });

            title.tracks.push(MediaTrack {
                id: track_counter,
                kind: TrackKind::Audio,
                codec_raw: bullet.to_string(),
                allowed_audio_codec,
                language,
                name: String::new(),
                channels,
                bitrate_kbps: None,
                delay_ms: None,
                forced: false,
                default_track: false,
                selected: false,
            });
        }
    }

    if let Some(t) = current {
        titles.push(t);
    }

    titles
}

fn inspect_dvd_disc(
    cfg: &AppConfig,
    dvd_target: &Path,
    cancel: &CancelHandle,
) -> Result<InspectedDisc> {
    let ffprobe = cfg.ffprobe_path.trim();
    if ffprobe.is_empty() {
        bail!("ffprobe não configurado nas Configurações. Ele é necessário para ler títulos de DVD.");
    }

    let mut titles = Vec::new();
    let mut consecutive_misses = 0u32;

    for title_idx in 1..=25u32 {
        if cancel.is_cancelled() {
            bail!("Tarefa cancelada pelo usuário.");
        }

        let mut cmd = Command::new(ffprobe);
        cmd.args([
            "-v",
            "quiet",
            "-f",
            "dvdvideo",
            "-title",
            &title_idx.to_string(),
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            "-show_chapters",
        ])
        .arg(dvd_target);

        let Ok(out) = run_command_cancellable(&mut cmd, cancel) else {
            if cancel.is_cancelled() {
                bail!("Tarefa cancelada pelo usuário.");
            }
            break;
        };

        if !out.status.success() || out.stdout.is_empty() {
            consecutive_misses += 1;
            if !titles.is_empty() || consecutive_misses >= 2 {
                break;
            }
            continue;
        }

        let Ok(json) = serde_json::from_slice::<Value>(&out.stdout) else {
            break;
        };

        consecutive_misses = 0;

        let dur_secs = json
            .pointer("/format/duration")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0) as u64;

        let dur_str = format_secs_hms(dur_secs);
        let mut fps: Option<String> = None;
        let mut tracks = Vec::new();

        if let Some(streams) = json.get("streams").and_then(|v| v.as_array()) {
            for s in streams {
                let id = s.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                let kind = match s.get("codec_type").and_then(|v| v.as_str()).unwrap_or("") {
                    "video" => TrackKind::Video,
                    "audio" => TrackKind::Audio,
                    "subtitle" => TrackKind::Subtitle,
                    _ => continue,
                };
                if kind == TrackKind::Video && fps.is_none() {
                    if let Some(rfr) = s.get("r_frame_rate").and_then(|v| v.as_str()) {
                        fps = parse_rational_fps(rfr);
                    }
                }
                let codec_name = s.get("codec_name").and_then(|v| v.as_str()).unwrap_or("");
                let tags = s.get("tags").cloned().unwrap_or(Value::Null);
                let language = tags
                    .get("language")
                    .and_then(|v| v.as_str())
                    .unwrap_or("und")
                    .to_string();
                let channels = s
                    .get("channels")
                    .and_then(|v| v.as_u64())
                    .map(format_channel_count);
                let bitrate_kbps = s
                    .get("bit_rate")
                    .and_then(|v| v.as_str())
                    .and_then(|b| b.parse::<u64>().ok())
                    .map(|bps| (bps / 1000) as u32);

                let allowed_audio_codec = if kind == TrackKind::Audio {
                    classify_audio_codec(codec_name, None)
                } else {
                    None
                };

                tracks.push(MediaTrack {
                    id,
                    kind,
                    codec_raw: codec_name.to_string(),
                    allowed_audio_codec,
                    language,
                    name: String::new(),
                    channels,
                    bitrate_kbps,
                    delay_ms: None,
                    forced: false,
                    default_track: false,
                    selected: false,
                });
            }
        }

        let chapters_arr = json
            .get("chapters")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let has_chapters = !chapters_arr.is_empty();
        let chapters_text = if has_chapters {
            Some(format_dvd_chapters(&chapters_arr))
        } else {
            None
        };

        auto_select_disc_tracks(&mut tracks);

        titles.push(DiscTitle {
            index: title_idx,
            label: format!(
                "Título {title_idx} — {dur_str} ({} capítulos)",
                chapters_arr.len()
            ),
            mpls_name: None,
            duration_str: dur_str,
            duration_secs: dur_secs,
            fps,
            has_chapters,
            chapters_track_id: None,
            chapters_text,
            tracks,
            selected: false,
        });
    }

    if titles.is_empty() {
        bail!(
            "Nenhum título de DVD legível encontrado em '{}'.",
            dvd_target.display()
        );
    }

    // Filtra vinhetas curtas (< 60s) se houver títulos mais longos
    if titles.iter().any(|t| t.duration_secs >= 60) {
        titles.retain(|t| t.duration_secs >= 60);
    }

    if let Some((best_idx, _)) = titles
        .iter()
        .enumerate()
        .max_by_key(|(_, t)| t.duration_secs)
    {
        titles[best_idx].selected = true;
    }

    let mediainfo_target = find_main_dvd_ifo(dvd_target);
    let raw_mediainfo = match &mediainfo_target {
        Some(p) => run_mediainfo(cfg, p, cancel).unwrap_or_default(),
        None => String::new(),
    };

    Ok(InspectedDisc {
        root_dir: dvd_target.to_path_buf(),
        kind: DiscKind::Dvd,
        titles,
        mediainfo_target,
        raw_mediainfo,
    })
}

/// Extrai áudio + legendas/capítulos dos títulos/playlists selecionados do disco raw
/// em uma única leitura por título, salva todos os arquivos na mesma pasta raiz e gera `.torrent` privado.
pub fn process_raw_disc_and_package<FLog, FProg>(
    cfg: &AppConfig,
    disc: &InspectedDisc,
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

    let selected_titles: Vec<&DiscTitle> = disc.titles.iter().filter(|t| t.selected).collect();
    if selected_titles.is_empty() {
        bail!("Selecione pelo menos 1 playlist/título do disco na aba 1.");
    }

    let safe_root_name = sanitize_windows_filename(&meta.release_name);
    let out_folder = output_base_dir.join(&safe_root_name);
    if out_folder.exists() {
        std::fs::remove_dir_all(&out_folder)?;
    }
    std::fs::create_dir_all(&out_folder)?;

    let mut created_files = Vec::new();
    let mut first_audio: Option<PathBuf> = None;
    let total_steps = (selected_titles.len() as f32) + 1.0;

    for (idx, title) in selected_titles.iter().enumerate() {
        if cancel.is_cancelled() {
            bail!("Tarefa cancelada pelo usuário.");
        }

        let ep_prefix = if meta.category == Category::Series || selected_titles.len() > 1 {
            format!("S{}E{:02}", meta.season_number.trim(), idx + 1)
        } else {
            String::new()
        };

        let sel_audios: Vec<&MediaTrack> = title
            .tracks
            .iter()
            .filter(|t| t.kind == TrackKind::Audio && t.selected)
            .collect();
        if sel_audios.len() != 1 {
            bail!(
                "Selecione exatamente 1 faixa de áudio em '{}'.",
                title.label
            );
        }
        let audio_track = sel_audios[0];
        let codec = audio_track.allowed_audio_codec.ok_or_else(|| {
            anyhow::anyhow!(
                "Codec '{}' não aceito pelo CapybaraBR em '{}'.",
                audio_track.codec_raw,
                title.label
            )
        })?;

        let audio_ext = codec.pure_extension();
        let audio_name = if ep_prefix.is_empty() {
            format!("{safe_root_name}.{audio_ext}")
        } else {
            format!("{safe_root_name} - {ep_prefix}.{audio_ext}")
        };
        let audio_dest = out_folder.join(&audio_name);

        match disc.kind {
            DiscKind::BluRay => {
                let eac3to = cfg.eac3to_path.trim();
                if eac3to.is_empty() {
                    bail!("eac3to não configurado nas Configurações.");
                }
                let mut cmd = Command::new(eac3to);
                cmd.arg(&disc.root_dir).arg(format!("{})", title.index));
                cmd.arg(format!("{}:{}", audio_track.id, audio_dest.to_string_lossy()));

                let sel_subs: Vec<&MediaTrack> = title
                    .tracks
                    .iter()
                    .filter(|t| t.kind == TrackKind::Subtitle && t.selected)
                    .collect();

                let mut sub_paths = Vec::new();
                for (sub_i, sub) in sel_subs.iter().enumerate() {
                    let lang_label = if is_portuguese_pt(sub) {
                        "PT-PT".to_string()
                    } else if is_portuguese(sub) {
                        "PT-BR".to_string()
                    } else {
                        sub.language.to_ascii_uppercase()
                    };
                    let tag = if sub.forced { "Forced" } else { "Full" };
                    let base = if ep_prefix.is_empty() {
                        format!("{lang_label} ({tag})")
                    } else {
                        format!("{ep_prefix} - {lang_label} ({tag})")
                    };
                    let mut dest = out_folder.join(format!("{base}.sup"));
                    if sub_paths.contains(&dest) || dest.exists() {
                        dest = out_folder.join(format!("{base} #{}.sup", sub_i + 1));
                    }
                    cmd.arg(format!("{}:{}", sub.id, dest.to_string_lossy()));
                    sub_paths.push(dest);
                }

                let mut chap_dest: Option<PathBuf> = None;
                if let Some(chap_id) = title.chapters_track_id {
                    if meta.category == Category::Movie && idx == 0 {
                        let cpath = out_folder.join("chapters.txt");
                        cmd.arg(format!("{chap_id}:{}", cpath.to_string_lossy()));
                        chap_dest = Some(cpath);
                    }
                }

                cmd.arg("-keepDialnorm");

                log(format!(
                    "[{}/{}] Extraindo Blu-ray playlist {} via eac3to ({} + {} legenda(s))...",
                    idx + 1,
                    selected_titles.len(),
                    title.label,
                    audio_name,
                    sub_paths.len()
                ));

                let out = run_command_cancellable(&mut cmd, cancel)
                    .context("Falha ao executar eac3to na extração do Blu-ray")?;
                if !out.status.success() {
                    let msg = format!(
                        "{}\n{}",
                        String::from_utf8_lossy(&out.stdout),
                        String::from_utf8_lossy(&out.stderr)
                    );
                    bail!("eac3to falhou ao extrair Blu-ray:\n{msg}");
                }

                // Remove qualquer arquivo "- Log.txt" gerado automaticamente pelo eac3to
                cleanup_eac3to_logs(&out_folder);

                if first_audio.is_none() {
                    first_audio = Some(audio_dest.clone());
                }
                created_files.push(audio_dest);
                for sp in sub_paths {
                    if sp.exists() {
                        created_files.push(sp);
                    }
                }
                if let Some(cp) = chap_dest {
                    if cp.exists() {
                        created_files.push(cp);
                    }
                }
            }
            DiscKind::Dvd => {
                let ffmpeg = cfg.ffmpeg_path.trim();
                if ffmpeg.is_empty() {
                    bail!("ffmpeg não configurado nas Configurações.");
                }
                let mut cmd = Command::new(ffmpeg);
                cmd.args([
                    "-y",
                    "-v",
                    "error",
                    "-f",
                    "dvdvideo",
                    "-title",
                    &title.index.to_string(),
                    "-i",
                ])
                .arg(&disc.root_dir)
                .args([
                    "-map",
                    &format!("0:{}", audio_track.id),
                    "-vn",
                    "-sn",
                    "-dn",
                    "-c:a",
                    "copy",
                ])
                .arg(&audio_dest);

                log(format!(
                    "[{}/{}] Extraindo DVD {} via ffmpeg dvdvideo -> {}...",
                    idx + 1,
                    selected_titles.len(),
                    title.label,
                    audio_name
                ));

                let out = run_command_cancellable(&mut cmd, cancel)
                    .context("Falha ao executar ffmpeg dvdvideo")?;
                if !out.status.success() {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    bail!("ffmpeg dvdvideo falhou: {stderr}");
                }

                if first_audio.is_none() {
                    first_audio = Some(audio_dest.clone());
                }
                created_files.push(audio_dest);

                if meta.category == Category::Movie && idx == 0 {
                    if let Some(ch_txt) = &title.chapters_text {
                        if !ch_txt.trim().is_empty() {
                            let cpath = out_folder.join("chapters.txt");
                            std::fs::write(&cpath, ch_txt)?;
                            created_files.push(cpath);
                            log("Salvo chapters.txt do DVD.".into());
                        }
                    }
                }
            }
        }

        on_progress(((idx + 1) as f32 / total_steps) * 0.85);
    }

    // Gravar mediainfo.txt da fonte
    let source_mediainfo = if !meta.source_mediainfo.trim().is_empty() {
        meta.source_mediainfo.clone()
    } else {
        disc.raw_mediainfo.clone()
    };
    if !source_mediainfo.trim().is_empty() {
        let mi_path = out_folder.join("mediainfo.txt");
        std::fs::write(&mi_path, &source_mediainfo)?;
        created_files.push(mi_path);
        log("Salvo mediainfo.txt da fonte.".into());
    }

    // MediaInfo do áudio extraído
    let audio_mediainfo = if let Some(audio_path) = &first_audio {
        log("Gerando MediaInfo do áudio extraído...".into());
        run_mediainfo(cfg, audio_path, cancel)?
    } else {
        String::new()
    };

    // Criar .torrent privado
    let torrent_path = output_base_dir.join(format!("{safe_root_name}.torrent"));
    log(format!(
        "Gerando .torrent privado em {}...",
        torrent_path.display()
    ));
    let created = create_private_torrent(
        &out_folder,
        &torrent_path,
        &cfg.announce_url,
        cancel,
        |p| on_progress(0.85 + p * 0.15),
    )?;
    log(format!(
        ".torrent criado! InfoHash: {} ({} bytes de conteúdo)",
        created.infohash, created.total_size
    ));

    Ok(PackageOutput {
        output_folder: out_folder,
        torrent_path: created.torrent_path,
        infohash: created.infohash,
        first_audio_path: first_audio,
        source_mediainfo,
        audio_mediainfo,
        created_files,
        delay_warnings: Vec::new(),
    })
}

fn cleanup_eac3to_logs(dir: &Path) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_file() {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().to_ascii_lowercase())
                    .unwrap_or_default();
                if name.ends_with("- log.txt") || name == "log.txt" {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
    }
}

fn find_main_dvd_ifo(dvd_target: &Path) -> Option<PathBuf> {
    if !dvd_target.is_dir() {
        return None;
    }
    let mut best: Option<(u64, PathBuf)> = None;
    if let Ok(rd) = std::fs::read_dir(dvd_target) {
        for entry in rd.flatten() {
            let p = entry.path();
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_ascii_uppercase())
                .unwrap_or_default();
            if name.starts_with("VTS_") && name.ends_with("_0.IFO") {
                let sz = entry.metadata().map(|m| m.len()).unwrap_or(0);
                if best.as_ref().map(|(bsz, _)| sz > *bsz).unwrap_or(true) {
                    best = Some((sz, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

fn auto_select_disc_tracks(tracks: &mut [MediaTrack]) {
    let mut audio_picked = false;
    for t in tracks.iter_mut() {
        if t.kind == TrackKind::Audio && t.allowed_audio_codec.is_some() && is_portuguese(t) {
            t.selected = true;
            audio_picked = true;
            break;
        }
    }
    if !audio_picked {
        if let Some(t) = tracks
            .iter_mut()
            .find(|t| t.kind == TrackKind::Audio && t.allowed_audio_codec.is_some())
        {
            t.selected = true;
        }
    }
    for t in tracks.iter_mut() {
        if t.kind == TrackKind::Subtitle && is_portuguese(t) {
            t.selected = true;
        }
    }
}

fn parse_hms_to_secs(hms: &str) -> u64 {
    let parts: Vec<u64> = hms.split(':').filter_map(|p| p.parse().ok()).collect();
    match parts.as_slice() {
        [h, m, s] => h * 3600 + m * 60 + s,
        [m, s] => m * 60 + s,
        _ => 0,
    }
}

fn format_secs_hms(total: u64) -> String {
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    format!("{h}:{m:02}:{s:02}")
}

fn parse_rational_fps(rfr: &str) -> Option<String> {
    let (num_s, den_s) = rfr.split_once('/')?;
    let num: f64 = num_s.parse().ok()?;
    let den: f64 = den_s.parse().ok()?;
    if den <= 0.0 {
        return None;
    }
    let val = num / den;
    if (val - 23.976).abs() < 0.02 {
        Some("23.976".to_string())
    } else if (val - 24.0).abs() < 0.02 {
        Some("24".to_string())
    } else if (val - 25.0).abs() < 0.02 {
        Some("25".to_string())
    } else if (val - 29.97).abs() < 0.02 {
        Some("29.97".to_string())
    } else {
        Some(format!("{val:.3}"))
    }
}

pub fn format_dvd_chapters(chapters: &[Value]) -> String {
    let mut out = String::new();
    for (i, ch) in chapters.iter().enumerate() {
        let start_s = ch
            .get("start_time")
            .and_then(|v| v.as_str())
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0);
        let total_ms = (start_s * 1000.0).round() as u64;
        let ms = total_ms % 1000;
        let total_secs = total_ms / 1000;
        let h = total_secs / 3600;
        let m = (total_secs % 3600) / 60;
        let s = total_secs % 60;
        let num = i + 1;
        out.push_str(&format!(
            "CHAPTER{num:02}={h:02}:{m:02}:{s:02}.{ms:03}\nCHAPTER{num:02}NAME=Chapter {num:02}\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::AudioCodec;

    #[test]
    fn test_parse_eac3to_disc_output() {
        let sample = "\
1) 00001.mpls, 00001.m2ts, 1:52:40
   - Chapters, 16 chapters
   - h264/AVC, 1080p24/1.001 (16:9)
   - DTS Master Audio, [eng], multi-channel, 48kHz
   - DTS Master Audio, [por], multi-channel, 48kHz
   - Subtitle (PGS), [por]

2) 00436.mpls, 0:45:16
   - Chapters, 7 chapters
   - h264/AVC, 1080p24/1.001 (16:9)
   - AC3, [eng], stereo, 48kHz
";
        let titles = parse_eac3to_disc_output(sample);
        assert_eq!(titles.len(), 2);
        assert_eq!(titles[0].index, 1);
        assert_eq!(titles[0].mpls_name.as_deref(), Some("00001.mpls"));
        assert_eq!(titles[0].fps.as_deref(), Some("23.976"));
        assert_eq!(titles[0].chapters_track_id, Some(1));

        let por_audio = titles[0]
            .tracks
            .iter()
            .find(|t| t.kind == TrackKind::Audio && t.language == "por")
            .unwrap();
        assert_eq!(por_audio.id, 4);
        assert_eq!(por_audio.allowed_audio_codec, Some(AudioCodec::DtsHdMa));
    }
}
