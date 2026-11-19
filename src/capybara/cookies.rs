use crate::config::{AppConfig, CookieSource};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// Carrega o valor do header `Cookie` a partir da configuração (arquivo Netscape ou texto bruto).
pub fn load_cookie_header(cfg: &AppConfig) -> Result<String> {
    match cfg.cookie_source {
        CookieSource::NetscapeFile => {
            let path = cfg.cookies_file_path.trim();
            if path.is_empty() {
                bail!("Arquivo cookies.txt não configurado nas Configurações.");
            }
            let content = std::fs::read_to_string(Path::new(path))
                .with_context(|| format!("Falha ao ler arquivo de cookies: {path}"))?;
            parse_netscape_cookies_for_domain(&content, "capybarabr.com")
        }
        CookieSource::RawHeader => {
            let raw = cfg.raw_cookie_header.trim();
            if raw.is_empty() {
                bail!("Header Cookie vazio nas Configurações.");
            }
            Ok(normalize_raw_cookie_header(raw))
        }
    }
}

pub fn normalize_raw_cookie_header(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some(rest) = trimmed.strip_prefix("Cookie:").or_else(|| trimmed.strip_prefix("cookie:")) {
        rest.trim().to_string()
    } else {
        trimmed.to_string()
    }
}

/// Faz parsing do formato Netscape `cookies.txt` (incluindo linhas `#HttpOnly_`) filtrando pelo domínio alvo.
pub fn parse_netscape_cookies_for_domain(content: &str, target_domain: &str) -> Result<String> {
    let now = chrono::Utc::now().timestamp();
    let target = target_domain.to_ascii_lowercase();
    let mut pairs: BTreeMap<String, String> = BTreeMap::new();

    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let cleaned = if let Some(rest) = trimmed.strip_prefix("#HttpOnly_") {
            rest
        } else if trimmed.starts_with('#') {
            continue;
        } else {
            trimmed
        };

        let cols: Vec<&str> = cleaned.split('\t').collect();
        if cols.len() < 7 {
            continue;
        }

        let domain = cols[0].trim().trim_start_matches('.').to_ascii_lowercase();
        if domain != target && !target.ends_with(&format!(".{domain}")) {
            continue;
        }

        let expiry: i64 = cols[4].trim().parse().unwrap_or(0);
        if expiry > 0 && expiry < now {
            continue;
        }

        let name = cols[5].trim();
        let value = cols[6].trim();
        if !name.is_empty() {
            pairs.insert(name.to_string(), value.to_string());
        }
    }

    if pairs.is_empty() {
        bail!(
            "Nenhum cookie válido encontrado para '{}' no arquivo fornecido.",
            target_domain
        );
    }

    let header = pairs
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("; ");
    Ok(header)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_netscape_cookies() {
        let sample = "\
# Netscape HTTP Cookie File
#HttpOnly_.capybarabr.com\tTRUE\t/\tTRUE\t0\tlaravel_session\tabc123
.capybarabr.com\tTRUE\t/\tTRUE\t0\tXSRF-TOKEN\txyz789
.google.com\tTRUE\t/\tTRUE\t0\tNID\tignoreme
.capybarabr.com\tTRUE\t/\tTRUE\t1000\texpired_cookie\tdead
";
        let header = parse_netscape_cookies_for_domain(sample, "capybarabr.com").unwrap();
        assert!(header.contains("laravel_session=abc123"));
        assert!(header.contains("XSRF-TOKEN=xyz789"));
        assert!(!header.contains("NID"));
        assert!(!header.contains("expired_cookie"));
    }

    #[test]
    fn test_normalize_raw_header() {
        assert_eq!(
            normalize_raw_cookie_header("Cookie: a=1; b=2"),
            "a=1; b=2"
        );
    }
}
