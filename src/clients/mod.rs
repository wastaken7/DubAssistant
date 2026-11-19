pub mod deluge;
pub mod qbittorrent;
pub mod transmission;
pub mod watch;

use crate::config::{TorrentClientConfig, TorrentClientType};
use anyhow::{bail, Result};
use std::path::Path;
use std::time::Duration;

/// Mapeia o diretório local para o caminho remoto configurado (ex: Windows -> Docker/Seedbox POSIX).
pub fn map_save_path(local_parent_dir: &Path, cfg: &TorrentClientConfig) -> String {
    let raw_local = local_parent_dir.to_string_lossy().to_string();
    let local_prefix = cfg.local_path_prefix.trim();
    let remote_prefix = cfg.remote_path_prefix.trim();

    if local_prefix.is_empty() || remote_prefix.is_empty() {
        return raw_local;
    }

    let norm_local = raw_local.replace('\\', "/");
    let norm_prefix = local_prefix.replace('\\', "/").trim_end_matches('/').to_string();

    let matches = if cfg!(target_os = "windows") {
        norm_local
            .to_ascii_lowercase()
            .starts_with(&norm_prefix.to_ascii_lowercase())
    } else {
        norm_local.starts_with(&norm_prefix)
    };

    if !matches {
        return raw_local;
    }

    let suffix = norm_local[norm_prefix.len()..].trim_start_matches('/');
    let remote_is_windows = remote_prefix.contains('\\')
        || (remote_prefix.len() >= 2 && remote_prefix.as_bytes()[1] == b':');
    let sep = if remote_is_windows { '\\' } else { '/' };
    let clean_remote = remote_prefix.trim_end_matches(['/', '\\']);

    if suffix.is_empty() {
        clean_remote.to_string()
    } else {
        let converted_suffix = if remote_is_windows {
            suffix.replace('/', "\\")
        } else {
            suffix.to_string()
        };
        format!("{clean_remote}{sep}{converted_suffix}")
    }
}

/// Injeta o `.torrent` baixado no cliente configurado, respeitando `inject_delay_secs`.
pub async fn inject_torrent(
    torrent_bytes: &[u8],
    torrent_filename: &str,
    infohash: &str,
    content_parent_dir: &Path,
    cfg: &TorrentClientConfig,
) -> Result<String> {
    if cfg.client_type == TorrentClientType::None {
        return Ok("Nenhum cliente configurado para injeção automática.".to_string());
    }

    if cfg.inject_delay_secs > 0 && cfg.client_type != TorrentClientType::WatchFolder {
        tokio::time::sleep(Duration::from_secs(cfg.inject_delay_secs)).await;
    }

    let save_path = map_save_path(content_parent_dir, cfg);

    match cfg.client_type {
        TorrentClientType::None => Ok("Injeção desabilitada.".to_string()),
        TorrentClientType::QBittorrent => {
            qbittorrent::add_torrent(torrent_bytes, torrent_filename, infohash, &save_path, cfg)
                .await
        }
        TorrentClientType::Transmission => {
            transmission::add_torrent(torrent_bytes, &save_path, cfg).await
        }
        TorrentClientType::Deluge => {
            deluge::add_torrent(torrent_bytes, torrent_filename, &save_path, cfg).await
        }
        TorrentClientType::WatchFolder => watch::copy_to_watch(torrent_bytes, torrent_filename, cfg),
    }
}

/// Testa a conectividade/autenticação com o cliente BitTorrent configurado.
pub async fn test_connection(cfg: &TorrentClientConfig) -> Result<String> {
    match cfg.client_type {
        TorrentClientType::None => bail!("Selecione um cliente BitTorrent antes de testar."),
        TorrentClientType::QBittorrent => qbittorrent::test_connection(cfg).await,
        TorrentClientType::Transmission => transmission::test_connection(cfg).await,
        TorrentClientType::Deluge => deluge::test_connection(cfg).await,
        TorrentClientType::WatchFolder => watch::test_watch_folder(cfg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_save_path_windows_to_posix() {
        let mut cfg = TorrentClientConfig::default();
        cfg.local_path_prefix = r"D:\Media\Dublagens".into();
        cfg.remote_path_prefix = "/data/dublagens".into();

        let mapped = map_save_path(Path::new(r"D:\Media\Dublagens\Filmes"), &cfg);
        assert_eq!(mapped, "/data/dublagens/Filmes");
    }

    #[test]
    fn test_map_save_path_no_mapping() {
        let cfg = TorrentClientConfig::default();
        let mapped = map_save_path(Path::new(r"D:\Media\Dublagens"), &cfg);
        assert_eq!(mapped, r"D:\Media\Dublagens");
    }
}
