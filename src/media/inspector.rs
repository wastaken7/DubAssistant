use crate::cancel::{run_command_cancellable, CancelHandle};
use crate::config::AppConfig;
use crate::domain::models::{
    AudioCodec, Category, ExtractedFolderScan, InspectedVideo, MediaTrack, TrackKind,
};
use crate::domain::rules::{
    classify_audio_codec, classify_extracted_file, extract_episode_tag, FolderFileClass,
};
use anyhow::{bail, Context, Result};
use regex::Regex;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Executa o MediaInfo CLI e retorna o texto completo padronizado.
pub fn run_mediainfo(cfg: &AppConfig, path: &Path, cancel: &CancelHandle) -> Result<String> {
    let bin = cfg.mediainfo_path.trim();
    if bin.is_empty() {
        bail!("MediaInfo CLI não configurado. Informe o caminho nas Configurações.");
    }
    let mut cmd = Command::new(bin);
    cmd.arg(path);
    let output = run_command_cancellable(&mut cmd, cancel)
        .with_context(|| format!("Falha ao executar MediaInfo ({bin})"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("MediaInfo falhou: {stderr}");
    }
    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok(sanitize_mediainfo_paths(&raw))
}

/// Remove caminhos locais sensíveis (ex: `C:\Users\<user>\...`, `/home/<user>/...`, `F:\...`)
/// das linhas `Complete name` / `Folder name` do MediaInfo, preservando apenas o nome do arquivo
/// (ou `<PastaDoDisco>/BDMV/...` e `<PastaDoDisco>/VIDEO_TS/...` para discos raw).
pub fn sanitize_mediainfo_paths(raw: &str) -> String {
    raw.lines()
        .map(|line| {
            let Some((left, right)) = line.split_once(':') else {
                return line.to_string();
            };
            let key = left.trim();
            if key.eq_ignore_ascii_case("Complete name") {
                let sanitized = sanitize_single_path_value(right.trim());
                return format!("{left}: {sanitized}");
            }
            if key.eq_ignore_ascii_case("Folder name") {
                return String::new();
            }
            line.to_string()
        })
        .filter(|l| !l.is_empty() || raw.contains("\n\n"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn sanitize_single_path_value(raw_path: &str) -> String {
    let normalized = raw_path.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').filter(|s| !s.is_empty()).collect();
    if parts.is_empty() {
        return raw_path.to_string();
    }

    // Preserva <PastaDoDisco>/BDMV/... ou <PastaDoDisco>/VIDEO_TS/... sem vazar drive nem pastas pai
    if let Some(idx) = parts
        .iter()
        .position(|p| p.eq_ignore_ascii_case("BDMV") || p.eq_ignore_ascii_case("VIDEO_TS"))
    {
        let start = idx.saturating_sub(1);
        let candidate = &parts[start..];
        // Evita manter letra de drive caso BDMV esteja direto na raiz (ex: F:/BDMV)
        if candidate.first().map(|s| s.ends_with(':')).unwrap_or(false) {
            return parts[idx..].join("/");
        }
        return candidate.join("/");
    }

    // Para arquivos normais (.mkv, .mp4, .ac3, .eac3, etc.), mantém apenas o nome do arquivo
    parts.last().copied().unwrap_or(raw_path).to_string()
}

/// Inspeciona um ou mais arquivos de vídeo (.mkv / .mp4), ordenando episódios se for série.
pub fn inspect_videos(
    cfg: &AppConfig,
    paths: &[PathBuf],
    cancel: &CancelHandle,
) -> Result<Vec<InspectedVideo>> {
    let mut list = Vec::with_capacity(paths.len());
    for path in paths {
        if cancel.is_cancelled() {
            bail!("Tarefa cancelada pelo usuário.");
        }
        list.push(inspect_single_video(cfg, path, cancel)?);
    }

    // Ordenação natural por temporada/episódio quando detectado
    list.sort_by(|a, b| {
        let ka = extract_episode_tag(&a.path).map(|(s, e, _)| (s, e));
        let kb = extract_episode_tag(&b.path).map(|(s, e, _)| (s, e));
        ka.cmp(&kb).then_with(|| a.path.cmp(&b.path))
    });

    Ok(list)
}

pub fn inspect_single_video(
    cfg: &AppConfig,
    path: &Path,
    cancel: &CancelHandle,
) -> Result<InspectedVideo> {
    if !path.is_file() {
        bail!("Arquivo de vídeo não encontrado: {}", path.display());
    }

    let ext = path
        .extension()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let is_mkv = ext == "mkv" || ext == "mks" || ext == "mka";

    let raw_mediainfo = run_mediainfo(cfg, path, cancel).unwrap_or_default();
    if cancel.is_cancelled() {
        bail!("Tarefa cancelada pelo usuário.");
    }
    let parsed_mi = parse_mediainfo_summary(&raw_mediainfo);

    let (mut tracks, has_chapters, ffprobe_tmdb) =
        if is_mkv && !cfg.mkvmerge_path.trim().is_empty() {
            let (t, ch) = inspect_with_mkvmerge(cfg, path, cancel)?;
            let tag = probe_embedded_tmdb_tag(cfg, path, cancel);
            (t, ch, tag)
        } else {
            inspect_with_ffprobe(cfg, path, cancel)?
        };

    // Enriquecer bitrate/canais/delay com dados do MediaInfo quando faltarem
    enrich_tracks_from_mediainfo(&mut tracks, &raw_mediainfo);

    // Auto-seleção inteligente: 1ª faixa de áudio PT-BR permitida + legendas PT-BR
    auto_select_tracks(&mut tracks);

    let episode_tag = extract_episode_tag(path).map(|(_, _, tag)| tag);

    let embedded_tmdb = ffprobe_tmdb.or(parsed_mi.tmdb_tag);
    let (embedded_tmdb_category, embedded_tmdb_id) = match embedded_tmdb {
        Some((cat, id)) => (cat, Some(id)),
        None => (None, None),
    };

    Ok(InspectedVideo {
        path: path.to_path_buf(),
        is_mkv,
        episode_tag,
        embedded_tmdb_id,
        embedded_tmdb_category,
        fps: parsed_mi.fps,
        duration_str: parsed_mi.duration,
        has_chapters,
        tracks,
        raw_mediainfo,
    })
}

/// Extrai a tag `TMDB` (ex: `movie/1368337`, `tv/12345`, `1368337`) via `ffprobe -show_entries format_tags`.
fn probe_embedded_tmdb_tag(
    cfg: &AppConfig,
    path: &Path,
    cancel: &CancelHandle,
) -> Option<(Option<Category>, String)> {
    let bin = cfg.ffprobe_path.trim();
    if bin.is_empty() || cancel.is_cancelled() {
        return None;
    }
    let mut cmd = Command::new(bin);
    cmd.args([
        "-v",
        "quiet",
        "-print_format",
        "json",
        "-show_entries",
        "format_tags",
    ])
    .arg(path);
    let output = run_command_cancellable(&mut cmd, cancel).ok()?;
    if !output.status.success() {
        return None;
    }
    let json: Value = serde_json::from_slice(&output.stdout).ok()?;
    extract_tmdb_from_ffprobe_json(&json)
}

fn extract_tmdb_from_ffprobe_json(json: &Value) -> Option<(Option<Category>, String)> {
    let tags = json.pointer("/format/tags")?.as_object()?;
    for (k, v) in tags {
        if k.eq_ignore_ascii_case("tmdb") || k.eq_ignore_ascii_case("tmdb_id") {
            if let Some(raw) = v.as_str() {
                if let Some(parsed) = parse_tmdb_tag(raw) {
                    return Some(parsed);
                }
            }
        }
    }
    None
}

/// Faz parsing de valores de tag TMDB embutidos em MKV/MP4:
/// - `"movie/1368337"` -> `(Some(Category::Movie), "1368337")`
/// - `"tv/66732"` -> `(Some(Category::Series), "66732")`
/// - `"1368337"` -> `(None, "1368337")`
pub fn parse_tmdb_tag(raw: &str) -> Option<(Option<Category>, String)> {
    let s = raw.trim();
    if s.is_empty() {
        return None;
    }
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(?i)(?:(movie|tv|series)[/:_-]+)?(\d+)").unwrap());
    let caps = re.captures(s)?;
    let cat = caps.get(1).map(|m| {
        if m.as_str().eq_ignore_ascii_case("movie") {
            Category::Movie
        } else {
            Category::Series
        }
    });
    let id = caps[2].to_string();
    Some((cat, id))
}

fn inspect_with_mkvmerge(
    cfg: &AppConfig,
    path: &Path,
    cancel: &CancelHandle,
) -> Result<(Vec<MediaTrack>, bool)> {
    let bin = cfg.mkvmerge_path.trim();
    let mut cmd = Command::new(bin);
    cmd.arg("-J").arg(path);
    let output = run_command_cancellable(&mut cmd, cancel)
        .with_context(|| format!("Falha ao executar mkvmerge ({bin})"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("mkvmerge -J falhou: {stderr}");
    }

    let json: Value = serde_json::from_slice(&output.stdout)
        .context("Falha ao interpretar JSON retornado pelo mkvmerge")?;

    let has_chapters = json
        .get("chapters")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);

    let mut tracks = Vec::new();
    if let Some(arr) = json.get("tracks").and_then(|v| v.as_array()) {
        for t in arr {
            let id = t.get("id").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let kind = match t.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "video" => TrackKind::Video,
                "audio" => TrackKind::Audio,
                "subtitles" => TrackKind::Subtitle,
                _ => continue,
            };
            let props = t.get("properties").cloned().unwrap_or(Value::Null);
            let codec_human = t.get("codec").and_then(|v| v.as_str()).unwrap_or("");
            let codec_id = props
                .get("codec_id")
                .and_then(|v| v.as_str())
                .unwrap_or(codec_human);

            let language = props
                .get("language_ietf")
                .or_else(|| props.get("language"))
                .and_then(|v| v.as_str())
                .unwrap_or("und")
                .to_string();
            let name = props
                .get("track_name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let forced = props
                .get("forced_track")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let default_track = props
                .get("default_track")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let channels = props
                .get("audio_channels")
                .and_then(|v| v.as_u64())
                .map(format_channel_count);
            let bitrate_kbps = props
                .get("tag_bps")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<u64>().ok())
                .map(|bps| (bps / 1000) as u32);
            let delay_ms = props
                .get("minimum_timestamp")
                .and_then(|v| v.as_i64())
                .map(|ns| ns / 1_000_000)
                .filter(|&ms| ms != 0);

            let allowed_audio_codec = if kind == TrackKind::Audio {
                classify_audio_codec(codec_id, Some(codec_human))
            } else {
                None
            };

            tracks.push(MediaTrack {
                id,
                kind,
                codec_raw: if codec_human.is_empty() {
                    codec_id.to_string()
                } else {
                    codec_human.to_string()
                },
                allowed_audio_codec,
                language,
                name,
                channels,
                bitrate_kbps,
                delay_ms,
                forced,
                default_track,
                selected: false,
            });
        }
    }

    Ok((tracks, has_chapters))
}

fn inspect_with_ffprobe(
    cfg: &AppConfig,
    path: &Path,
    cancel: &CancelHandle,
) -> Result<(Vec<MediaTrack>, bool, Option<(Option<Category>, String)>)> {
    let bin = cfg.ffprobe_path.trim();
    if bin.is_empty() {
        bail!("ffprobe não configurado. Informe o caminho nas Configurações.");
    }
    let mut cmd = Command::new(bin);
    cmd.args([
        "-v",
        "quiet",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
        "-show_chapters",
    ])
    .arg(path);
    let output = run_command_cancellable(&mut cmd, cancel)
        .with_context(|| format!("Falha ao executar ffprobe ({bin})"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ffprobe falhou: {stderr}");
    }

    let json: Value = serde_json::from_slice(&output.stdout)
        .context("Falha ao interpretar JSON do ffprobe")?;

    let tmdb_tag = extract_tmdb_from_ffprobe_json(&json);

    let has_chapters = json
        .get("chapters")
        .and_then(|v| v.as_array())
        .map(|a| !a.is_empty())
        .unwrap_or(false);

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
            let codec_name = s.get("codec_name").and_then(|v| v.as_str()).unwrap_or("");
            let profile = s.get("profile").and_then(|v| v.as_str());
            let tags = s.get("tags").cloned().unwrap_or(Value::Null);
            let language = tags
                .get("language")
                .and_then(|v| v.as_str())
                .unwrap_or("und")
                .to_string();
            let name = tags
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let disp = s.get("disposition").cloned().unwrap_or(Value::Null);
            let forced = disp.get("forced").and_then(|v| v.as_i64()).unwrap_or(0) == 1;
            let default_track = disp.get("default").and_then(|v| v.as_i64()).unwrap_or(0) == 1;

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
                classify_audio_codec(codec_name, profile)
            } else {
                None
            };

            let codec_raw = match profile {
                Some(p) if !p.is_empty() && p != "unknown" => format!("{codec_name} ({p})"),
                _ => codec_name.to_string(),
            };

            tracks.push(MediaTrack {
                id,
                kind,
                codec_raw,
                allowed_audio_codec,
                language,
                name,
                channels,
                bitrate_kbps,
                delay_ms: None,
                forced,
                default_track,
                selected: false,
            });
        }
    }

    Ok((tracks, has_chapters, tmdb_tag))
}

fn auto_select_tracks(tracks: &mut [MediaTrack]) {
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

pub fn is_portuguese_pt(track: &MediaTrack) -> bool {
    let lang = track.language.to_ascii_lowercase();
    let name = track.name.to_ascii_lowercase();
    let explicitly_br = lang == "pt-br"
        || lang == "pt_br"
        || name.contains("pt-br")
        || name.contains("ptbr")
        || name.contains("brazil")
        || name.contains("brasil");
    if explicitly_br {
        return false;
    }
    lang == "pt-pt"
        || lang == "pt_pt"
        || name.contains("pt-pt")
        || name.contains("ptpt")
        || name.contains("portugal")
        || name.contains("european")
        || name.contains("europeu")
        || name.contains("ibérico")
        || name.contains("iberico")
}

pub fn is_portuguese(track: &MediaTrack) -> bool {
    if is_portuguese_pt(track) {
        return false;
    }
    let lang = track.language.to_ascii_lowercase();
    let name = track.name.to_ascii_lowercase();
    lang == "por"
        || lang == "pt"
        || lang.starts_with("pt-")
        || lang.contains("portug")
        || name.contains("pt-br")
        || name.contains("ptbr")
        || name.contains("portugu")
        || name.contains("brasil")
        || name.contains("brazil")
        || name.contains("dublad")
}

pub fn format_channel_count(ch: u64) -> String {
    match ch {
        1 => "1.0".to_string(),
        2 => "2.0".to_string(),
        3 => "2.1".to_string(),
        4 => "4.0".to_string(),
        5 => "5.0".to_string(),
        6 => "5.1".to_string(),
        7 => "6.1".to_string(),
        8 => "7.1".to_string(),
        other => format!("{other}.0"),
    }
}

#[derive(Debug, Default, Clone)]
pub struct MediaInfoSummary {
    pub fps: Option<String>,
    pub duration: Option<String>,
    pub audio_codec: Option<AudioCodec>,
    pub audio_bitrate_kbps: Option<u32>,
    pub audio_channels: Option<String>,
    pub tmdb_tag: Option<(Option<Category>, String)>,
}

/// Extrai FPS, duração, codec de áudio, bitrate, canais e tag TMDB de um texto MediaInfo CLI.
pub fn parse_mediainfo_summary(mi: &str) -> MediaInfoSummary {
    let mut summary = MediaInfoSummary::default();
    let mut section = "";
    let mut audio_format = String::new();
    let mut audio_commercial = String::new();

    for line in mi.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.contains(':') {
            if trimmed.starts_with("General") {
                section = "general";
            } else if trimmed.starts_with("Video") {
                section = "video";
            } else if trimmed.starts_with("Audio") && section != "audio_done" {
                section = "audio";
            } else if section == "audio" {
                section = "audio_done";
            } else {
                section = "other";
            }
            continue;
        }

        let Some((key, val)) = trimmed.split_once(':') else {
            continue;
        };
        let key = key.trim();
        let val = val.trim();

        match section {
            "general" => {
                if key == "Duration" && summary.duration.is_none() {
                    summary.duration = Some(val.to_string());
                } else if (key.eq_ignore_ascii_case("TMDB") || key.eq_ignore_ascii_case("TMDB_ID"))
                    && summary.tmdb_tag.is_none()
                {
                    summary.tmdb_tag = parse_tmdb_tag(val);
                }
            }
            "video" => {
                if key == "Frame rate" && summary.fps.is_none() {
                    summary.fps = extract_fps_number(val);
                }
            }
            "audio" => match key {
                "Format" => audio_format = val.to_string(),
                "Commercial name" => audio_commercial = val.to_string(),
                "Bit rate" if summary.audio_bitrate_kbps.is_none() => {
                    summary.audio_bitrate_kbps = extract_kbps(val);
                }
                "Channel(s)" if summary.audio_channels.is_none() => {
                    summary.audio_channels = extract_channels_from_mi(val);
                }
                _ => {}
            },
            _ => {}
        }
    }

    if !audio_format.is_empty() || !audio_commercial.is_empty() {
        summary.audio_codec = classify_audio_codec(&audio_format, Some(&audio_commercial));
    }

    summary
}

fn enrich_tracks_from_mediainfo(tracks: &mut [MediaTrack], mi: &str) {
    let mut audio_blocks: Vec<(Option<AudioCodec>, Option<u32>, Option<String>, Option<i64>)> =
        Vec::new();
    let mut in_audio = false;
    let mut cur_fmt = String::new();
    let mut cur_comm = String::new();
    let mut cur_kbps: Option<u32> = None;
    let mut cur_ch: Option<String> = None;
    let mut cur_delay: Option<i64> = None;

    let flush = |in_audio: &mut bool,
                 cur_fmt: &mut String,
                 cur_comm: &mut String,
                 cur_kbps: &mut Option<u32>,
                 cur_ch: &mut Option<String>,
                 cur_delay: &mut Option<i64>,
                 out: &mut Vec<_>| {
        if *in_audio {
            let codec = classify_audio_codec(cur_fmt, Some(cur_comm));
            out.push((codec, *cur_kbps, cur_ch.take(), *cur_delay));
            cur_fmt.clear();
            cur_comm.clear();
            *cur_kbps = None;
            *cur_delay = None;
            *in_audio = false;
        }
    };

    for line in mi.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.contains(':') {
            flush(
                &mut in_audio,
                &mut cur_fmt,
                &mut cur_comm,
                &mut cur_kbps,
                &mut cur_ch,
                &mut cur_delay,
                &mut audio_blocks,
            );
            if trimmed.starts_with("Audio") {
                in_audio = true;
            }
            continue;
        }
        if in_audio {
            if let Some((k, v)) = trimmed.split_once(':') {
                match k.trim() {
                    "Format" => cur_fmt = v.trim().to_string(),
                    "Commercial name" => cur_comm = v.trim().to_string(),
                    "Bit rate" => cur_kbps = extract_kbps(v.trim()),
                    "Channel(s)" => cur_ch = extract_channels_from_mi(v.trim()),
                    "Delay relative to video" => cur_delay = extract_delay_ms(v.trim()),
                    _ => {}
                }
            }
        }
    }
    flush(
        &mut in_audio,
        &mut cur_fmt,
        &mut cur_comm,
        &mut cur_kbps,
        &mut cur_ch,
        &mut cur_delay,
        &mut audio_blocks,
    );

    let mut audio_idx = 0usize;
    for t in tracks.iter_mut() {
        if t.kind == TrackKind::Audio {
            if let Some((codec, kbps, ch, delay)) = audio_blocks.get(audio_idx) {
                if let Some(c) = codec {
                    t.allowed_audio_codec = Some(*c);
                }
                if t.bitrate_kbps.is_none() {
                    t.bitrate_kbps = *kbps;
                }
                if t.channels.is_none() {
                    t.channels = ch.clone();
                }
                if t.delay_ms.is_none() {
                    t.delay_ms = *delay;
                }
            }
            audio_idx += 1;
        }
    }
}

