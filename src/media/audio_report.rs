//! Relatório de análise de áudio anexado à descrição do envio (BBCode do CapybaraBR).
//!
//! Uma única decodificação via `ffmpeg` alimenta tudo:
//! - `ebur128` (loudness integrado, LRA, true peak) lido do stderr;
//! - PCM `f32le` em streaming (RAM constante) para níveis/correlação por canal,
//!   clipping, silêncio digital e largura de banda (FFT média por canal).
//!
//! As heurísticas são conservadoras: na dúvida, o relatório informa o número e não acusa.

use crate::cancel::{run_command_cancellable, CancelHandle};
use crate::config::AppConfig;
use crate::domain::models::{AudioCodec, PackageOutput};
use crate::media::configure_no_window;
use crate::media::inspector::format_channel_count;
use anyhow::{bail, Context, Result};
use rustfft::{num_complex::Complex, FftPlanner};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

// ---------------------------------------------------------------------------
// Limiares (calibrados com o acervo local; ver testes)
// ---------------------------------------------------------------------------

/// Canal com RMS médio abaixo disto é considerado "sem sinal".
const SILENT_CHANNEL_DB: f64 = -75.0;
/// Amplitude abaixo da qual um bloco de 100 ms é silêncio digital (-80 dBFS).
const DIGITAL_SILENCE_AMP: f32 = 1.0e-4;
/// Duração mínima de um silêncio digital interno para ser reportado.
const MIN_INTERNAL_SILENCE_SECS: f64 = 3.0;
/// Margem no início/fim do arquivo em que silêncio é considerado normal.
const EDGE_MARGIN_SECS: f64 = 5.0;
/// Amostra considerada no teto (|x| >= 0.999 ≈ -0.01 dBFS).
const CLIP_AMP: f32 = 0.999;
/// Amostras consecutivas no teto que caracterizam um evento de clipping.
const CLIP_RUN: u32 = 3;
/// Acima disto o clipping vira alerta (abaixo é só informado).
const CLIP_EVENTS_ALERT: u64 = 100;
/// Correlação a partir da qual dois canais são considerados "o mesmo sinal".
const SAME_SIGNAL_CORR: f64 = 0.98;
/// Correlação surround × frontal que caracteriza upmix por cópia.
const UPMIX_CORR: f64 = 0.90;
/// Diferença de duração (s) áudio × vídeo que vira alerta. ~1 s é comum (alinhamento de frames no fim).
const DURATION_ALERT_SECS: f64 = 2.0;
/// Em codec lossless, corte abrupto abaixo disto sugere origem lossy.
const LOSSY_ORIGIN_MAX_HZ: f64 = 19_000.0;

/// Fração mínima da duração informada pelo ffprobe que precisa ser decodificada.
const MIN_DECODED_FRACTION: f64 = 0.98;

const FFT_N: usize = 4096;
/// True peak só é interpolado perto do máximo já visto (−6 dB): mesmo resultado, fração do custo.
const TRUE_PEAK_GATE: f32 = 0.5;

/// Filtro polifásico de oversampling 4x da ITU-R BS.1770-4 (Anexo 2), 4 fases × 12 taps.
const TP_COEFFS: [[f32; 12]; 4] = [
    [
        0.001_708_984_4,
        0.010_986_328,
        -0.019_653_32,
        0.033_203_125,
        -0.059_448_242,
        0.137_329_1,
        0.972_167_97,
        -0.102_294_92,
        0.047_607_42,
        -0.026_611_328,
        0.014_892_578,
        -0.008_300_781,
    ],
    [
        -0.029_174_805,
        0.029_296_875,
        -0.051_757_813,
        0.089_111_33,
        -0.166_503_9,
        0.465_087_9,
        0.779_785_16,
        -0.200_317_38,
        0.101_562_5,
        -0.058_227_54,
        0.033_081_055,
        -0.018_920_898,
    ],
    [
        -0.018_920_898,
        0.033_081_055,
        -0.058_227_54,
        0.101_562_5,
        -0.200_317_38,
        0.779_785_16,
        0.465_087_9,
        -0.166_503_9,
        0.089_111_33,
        -0.051_757_813,
        0.029_296_875,
        -0.029_174_805,
    ],
    [
        -0.008_300_781,
        0.014_892_578,
        -0.026_611_328,
        0.047_607_42,
        -0.102_294_92,
        0.972_167_97,
        0.137_329_1,
        -0.059_448_242,
        0.033_203_125,
        -0.019_653_32,
        0.010_986_328,
        0.001_708_984_4,
    ],
];
const BAND_HZ: f64 = 250.0;

// ---------------------------------------------------------------------------
// Modelos
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChannelRole {
    Left,
    Right,
    Center,
    Lfe,
    SurroundLeft,
    SurroundRight,
    BackLeft,
    BackRight,
    BackCenter,
    Other,
}

impl ChannelRole {
    fn is_surround(self) -> bool {
        matches!(
            self,
            ChannelRole::SurroundLeft
                | ChannelRole::SurroundRight
                | ChannelRole::BackLeft
                | ChannelRole::BackRight
                | ChannelRole::BackCenter
        )
    }
}

