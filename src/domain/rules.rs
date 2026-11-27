use crate::domain::models::{AudioCodec, Category, UploadMetadata};
use regex::Regex;
use std::path::Path;
use std::sync::OnceLock;

/// Extensões de legendas aceitas na subpasta Legendas/
pub const ALLOWED_SUBTITLE_EXTS: &[&str] = &["srt", "ass", "ssa", "sup", "sub", "idx", "vtt"];

/// Extensões explicitamente proibidas pelo /dublagens/regras
pub const FORBIDDEN_EXTS: &[&str] = &[
    "mka", "m4a", "mp3", "opus", "ogg", "wma", "mkv", "mp4", "avi", "mov", "wmv", "ts", "m2ts",
    "zip", "rar", "7z", "tar", "gz", "jpg", "jpeg", "png", "webp", "bmp", "gif", "exe", "bat",
    "cmd", "msi", "dll", "nfo", "torrent",
];

/// Mapeia identificador de codec vindo do mkvmerge / ffprobe / MediaInfo para AudioCodec permitido.
/// Retorna `None` para codecs proibidos (ex: MP3, Opus, Vorbis).
pub fn classify_audio_codec(codec_raw: &str, profile_hint: Option<&str>) -> Option<AudioCodec> {
    let raw = codec_raw.to_ascii_uppercase();
    let hint = profile_hint.unwrap_or("").to_ascii_uppercase();
    let combined = format!("{raw} {hint}");

    if combined.contains("MP3")
        || combined.contains("MPEG AUDIO")
        || combined.contains("OPUS")
        || combined.contains("VORBIS")
        || combined.contains("WMA")
    {
        return None;
    }

    if combined.contains("TRUEHD") || combined.contains("MLP") || combined.contains("A_TRUEHD") {
        return Some(AudioCodec::TrueHd);
    }
    if combined.contains("DTS-HD")
        || combined.contains("DTS HD")
        || combined.contains("DTS:X")
        || combined.contains("XLL")
        || combined.contains("MA") && combined.contains("DTS")
    {
        return Some(AudioCodec::DtsHdMa);
    }
    if combined.contains("E-AC-3")
        || combined.contains("EAC3")
        || combined.contains("EC3")
        || combined.contains("A_EAC3")
        || combined.contains("DD+")
        || combined.contains("DDP")
    {
        return Some(AudioCodec::Eac3);
    }
    if combined.contains("AC-3")
        || combined.contains("AC3")
        || combined.contains("A_AC3")
        || combined.contains("DOLBY DIGITAL")
    {
        return Some(AudioCodec::Ac3);
    }
    if combined.contains("DTS") || combined.contains("A_DTS") {
        return Some(AudioCodec::Dts);
    }
    if combined.contains("FLAC") || combined.contains("A_FLAC") {
        return Some(AudioCodec::Flac);
    }
    if combined.contains("PCM") || combined.contains("LPCM") || combined.contains("A_PCM") {
        return Some(AudioCodec::Pcm);
    }
    if combined.contains("AAC") || combined.contains("A_AAC") {
        return Some(AudioCodec::Aac);
    }

    None
}

/// Formata o nome oficial de envio seguindo `/dublagens/regras`:
/// `Nome em pt-BR (Nome original) Ano Codec Bitrate Canais Fonte`
/// Para séries inclui `SXX` logo antes do ano (se temporada informada).
/// Separador obrigatório: espaço (sem `.` ou `_`, preservando decimais como `5.1` e `23.976`).
pub fn build_official_release_name(meta: &UploadMetadata) -> String {
    let pt = normalize_title_spaces(&meta.title_pt_br);
    let orig = normalize_title_spaces(&meta.title_original);

    let title_part = if !pt.is_empty() && !orig.is_empty() && !pt.eq_ignore_ascii_case(&orig) {
        format!("{pt} ({orig})")
    } else if !pt.is_empty() {
        pt
    } else {
        orig
    };

    let mut parts: Vec<String> = Vec::new();
    if !title_part.is_empty() {
        parts.push(title_part);
    }

    if meta.category == Category::Series {
        if let Ok(season) = meta.season_number.trim().parse::<u32>() {
            parts.push(format!("S{season:02}"));
        }
    }

    let year = meta.year.trim();
    if !year.is_empty() {
        parts.push(year.to_string());
    }

    parts.push(meta.audio_codec.title_label().to_string());

    let bitrate_digits: String = meta
        .bitrate_kbps
        .chars()
        .filter(|c| c.is_ascii_digit())
        .collect();
    if !bitrate_digits.is_empty() {
        parts.push(bitrate_digits);
    }

    let channels = meta.channels.trim();
    if !channels.is_empty() {
        parts.push(channels.to_string());
    }

    parts.push(meta.source.title_label().to_string());

    collapse_spaces(&parts.join(" "))
}

