use crate::cancel::CancelHandle;
use crate::torrent::bencode::{validate_and_extract_infohash, BValue};
use anyhow::{bail, Context, Result};
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct CreatedTorrent {
    pub torrent_path: PathBuf,
    pub infohash: String,
    pub total_size: u64,
}

/// Escolhe o piece length ideal (256 KiB até 16 MiB) de acordo com o tamanho total do conteúdo.
pub fn choose_piece_length(total_bytes: u64) -> u64 {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * 1024;
    if total_bytes < 256 * MIB {
        256 * KIB
    } else if total_bytes < 512 * MIB {
        512 * KIB
    } else if total_bytes < 1024 * MIB {
        MIB
    } else if total_bytes < 2 * 1024 * MIB {
        2 * MIB
    } else if total_bytes < 4 * 1024 * MIB {
        4 * MIB
    } else if total_bytes < 8 * 1024 * MIB {
        8 * MIB
    } else {
        16 * MIB
    }
}

/// Cria um arquivo `.torrent` v1 privado (`private = 1`) a partir de uma pasta organizada.
pub fn create_private_torrent<F>(
    folder_path: &Path,
    output_torrent_path: &Path,
    announce_url: &str,
    cancel: &CancelHandle,
    mut on_progress: F,
) -> Result<CreatedTorrent>
where
    F: FnMut(f32),
{
    if cancel.is_cancelled() {
        bail!("Tarefa cancelada pelo usuário.");
    }
    if !folder_path.is_dir() {
        bail!("Pasta de entrada não existe: {}", folder_path.display());
    }
    let announce = announce_url.trim();
    if announce.is_empty() {
        bail!("URL de announce não configurada. Teste sua sessão ou informe a announce URL nas Configurações.");
    }

    let root_name = folder_path
        .file_name()
        .context("Pasta raiz inválida")?
        .to_string_lossy()
        .to_string();

    let mut files = collect_files_sorted(folder_path)?;
    files.sort_by(|a, b| a.0.cmp(&b.0));

    if files.is_empty() {
        bail!(
            "A pasta {} não contém arquivos para criar o .torrent",
            folder_path.display()
        );
    }

    let total_size: u64 = files.iter().map(|(_, _, sz)| *sz).sum();
    let piece_length = choose_piece_length(total_size);

    let mut pieces_bytes: Vec<u8> =
        Vec::with_capacity(((total_size / piece_length) as usize + 1) * 20);
    let mut hasher = Sha1::new();
    let mut in_piece: u64 = 0;
    let mut processed: u64 = 0;
    let mut buf = vec![0u8; 64 * 1024];

    for (_, abs_path, _) in &files {
        if cancel.is_cancelled() {
            bail!("Tarefa cancelada pelo usuário.");
        }
        let mut f = File::open(abs_path)
            .with_context(|| format!("Falha ao abrir arquivo para hash: {}", abs_path.display()))?;
        loop {
            if cancel.is_cancelled() {
                bail!("Tarefa cancelada pelo usuário.");
            }
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            let mut offset = 0usize;
            while offset < n {
                let remaining_in_piece = (piece_length - in_piece) as usize;
                let take = remaining_in_piece.min(n - offset);
                hasher.update(&buf[offset..offset + take]);
                in_piece += take as u64;
                offset += take;
                processed += take as u64;

                if in_piece == piece_length {
                    pieces_bytes.extend_from_slice(&hasher.finalize_reset());
                    in_piece = 0;
                }
            }
            if total_size > 0 {
                on_progress((processed as f32 / total_size as f32).clamp(0.0, 1.0));
            }
        }
    }

    if in_piece > 0 {
        pieces_bytes.extend_from_slice(&hasher.finalize());
    }
    on_progress(1.0);

    let mut bencode_files = Vec::with_capacity(files.len());
    for (rel_components, _, size) in &files {
        let mut entry = BTreeMap::new();
        entry.insert(b"length".to_vec(), BValue::Int(*size as i64));
        let path_list = rel_components.iter().map(|c| BValue::str_val(c)).collect();
        entry.insert(b"path".to_vec(), BValue::List(path_list));
        bencode_files.push(BValue::Dict(entry));
    }

    let mut info = BTreeMap::new();
    info.insert(b"files".to_vec(), BValue::List(bencode_files));
    info.insert(b"name".to_vec(), BValue::str_val(&root_name));
    info.insert(b"piece length".to_vec(), BValue::Int(piece_length as i64));
    info.insert(b"pieces".to_vec(), BValue::Bytes(pieces_bytes));
    info.insert(b"private".to_vec(), BValue::Int(1));

    let mut root = BTreeMap::new();
    root.insert(b"announce".to_vec(), BValue::str_val(announce));
    root.insert(b"created by".to_vec(), BValue::str_val("DubAssistant"));
    root.insert(
        b"creation date".to_vec(),
        BValue::Int(chrono::Utc::now().timestamp()),
    );
    root.insert(b"info".to_vec(), BValue::Dict(info));

    let encoded = BValue::Dict(root).encode();
    let infohash = validate_and_extract_infohash(&encoded)?;

    if let Some(parent) = output_torrent_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output_torrent_path, &encoded)?;

    Ok(CreatedTorrent {
        torrent_path: output_torrent_path.to_path_buf(),
        infohash,
        total_size,
    })
}