#[derive(Debug, Clone)]
pub struct ChannelStats {
    pub name: String,
    pub role: ChannelRole,
    pub rms_db: f64,
    pub peak_db: f64,
    pub bandwidth: Option<Bandwidth>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bandwidth {
    pub cutoff_hz: f64,
    /// Queda brusca logo acima do corte (assinatura de filtro de codec lossy).
    pub abrupt: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Loudness {
    pub integrated_lufs: f64,
    pub lra_lu: f64,
    pub true_peak_dbtp: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SilenceGap {
    pub start_secs: f64,
    pub duration_secs: f64,
}

#[derive(Debug, Clone)]
pub struct AudioAnalysis {
    pub file_name: String,
    pub codec_label: String,
    pub lossless: bool,
    pub sample_rate: u32,
    pub layout_label: String,
    pub duration_secs: f64,
    pub dialnorm_db: Option<i32>,
    pub loudness: Option<Loudness>,
    pub channels: Vec<ChannelStats>,
    /// Correlação normalizada (cosseno) entre pares de canais, matriz simétrica.
    pub corr: Vec<Vec<f64>>,
    pub clipped_samples: u64,
    pub clip_events: u64,
    pub silences: Vec<SilenceGap>,
}

/// Informações externas ao arquivo de áudio usadas no relatório.
#[derive(Debug, Clone, Default)]
pub struct ReportContext {
    /// Rótulo curto (ex: "S01E03") usado em relatórios com vários arquivos.
    pub label: Option<String>,
    /// Duração do vídeo de referência (mesmo título/episódio), quando conhecida.
    pub reference_duration_secs: Option<f64>,
    /// FPS de sincronia informado no envio.
    pub fps: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Note,
    Alert,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub severity: Severity,
    pub text: String,
}

impl Finding {
    fn alert(text: impl Into<String>) -> Self {
        Self {
            severity: Severity::Alert,
            text: text.into(),
        }
    }
    fn note(text: impl Into<String>) -> Self {
        Self {
            severity: Severity::Note,
            text: text.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// Probe + decodificação
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct StreamProbe {
    codec_name: String,
    profile: String,
    sample_rate: u32,
    channels: usize,
    channel_layout: String,
    duration_secs: Option<f64>,
}

fn probe_stream(
    cfg: &AppConfig,
    path: &Path,
    stream_index: usize,
    cancel: &CancelHandle,
) -> Result<StreamProbe> {
    let ffprobe = cfg.ffprobe_path.trim();
    if ffprobe.is_empty() {
        bail!("ffprobe não configurado nas Configurações.");
    }
    let mut cmd = Command::new(ffprobe);
    cmd.args([
        "-v",
        "quiet",
        "-print_format",
        "json",
        "-show_format",
        "-show_streams",
    ])
    .args(["-select_streams", &format!("a:{stream_index}")])
    .arg(path);
    let out = run_command_cancellable(&mut cmd, cancel)?;
    let json: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    let s = json
        .get("streams")
        .and_then(|v| v.as_array())
        .and_then(|a| a.first())
        .with_context(|| format!("Nenhuma faixa de áudio encontrada em {}", path.display()))?;

    let str_of = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let duration_secs = s
        .get("duration")
        .and_then(|v| v.as_str())
        .and_then(|v| v.parse::<f64>().ok())
        .or_else(|| {
            json.pointer("/format/duration")
                .and_then(|v| v.as_str())
                .and_then(|v| v.parse::<f64>().ok())
        });

    Ok(StreamProbe {
        codec_name: str_of(s, "codec_name"),
        profile: str_of(s, "profile"),
        sample_rate: str_of(s, "sample_rate").parse().unwrap_or(0),
        channels: s.get("channels").and_then(|v| v.as_u64()).unwrap_or(2) as usize,
        channel_layout: str_of(s, "channel_layout"),
        duration_secs,
    })
}

/// Duração da faixa de **vídeo** de um arquivo de referência via ffprobe.
///
/// Não usa a duração do container: legendas costumam terminar depois do vídeo e
/// inflariam a referência (falso "áudio mais curto").
pub fn probe_duration_secs(cfg: &AppConfig, path: &Path, cancel: &CancelHandle) -> Option<f64> {
    let ffprobe = cfg.ffprobe_path.trim();
    if ffprobe.is_empty() {
        return None;
    }
    let mut cmd = Command::new(ffprobe);
    cmd.args([
        "-v",
        "quiet",
        "-print_format",
        "json",
        "-select_streams",
        // V maiúsculo: ignora capas/thumbnails (attached pictures)
        "V:0",
        "-show_streams",
    ])
    .arg(path);
    let out = run_command_cancellable(&mut cmd, cancel).ok()?;
    let json: Value = serde_json::from_slice(&out.stdout).ok()?;
    let stream = json.get("streams")?.as_array()?.first()?;
    stream
        .get("duration")
        .and_then(|v| v.as_str())
        .and_then(|v| v.parse::<f64>().ok())
        .or_else(|| {
            // MKV: duração por faixa fica na tag DURATION (HH:MM:SS.nnnnnnnnn)
            let tags = stream.get("tags")?.as_object()?;
            let raw = tags
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("DURATION"))?
                .1
                .as_str()?;
            parse_hms(raw)
        })
}

fn parse_hms(raw: &str) -> Option<f64> {
    let mut parts = raw.trim().split(':');
    let h: f64 = parts.next()?.parse().ok()?;
    let m: f64 = parts.next()?.parse().ok()?;
    let s: f64 = parts.next()?.parse().ok()?;
    Some(h * 3600.0 + m * 60.0 + s)
}

fn codec_label(codec_name: &str, profile: &str) -> (String, bool) {
    let p = profile.to_ascii_lowercase();
    match codec_name {
        "ac3" => ("AC3".into(), false),
        "eac3" => ("E-AC3".into(), false),
        "dts" if p.contains("ma") => ("DTS-HD MA".into(), true),
        "dts" if p.contains("hra") || p.contains("hi res") => ("DTS-HD HRA".into(), false),
        "dts" => ("DTS".into(), false),
        "truehd" | "mlp" => ("TrueHD".into(), true),
        "flac" => ("FLAC".into(), true),
        "alac" => ("ALAC".into(), true),
        "aac" => ("AAC".into(), false),
        "opus" => ("Opus".into(), false),
        "mp3" => ("MP3".into(), false),
        c if c.starts_with("pcm_") => ("PCM".into(), true),
        other => (other.to_ascii_uppercase(), false),
    }
}

fn layout_is_unknown(layout_lower: &str) -> bool {
    layout_lower.is_empty() || layout_lower == "unknown" || layout_lower.ends_with("channels")
}

/// Rótulo do layout para o relatório (ex: "5.1", "2.0", "6.0"), a partir do layout do ffprobe.
pub fn layout_label(layout: &str, channels: usize) -> String {
    let l = layout.trim().to_ascii_lowercase();
    let known = match l.as_str() {
        "mono" => "1.0",
        "stereo" | "downmix" => "2.0",
        "2.1" => "2.1",
        "3.0" => "3.0",
        "4.0" | "quad" | "quad(side)" => "4.0",
        "5.0" | "5.0(side)" => "5.0",
        "5.1" | "5.1(side)" => "5.1",
        "6.1" => "6.1",
        "7.1" => "7.1",
        _ => "",
    };
    if !known.is_empty() {
        known.to_string()
    } else if layout_is_unknown(&l) {
        format_channel_count(channels as u64)
    } else {
        layout.trim().to_string()
    }
}

/// Nomes e papéis dos canais na ordem nativa do ffmpeg para o layout informado.
pub fn channel_map(layout: &str, channels: usize) -> Vec<(String, ChannelRole)> {
    use ChannelRole::*;
    let l = layout.trim().to_ascii_lowercase();
    let spec: &[(&str, ChannelRole)] = match l.as_str() {
        "mono" => &[("C", Center)],
        "stereo" | "downmix" => &[("L", Left), ("R", Right)],
        "2.1" => &[("L", Left), ("R", Right), ("LFE", Lfe)],
        "3.0" => &[("L", Left), ("R", Right), ("C", Center)],
        "4.0" => &[("L", Left), ("R", Right), ("C", Center), ("Cs", BackCenter)],
        "quad" => &[
            ("L", Left),
            ("R", Right),
            ("Ls", BackLeft),
            ("Rs", BackRight),
        ],
        "quad(side)" => &[
            ("L", Left),
            ("R", Right),
            ("Ls", SurroundLeft),
            ("Rs", SurroundRight),
        ],
        "5.0" => &[
            ("L", Left),
            ("R", Right),
            ("C", Center),
            ("Ls", BackLeft),
            ("Rs", BackRight),
        ],
        "5.0(side)" => &[
            ("L", Left),
            ("R", Right),
            ("C", Center),
            ("Ls", SurroundLeft),
            ("Rs", SurroundRight),
        ],
        "5.1" => &[
            ("L", Left),
            ("R", Right),
            ("C", Center),
            ("LFE", Lfe),
            ("Ls", BackLeft),
            ("Rs", BackRight),
        ],
        "5.1(side)" => &[
            ("L", Left),
            ("R", Right),
            ("C", Center),
            ("LFE", Lfe),
            ("Ls", SurroundLeft),
            ("Rs", SurroundRight),
        ],
        "6.1" => &[
            ("L", Left),
            ("R", Right),
            ("C", Center),
            ("LFE", Lfe),
            ("Cs", BackCenter),
            ("Ls", SurroundLeft),
            ("Rs", SurroundRight),
        ],
        "7.1" => &[
            ("L", Left),
            ("R", Right),
            ("C", Center),
            ("LFE", Lfe),
            ("Lb", BackLeft),
            ("Rb", BackRight),
            ("Ls", SurroundLeft),
            ("Rs", SurroundRight),
        ],
        _ => &[],
    };
    if spec.len() == channels {
        return spec.iter().map(|(n, r)| (n.to_string(), *r)).collect();
    }
    // Só deduz pelo número de canais quando o layout é desconhecido; um layout conhecido
    // fora da tabela (ex: 6.0, hexagonal) recebe nomes genéricos em vez de um LFE inventado.
    if !layout_is_unknown(&l) {
        return (0..channels)
            .map(|i| (format!("Ch{}", i + 1), Other))
            .collect();
    }
    let fallback = match channels {
        1 => "mono",
        2 => "stereo",
        6 => "5.1(side)",
        8 => "7.1",
        _ => "",
    };
    if !fallback.is_empty() && fallback != l {
        return channel_map(fallback, channels);
    }
    (0..channels)
        .map(|i| (format!("Ch{}", i + 1), Other))
        .collect()
}

/// Lê o dialnorm do primeiro frame de um AC3/E-AC3 elementar (o arquivo deve começar no sync).
pub fn parse_ac3_dialnorm(bytes: &[u8]) -> Option<i32> {
    if bytes.len() < 8 || bytes[0] != 0x0B || bytes[1] != 0x77 {
        return None;
    }
    let bsid = bytes[5] >> 3;
    let raw = if bsid <= 8 {
        // AC3: bsid(5) bsmod(3) | acmod(3) [cmixlev 2] [surmixlev 2] [dsurmod 2] lfeon(1) dialnorm(5)
        let mut r = BitReader::new(&bytes[6..]);
        let acmod = r.read(3)?;
        if acmod & 1 == 1 && acmod != 1 {
            r.read(2)?;
        }
        if acmod & 4 == 4 {
            r.read(2)?;
        }
        if acmod == 2 {
            r.read(2)?;
        }
        r.read(1)?;
        r.read(5)?
    } else if (11..=16).contains(&bsid) {
        // E-AC3: ... bsid(5) dialnorm(5)
        (((bytes[5] & 0x07) as u32) << 2) | ((bytes[6] >> 6) as u32)
    } else {
        return None;
    };
    Some(if raw == 0 { -31 } else { -(raw as i32) })
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn read(&mut self, n: usize) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            let bit = (byte >> (7 - (self.pos % 8))) & 1;
            v = (v << 1) | bit as u32;
            self.pos += 1;
        }
        Some(v)
    }
}

fn read_dialnorm(path: &Path, codec_name: &str) -> Option<i32> {
    if codec_name != "ac3" && codec_name != "eac3" {
        return None;
    }
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 16];
    f.read_exact(&mut buf).ok()?;
    parse_ac3_dialnorm(&buf)
}

/// Extrai o resumo do filtro `ebur128` do stderr do ffmpeg.
pub fn parse_ebur128_summary(stderr: &str) -> Option<Loudness> {
    let idx = stderr.rfind("Summary:")?;
    let summary = &stderr[idx..];
    let value_after = |key: &str| -> Option<f64> {
        summary.lines().find_map(|line| {
            let t = line.trim();
            let rest = t.strip_prefix(key)?;
            rest.split_whitespace().next()?.parse::<f64>().ok()
        })
    };
    let integrated = value_after("I:")?;
    let lra = value_after("LRA:").unwrap_or(0.0);
    let tp = value_after("Peak:");
    Some(Loudness {
        integrated_lufs: integrated,
        lra_lu: lra,
        true_peak_dbtp: tp,
    })
}

/// Analisa uma faixa de áudio (arquivo elementar ou container; `stream_index` = índice entre as faixas de áudio).
pub fn analyze_audio_file<F: FnMut(f32)>(
    cfg: &AppConfig,
    path: &Path,
    stream_index: usize,
    cancel: &CancelHandle,
    mut on_progress: F,
) -> Result<AudioAnalysis> {
    if !path.is_file() {
        bail!("Arquivo de áudio não encontrado: {}", path.display());
    }
    let ffmpeg = cfg.ffmpeg_path.trim();
    if ffmpeg.is_empty() {
        bail!("ffmpeg não configurado nas Configurações.");
    }
    let probe = probe_stream(cfg, path, stream_index, cancel)?;
    let channels = probe.channels.max(1);
    // O ffmpeg entrega o PCM na taxa nativa (sem -ar): usar exatamente essa taxa.
    let sample_rate = probe.sample_rate;
    if sample_rate == 0 {
        bail!("Taxa de amostragem inválida/ausente em {}", path.display());
    }
    let (codec_lbl, lossless) = codec_label(&probe.codec_name, &probe.profile);

    let mut cmd = Command::new(ffmpeg);
    configure_no_window(&mut cmd);
    cmd.args(["-hide_banner", "-nostdin", "-nostats", "-v", "info"]);
    if probe.codec_name == "ac3" || probe.codec_name == "eac3" {
        // Mede o sinal como gravado, sem a compressão de dinâmica (DRC) aplicada pelo decoder.
        cmd.args(["-drc_scale", "0"]);
    }
    cmd.arg("-i")
        .arg(path)
        .args([
            "-filter_complex",
            &format!(
                "[0:a:{stream_index}]asplit=2[meter][pcm];[meter]ebur128=framelog=quiet,anullsink"
            ),
            "-map",
            "[pcm]",
            "-f",
            "f32le",
            "-c:a",
            "pcm_f32le",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Falha ao iniciar ffmpeg ({ffmpeg}) para análise de áudio"))?;
    // Permite que cancel() mate o ffmpeg mesmo com a thread bloqueada em stdout.read().
    let _pid_guard = cancel.track_pid(child.id());
    if cancel.is_cancelled() {
        let _ = child.kill();
        let _ = child.wait();
        bail!("Tarefa cancelada pelo usuário.");
    }
    let mut stdout = child
        .stdout
        .take()
        .context("Falha ao abrir stdout do ffmpeg")?;
    let mut stderr_pipe = child
        .stderr
        .take()
        .context("Falha ao abrir stderr do ffmpeg")?;
    let stderr_thread = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr_pipe.read_to_string(&mut s);
        s
    });

    let roles: Vec<ChannelRole> = channel_map(&probe.channel_layout, channels)
        .into_iter()
        .map(|(_, r)| r)
        .collect();
    let mut acc = Accumulator::new(channels, sample_rate, &roles);
    let expected_frames = probe
        .duration_secs
        .map(|d| (d * sample_rate as f64).max(1.0))
        .unwrap_or(0.0);

    let frame_bytes = channels * 4;
    let mut read_buf = vec![0u8; 8192 * frame_bytes];
    let mut leftover: Vec<u8> = Vec::new();
    let mut frame = vec![0f32; channels];
    let mut last_progress = 0.0f32;

    loop {
        if cancel.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Tarefa cancelada pelo usuário.");
        }
        let n = stdout.read(&mut read_buf)?;
        if n == 0 {
            break;
        }
        leftover.extend_from_slice(&read_buf[..n]);
        let complete = leftover.len() / frame_bytes;
        for f in 0..complete {
            let base = f * frame_bytes;
            for (ch, slot) in frame.iter_mut().enumerate() {
                let o = base + ch * 4;
                *slot = f32::from_le_bytes([
                    leftover[o],
                    leftover[o + 1],
                    leftover[o + 2],
                    leftover[o + 3],
                ]);
            }
            acc.push(&frame);
        }
        leftover.drain(..complete * frame_bytes);

        if expected_frames > 0.0 {
            let p = (acc.frames as f64 / expected_frames).min(1.0) as f32;
            if p - last_progress >= 0.01 {
                last_progress = p;
                on_progress(p);
            }
        }
    }

    let status = child.wait()?;
    let stderr = stderr_thread.join().unwrap_or_default();
    if cancel.is_cancelled() {
        bail!("Tarefa cancelada pelo usuário.");
    }
    // Um resultado parcial pareceria completo (duração errada, sem loudness): nunca aceitar.
    if !status.success() {
        let tail: String = stderr.lines().rev().take(4).collect::<Vec<_>>().join(" | ");
        bail!("ffmpeg falhou ao decodificar o áudio ({status}): {tail}");
    }
    // TrueHD/DTS-HD MA elementares são VBR: o ffprobe estima a duração pelo bitrate.
    let duration_reliable = !(probe.codec_name == "truehd"
        || probe.codec_name == "mlp"
        || (probe.codec_name == "dts" && lossless));
    if duration_reliable
        && expected_frames > 0.0
        && (acc.frames as f64) < expected_frames * MIN_DECODED_FRACTION
    {
        bail!(
            "Decodificação incompleta: {} de {} decodificados.",
            fmt_duration(acc.frames as f64 / sample_rate as f64),
            fmt_duration(expected_frames / sample_rate as f64)
        );
    }
    on_progress(1.0);

    let names: Vec<String> = channel_map(&probe.channel_layout, channels)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    let file_name = path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    Ok(acc.finish(AnalysisHeader {
        file_name,
        codec_label: codec_lbl,
        lossless,
        sample_rate,
        layout_label: layout_label(&probe.channel_layout, channels),
        dialnorm_db: read_dialnorm(path, &probe.codec_name),
        loudness: parse_ebur128_summary(&stderr),
        // true peak calculado no acumulador (ver TP_COEFFS)
        names,
        roles,
    }))
}

struct AnalysisHeader {
    file_name: String,
    codec_label: String,
    lossless: bool,
    sample_rate: u32,
    layout_label: String,
    dialnorm_db: Option<i32>,
    loudness: Option<Loudness>,
    names: Vec<String>,
    roles: Vec<ChannelRole>,
}

// ---------------------------------------------------------------------------
// Acumulador em streaming (independente do ffmpeg para ser testável)
// ---------------------------------------------------------------------------

struct Accumulator {
    channels: usize,
    sample_rate: u32,
    frames: u64,
    sum_sq: Vec<f64>,
    peak: Vec<f32>,
    /// Somatórios cruzados (triângulo superior, i < j).
    cross: Vec<f64>,
    clipped_samples: u64,
    clip_events: u64,
    clip_run: Vec<u32>,
    // True peak: histórico duplicado (24) para janela contígua de 12 amostras por canal
    tp_hist: Vec<[f32; 24]>,
    tp_pos: usize,
    tp_max: f32,
    // Silêncio digital em blocos de 100 ms
    block_frames: u64,
    block_pos: u64,
    block_max: f32,
    silent_run_start: Option<u64>,
    silences: Vec<(u64, u64)>,
    // Espectro médio por canal (exceto LFE)
    spectral: Vec<bool>,
    hop: u64,
    capture: Option<usize>,
    window: Vec<Vec<f32>>,
    power: Vec<Vec<f64>>,
    windows_done: u64,
    hann: Vec<f32>,
    fft: std::sync::Arc<dyn rustfft::Fft<f32>>,
    fft_buf: Vec<Complex<f32>>,
}

impl Accumulator {
    fn new(channels: usize, sample_rate: u32, roles: &[ChannelRole]) -> Self {
        let hann: Vec<f32> = (0..FFT_N)
            .map(|i| {
                let x = (std::f32::consts::PI * 2.0 * i as f32) / (FFT_N as f32 - 1.0);
                0.5 * (1.0 - x.cos())
            })
            .collect();
        let fft = FftPlanner::<f32>::new().plan_fft_forward(FFT_N);
        Self {
            channels,
            sample_rate,
            frames: 0,
            sum_sq: vec![0.0; channels],
            peak: vec![0.0; channels],
            cross: vec![0.0; channels * channels],
            clipped_samples: 0,
            clip_events: 0,
            clip_run: vec![0; channels],
            tp_hist: vec![[0.0; 24]; channels],
            tp_pos: 0,
            tp_max: 0.0,
            block_frames: (sample_rate as u64 / 10).max(1),
            block_pos: 0,
            block_max: 0.0,
            silent_run_start: None,
            silences: Vec::new(),
            spectral: (0..channels)
                .map(|i| roles.get(i).copied() != Some(ChannelRole::Lfe))
                .collect(),
            // ~4 janelas por segundo: espectro estável sem custo relevante.
            hop: (sample_rate as u64 / 4).max(FFT_N as u64),
            capture: None,
            window: vec![vec![0.0; FFT_N]; channels],
            power: vec![vec![0.0; FFT_N / 2 + 1]; channels],
            windows_done: 0,
            hann,
            fft,
            fft_buf: vec![Complex::new(0.0, 0.0); FFT_N],
        }
    }

