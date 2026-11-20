use crate::cancel::{run_command_cancellable, CancelHandle};
use crate::config::AppConfig;
use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const DEFAULT_DURATION_SECS: u32 = 600;

/// Monta o filtro `ffmpeg` plotando **todos os canais individualmente** (`mode=separate`,
/// sem downmix para mono) e escalando a altura automaticamente para manter 5.1 / 7.1 legíveis:
/// - 1.0 / 2.0 -> `1920x1080`
/// - 5.1 (6 ch) -> `1920x1800` (300px por canal)
/// - 7.1 (8 ch) -> `1920x2400` (300px por canal)
pub fn build_spectrogram_filter(channels: u32) -> String {
    let height = if channels <= 2 {
        1080
    } else {
        (channels * 300).clamp(1080, 2400)
    };
    format!("[0:a:0]showspectrumpic=s=1920x{height}:mode=separate:legend=1[spectrum]")
}

/// Gera o PNG de espectrograma do arquivo de áudio extraído em uma pasta temporária
/// (fora da pasta do torrent, respeitando a regra de não incluir imagens soltas no torrent).
/// Se `duration_secs == 0`, analisa o arquivo inteiro; caso contrário limita aos primeiros `duration_secs`.
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
        bail!(
            "Arquivo de áudio não encontrado para gerar espectrograma: {}",
            audio_path.display()
        );
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
    let filter = build_spectrogram_filter(ch_count);

    let mut cmd = Command::new(ffmpeg);
    cmd.args(["-v", "error", "-nostdin", "-y"]);
    if duration_secs > 0 {
        cmd.args(["-t", &duration_secs.to_string()]);
    }
    cmd.arg("-i")
        .arg(audio_path)
        .args([
            "-filter_complex",
            &filter,
            "-map",
            "[spectrum]",
            "-frames:v",
            "1",
        ])
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
    fn test_spectrogram_filter_separate_channels_scaled() {
        assert_eq!(
            build_spectrogram_filter(2),
            "[0:a:0]showspectrumpic=s=1920x1080:mode=separate:legend=1[spectrum]"
        );
        assert_eq!(
            build_spectrogram_filter(6),
            "[0:a:0]showspectrumpic=s=1920x1800:mode=separate:legend=1[spectrum]"
        );
        assert_eq!(
            build_spectrogram_filter(8),
            "[0:a:0]showspectrumpic=s=1920x2400:mode=separate:legend=1[spectrum]"
        );
    }

    #[test]
    fn test_parse_channels_count() {
        assert_eq!(parse_channels_count("2.0"), 2);
        assert_eq!(parse_channels_count("5.1"), 6);
        assert_eq!(parse_channels_count("7.1"), 8);
    }
}