fn collect_files_sorted(root: &Path) -> Result<Vec<(Vec<String>, PathBuf, u64)>> {
    let mut acc = Vec::new();
    walk_dir(root, root, &mut acc)?;
    Ok(acc)
}

fn walk_dir(root: &Path, current: &Path, acc: &mut Vec<(Vec<String>, PathBuf, u64)>) -> Result<()> {
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let meta = entry.metadata()?;
        if meta.is_dir() {
            walk_dir(root, &path, acc)?;
        } else if meta.is_file() {
            let rel = path.strip_prefix(root)?;
            let components: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().to_string())
                .collect();
            acc.push((components, path, meta.len()));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::bencode::decode;

    #[test]
    fn test_create_private_torrent_multi_file() {
        let tmp = std::env::temp_dir().join("capybara_torrent_unit_test");
        let _ = std::fs::remove_dir_all(&tmp);
        let content_dir = tmp.join("Filme Teste 2024 AC3 640 5.1 Blu-ray");
        std::fs::create_dir_all(&content_dir).unwrap();
        std::fs::write(content_dir.join("Filme Teste.ac3"), vec![42u8; 300_000]).unwrap();
        std::fs::write(
            content_dir.join("PT-BR (Full).srt"),
            b"1\n00:00:01,000 --> 00:00:02,000\nOi\n",
        )
        .unwrap();
        std::fs::write(
            content_dir.join("mediainfo.txt"),
            b"General\nComplete name: test",
        )
        .unwrap();

        let out_torrent = tmp.join("out.torrent");
        let cancel = CancelHandle::new();
        let created = create_private_torrent(
            &content_dir,
            &out_torrent,
            "https://capybarabr.com/announce/dummykey",
            &cancel,
            |_| {},
        )
        .unwrap();

        assert_eq!(created.infohash.len(), 40);
        let bytes = std::fs::read(&out_torrent).unwrap();
        let parsed = decode(&bytes).unwrap();
        if let BValue::Dict(map) = parsed {
            assert_eq!(
                map.get(b"announce".as_slice()),
                Some(&BValue::str_val("https://capybarabr.com/announce/dummykey"))
            );
            if let Some(BValue::Dict(info)) = map.get(b"info".as_slice()) {
                assert_eq!(info.get(b"private".as_slice()), Some(&BValue::Int(1)));
            } else {
                panic!("Missing info dict");
            }
        } else {
            panic!("Root is not dict");
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