    fn push(&mut self, frame: &[f32]) {
        let c = self.channels;
        let mut fmax = 0f32;
        for i in 0..c {
            let x = frame[i];
            let ax = x.abs();
            let xd = x as f64;
            self.sum_sq[i] += xd * xd;
            if ax > self.peak[i] {
                self.peak[i] = ax;
            }
            if ax > fmax {
                fmax = ax;
            }
            if ax >= CLIP_AMP {
                self.clipped_samples += 1;
                self.clip_run[i] += 1;
                if self.clip_run[i] == CLIP_RUN {
                    self.clip_events += 1;
                }
            } else {
                self.clip_run[i] = 0;
            }
            for j in (i + 1)..c {
                self.cross[i * c + j] += xd * frame[j] as f64;
            }
            self.true_peak_step(i, x);
        }
        self.tp_pos = (self.tp_pos + 1) % 12;

        // Silêncio digital
        if fmax > self.block_max {
            self.block_max = fmax;
        }
        self.block_pos += 1;
        if self.block_pos == self.block_frames {
            let block_start = self.frames + 1 - self.block_frames;
            if self.block_max < DIGITAL_SILENCE_AMP {
                if self.silent_run_start.is_none() {
                    self.silent_run_start = Some(block_start);
                }
            } else if let Some(s) = self.silent_run_start.take() {
                self.silences.push((s, block_start));
            }
            self.block_pos = 0;
            self.block_max = 0.0;
        }

        // Janelas para o espectro médio
        if self.capture.is_none() && self.frames % self.hop == 0 {
            self.capture = Some(0);
        }
        if let Some(pos) = self.capture {
            for (ch, w) in self.window.iter_mut().enumerate() {
                w[pos] = frame[ch];
            }
            if pos + 1 == FFT_N {
                self.flush_window();
                self.capture = None;
            } else {
                self.capture = Some(pos + 1);
            }
        }

        self.frames += 1;
    }

