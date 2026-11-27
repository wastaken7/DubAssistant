//! Bitrate médio para codecs sem bitrate fixo (DTS-HD MA, TrueHD, FLAC, PCM...).
//!
//! O MediaInfo/eac3to não informam bitrate para áudio lossless (VBR), mas o nome oficial
//! sempre exige o bitrate. Depois da extração, o bitrate médio é calculado pelo tamanho do
//! áudio extraído dividido pela sua duração, e a pasta/arquivos são renomeados para o nome final.

use crate::cancel::{run_command_cancellable, CancelHandle};
use crate::config::AppConfig;
use crate::domain::models::UploadMetadata;
use crate::domain::rules::{build_official_release_name, sanitize_windows_filename};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// `true` quando o campo de bitrate não tem nenhum dígito (não detectado).
pub fn bitrate_missing(meta: &UploadMetadata) -> bool {
    !meta.bitrate_kbps.chars().any(|c| c.is_ascii_digit())
}

/// Bitrate médio em kbps (arredondado) a partir do tamanho em bytes e da duração em segundos.
pub fn average_kbps(size_bytes: u64, duration_secs: f64) -> Option<u32> {
    if size_bytes == 0 || !duration_secs.is_finite() || duration_secs <= 0.0 {
        return None;
    }
    let kbps = (size_bytes as f64 * 8.0 / duration_secs / 1000.0).round();
    (kbps >= 1.0).then_some(kbps as u32)
}

/// Mede a duração real de um arquivo de áudio puro lendo todos os pacotes com
/// `ffmpeg -c copy -f null` (funciona em .dtshd/.thd, onde ffprobe não informa duração).
pub fn measure_audio_duration_secs(
    cfg: &AppConfig,
    path: &Path,
    cancel: &CancelHandle,
) -> Option<f64> {
    let ffmpeg = cfg.ffmpeg_path.trim();
    if ffmpeg.is_empty() {
        return None;
    }
    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-hide_banner", "-nostdin", "-i"])
        .arg(path)
        .args(["-map", "0:a:0", "-c", "copy", "-f", "null", "-"]);
    let out = run_command_cancellable(&mut cmd, cancel).ok()?;
    if !out.status.success() {
        return None;
    }
    parse_last_ffmpeg_time(&String::from_utf8_lossy(&out.stderr))
}

/// Extrai o último `time=HH:MM:SS.xx` da saída de progresso do ffmpeg.
pub fn parse_last_ffmpeg_time(stderr: &str) -> Option<f64> {
    let idx = stderr.rfind("time=")?;
    let raw = stderr[idx + 5..].split_whitespace().next()?;
    let mut parts = raw.split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let s: f64 = parts.next()?.parse().ok()?;
    let total = h * 3600.0 + m * 60.0 + s;
    (total > 0.0).then_some(total)
}

/// Resultado do preenchimento do bitrate após a extração.
pub struct BitrateFill {
    pub kbps: u32,
    /// Novo nome oficial (None quando o nome foi editado manualmente e não é tocado).
    pub release_name: Option<String>,
    pub out_folder: PathBuf,
    pub safe_root_name: String,
}