fn extract_fps_number(val: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(\d+(?:\.\d+)?)").unwrap());
    re.captures(val).map(|c| {
        let s = c[1].to_string();
        match s.as_str() {
            "23.976" | "24.000" => {
                if s == "24.000" {
                    "24".to_string()
                } else {
                    s
                }
            }
            "25.000" => "25".to_string(),
            "29.970" => "29.97".to_string(),
            _ => s,
        }
    })
}

fn extract_kbps(val: &str) -> Option<u32> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"([\d\s]+)\s*kb/s").unwrap());
    let caps = re.captures(val)?;
    let digits: String = caps[1].chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse::<u32>().ok()
}

fn extract_channels_from_mi(val: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(\d+)\s*channel").unwrap());
    let n: u64 = re.captures(val)?[1].parse().ok()?;
    Some(format_channel_count(n))
}

fn extract_delay_ms(val: &str) -> Option<i64> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r"(-?\d+)\s*ms").unwrap());
    let ms: i64 = re.captures(val)?[1].parse().ok()?;
    if ms == 0 {
        None
    } else {
        Some(ms)
    }
}

/// Escaneia uma pasta já extraída (Modo B), classificando arquivos válidos e bloqueando proibidos.
pub fn scan_extracted_folder(
    cfg: &AppConfig,
    root: &Path,
    cancel: &CancelHandle,
) -> Result<ExtractedFolderScan> {
    if !root.is_dir() {
        bail!("Pasta selecionada não existe: {}", root.display());
    }

    let mut audio_files = Vec::new();
    let mut subtitle_files = Vec::new();
    let mut chapter_files = Vec::new();
    let mut mediainfo_file = None;
    let mut forbidden_files = Vec::new();

    collect_folder_recursive(
        root,
        &mut audio_files,
        &mut subtitle_files,
        &mut chapter_files,
        &mut mediainfo_file,
        &mut forbidden_files,
    )?;

    audio_files.sort_by(|a, b| {
        let ka = extract_episode_tag(a).map(|(s, e, _)| (s, e));
        let kb = extract_episode_tag(b).map(|(s, e, _)| (s, e));
        ka.cmp(&kb).then_with(|| a.cmp(b))
    });
    subtitle_files.sort();
    chapter_files.sort();
    forbidden_files.sort();

    let source_mediainfo = match &mediainfo_file {
        Some(p) => std::fs::read_to_string(p)
            .map(|s| sanitize_mediainfo_paths(&s))
            .unwrap_or_default(),
        None => String::new(),
    };
    let source_summary = parse_mediainfo_summary(&source_mediainfo);

    let audio_mediainfo = if let Some(first_audio) = audio_files.first() {
        run_mediainfo(cfg, first_audio, cancel).unwrap_or_default()
    } else {
        String::new()
    };
    let audio_summary = parse_mediainfo_summary(&audio_mediainfo);

    let detected_codec = audio_summary.audio_codec.or_else(|| {
        audio_files
            .first()
            .and_then(|p| p.extension())
            .and_then(|e| AudioCodec::from_extension(&e.to_string_lossy()))
    });

    Ok(ExtractedFolderScan {
        root_dir: root.to_path_buf(),
        audio_files,
        subtitle_files,
        chapter_files,
        mediainfo_file,
        forbidden_files,
        detected_codec,
        detected_bitrate_kbps: audio_summary.audio_bitrate_kbps,
        detected_channels: audio_summary.audio_channels,
        detected_fps: source_summary.fps,
        source_mediainfo,
        audio_mediainfo,
    })
}