    /// Atualiza o true peak (oversampling 4x BS.1770) do canal `ch` com a nova amostra `x`.
    fn true_peak_step(&mut self, ch: usize, x: f32) {
        let ax = x.abs();
        if ax > self.tp_max {
            self.tp_max = ax;
        }
        let pos = self.tp_pos;
        let h = &mut self.tp_hist[ch];
        h[pos] = x;
        h[pos + 12] = x;
        // Janela contígua do mais antigo ao mais novo
        let w = &h[pos + 1..pos + 13];
        let gate = self.tp_max * TRUE_PEAK_GATE;
        if w[5].abs() < gate && w[6].abs() < gate {
            return;
        }
        for phase in &TP_COEFFS {
            let mut y = 0f32;
            for k in 0..12 {
                y += phase[k] * w[11 - k];
            }
            let ay = y.abs();
            if ay > self.tp_max {
                self.tp_max = ay;
            }
        }
    }

    fn flush_window(&mut self) {
        for ch in 0..self.channels {
            if !self.spectral[ch] {
                continue;
            }
            for i in 0..FFT_N {
                self.fft_buf[i] = Complex::new(self.window[ch][i] * self.hann[i], 0.0);
            }
            self.fft.process(&mut self.fft_buf);
            for (b, p) in self.power[ch].iter_mut().enumerate() {
                *p += self.fft_buf[b].norm_sqr() as f64;
            }
        }
        self.windows_done += 1;
    }

    fn finish(mut self, h: AnalysisHeader) -> AudioAnalysis {
        if let Some(s) = self.silent_run_start.take() {
            self.silences.push((s, self.frames));
        }
        let sr = self.sample_rate as f64;
        let n = self.frames.max(1) as f64;
        let duration_secs = self.frames as f64 / sr;
        let to_db = |p: f64| if p > 1e-20 { 10.0 * p.log10() } else { -200.0 };

        let channels: Vec<ChannelStats> = (0..self.channels)
            .map(|i| ChannelStats {
                name: h
                    .names
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| format!("Ch{}", i + 1)),
                role: h.roles.get(i).copied().unwrap_or(ChannelRole::Other),
                rms_db: to_db(self.sum_sq[i] / n),
                peak_db: if self.peak[i] > 0.0 {
                    20.0 * (self.peak[i] as f64).log10()
                } else {
                    -200.0
                },
                bandwidth: if self.spectral[i] && self.windows_done > 0 {
                    estimate_bandwidth(&self.power[i], self.sample_rate)
                } else {
                    None
                },
            })
            .collect();

        let c = self.channels;
        let mut corr = vec![vec![0.0; c]; c];
        for i in 0..c {
            corr[i][i] = 1.0;
            for j in (i + 1)..c {
                let d = (self.sum_sq[i] * self.sum_sq[j]).sqrt();
                let v = if d > 1e-12 {
                    self.cross[i * c + j] / d
                } else {
                    0.0
                };
                corr[i][j] = v;
                corr[j][i] = v;
            }
        }

        // Esvazia o FIR do true peak: as últimas amostras ainda não passaram pelos taps centrais.
        // Só o filtro recebe os zeros; duração e demais medições já foram fechadas acima.
        for _ in 0..12 {
            for ch in 0..self.channels {
                self.true_peak_step(ch, 0.0);
            }
            self.tp_pos = (self.tp_pos + 1) % 12;
        }
        let true_peak_db = (self.tp_max > 0.0).then(|| 20.0 * (self.tp_max as f64).log10());
        let loudness = h.loudness.map(|l| Loudness {
            true_peak_dbtp: true_peak_db,
            ..l
        });

        let silences = self
            .silences
            .iter()
            .map(|&(s, e)| SilenceGap {
                start_secs: s as f64 / sr,
                duration_secs: (e - s) as f64 / sr,
            })
            .filter(|g| {
                g.duration_secs >= MIN_INTERNAL_SILENCE_SECS
                    && g.start_secs > EDGE_MARGIN_SECS
                    && g.start_secs + g.duration_secs < duration_secs - EDGE_MARGIN_SECS
            })
            .collect();

        AudioAnalysis {
            file_name: h.file_name,
            codec_label: h.codec_label,
            lossless: h.lossless,
            sample_rate: h.sample_rate,
            layout_label: h.layout_label,
            duration_secs,
            dialnorm_db: h.dialnorm_db,
            loudness,
            channels,
            corr,
            clipped_samples: self.clipped_samples,
            clip_events: self.clip_events,
            silences,
        }
    }
}

/// Frequência mais alta com conteúdo relevante no espectro médio de um canal.
///
/// Agrupa em faixas de 250 Hz, usa como referência a faixa mais forte entre 200 Hz e 6 kHz
/// e procura, de cima para baixo, a primeira faixa a menos de 70 dB da referência.
/// "Abrupto" = queda de 25 dB+ nas 2 faixas seguintes (parede de filtro de codec).
pub fn estimate_bandwidth(power: &[f64], sample_rate: u32) -> Option<Bandwidth> {
    if power.len() < 4 {
        return None;
    }
    let bin_hz = sample_rate as f64 / ((power.len() - 1) * 2) as f64;
    let per_band = ((BAND_HZ / bin_hz).round() as usize).max(1);
    let bands: Vec<f64> = power
        .chunks(per_band)
        .map(|c| {
            let m = c.iter().sum::<f64>() / c.len() as f64;
            if m > 1e-30 {
                10.0 * m.log10()
            } else {
                -300.0
            }
        })
        .collect();
    let band_center = |i: usize| (i as f64 + 0.5) * per_band as f64 * bin_hz;

    let reference = bands
        .iter()
        .enumerate()
        .filter(|(i, _)| (200.0..=6000.0).contains(&band_center(*i)))
        .map(|(_, &v)| v)
        .fold(f64::NEG_INFINITY, f64::max);
    if !reference.is_finite() || reference < -250.0 {
        return None;
    }
    let threshold = reference - 70.0;
    // Ignora a última faixa (borda de Nyquist / filtro anti-alias).
    let top = bands.len().saturating_sub(2);
    let cut = (0..=top).rev().find(|&i| bands[i] > threshold)?;
    let above: Vec<f64> = bands[(cut + 1).min(bands.len())..(cut + 3).min(bands.len())].to_vec();
    let abrupt = !above.is_empty() && above.iter().all(|&v| bands[cut] - v >= 25.0) && cut < top;
    Some(Bandwidth {
        cutoff_hz: (cut as f64 + 1.0) * per_band as f64 * bin_hz,
        abrupt,
    })
}

// ---------------------------------------------------------------------------
// Diagnóstico
// ---------------------------------------------------------------------------

fn fmt_db(v: f64) -> String {
    if v <= -150.0 {
        "−∞".into()
    } else {
        format!("{v:.1}")
    }
}

pub fn fmt_duration(secs: f64) -> String {
    let total = secs.max(0.0);
    let h = (total / 3600.0).floor() as u64;
    let m = ((total % 3600.0) / 60.0).floor() as u64;
    let s = total % 60.0;
    if h > 0 {
        format!("{h}:{m:02}:{s:04.1}")
    } else {
        format!("{m:02}:{s:04.1}")
    }
}

fn fmt_ts(secs: f64) -> String {
    let t = secs.max(0.0) as u64;
    format!("{}:{:02}:{:02}", t / 3600, (t % 3600) / 60, t % 60)
}

fn fmt_khz(hz: f64) -> String {
    format!("{:.1} kHz", hz / 1000.0)
}

struct ChannelVerdict {
    summary: String,
    findings: Vec<Finding>,
}

