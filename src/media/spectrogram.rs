use crate::cancel::{run_command_cancellable, CancelHandle};
use crate::config::AppConfig;
use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DEFAULT_DURATION_SECS: u32 = 600;
const SPECTRUM_SIZE: &str = "1920x1080";
const MAX_SEPARATE_CHANNELS: u32 = 2;

/// Monta o filtro `ffmpeg` idêntico ao Upload-Assistant:
/// - Mono / Estéreo (<= 2 canais): plota canais separados.
/// - Surround (> 2 canais): faz downmix para mono antes do `showspectrumpic` para manter legibilidade.
pub fn build_spectrogram_filter(channels: u32) -> String {
    let downmix = if channels > MAX_SEPARATE_CHANNELS {
        "aformat=channel_layouts=mono,"
    } else {
        ""
    };
    format!("[0:a:0]{downmix}showspectrumpic=s={SPECTRUM_SIZE}:mode=separate:legend=1[spectrum]")
}

/// Gera o PNG de espectrograma do arquivo de áudio extraído em uma pasta temporária
/// (fora da pasta do torrent, respeitando a regra de não incluir imagens soltas no torrent).
pub fn generate_audio_spectrogram(
    cfg: &AppConfig,
    audio_path: &Path,
    channels_hint: &str,
    duration_secs: u32,
    cancel: &CancelHandle,
) -> Result<PathBuf> {
    if cancel.is_cancelled() {
        bail!("Tarefa cancelada pelo usuário.");
    }
    if !audio_path.is_file() {
        bail!("Arquivo de áudio não encontrado para gerar espectrograma: {}", audio_path.display());
    }
    let ffmpeg = cfg.ffmpeg_path.trim();
    if ffmpeg.is_empty() {
        bail!("ffmpeg não configurado nas Configurações.");
    }

    let out_dir = spectrogram_temp_dir()?;
    let stem = audio_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "audio".to_string());
    let ts = chrono::Utc::now().timestamp();
    let out_path = out_dir.join(format!("{stem}_spec_{ts}.png"));

    let ch_count = parse_channels_count(channels_hint);
    let dur = if duration_secs == 0 {
        DEFAULT_DURATION_SECS
    } else {
        duration_secs
    };
    let filter = build_spectrogram_filter(ch_count);

    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-v", "error", "-nostdin", "-y", "-t", &dur.to_string(), "-i"])
        .arg(audio_path)
        .args(["-filter_complex", &filter, "-map", "[spectrum]", "-frames:v", "1"])
        .arg(&out_path);

    let output = run_command_cancellable(&mut cmd, cancel)
        .context("Falha ao executar ffmpeg para gerar espectrograma")?;
    if !output.status.success() || !out_path.is_file() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ffmpeg falhou ao gerar espectrograma: {stderr}");
    }

    Ok(out_path)
}

fn spectrogram_temp_dir() -> Result<PathBuf> {
    let dir = if let Some(proj) = ProjectDirs::from("com", "CapybaraBR", "DubAssistant") {
        proj.data_local_dir().join("spectrograms")
    } else {
        std::env::temp_dir().join("DubAssistant_spectrograms")
    };
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn parse_channels_count(channels_hint: &str) -> u32 {
    let s = channels_hint.trim();
    if let Some((main, lfe)) = s.split_once('.') {
        let a: u32 = main.parse().unwrap_or(2);
        let b: u32 = lfe.parse().unwrap_or(0);
        return a + b;
    }
    s.parse::<u32>().unwrap_or(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_spectrogram_filter_stereo_vs_surround() {
        assert_eq!(
            build_spectrogram_filter(2),
            "[0:a:0]showspectrumpic=s=1920x1080:mode=separate:legend=1[spectrum]"
        );
        assert_eq!(
            build_spectrogram_filter(6),
            "[0:a:0]aformat=channel_layouts=mono,showspectrumpic=s=1920x1080:mode=separate:legend=1[spectrum]"
        );
    }

    #[test]
    fn test_parse_channels_count() {
        assert_eq!(parse_channels_count("2.0"), 2);
        assert_eq!(parse_channels_count("5.1"), 6);
        assert_eq!(parse_channels_count("7.1"), 8);
    }
}
