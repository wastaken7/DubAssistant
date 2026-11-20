use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InputMode {
    VideoFiles,
    ExtractedFolder,
    RawDisc,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiscKind {
    BluRay,
    Dvd,
}

impl DiscKind {
    pub fn label(self) -> &'static str {
        match self {
            DiscKind::BluRay => "Blu-ray (BDMV)",
            DiscKind::Dvd => "DVD (VIDEO_TS)",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscTitle {
    pub index: u32,
    pub label: String,
    pub mpls_name: Option<String>,
    pub duration_str: String,
    pub duration_secs: u64,
    pub fps: Option<String>,
    pub has_chapters: bool,
    pub chapters_track_id: Option<u32>,
    pub chapters_text: Option<String>,
    pub tracks: Vec<MediaTrack>,
    pub selected: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectedDisc {
    pub root_dir: PathBuf,
    pub kind: DiscKind,
    pub titles: Vec<DiscTitle>,
    pub mediainfo_target: Option<PathBuf>,
    pub raw_mediainfo: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Category {
    Movie,
    Series,
}

impl Category {
    pub fn id(self) -> &'static str {
        match self {
            Category::Movie => "14",
            Category::Series => "15",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Category::Movie => "Dublagens - Filmes",
            Category::Series => "Dublagens - Séries",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DubVersion {
    Cinema,
    Tv,
    Redub,
    Streaming,
    HomeVideo,
    Other,
}

impl DubVersion {
    pub const ALL: [DubVersion; 6] = [
        DubVersion::Cinema,
        DubVersion::Tv,
        DubVersion::Redub,
        DubVersion::Streaming,
        DubVersion::HomeVideo,
        DubVersion::Other,
    ];

    pub fn form_value(self) -> &'static str {
        match self {
            DubVersion::Cinema => "cinema",
            DubVersion::Tv => "tv",
            DubVersion::Redub => "redub",
            DubVersion::Streaming => "streaming",
            DubVersion::HomeVideo => "home_video",
            DubVersion::Other => "other",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            DubVersion::Cinema => "Cinema",
            DubVersion::Tv => "TV / Clássica",
            DubVersion::Redub => "Redublagem",
            DubVersion::Streaming => "Streaming",
            DubVersion::HomeVideo => "DVD / Blu-ray",
            DubVersion::Other => "Outro",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SourceType {
    Bluray,
    Web,
    Dvd,
    Tv,
}

impl SourceType {
    pub const ALL: [SourceType; 4] = [
        SourceType::Bluray,
        SourceType::Web,
        SourceType::Dvd,
        SourceType::Tv,
    ];

    pub fn form_value(self) -> &'static str {
        match self {
            SourceType::Bluray => "bluray",
            SourceType::Web => "web",
            SourceType::Dvd => "dvd",
            SourceType::Tv => "tv",
        }
    }

    pub fn title_label(self) -> &'static str {
        match self {
            SourceType::Bluray => "Blu-ray",
            SourceType::Web => "WEB-DL",
            SourceType::Dvd => "DVD",
            SourceType::Tv => "TV / VHS",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AudioCodec {
    Ac3,
    Eac3,
    Dts,
    DtsHdMa,
    TrueHd,
    Aac,
    Flac,
    Pcm,
}

impl AudioCodec {
    pub const ALL: [AudioCodec; 8] = [
        AudioCodec::Ac3,
        AudioCodec::Eac3,
        AudioCodec::Dts,
        AudioCodec::DtsHdMa,
        AudioCodec::TrueHd,
        AudioCodec::Aac,
        AudioCodec::Flac,
        AudioCodec::Pcm,
    ];

    pub fn title_label(self) -> &'static str {
        match self {
            AudioCodec::Ac3 => "AC3",
            AudioCodec::Eac3 => "EAC3",
            AudioCodec::Dts => "DTS",
            AudioCodec::DtsHdMa => "DTS-HD MA",
            AudioCodec::TrueHd => "TrueHD",
            AudioCodec::Aac => "AAC",
            AudioCodec::Flac => "FLAC",
            AudioCodec::Pcm => "PCM",
        }
    }

    /// Extensão pura obrigatória conforme /dublagens/regras (sem container .mka/.m4a)
    pub fn pure_extension(self) -> &'static str {
        match self {
            AudioCodec::Ac3 => "ac3",
            AudioCodec::Eac3 => "eac3",
            AudioCodec::Dts => "dts",
            AudioCodec::DtsHdMa => "dtshd",
            AudioCodec::TrueHd => "thd",
            AudioCodec::Aac => "aac",
            AudioCodec::Flac => "flac",
            AudioCodec::Pcm => "wav",
        }
    }

    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.trim().trim_start_matches('.').to_ascii_lowercase().as_str() {
            "ac3" => Some(AudioCodec::Ac3),
            "eac3" | "ec3" => Some(AudioCodec::Eac3),
            "dts" => Some(AudioCodec::Dts),
            "dtshd" => Some(AudioCodec::DtsHdMa),
            "thd" | "mlp" => Some(AudioCodec::TrueHd),
            "aac" => Some(AudioCodec::Aac),
            "flac" => Some(AudioCodec::Flac),
            "wav" | "w64" | "pcm" => Some(AudioCodec::Pcm),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaTrack {
    pub id: u32,
    /// Índice relativo ao tipo ou ao container (mkvmerge track id / ffprobe stream index)
    pub kind: TrackKind,
    pub codec_raw: String,
    pub allowed_audio_codec: Option<AudioCodec>,
    pub language: String,
    pub name: String,
    pub channels: Option<String>,
    pub bitrate_kbps: Option<u32>,
    pub delay_ms: Option<i64>,
    pub forced: bool,
    pub default_track: bool,
    pub selected: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InspectedVideo {
    pub path: PathBuf,
    pub is_mkv: bool,
    pub episode_tag: Option<String>,
    pub embedded_tmdb_id: Option<String>,
    pub embedded_tmdb_category: Option<Category>,
    pub fps: Option<String>,
    pub duration_str: Option<String>,
    pub has_chapters: bool,
    pub tracks: Vec<MediaTrack>,
    pub raw_mediainfo: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedFolderScan {
    pub root_dir: PathBuf,
    pub audio_files: Vec<PathBuf>,
    pub subtitle_files: Vec<PathBuf>,
    pub chapter_files: Vec<PathBuf>,
    pub mediainfo_file: Option<PathBuf>,
    pub forbidden_files: Vec<PathBuf>,
    pub detected_codec: Option<AudioCodec>,
    pub detected_bitrate_kbps: Option<u32>,
    pub detected_channels: Option<String>,
    pub detected_fps: Option<String>,
    pub source_mediainfo: String,
    pub audio_mediainfo: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UploadMetadata {
    pub category: Category,
    pub tmdb_id: String,
    pub season_number: String,
    pub title_pt_br: String,
    pub title_original: String,
    pub year: String,
    pub audio_codec: AudioCodec,
    pub bitrate_kbps: String,
    pub channels: String,
    pub version: DubVersion,
    pub studio: String,
    pub dub_year: String,
    pub source: SourceType,
    pub source_label: String,
    pub fps: String,
    pub note: String,
    pub description: String,
    pub anon: bool,
    /// Nome final do envio (editável; preenchido automaticamente pelo gerador oficial)
    pub release_name: String,
    pub manual_name_override: bool,
    pub source_mediainfo: String,
    pub audio_mediainfo: String,
}

impl Default for UploadMetadata {
    fn default() -> Self {
        Self {
            category: Category::Movie,
            tmdb_id: String::new(),
            season_number: "1".to_string(),
            title_pt_br: String::new(),
            title_original: String::new(),
            year: String::new(),
            audio_codec: AudioCodec::Ac3,
            bitrate_kbps: "640".to_string(),
            channels: "5.1".to_string(),
            version: DubVersion::Cinema,
            studio: String::new(),
            dub_year: String::new(),
            source: SourceType::Bluray,
            source_label: String::new(),
            fps: "23.976".to_string(),
            note: String::new(),
            description: String::new(),
            anon: false,
            release_name: String::new(),
            manual_name_override: false,
            source_mediainfo: String::new(),
            audio_mediainfo: String::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PackageOutput {
    pub output_folder: PathBuf,
    pub torrent_path: PathBuf,
    pub infohash: String,
    pub first_audio_path: Option<PathBuf>,
    pub source_mediainfo: String,
    pub audio_mediainfo: String,
    pub created_files: Vec<PathBuf>,
    pub delay_warnings: Vec<String>,
}