fn channel_verdict(a: &AudioAnalysis) -> ChannelVerdict {
    let mut findings = Vec::new();
    let idx = |role: ChannelRole| a.channels.iter().position(|c| c.role == role);
    let silent = |i: usize| a.channels[i].rms_db < SILENT_CHANNEL_DB;
    let corr = |i: usize, j: usize| a.corr[i][j];

    let layout = a.layout_label.clone();
    if a.channels.len() == 1 {
        return ChannelVerdict {
            summary: "Mono".into(),
            findings,
        };
    }

    let (l, r, c, lfe) = (
        idx(ChannelRole::Left),
        idx(ChannelRole::Right),
        idx(ChannelRole::Center),
        idx(ChannelRole::Lfe),
    );
    let lr_mono = match (l, r) {
        (Some(l), Some(r)) => {
            !silent(l)
                && corr(l, r) > SAME_SIGNAL_CORR
                && (a.channels[l].rms_db - a.channels[r].rms_db).abs() < 1.0
        }
        _ => false,
    };

    if a.channels.len() == 2 {
        if lr_mono {
            findings.push(Finding::note(format!(
                "Estéreo com L e R idênticos (correlação {:.2}): o conteúdo é mono.",
                corr(l.unwrap(), r.unwrap())
            )));
            return ChannelVerdict {
                summary: "Mono duplicado (L = R)".into(),
                findings,
            };
        }
        return ChannelVerdict {
            summary: "Estéreo".into(),
            findings,
        };
    }

    let mut tag: Option<&str> = None;
    let surrounds: Vec<usize> = (0..a.channels.len())
        .filter(|&i| a.channels[i].role.is_surround())
        .collect();
    let active_surrounds: Vec<usize> = surrounds.iter().copied().filter(|&i| !silent(i)).collect();

    // L, C e R com o mesmo sinal
    if let (Some(l), Some(r), Some(c)) = (l, r, c) {
        if lr_mono && !silent(c) && corr(l, c) > SAME_SIGNAL_CORR && corr(r, c) > SAME_SIGNAL_CORR {
            findings.push(Finding::alert(
                "L, C e R carregam o mesmo sinal (mono espalhado nos frontais): provável upmix de fonte mono.",
            ));
            tag = Some("mono espalhado");
        } else if lr_mono {
            findings.push(Finding::note("Frontais L e R idênticos (mono)."));
        }
        if silent(c) {
            findings.push(Finding::note("Canal central sem sinal."));
        }
    }

    if !surrounds.is_empty() {
        if active_surrounds.is_empty() {
            let names: Vec<&str> = surrounds
                .iter()
                .map(|&i| a.channels[i].name.as_str())
                .collect();
            findings.push(Finding::alert(format!(
                "Surrounds ({}) sem sinal: o conteúdo é só frontal dentro de um container {layout}.",
                names.join("/")
            )));
            tag = tag.or(Some("surrounds vazios"));
        } else {
            for &i in &surrounds {
                if silent(i) {
                    findings.push(Finding::note(format!(
                        "Canal {} sem sinal.",
                        a.channels[i].name
                    )));
                }
            }
            // Surround copiando o frontal do mesmo lado
            let pair = |s: ChannelRole, f: Option<usize>| -> Option<f64> {
                let si = idx(s)?;
                let fi = f?;
                (!silent(si)).then(|| corr(si, fi))
            };
            let side_corrs: Vec<f64> = [
                pair(ChannelRole::SurroundLeft, l),
                pair(ChannelRole::SurroundRight, r),
                pair(ChannelRole::BackLeft, l),
                pair(ChannelRole::BackRight, r),
            ]
            .into_iter()
            .flatten()
            .collect();
            if !side_corrs.is_empty() && side_corrs.iter().all(|&v| v > UPMIX_CORR) {
                let min = side_corrs.iter().cloned().fold(f64::INFINITY, f64::min);
                findings.push(Finding::alert(format!(
                    "Surrounds replicam os frontais (correlação ≥ {min:.2}): provável upmix."
                )));
                tag = tag.or(Some("provável upmix"));
            } else if active_surrounds.len() >= 2 {
                let (s1, s2) = (active_surrounds[0], active_surrounds[1]);
                if corr(s1, s2) > SAME_SIGNAL_CORR {
                    let bw = a.channels[s1].bandwidth.map(|b| b.cutoff_hz);
                    let limited = bw.filter(|&hz| hz < 9_000.0);
                    let extra = limited
                        .map(|hz| format!(", limitado a ~{}", fmt_khz(hz)))
                        .unwrap_or_default();
                    findings.push(Finding::note(format!(
                        "Surround mono ({} = {}{extra}), típico de matriz Dolby Surround / Pro Logic.",
                        a.channels[s1].name, a.channels[s2].name
                    )));
                    tag = tag.or(Some("surround matricial"));
                }
            }
        }
    }

    if let Some(lfe) = lfe {
        if silent(lfe) {
            findings.push(Finding::note("LFE sem sinal."));
        }
    }

    let summary = match tag {
        Some(t) => format!("{layout} ({t})"),
        None => format!("{layout} discreto"),
    };
    ChannelVerdict { summary, findings }
}

fn front_bandwidth(a: &AudioAnalysis) -> Option<Bandwidth> {
    let fronts: Vec<Bandwidth> = a
        .channels
        .iter()
        .filter(|c| {
            matches!(
                c.role,
                ChannelRole::Left | ChannelRole::Right | ChannelRole::Center
            ) || a.channels.len() <= 2
        })
        .filter(|c| c.rms_db >= SILENT_CHANNEL_DB)
        .filter_map(|c| c.bandwidth)
        .collect();
    let best = fronts
        .iter()
        .copied()
        .fold(None::<Bandwidth>, |acc, b| match acc {
            Some(x) if x.cutoff_hz >= b.cutoff_hz => Some(x),
            _ => Some(b),
        })?;
    Some(best)
}

/// Todos os avisos/notas do arquivo, alertas primeiro.
pub fn findings(a: &AudioAnalysis, ctx: &ReportContext) -> Vec<Finding> {
    let mut out = channel_verdict(a).findings;

    if let Some(reference) = ctx.reference_duration_secs {
        let delta = a.duration_secs - reference;
        if delta.abs() >= DURATION_ALERT_SECS {
            let dir = if delta < 0.0 {
                "mais curto"
            } else {
                "mais longo"
            };
            out.push(Finding::alert(format!(
                "Áudio {:.1} s {dir} que o vídeo de referência ({} × {}).",
                delta.abs(),
                fmt_duration(a.duration_secs),
                fmt_duration(reference)
            )));
        }
    }

    if !a.silences.is_empty() {
        let list: Vec<String> = a
            .silences
            .iter()
            .take(5)
            .map(|g| format!("{} ({:.1} s)", fmt_ts(g.start_secs), g.duration_secs))
            .collect();
        let more = a.silences.len().saturating_sub(5);
        let more = if more > 0 {
            format!(" e mais {more}")
        } else {
            String::new()
        };
        out.push(Finding::alert(format!(
            "Silêncio digital em {}{more}: verificar se há trecho sem áudio.",
            list.join(", ")
        )));
    }

    if a.clip_events >= CLIP_EVENTS_ALERT {
        out.push(Finding::alert(format!(
            "Clipping: {} trechos com amostras no teto (0 dBFS).",
            a.clip_events
        )));
    } else if a.clip_events > 0 {
        out.push(Finding::note(format!(
            "Clipping pontual: {} trecho(s) com amostras no teto (0 dBFS).",
            a.clip_events
        )));
    }

    if let Some(bw) = front_bandwidth(a) {
        if a.lossless && bw.abrupt && bw.cutoff_hz < LOSSY_ORIGIN_MAX_HZ {
            out.push(Finding::alert(format!(
                "Corte abrupto em ~{} num codec lossless ({}): indício de origem lossy (transcode).",
                fmt_khz(bw.cutoff_hz),
                a.codec_label
            )));
        }
    }

    out.sort_by(|x, y| y.severity.cmp(&x.severity));
    out
}

// ---------------------------------------------------------------------------
// BBCode
// ---------------------------------------------------------------------------

pub const REPORT_TITLE: &str = "Análise de Áudio";

fn loudness_cell(a: &AudioAnalysis) -> String {
    let mut parts = Vec::new();
    if let Some(l) = a.loudness {
        parts.push(format!("{:.1} LUFS", l.integrated_lufs));
        parts.push(format!("LRA {:.1} LU", l.lra_lu));
        if let Some(tp) = l.true_peak_dbtp {
            parts.push(format!("True Peak {tp:.1} dBTP"));
        }
    }
    if let Some(d) = a.dialnorm_db {
        parts.push(format!("Dialnorm {d} dB"));
    }
    if parts.is_empty() {
        "—".into()
    } else {
        parts.join(" · ")
    }
}

fn bandwidth_cell(a: &AudioAnalysis) -> String {
    match front_bandwidth(a) {
        Some(b) if b.abrupt => format!("~{} (corte abrupto)", fmt_khz(b.cutoff_hz)),
        Some(b) => format!("~{}", fmt_khz(b.cutoff_hz)),
        None => "—".into(),
    }
}

fn clipping_cell(a: &AudioAnalysis) -> String {
    if a.clip_events == 0 {
        "nenhum".into()
    } else {
        format!("{} trecho(s)", a.clip_events)
    }
}