/// Remove `_` e `.` indevidos (preservando decimais como `5.1` e `23.976`), colapsa espaços.
pub fn normalize_title_spaces(input: &str) -> String {
    static RE_NON_DECIMAL_DOT: OnceLock<Regex> = OnceLock::new();
    let re =
        RE_NON_DECIMAL_DOT.get_or_init(|| Regex::new(r"(?P<a>[^0-9])\.|\.(?P<b>[^0-9])").unwrap());

    let replaced_underscores = input.replace('_', " ");
    let mut s = replaced_underscores;
    // Duas passadas para casos como "S.H.I.E.L.D." -> "S H I E L D"
    for _ in 0..2 {
        s = re
            .replace_all(&s, |caps: &regex::Captures| {
                let a = caps.name("a").map(|m| m.as_str()).unwrap_or("");
                let b = caps.name("b").map(|m| m.as_str()).unwrap_or("");
                format!("{a} {b}")
            })
            .to_string();
    }
    collapse_spaces(&s)
}

pub fn collapse_spaces(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Sanitiza um nome para ser válido em caminhos do Windows (pasta raiz / nome do arquivo)
/// preservando o nome de exibição separado no formulário `name` (ex: `Face/Off` -> `Face-Off`).
pub fn sanitize_windows_filename(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        match ch {
            '/' | '\\' => out.push('-'),
            ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push(' '),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    let cleaned = collapse_spaces(&out)
        .trim_matches(|c| c == '.' || c == ' ')
        .to_string();
    if cleaned.is_empty() {
        "Dublagem".to_string()
    } else {
        cleaned
    }
}

/// Extrai tag de episódio (ex: `S01E05`, `1x05`, `E05`) de um nome de arquivo para ordenação de séries.
pub fn extract_episode_tag(path: &Path) -> Option<(u32, u32, String)> {
    static RE_SE: OnceLock<Regex> = OnceLock::new();
    static RE_X: OnceLock<Regex> = OnceLock::new();
    static RE_EP: OnceLock<Regex> = OnceLock::new();

    let stem = path.file_stem()?.to_string_lossy();

    let re_se = RE_SE.get_or_init(|| Regex::new(r"(?i)S(\d{1,2})[\s._-]*E(\d{1,3})").unwrap());
    if let Some(c) = re_se.captures(&stem) {
        let s: u32 = c[1].parse().ok()?;
        let e: u32 = c[2].parse().ok()?;
        return Some((s, e, format!("S{s:02}E{e:02}")));
    }

    let re_x = RE_X.get_or_init(|| Regex::new(r"(?i)\b(\d{1,2})x(\d{1,3})\b").unwrap());
    if let Some(c) = re_x.captures(&stem) {
        let s: u32 = c[1].parse().ok()?;
        let e: u32 = c[2].parse().ok()?;
        return Some((s, e, format!("S{s:02}E{e:02}")));
    }

    let re_ep = RE_EP
        .get_or_init(|| Regex::new(r"(?i)\b(?:EP?|Episodio|Episode)[\s._-]*(\d{1,3})\b").unwrap());
    if let Some(c) = re_ep.captures(&stem) {
        let e: u32 = c[1].parse().ok()?;
        return Some((1, e, format!("E{e:02}")));
    }

    None
}

/// Classifica um arquivo encontrado no modo "Pasta já Extraída" contra as regras do CapybaraBR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderFileClass {
    Audio(AudioCodec),
    Subtitle,
    Chapters,
    MediaInfo,
    Forbidden,
}

pub fn classify_extracted_file(path: &Path) -> FolderFileClass {
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let ext = path
        .extension()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();

    if file_name == "mediainfo.txt" || (ext == "txt" && file_name.contains("mediainfo")) {
        return FolderFileClass::MediaInfo;
    }

    if ext == "xml"
        || (ext == "txt" && (file_name.contains("chapter") || file_name.contains("cap")))
    {
        return FolderFileClass::Chapters;
    }

    if let Some(codec) = AudioCodec::from_extension(&ext) {
        return FolderFileClass::Audio(codec);
    }

    if ALLOWED_SUBTITLE_EXTS.contains(&ext.as_str()) {
        return FolderFileClass::Subtitle;
    }

    if FORBIDDEN_EXTS.contains(&ext.as_str()) {
        return FolderFileClass::Forbidden;
    }

    // Qualquer outro arquivo desconhecido é bloqueado por segurança contra as regras do acervo
    FolderFileClass::Forbidden
}

/// Valida os metadados obrigatórios antes do envio para `/dublagens`.
pub fn validate_upload_metadata(meta: &UploadMetadata) -> Vec<String> {
    let mut errors = Vec::new();

    if meta.release_name.trim().is_empty() {
        errors.push("O nome do envio está vazio.".to_string());
    } else if meta.release_name.chars().count() > 255 {
        errors.push("O nome do envio excede o limite de 255 caracteres.".to_string());
    }

    if meta.tmdb_id.trim().is_empty() || !meta.tmdb_id.trim().chars().all(|c| c.is_ascii_digit()) {
        errors.push("TMDB ID é obrigatório e deve conter apenas números.".to_string());
    }

    if meta.category == Category::Series {
        if meta.season_number.trim().is_empty()
            || !meta
                .season_number
                .trim()
                .chars()
                .all(|c| c.is_ascii_digit())
        {
            errors.push("Número da temporada é obrigatório para Séries.".to_string());
        }
    }

    if meta.source_mediainfo.trim().is_empty() {
        errors.push("MediaInfo do vídeo fonte é obrigatório.".to_string());
    }

    if meta.audio_mediainfo.trim().is_empty() {
        errors.push("MediaInfo do áudio extraído é obrigatório.".to_string());
    }

    errors
}

/// Detecta automaticamente a Fonte (`Blu-ray`, `WEB-DL`, `DVD`, `TV / VHS`) a partir do nome
/// do arquivo/pasta (case-insensitive) e, como fallback, pelo codec de áudio detectado.
pub fn detect_source_from_name_and_codec(
    name_or_path: &str,
    codec: Option<AudioCodec>,
) -> Option<crate::domain::models::SourceType> {
    use crate::domain::models::SourceType;

    static RE_BLURAY: OnceLock<Regex> = OnceLock::new();
    static RE_WEB: OnceLock<Regex> = OnceLock::new();
    static RE_DVD: OnceLock<Regex> = OnceLock::new();
    static RE_TV: OnceLock<Regex> = OnceLock::new();

    let re_bluray = RE_BLURAY.get_or_init(|| {
        Regex::new(r"(?i)(?:^|[\s._\-\(\[])(blu-?ray|bdrip|brrip|bdremux|remux|bdmv|uhd\.?bd)(?:$|[\s._\-\)\]])").unwrap()
    });
    let re_web = RE_WEB.get_or_init(|| {
        Regex::new(r"(?i)(?:^|[\s._\-\(\[])(web-?dl|webrip|web|amzn|nf|netflix|dsnp|atvp|hmax|pcok|pmtp|glob|globoplay|hulu|stan)(?:$|[\s._\-\)\]])").unwrap()
    });
    let re_dvd = RE_DVD.get_or_init(|| {
        Regex::new(r"(?i)(?:^|[\s._\-\(\[])(dvd-?rip|dvd-?r|dvdr|dvd5|dvd9|dvd|video_ts)(?:$|[\s._\-\)\]])").unwrap()
    });
    let re_tv = RE_TV.get_or_init(|| {
        Regex::new(r"(?i)(?:^|[\s._\-\(\[])(hdtv|pdtv|sdtv|dsr|tvrip|satrip|vhsrip|vhs|tv)(?:$|[\s._\-\)\]])").unwrap()
    });

    // Prioridade para nomes explícitos de mídia no título/pasta
    if re_bluray.is_match(name_or_path) {
        return Some(SourceType::Bluray);
    }
    if re_web.is_match(name_or_path) {
        return Some(SourceType::Web);
    }
    if re_dvd.is_match(name_or_path) {
        return Some(SourceType::Dvd);
    }
    if re_tv.is_match(name_or_path) {
        return Some(SourceType::Tv);
    }

    // Fallback heurístico pelo codec quando o nome não traz a fonte explícita
    match codec {
        Some(AudioCodec::TrueHd) | Some(AudioCodec::DtsHdMa) => Some(SourceType::Bluray),
        Some(AudioCodec::Eac3) => Some(SourceType::Web),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::models::SourceType;

    #[test]
    fn test_official_movie_name_format() {
        let mut meta = UploadMetadata::default();
        meta.category = Category::Movie;
        meta.title_pt_br = "A Outra Face".into();
        meta.title_original = "Face/Off".into();
        meta.year = "1997".into();
        meta.audio_codec = AudioCodec::Ac3;
        meta.bitrate_kbps = "192 kbps".into();
        meta.channels = "2.0".into();
        meta.source = SourceType::Dvd;

        assert_eq!(
            build_official_release_name(&meta),
            "A Outra Face (Face/Off) 1997 AC3 192 2.0 DVD"
        );
        assert_eq!(
            sanitize_windows_filename(&build_official_release_name(&meta)),
            "A Outra Face (Face-Off) 1997 AC3 192 2.0 DVD"
        );
    }

    #[test]
    fn test_official_series_name_format() {
        let mut meta = UploadMetadata::default();
        meta.category = Category::Series;
        meta.season_number = "2".into();
        meta.title_pt_br = "Breaking Bad".into();
        meta.title_original = "Breaking Bad".into();
        meta.year = "2009".into();
        meta.audio_codec = AudioCodec::Eac3;
        meta.bitrate_kbps = "640".into();
        meta.channels = "5.1".into();
        meta.source = SourceType::Web;

        assert_eq!(
            build_official_release_name(&meta),
            "Breaking Bad S02 2009 EAC3 640 5.1 WEB-DL"
        );
    }

    #[test]
    fn test_codec_classification() {
        assert_eq!(classify_audio_codec("A_AC3", None), Some(AudioCodec::Ac3));
        assert_eq!(classify_audio_codec("A_EAC3", None), Some(AudioCodec::Eac3));
        assert_eq!(
            classify_audio_codec("A_DTS", Some("DTS-HD MA")),
            Some(AudioCodec::DtsHdMa)
        );
        assert_eq!(
            classify_audio_codec("A_TRUEHD", None),
            Some(AudioCodec::TrueHd)
        );
        assert_eq!(classify_audio_codec("A_MPEG/L3", Some("MP3")), None);
        assert_eq!(classify_audio_codec("A_OPUS", None), None);
    }

    #[test]
    fn test_folder_file_classification() {
        assert_eq!(
            classify_extracted_file(Path::new("filme.ac3")),
            FolderFileClass::Audio(AudioCodec::Ac3)
        );
        assert_eq!(
            classify_extracted_file(Path::new("filme.mka")),
            FolderFileClass::Forbidden
        );
        assert_eq!(
            classify_extracted_file(Path::new("filme.zip")),
            FolderFileClass::Forbidden
        );
        assert_eq!(
            classify_extracted_file(Path::new("PT-BR (Full).sup")),
            FolderFileClass::Subtitle
        );
        assert_eq!(
            classify_extracted_file(Path::new("mediainfo.txt")),
            FolderFileClass::MediaInfo
        );
        assert_eq!(
            classify_extracted_file(Path::new("chapters.txt")),
            FolderFileClass::Chapters
        );
    }

    #[test]
    fn test_episode_tag_extraction() {
        assert_eq!(
            extract_episode_tag(Path::new("Show.S01E04.1080p.mkv")),
            Some((1, 4, "S01E04".to_string()))
        );
        assert_eq!(
            extract_episode_tag(Path::new("Show 2x12.mp4")),
            Some((2, 12, "S02E12".to_string()))
        );
    }

    #[test]
    fn test_detect_source_from_name_and_codec() {
        assert_eq!(
            detect_source_from_name_and_codec(
                "The.Odyssey.2026.1080p.AMZN.WEB-DL.DDP5.1.mkv",
                None
            ),
            Some(SourceType::Web)
        );
        assert_eq!(
            detect_source_from_name_and_codec("Movie.2024.1080p.WEB.H264.mkv", None),
            Some(SourceType::Web)
        );
        assert_eq!(
            detect_source_from_name_and_codec("Movie.2014.1080p.Blu-ray.REMUX.mkv", None),
            Some(SourceType::Bluray)
        );
        assert_eq!(
            detect_source_from_name_and_codec("Classic.1997.DVD-R.NTSC.mkv", None),
            Some(SourceType::Dvd)
        );
        assert_eq!(
            detect_source_from_name_and_codec("Show.S01E01.HDTV.x264.mkv", None),
            Some(SourceType::Tv)
        );
        assert_eq!(
            detect_source_from_name_and_codec("sem_fonte_no_nome.mkv", Some(AudioCodec::Eac3)),
            Some(SourceType::Web)
        );
        assert_eq!(
            detect_source_from_name_and_codec("sem_fonte_no_nome.mkv", Some(AudioCodec::TrueHd)),
            Some(SourceType::Bluray)
        );
    }
}