fn collect_folder_recursive(
    dir: &Path,
    audio_files: &mut Vec<PathBuf>,
    subtitle_files: &mut Vec<PathBuf>,
    chapter_files: &mut Vec<PathBuf>,
    mediainfo_file: &mut Option<PathBuf>,
    forbidden_files: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_folder_recursive(
                &path,
                audio_files,
                subtitle_files,
                chapter_files,
                mediainfo_file,
                forbidden_files,
            )?;
        } else if path.is_file() {
            match classify_extracted_file(&path) {
                FolderFileClass::Audio(_) => audio_files.push(path),
                FolderFileClass::Subtitle => subtitle_files.push(path),
                FolderFileClass::Chapters => chapter_files.push(path),
                FolderFileClass::MediaInfo => {
                    if mediainfo_file.is_none() {
                        *mediainfo_file = Some(path);
                    }
                }
                FolderFileClass::Forbidden => forbidden_files.push(path),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_tmdb_tag_formats() {
        assert_eq!(
            parse_tmdb_tag("movie/1368337"),
            Some((Some(Category::Movie), "1368337".to_string()))
        );
        assert_eq!(
            parse_tmdb_tag("tv/66732"),
            Some((Some(Category::Series), "66732".to_string()))
        );
        assert_eq!(
            parse_tmdb_tag("98566"),
            Some((None, "98566".to_string()))
        );
    }

    #[test]
    fn test_is_portuguese_excludes_pt_pt() {
        let mk = |lang: &str, name: &str| MediaTrack {
            id: 1,
            kind: TrackKind::Subtitle,
            codec_raw: "SubRip/SRT".into(),
            allowed_audio_codec: None,
            language: lang.into(),
            name: name.into(),
            channels: None,
            bitrate_kbps: None,
            delay_ms: None,
            forced: false,
            default_track: false,
            selected: false,
        };
        assert!(is_portuguese(&mk("pt-BR", "Portuguese (Brazil)")));
        assert!(is_portuguese(&mk("por", "Português")));
        assert!(!is_portuguese(&mk("pt-PT", "Portuguese (Portugal)")));
        assert!(!is_portuguese(&mk("por", "Portuguese (Portugal)")));
        assert!(!is_portuguese(&mk("pt-PT", "")));
    }

    #[test]
    fn test_sanitize_mediainfo_paths() {
        let mi_win = "General\nComplete name                            : C:\\Users\\secret_user\\Downloads\\Filme.2024.mkv\nFormat                                   : Matroska";
        let clean_win = sanitize_mediainfo_paths(mi_win);
        assert!(clean_win.contains("Complete name                            : Filme.2024.mkv"));
        assert!(!clean_win.contains("secret_user"));

        let mi_bd = "General\nComplete name                            : F:\\Blu-ray\\Movie.1951.Blu-ray-GRP\\BDMV\\PLAYLIST\\00001.mpls\nFormat                                   : Blu-ray Playlist";
        let clean_bd = sanitize_mediainfo_paths(mi_bd);
        assert!(clean_bd.contains("Movie.1951.Blu-ray-GRP/BDMV/PLAYLIST/00001.mpls"));
        assert!(!clean_bd.contains("F:\\"));

        let mi_linux = "General\nComplete name                            : /home/alice/media/audio.eac3\nFormat                                   : E-AC-3";
        let clean_linux = sanitize_mediainfo_paths(mi_linux);
        assert!(clean_linux.contains(": audio.eac3"));
        assert!(!clean_linux.contains("alice"));
    }
}