fn duration_cell(a: &AudioAnalysis, ctx: &ReportContext) -> String {
    let mut s = fmt_duration(a.duration_secs);
    if let Some(r) = ctx.reference_duration_secs {
        s.push_str(&format!(
            " · vídeo {} (Δ {:+.1} s)",
            fmt_duration(r),
            a.duration_secs - r
        ));
    }
    if let Some(fps) = ctx.fps.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        s.push_str(&format!(" · {fps} fps"));
    }
    s
}

fn row(k: &str, v: &str) -> String {
    format!("[tr][td][b]{k}[/b][/td][td]{v}[/td][/tr]\n")
}

fn channel_table(a: &AudioAnalysis) -> String {
    let mut t = String::from(
        "[table]\n[tr][th]Canal[/th][th]RMS (dBFS)[/th][th]Pico (dBFS)[/th][th]Banda[/th][/tr]\n",
    );
    for c in &a.channels {
        let bw = if c.role == ChannelRole::Lfe || c.rms_db < SILENT_CHANNEL_DB {
            "—".to_string()
        } else {
            c.bandwidth
                .map(|b| format!("~{}", fmt_khz(b.cutoff_hz)))
                .unwrap_or_else(|| "—".into())
        };
        t.push_str(&format!(
            "[tr][td]{}[/td][td]{}[/td][td]{}[/td][td]{}[/td][/tr]\n",
            c.name,
            fmt_db(c.rms_db),
            fmt_db(c.peak_db),
            bw
        ));
    }
    t.push_str("[/table]");
    t
}

fn findings_bbcode(list: &[Finding], prefix: Option<&str>) -> String {
    let fmt = |f: &Finding| match prefix {
        Some(p) => format!("[b]{p}:[/b] {}", f.text),
        None => f.text.clone(),
    };
    let alerts: Vec<String> = list
        .iter()
        .filter(|f| f.severity == Severity::Alert)
        .map(fmt)
        .collect();
    let notes: Vec<String> = list
        .iter()
        .filter(|f| f.severity == Severity::Note)
        .map(fmt)
        .collect();
    let mut s = String::new();
    if !alerts.is_empty() {
        s.push_str(&format!("[alert]{}[/alert]\n", alerts.join("\n")));
    }
    if !notes.is_empty() {
        s.push_str(&format!("[note]{}[/note]\n", notes.join("\n")));
    }
    s
}

/// Relatório completo de um único arquivo (filmes).
pub fn render_single(a: &AudioAnalysis, ctx: &ReportContext) -> String {
    let verdict = channel_verdict(a);
    let fmt_line = format!(
        "{} · {} · {} kHz{}",
        a.codec_label,
        a.layout_label,
        a.sample_rate as f64 / 1000.0,
        if a.lossless { " · lossless" } else { "" }
    );
    let mut s = format!("[b]{REPORT_TITLE}[/b]\n[table]\n");
    s.push_str(&row("Formato", &fmt_line));
    s.push_str(&row("Duração", &duration_cell(a, ctx)));
    s.push_str(&row("Loudness", &loudness_cell(a)));
    s.push_str(&row("Canais", &verdict.summary));
    s.push_str(&row("Banda", &bandwidth_cell(a)));
    s.push_str(&row("Clipping", &clipping_cell(a)));
    s.push_str("[/table]\n");
    s.push_str(&findings_bbcode(&findings(a, ctx), None));
    if a.channels.len() > 1 {
        s.push_str(&format!(
            "[spoiler=Níveis por canal]\n{}\n[/spoiler]\n",
            channel_table(a)
        ));
    }
    s.trim_end().to_string()
}

/// Relatório compacto de vários arquivos (temporadas): uma linha por arquivo.
fn format_summary(a: &AudioAnalysis) -> String {
    format!(
        "{} · {} · {} kHz",
        a.codec_label,
        a.layout_label,
        a.sample_rate as f64 / 1000.0
    )
}

pub fn render_multi(items: &[(AudioAnalysis, ReportContext)]) -> String {
    // Temporada uniforme: formato no cabeçalho. Mista: coluna "Formato" por arquivo.
    let uniform = items
        .iter()
        .all(|(a, _)| format_summary(a) == format_summary(&items[0].0));
    let mut table = format!(
        "[table]\n[tr][th]Arquivo[/th]{}[th]Duração[/th][th]Loudness[/th][th]LRA[/th][th]True Peak[/th][th]Canais[/th][th]Banda[/th][th]Clipping[/th][/tr]\n",
        if uniform { "" } else { "[th]Formato[/th]" }
    );
    let mut all_findings = String::new();
    for (a, ctx) in items {
        let label = ctx.label.clone().unwrap_or_else(|| a.file_name.clone());
        let l = a.loudness;
        let dur = match ctx.reference_duration_secs {
            Some(r) => format!(
                "{} (Δ {:+.1} s)",
                fmt_duration(a.duration_secs),
                a.duration_secs - r
            ),
            None => fmt_duration(a.duration_secs),
        };
        let fmt_col = if uniform {
            String::new()
        } else {
            format!("[td]{}[/td]", format_summary(a))
        };
        table.push_str(&format!(
            "[tr][td]{label}[/td]{fmt_col}[td]{dur}[/td][td]{}[/td][td]{}[/td][td]{}[/td][td]{}[/td][td]{}[/td][td]{}[/td][/tr]\n",
            l.map(|l| format!("{:.1} LUFS", l.integrated_lufs)).unwrap_or_else(|| "—".into()),
            l.map(|l| format!("{:.1} LU", l.lra_lu)).unwrap_or_else(|| "—".into()),
            l.and_then(|l| l.true_peak_dbtp).map(|v| format!("{v:.1} dBTP")).unwrap_or_else(|| "—".into()),
            channel_verdict(a).summary,
            bandwidth_cell(a),
            clipping_cell(a),
        ));
        all_findings.push_str(&findings_bbcode(&findings(a, ctx), Some(&label)));
    }
    table.push_str("[/table]");

    let mut header = if uniform {
        format_summary(&items[0].0)
    } else {
        "formatos variados".to_string()
    };
    if let Some(fps) = items[0]
        .1
        .fps
        .as_deref()
        .map(str::trim)
        .filter(|f| !f.is_empty())
    {
        header.push_str(&format!(" · {fps} fps"));
    }
    let mut s = format!(
        "[b]{REPORT_TITLE}[/b] ({} arquivos · {header})\n",
        items.len()
    );
    s.push_str(&merge_findings(&all_findings));
    s.push_str(&format!(
        "[spoiler=Detalhes por arquivo]\n{table}\n[/spoiler]"
    ));
    s
}

/// Junta os vários blocos [alert]/[note] de uma temporada em no máximo um de cada.
fn merge_findings(blocks: &str) -> String {
    let collect = |tag: &str| -> Vec<String> {
        let open = format!("[{tag}]");
        let close = format!("[/{tag}]");
        let mut out = Vec::new();
        let mut rest = blocks;
        while let Some(i) = rest.find(&open) {
            let after = &rest[i + open.len()..];
            let Some(j) = after.find(&close) else { break };
            out.push(after[..j].to_string());
            rest = &after[j + close.len()..];
        }
        out
    };
    let mut s = String::new();
    let alerts = collect("alert");
    if !alerts.is_empty() {
        s.push_str(&format!("[alert]{}[/alert]\n", alerts.join("\n")));
    }
    let notes = collect("note");
    if !notes.is_empty() {
        s.push_str(&format!("[note]{}[/note]\n", notes.join("\n")));
    }
    s
}

pub fn render_report(items: &[(AudioAnalysis, ReportContext)]) -> Option<String> {
    match items.len() {
        0 => None,
        1 => Some(render_single(&items[0].0, &items[0].1)),
        _ => Some(render_multi(items)),
    }
}

// ---------------------------------------------------------------------------
// Integração com o pacote gerado
// ---------------------------------------------------------------------------

fn episode_label(path: &Path) -> Option<String> {
    let name = path.file_stem()?.to_string_lossy().to_string();
    let re = regex::Regex::new(r"(?i)S\d{1,2}E\d{1,3}(?:-?E\d{1,3})?").ok()?;
    // O sufixo gerado no empacotamento fica no fim ("Série S01E01 - S01E02"): usa a última ocorrência.
    re.find_iter(&name)
        .last()
        .map(|m| m.as_str().to_ascii_uppercase())
}

/// Arquivos de áudio do pacote, na ordem em que foram criados (sem repetições: se dois
/// vídeos gravaram no mesmo destino, o arquivo é analisado uma vez e sem referência de duração).
pub fn package_audio_files(pkg: &PackageOutput) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for p in &pkg.created_files {
        let is_audio = p
            .extension()
            .and_then(|e| e.to_str())
            .and_then(AudioCodec::from_extension)
            .is_some();
        if is_audio && !out.contains(p) {
            out.push(p.clone());
        }
    }
    out
}

