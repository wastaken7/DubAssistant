use anyhow::{bail, Result};
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BValue {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<BValue>),
    Dict(BTreeMap<Vec<u8>, BValue>),
}

impl BValue {
    pub fn str_val(s: &str) -> Self {
        BValue::Bytes(s.as_bytes().to_vec())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            BValue::Int(n) => {
                out.push(b'i');
                out.extend_from_slice(n.to_string().as_bytes());
                out.push(b'e');
            }
            BValue::Bytes(b) => {
                out.extend_from_slice(b.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(b);
            }
            BValue::List(items) => {
                out.push(b'l');
                for item in items {
                    item.encode_into(out);
                }
                out.push(b'e');
            }
            BValue::Dict(map) => {
                out.push(b'd');
                for (k, v) in map {
                    out.extend_from_slice(k.len().to_string().as_bytes());
                    out.push(b':');
                    out.extend_from_slice(k);
                    v.encode_into(out);
                }
                out.push(b'e');
            }
        }
    }
}

pub fn decode(data: &[u8]) -> Result<BValue> {
    let (val, rest) = decode_next(data)?;
    if !rest.is_empty() {
        bail!("Dados extras após o fim do objeto Bencode ({} bytes)", rest.len());
    }
    Ok(val)
}

fn decode_next(data: &[u8]) -> Result<(BValue, &[u8])> {
    if data.is_empty() {
        bail!("Fim inesperado de dados Bencode");
    }
    match data[0] {
        b'i' => {
            let end = data
                .iter()
                .position(|&b| b == b'e')
                .ok_or_else(|| anyhow::anyhow!("Inteiro Bencode sem delimitador 'e'"))?;
            let num_str = std::str::from_utf8(&data[1..end])?;
            let n: i64 = num_str.parse()?;
            Ok((BValue::Int(n), &data[end + 1..]))
        }
        b'l' => {
            let mut rest = &data[1..];
            let mut items = Vec::new();
            while !rest.is_empty() && rest[0] != b'e' {
                let (v, next) = decode_next(rest)?;
                items.push(v);
                rest = next;
            }
            if rest.is_empty() {
                bail!("Lista Bencode sem delimitador 'e'");
            }
            Ok((BValue::List(items), &rest[1..]))
        }
        b'd' => {
            let mut rest = &data[1..];
            let mut map = BTreeMap::new();
            while !rest.is_empty() && rest[0] != b'e' {
                let (k, next) = decode_next(rest)?;
                let key = match k {
                    BValue::Bytes(b) => b,
                    _ => bail!("Chave de dicionário Bencode deve ser byte string"),
                };
                let (v, next2) = decode_next(next)?;
                map.insert(key, v);
                rest = next2;
            }
            if rest.is_empty() {
                bail!("Dicionário Bencode sem delimitador 'e'");
            }
            Ok((BValue::Dict(map), &rest[1..]))
        }
        b'0'..=b'9' => {
            let colon = data
                .iter()
                .position(|&b| b == b':')
                .ok_or_else(|| anyhow::anyhow!("Byte string Bencode sem ':'"))?;
            let len: usize = std::str::from_utf8(&data[..colon])?.parse()?;
            let start = colon + 1;
            let end = start + len;
            if end > data.len() {
                bail!("Byte string Bencode truncada");
            }
            Ok((BValue::Bytes(data[start..end].to_vec()), &data[end..]))
        }
        other => bail!("Prefixo Bencode inválido: 0x{other:02x}"),
    }
}

/// Valida os bytes de um `.torrent` e retorna seu `infohash` v1 (hex minúsculo, 40 chars).
pub fn validate_and_extract_infohash(torrent_bytes: &[u8]) -> Result<String> {
    let root = decode(torrent_bytes)?;
    let BValue::Dict(map) = root else {
        bail!("Raiz do .torrent não é um dicionário Bencode");
    };
    let info = map
        .get(b"info".as_slice())
        .ok_or_else(|| anyhow::anyhow!("Arquivo .torrent sem seção 'info'"))?;
    let info_encoded = info.encode();
    let digest = Sha1::digest(&info_encoded);
    Ok(hex::encode(digest))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_roundtrip_and_infohash() {
        let mut info = BTreeMap::new();
        info.insert(b"name".to_vec(), BValue::str_val("test"));
        info.insert(b"piece length".to_vec(), BValue::Int(262144));
        info.insert(b"pieces".to_vec(), BValue::Bytes(vec![0u8; 20]));
        info.insert(b"private".to_vec(), BValue::Int(1));

        let mut root = BTreeMap::new();
        root.insert(
            b"announce".to_vec(),
            BValue::str_val("https://capybarabr.com/announce/test"),
        );
        root.insert(b"info".to_vec(), BValue::Dict(info));

        let encoded = BValue::Dict(root.clone()).encode();
        let decoded = decode(&encoded).unwrap();
        assert_eq!(decoded, BValue::Dict(root));

        let hash = validate_and_extract_infohash(&encoded).unwrap();
        assert_eq!(hash.len(), 40);
    }
}