/// Se o bitrate não foi detectado, calcula o bitrate médio dos áudios extraídos e renomeia
/// a pasta de saída e os arquivos que começam com o nome antigo para o novo nome oficial.
///
/// `fallback_secs` é usado por arquivo quando a duração não pode ser medida via ffmpeg
/// (ex: duração do título do Blu-ray informada pelo eac3to).
#[allow(clippy::too_many_arguments)]
pub fn fill_missing_bitrate<FLog>(
    cfg: &AppConfig,
    meta: &UploadMetadata,
    audio_files: &[(PathBuf, Option<f64>)],
    out_folder: &Path,
    safe_root_name: &str,
    created_files: &mut [PathBuf],
    first_audio: &mut Option<PathBuf>,
    cancel: &CancelHandle,
    log: &mut FLog,
) -> Result<Option<BitrateFill>>
where
    FLog: FnMut(String),
{
    if !bitrate_missing(meta) || audio_files.is_empty() {
        return Ok(None);
    }

    log("Bitrate não informado pela fonte (lossless/VBR): calculando bitrate médio do áudio extraído...".into());
    let mut total_bytes: u64 = 0;
    let mut total_secs: f64 = 0.0;
    for (path, fallback_secs) in audio_files {
        let size = std::fs::metadata(path)
            .with_context(|| format!("Falha ao ler tamanho de {}", path.display()))?
            .len();
        let secs = measure_audio_duration_secs(cfg, path, cancel).or(*fallback_secs);
        let Some(secs) = secs else {
            log(format!(
                "[AVISO] Não foi possível medir a duração de {} (configure o ffmpeg). Informe o bitrate manualmente.",
                path.display()
            ));
            return Ok(None);
        };
        total_bytes += size;
        total_secs += secs;
    }

    let Some(kbps) = average_kbps(total_bytes, total_secs) else {
        return Ok(None);
    };
    log(format!(
        "Bitrate médio calculado: {kbps} kbps ({total_bytes} bytes em {total_secs:.3} s)."
    ));

    let mut new_meta = meta.clone();
    new_meta.bitrate_kbps = kbps.to_string();
    if meta.manual_name_override {
        log("[AVISO] Nome editado manualmente: a pasta não foi renomeada com o bitrate.".into());
        return Ok(Some(BitrateFill {
            kbps,
            release_name: None,
            out_folder: out_folder.to_path_buf(),
            safe_root_name: safe_root_name.to_string(),
        }));
    }

    let new_name = build_official_release_name(&new_meta);
    let new_safe = sanitize_windows_filename(&new_name);
    if new_safe == safe_root_name {
        return Ok(Some(BitrateFill {
            kbps,
            release_name: Some(new_name),
            out_folder: out_folder.to_path_buf(),
            safe_root_name: new_safe,
        }));
    }

    let parent = out_folder
        .parent()
        .context("Pasta de saída sem diretório pai")?;
    let new_folder = parent.join(&new_safe);
    if new_folder.exists() {
        std::fs::remove_dir_all(&new_folder).with_context(|| {
            format!("Falha ao limpar pasta existente: {}", new_folder.display())
        })?;
    }
    std::fs::rename(out_folder, &new_folder).with_context(|| {
        format!(
            "Falha ao renomear {} -> {}",
            out_folder.display(),
            new_folder.display()
        )
    })?;

    let remap = |old: &Path| -> Result<PathBuf> {
        let rel = old.strip_prefix(out_folder).unwrap_or(old);
        let moved = new_folder.join(rel);
        let Some(fname) = moved.file_name().map(|f| f.to_string_lossy().to_string()) else {
            return Ok(moved);
        };
        match fname.strip_prefix(safe_root_name) {
            Some(rest) => {
                let renamed = moved.with_file_name(format!("{new_safe}{rest}"));
                if moved.exists() {
                    std::fs::rename(&moved, &renamed).with_context(|| {
                        format!(
                            "Falha ao renomear {} -> {}",
                            moved.display(),
                            renamed.display()
                        )
                    })?;
                }
                Ok(renamed)
            }
            None => Ok(moved),
        }
    };

    let old_first = first_audio.clone();
    for f in created_files.iter_mut() {
        let new_path = remap(f)?;
        if old_first.as_deref() == Some(f.as_path()) {
            *first_audio = Some(new_path.clone());
        }
        *f = new_path;
    }
    if let Some(fa) = first_audio.as_ref() {
        if !fa.starts_with(&new_folder) {
            *first_audio = Some(remap(fa)?);
        }
    }

    log(format!("Pasta renomeada para o nome final: {new_safe}"));
    Ok(Some(BitrateFill {
        kbps,
        release_name: Some(new_name),
        out_folder: new_folder,
        safe_root_name: new_safe,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn average_of_baby_driver_dtshd() {
        // 633727 frames DTS de 512 amostras a 48 kHz
        let secs = 633_727.0 * 512.0 / 48_000.0;
        assert_eq!(average_kbps(2_012_451_892, secs), Some(2382));
    }

    #[test]
    fn average_rejects_invalid() {
        assert_eq!(average_kbps(0, 10.0), None);
        assert_eq!(average_kbps(1000, 0.0), None);
    }

    #[test]
    fn parses_last_time() {
        let err = "size=N/A time=00:00:01.00 bitrate=N/A\rsize=N/A time=01:52:39.75 bitrate=N/A speed=300x\n";
        let t = parse_last_ffmpeg_time(err).unwrap();
        assert!((t - 6759.75).abs() < 1e-6);
    }

    #[test]
    fn missing_detects_empty_and_text() {
        let mut m = UploadMetadata::default();
        m.bitrate_kbps = String::new();
        assert!(bitrate_missing(&m));
        m.bitrate_kbps = "640".into();
        assert!(!bitrate_missing(&m));
    }

    #[test]
    fn renames_folder_and_prefixed_files() {
        let base = std::env::temp_dir().join(format!("dubassist_bitrate_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut meta = UploadMetadata::default();
        meta.title_pt_br = "Filme".into();
        meta.year = "2017".into();
        meta.audio_codec = crate::domain::models::AudioCodec::DtsHdMa;
        meta.bitrate_kbps = String::new();
        let old_name = build_official_release_name(&meta);
        assert_eq!(old_name, "Filme 2017 DTS-HD MA 5.1 Blu-ray");
        let old_folder = base.join(&old_name);
        std::fs::create_dir_all(&old_folder).unwrap();
        let audio = old_folder.join(format!("{old_name}.dtshd"));
        std::fs::write(&audio, vec![0u8; 1000]).unwrap();
        let sub = old_folder.join("PT-BR (Full).sup");
        std::fs::write(&sub, b"x").unwrap();
        let mut created = vec![audio.clone(), sub.clone()];
        let mut first = Some(audio.clone());
        let cfg = AppConfig {
            ffmpeg_path: String::new(),
            ..AppConfig::default()
        };
        let fill = fill_missing_bitrate(
            &cfg,
            &meta,
            &[(audio.clone(), Some(1.0))],
            &old_folder,
            &old_name,
            &mut created,
            &mut first,
            &CancelHandle::new(),
            &mut |_| {},
        )
        .unwrap()
        .unwrap();
        assert_eq!(fill.kbps, 8);
        assert_eq!(fill.safe_root_name, "Filme 2017 DTS-HD MA 8 5.1 Blu-ray");
        let new_audio = fill
            .out_folder
            .join("Filme 2017 DTS-HD MA 8 5.1 Blu-ray.dtshd");
        assert!(new_audio.exists());
        assert!(fill.out_folder.join("PT-BR (Full).sup").exists());
        assert!(!old_folder.exists());
        assert_eq!(first, Some(new_audio.clone()));
        assert_eq!(created[0], new_audio);
        let _ = std::fs::remove_dir_all(&base);
    }
}