/// Analisa todos os áudios do pacote e devolve o BBCode do relatório.
/// `reference_durations[i]` é a duração do vídeo correspondente ao i-ésimo áudio, se conhecida.
pub fn build_package_report<FLog: FnMut(String)>(
    cfg: &AppConfig,
    pkg: &PackageOutput,
    fps: &str,
    reference_durations: &[Option<f64>],
    cancel: &CancelHandle,
    mut log: FLog,
) -> Result<Option<String>> {
    let files = package_audio_files(pkg);
    let mut items = Vec::new();
    for (i, path) in files.iter().enumerate() {
        log(format!(
            "Analisando áudio [{}/{}]: {}",
            i + 1,
            files.len(),
            path.file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default()
        ));
        let analysis = analyze_audio_file(cfg, path, 0, cancel, |_| {})?;
        let ctx = ReportContext {
            label: if files.len() > 1 {
                episode_label(path)
            } else {
                None
            },
            reference_duration_secs: if reference_durations.len() == files.len() {
                reference_durations[i]
            } else {
                None
            },
            fps: Some(fps.to_string()),
        };
        items.push((analysis, ctx));
    }
    Ok(render_report(&items))
}

/// Durações dos vídeos de referência, na mesma ordem dos áudios extraídos.
pub fn reference_durations(
    cfg: &AppConfig,
    videos: &[PathBuf],
    cancel: &CancelHandle,
) -> Vec<Option<f64>> {
    videos
        .iter()
        .map(|v| probe_duration_secs(cfg, v, cancel))
        .collect()
}

/// Roda a análise sobre o pacote e grava o relatório em `pkg.audio_report`.
/// Falhas da análise não invalidam o pacote (apenas geram aviso no log); cancelamento sim.
pub fn attach_package_report<FLog: FnMut(String)>(
    cfg: &AppConfig,
    pkg: &mut PackageOutput,
    fps: &str,
    reference_videos: &[PathBuf],
    cancel: &CancelHandle,
    mut log: FLog,
) -> Result<()> {
    if !cfg.audio_report {
        return Ok(());
    }
    let refs = reference_durations(cfg, reference_videos, cancel);
    match build_package_report(cfg, pkg, fps, &refs, cancel, &mut log) {
        Ok(_) if cancel.is_cancelled() => bail!("Tarefa cancelada pelo usuário."),
        Ok(report) => {
            if report.is_some() {
                log("Análise de áudio concluída e adicionada à descrição.".into());
            }
            pkg.audio_report = report;
            Ok(())
        }
        Err(_) if cancel.is_cancelled() => bail!("Tarefa cancelada pelo usuário."),
        Err(e) => {
            log(format!(
                "[AVISO] Análise de áudio falhou (envio segue sem o relatório): {e:#}"
            ));
            Ok(())
        }
    }
}

/// Localiza o relatório gerado na descrição: do título `[b]Análise de Áudio[/b]` até a
/// próxima linha em branco (o relatório nunca contém linhas em branco). Sobrevive a edições
/// dentro do bloco, ao contrário de procurar o texto original.
fn report_section(description: &str) -> Option<(usize, usize)> {
    let start = description.find(&format!("[b]{REPORT_TITLE}[/b]"))?;
    let rest = &description[start..];
    let end = regex::Regex::new(r"\r?\n[ \t]*\r?\n")
        .ok()?
        .find(rest)
        .map(|m| start + m.start())
        .unwrap_or(description.len());
    Some((start, end))
}

/// Substitui o relatório existente na descrição (mesmo editado) ou anexa ao final.
pub fn upsert_report(description: &mut String, report: &str) {
    if let Some((start, end)) = report_section(description) {
        description.replace_range(start..end, report);
        return;
    }
    if !description.trim().is_empty() {
        description.push_str("\n\n");
    }
    description.push_str(report);
}

