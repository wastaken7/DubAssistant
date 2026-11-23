pub mod audio_report;
pub mod disc;
pub mod extractor;
pub mod fps;
pub mod inspector;
pub mod spectrogram;

use crate::domain::models::AudioCodec;
use anyhow::{bail, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
pub enum DetectedInput {
    Videos(Vec<PathBuf>),
    RawDisc(PathBuf),
    ExtractedFolder(PathBuf),
}

pub fn detect_input_kind(inputs: &[PathBuf], force_extracted: bool) -> Result<DetectedInput> {
    if inputs.is_empty() {
        bail!("Informe pelo menos um arquivo ou pasta de entrada.");
    }

    if force_extracted {
        if inputs.len() != 1 || !inputs[0].is_dir() {
            bail!("--extracted exige exatamente uma pasta válida.");
        }
        return Ok(DetectedInput::ExtractedFolder(inputs[0].clone()));
    }

    if inputs.len() == 1 {
        let p = &inputs[0];
        if p.is_file() {
            let ext = p
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if ext == "iso" {
                return Ok(DetectedInput::RawDisc(p.clone()));
            }
            if AudioCodec::from_extension(&ext).is_some() {
                if let Some(parent) = p.parent() {
                    return Ok(DetectedInput::ExtractedFolder(parent.to_path_buf()));
                }
            }
            return Ok(DetectedInput::Videos(vec![p.clone()]));
        }
        if p.is_dir() {
            if is_raw_disc_dir(p) {
                return Ok(DetectedInput::RawDisc(p.clone()));
            }
            let videos = collect_videos_in_dir(p);
            if !videos.is_empty() {
                return Ok(DetectedInput::Videos(videos));
            }
            return Ok(DetectedInput::ExtractedFolder(p.clone()));
        }
        bail!("Caminho não encontrado: {}", p.display());
    }

    Ok(DetectedInput::Videos(inputs.to_vec()))
}

pub fn is_raw_disc_dir(p: &Path) -> bool {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_uppercase())
        .unwrap_or_default();
    p.join("BDMV").is_dir()
        || p.join("VIDEO_TS").is_dir()
        || p.join("VIDEO_TS.IFO").is_file()
        || name == "BDMV"
        || name == "VIDEO_TS"
}

pub fn collect_videos_in_dir(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_file() {
                if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                    let low = ext.to_ascii_lowercase();
                    if low == "mkv" || low == "mp4" {
                        files.push(p);
                    }
                }
            }
        }
    }
    files.sort();
    files
}

#[cfg(target_os = "windows")]
pub fn configure_no_window(cmd: &mut Command) -> &mut Command {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x08000000;
    cmd.creation_flags(CREATE_NO_WINDOW)
}

#[cfg(not(target_os = "windows"))]
pub fn configure_no_window(cmd: &mut Command) -> &mut Command {
    cmd
}
