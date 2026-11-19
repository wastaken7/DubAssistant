use crate::config::TorrentClientConfig;
use anyhow::{bail, Context, Result};
use std::path::Path;

pub fn test_watch_folder(cfg: &TorrentClientConfig) -> Result<String> {
    let dir = cfg.watch_folder.trim();
    if dir.is_empty() {
        bail!("Pasta Watch Folder não configurada.");
    }
    let path = Path::new(dir);
    if !path.is_dir() {
        bail!("Pasta Watch Folder não existe: {dir}");
    }
    Ok(format!("Watch Folder válida: {dir}"))
}

pub fn copy_to_watch(
    torrent_bytes: &[u8],
    torrent_filename: &str,
    cfg: &TorrentClientConfig,
) -> Result<String> {
    let dir = cfg.watch_folder.trim();
    if dir.is_empty() {
        bail!("Pasta Watch Folder não configurada.");
    }
    let watch_dir = Path::new(dir);
    std::fs::create_dir_all(watch_dir)?;
    let dest = watch_dir.join(torrent_filename);
    std::fs::write(&dest, torrent_bytes)
        .with_context(|| format!("Falha ao gravar .torrent na Watch Folder: {}", dest.display()))?;
    Ok(format!("Copiado para Watch Folder: {}", dest.display()))
}