/// Remove o relatório gerado (se houver), preservando o restante da descrição.
pub fn remove_report(description: &mut String) {
    let Some((start, end)) = report_section(description) else {
        return;
    };
    let before = description[..start].trim_end();
    let after = description[end..].trim_start();
    *description = match (before.is_empty(), after.is_empty()) {
        (true, _) => after.to_string(),
        (_, true) => before.to_string(),
        _ => format!("{before}\n\n{after}"),
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth(
        channels: usize,
        roles: &[ChannelRole],
        secs: f64,
        f: impl Fn(u64, usize) -> f32,
    ) -> AudioAnalysis {
        let sr = 48_000u32;
        let mut acc = Accumulator::new(channels, sr, roles);
        let total = (secs * sr as f64) as u64;
        let mut frame = vec![0f32; channels];
        for t in 0..total {
            for (ch, s) in frame.iter_mut().enumerate() {
                *s = f(t, ch);
            }
            acc.push(&frame);
        }
        let names = roles
            .iter()
            .enumerate()
            .map(|(i, _)| format!("Ch{i}"))
            .collect();
        acc.finish(AnalysisHeader {
            file_name: "t.ac3".into(),
            codec_label: "AC3".into(),
            lossless: false,
            sample_rate: sr,
            layout_label: format_channel_count(channels as u64),
            dialnorm_db: None,
            loudness: None,
            names,
            roles: roles.to_vec(),
        })
    }

    /// Ruído pseudoaleatório determinístico (LCG) por canal.
    fn noise(t: u64, seed: u64) -> f32 {
        let mut x = t
            .wrapping_mul(6364136223846793005)
            .wrapping_add(seed.wrapping_mul(1442695040888963407));
        x ^= x >> 33;
        ((x % 20001) as f32 / 10000.0 - 1.0) * 0.1
    }

    const R51: [ChannelRole; 6] = [
        ChannelRole::Left,
        ChannelRole::Right,
        ChannelRole::Center,
        ChannelRole::Lfe,
        ChannelRole::SurroundLeft,
        ChannelRole::SurroundRight,
    ];

    #[test]
    fn discrete_51_has_no_findings() {
        let a = synth(6, &R51, 20.0, |t, ch| noise(t, ch as u64 + 1));
        let v = channel_verdict(&a);
        assert_eq!(v.summary, "5.1 discreto");
        assert!(findings(&a, &ReportContext::default()).is_empty());
    }

    #[test]
    fn silent_surrounds_alert() {
        let a = synth(6, &R51, 20.0, |t, ch| {
            if ch >= 4 {
                0.0
            } else {
                noise(t, ch as u64 + 1)
            }
        });
        let f = findings(&a, &ReportContext::default());
        assert!(f
            .iter()
            .any(|x| x.severity == Severity::Alert && x.text.contains("sem sinal")));
        assert_eq!(channel_verdict(&a).summary, "5.1 (surrounds vazios)");
    }

    #[test]
    fn copied_surrounds_are_upmix() {
        let a = synth(6, &R51, 20.0, |t, ch| match ch {
            0 | 4 => noise(t, 1),
            1 | 5 => noise(t, 2),
            _ => noise(t, ch as u64 + 10),
        });
        assert_eq!(channel_verdict(&a).summary, "5.1 (provável upmix)");
    }

    #[test]
    fn dual_mono_stereo_is_note() {
        let a = synth(2, &[ChannelRole::Left, ChannelRole::Right], 10.0, |t, _| {
            noise(t, 7)
        });
        let v = channel_verdict(&a);
        assert_eq!(v.summary, "Mono duplicado (L = R)");
        assert_eq!(v.findings[0].severity, Severity::Note);
    }

    #[test]
    fn internal_digital_silence_detected_edges_ignored() {
        // 30 s: silêncio 0–2 s (borda, ignorado) e 12–16 s (interno, reportado)
        let a = synth(
            2,
            &[ChannelRole::Left, ChannelRole::Right],
            30.0,
            |t, ch| {
                let s = t as f64 / 48_000.0;
                if s < 2.0 || (12.0..16.0).contains(&s) {
                    0.0
                } else {
                    noise(t, ch as u64 + 3)
                }
            },
        );
        assert_eq!(a.silences.len(), 1);
        assert!((a.silences[0].start_secs - 12.0).abs() < 0.15);
        assert!((a.silences[0].duration_secs - 4.0).abs() < 0.25);
    }

    #[test]
    fn clipping_events_counted() {
        let a = synth(1, &[ChannelRole::Center], 2.0, |t, _| {
            if t % 10_000 < 5 {
                1.0
            } else {
                0.1
            }
        });
        assert_eq!(a.clip_events, 10);
    }

    #[test]
    fn bandwidth_detects_lowpass_wall() {
        // Soma de senos até 15 kHz: corte abrupto perto de 15 kHz
        let a = synth(1, &[ChannelRole::Center], 5.0, |t, _| {
            (1..=60u64)
                .map(|k| {
                    // fase exata em f64 (evita ruído de precisão do f32)
                    let cycles = ((k * 250 * t) % 48_000) as f64 / 48_000.0;
                    ((2.0 * std::f64::consts::PI * cycles).sin() * 0.01) as f32
                })
                .sum()
        });
        let bw = a.channels[0].bandwidth.expect("banda");
        assert!((14_500.0..=15_600.0).contains(&bw.cutoff_hz), "{bw:?}");
        assert!(bw.abrupt);
    }

    #[test]
    fn true_peak_finds_intersample_peak() {
        // Seno em fs/4 com fase de 45°: amostras em ±0.707·A, pico real = A (≈ +3 dB acima).
        let mut acc = Accumulator::new(1, 48_000, &[ChannelRole::Center]);
        for t in 0..48_000u64 {
            let ph = std::f64::consts::FRAC_PI_4 + std::f64::consts::FRAC_PI_2 * (t % 4) as f64;
            acc.push(&[(0.5 * ph.sin()) as f32]);
        }
        let sample_peak = 20.0 * (acc.peak[0] as f64).log10();
        let tp = 20.0 * (acc.tp_max as f64).log10();
        assert!((sample_peak - (-9.03)).abs() < 0.1, "{sample_peak}");
        assert!((tp - (-6.02)).abs() < 0.3, "{tp}");
    }

    #[test]
    fn ac3_and_eac3_dialnorm() {
        // AC3 5.1 (acmod=7, lfe=1), dialnorm=27
        // byte5: bsid=8 (01000) bsmod=0 (000) -> 0x40
        // bits: acmod 111, cmixlev 00, surmixlev 00, lfeon 1, dialnorm 11011
        // -> 1110 0001 | 1101 1xxx -> 0xE1 0xD8
        let ac3 = [0x0B, 0x77, 0, 0, 0, 0x40, 0xE1, 0xD8];
        assert_eq!(parse_ac3_dialnorm(&ac3), Some(-27));
        // E-AC3: bsid=16 (10000), dialnorm=24 (11000): byte5 = 10000 110, byte6 = 00xxxxxx
        let eac3 = [0x0B, 0x77, 0, 0, 0, 0x86, 0x00, 0];
        assert_eq!(parse_ac3_dialnorm(&eac3), Some(-24));
        assert_eq!(parse_ac3_dialnorm(&[0, 1, 2, 3, 4, 5, 6, 7]), None);
    }

    #[test]
    fn parses_ebur128_summary() {
        let s = "[Parsed_ebur128_1 @ 0x1] Summary:\n\n  Integrated loudness:\n    I:         -23.4 LUFS\n    Threshold: -33.6 LUFS\n\n  Loudness range:\n    LRA:        12.0 LU\n    Threshold: -43.6 LUFS\n\n  True peak:\n    Peak:       -1.2 dBFS\n";
        let l = parse_ebur128_summary(s).unwrap();
        assert_eq!(l.integrated_lufs, -23.4);
        assert_eq!(l.lra_lu, 12.0);
        assert_eq!(l.true_peak_dbtp, Some(-1.2));
    }

    #[test]
    fn duration_mismatch_alert() {
        let a = synth(
            2,
            &[ChannelRole::Left, ChannelRole::Right],
            10.0,
            |t, ch| noise(t, ch as u64 + 1),
        );
        let ctx = ReportContext {
            reference_duration_secs: Some(12.5),
            ..Default::default()
        };
        assert!(findings(&a, &ctx)
            .iter()
            .any(|f| f.text.contains("mais curto")));
    }

    #[test]
    fn multi_report_has_one_row_per_file_and_merged_findings() {
        let ok = synth(
            2,
            &[ChannelRole::Left, ChannelRole::Right],
            10.0,
            |t, ch| noise(t, ch as u64 + 1),
        );
        let mono = synth(2, &[ChannelRole::Left, ChannelRole::Right], 10.0, |t, _| {
            noise(t, 9)
        });
        let ctx = |l: &str| ReportContext {
            label: Some(l.into()),
            fps: Some("23.976".into()),
            ..Default::default()
        };
        let r = render_report(&[
            (ok.clone(), ctx("S01E01")),
            (mono.clone(), ctx("S01E02")),
            (mono, ctx("S01E03")),
        ])
        .unwrap();
        assert_eq!(r.matches("[tr][td]S01E").count(), 3);
        assert_eq!(r.matches("[note]").count(), 1);
        assert!(r.contains("[b]S01E02:[/b]") && r.contains("[b]S01E03:[/b]"));
        assert!(!r.contains("[alert]"));
    }

    #[test]
    fn parses_mkv_duration_tag() {
        assert_eq!(parse_hms("01:43:10.150000000"), Some(6190.15));
        assert_eq!(parse_hms("garbage"), None);
    }

    #[test]
    fn upsert_replaces_edited_report_and_remove_keeps_rest() {
        let old = format!("[b]{REPORT_TITLE}[/b]\n[table]\nlinha editada pelo usuário\n[/table]");
        let new = format!("[b]{REPORT_TITLE}[/b]\n[table]\nnovo\n[/table]");
        let mut d = format!("Fonte: X\n\n{old}\n\n[img]espectro[/img]");
        upsert_report(&mut d, &new);
        assert_eq!(d, format!("Fonte: X\n\n{new}\n\n[img]espectro[/img]"));
        assert_eq!(d.matches(REPORT_TITLE).count(), 1);

        // CRLF (descrição vinda de arquivo no Windows)
        let mut d = format!("Fonte: X\r\n\r\n{old}\r\n\r\nfim");
        upsert_report(&mut d, &new);
        assert_eq!(d.matches(REPORT_TITLE).count(), 1);
        assert!(d.ends_with("fim"));

        let mut d = format!("Fonte: X\n\n{new}\n\n[img]espectro[/img]");
        remove_report(&mut d);
        assert_eq!(d, "Fonte: X\n\n[img]espectro[/img]");

        let mut d = "Sem relatório".to_string();
        remove_report(&mut d);
        assert_eq!(d, "Sem relatório");
        upsert_report(&mut d, &new);
        assert_eq!(d, format!("Sem relatório\n\n{new}"));
    }

    #[test]
    fn rendered_reports_have_no_blank_lines() {
        let a = synth(6, &R51, 20.0, |t, ch| {
            if ch >= 4 {
                0.0
            } else {
                noise(t, ch as u64 + 1)
            }
        });
        let ctx = ReportContext {
            reference_duration_secs: Some(30.0),
            ..Default::default()
        };
        let single = render_single(&a, &ctx);
        let multi = render_multi(&[(a.clone(), ctx.clone()), (a, ctx)]);
        for r in [single, multi] {
            assert!(!r.contains("\n\n"), "{r}");
        }
    }

    #[test]
    fn true_peak_flushes_samples_at_end_of_stream() {
        let mut acc = Accumulator::new(1, 48_000, &[ChannelRole::Center]);
        for _ in 0..1000 {
            acc.push(&[0.0]);
        }
        acc.push(&[1.0]);
        acc.push(&[1.0]);
        let a = acc.finish(AnalysisHeader {
            file_name: "t".into(),
            codec_label: "PCM".into(),
            lossless: true,
            sample_rate: 48_000,
            layout_label: "1.0".into(),
            dialnorm_db: None,
            loudness: Some(Loudness {
                integrated_lufs: -20.0,
                lra_lu: 1.0,
                true_peak_dbtp: None,
            }),
            names: vec!["C".into()],
            roles: vec![ChannelRole::Center],
        });
        let tp = a.loudness.unwrap().true_peak_dbtp.unwrap();
        assert!(tp > 1.5, "{tp}");
    }

    #[test]
    fn layouts_outside_table_are_not_relabelled_as_51() {
        assert_eq!(layout_label("6.0", 6), "6.0");
        assert_eq!(layout_label("5.1(side)", 6), "5.1");
        assert_eq!(layout_label("", 6), "5.1");
        let m = channel_map("6.0", 6);
        assert!(m.iter().all(|(_, r)| *r == ChannelRole::Other));
        assert_eq!(channel_map("6 channels", 6)[3].1, ChannelRole::Lfe);
    }

    #[test]
    fn episode_label_uses_generated_suffix() {
        let p = Path::new("Show S01E01 - S01E02.ac3");
        assert_eq!(episode_label(p).as_deref(), Some("S01E02"));
    }

    #[test]
    fn mixed_season_shows_format_per_row() {
        let mut flac = synth(2, &[ChannelRole::Left, ChannelRole::Right], 5.0, |t, ch| {
            noise(t, ch as u64 + 1)
        });
        flac.codec_label = "FLAC".into();
        let ac3 = synth(2, &[ChannelRole::Left, ChannelRole::Right], 5.0, |t, ch| {
            noise(t, ch as u64 + 1)
        });
        let ctx = ReportContext::default();
        let mixed = render_multi(&[(ac3.clone(), ctx.clone()), (flac, ctx.clone())]);
        assert!(mixed.contains("[th]Formato[/th]") && mixed.contains("formatos variados"));
        let uniform = render_multi(&[(ac3.clone(), ctx.clone()), (ac3, ctx)]);
        assert!(!uniform.contains("[th]Formato[/th]"));
    }

    #[test]
    fn duplicated_audio_paths_are_analyzed_once() {
        let pkg = PackageOutput {
            output_folder: PathBuf::from("out"),
            torrent_path: PathBuf::from("x.torrent"),
            infohash: String::new(),
            first_audio_path: None,
            source_mediainfo: String::new(),
            audio_mediainfo: String::new(),
            created_files: vec![
                PathBuf::from("out/Filme.ac3"),
                PathBuf::from("out/PT-BR (Full).sup"),
                PathBuf::from("out/Filme.ac3"),
            ],
            delay_warnings: Vec::new(),
            audio_report: None,
        };
        assert_eq!(
            package_audio_files(&pkg),
            vec![PathBuf::from("out/Filme.ac3")]
        );
    }

    #[test]
    fn channel_map_known_layouts() {
        let m = channel_map("5.1(side)", 6);
        assert_eq!(m[3].1, ChannelRole::Lfe);
        assert_eq!(m[4].0, "Ls");
        let m = channel_map("unknown", 6);
        assert_eq!(m.len(), 6);
        assert_eq!(m[0].1, ChannelRole::Left);
    }
}
