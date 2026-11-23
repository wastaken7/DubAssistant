//! Detecção do FPS *real* do conteúdo, além do FPS nominal do container.
//!
//! O FPS nominal mente em dois casos comuns:
//! - **Telecine / pulldown** (DVD NTSC, TS de TV, Blu-ray 1080i, 720p59.94): filme a 23.976
//!   armazenado a 29.97 (ou 59.94) com campos/frames repetidos. O container diz 29.97,
//!   mas o áudio casa com releases 23.976.
//! - **VFR** (alguns WEB-DL): o MediaInfo só informa uma média.
//!
//! A análise de cadência funciona para soft telecine (flags RFF), hard telecine e híbridos:
//! `repeatfields` aplica as flags RFF, `fieldmatch` reconstrói os frames progressivos e a
//! diferença entre frames consecutivos revela frames duplicados. Filme 3:2 tem 1 duplicado
//! a cada 5 frames (29.97 × 4/5 = 23.976); vídeo entrelaçado de verdade não tem nenhum.

use crate::cancel::{run_command_cancellable, CancelHandle};
use crate::config::AppConfig;
use std::path::Path;
use std::process::Command;

/// Resultado da análise: FPS real e uma explicação curta para exibir ao usuário.
#[derive(Debug, Clone, PartialEq)]
pub struct FpsVerdict {
    pub fps: String,
    pub detail: String,
}

/// De onde ler o vídeo a ser analisado.
#[derive(Debug, Clone, Copy)]
pub enum ProbeSource<'a> {
    File(&'a Path),
    DvdTitle { root: &'a Path, title: u32 },
}

impl ProbeSource<'_> {
    fn add_input_args(&self, cmd: &mut Command) {
        match self {
            ProbeSource::File(p) => {
                cmd.arg("-i").arg(p);
            }
            ProbeSource::DvdTitle { root, title } => {
                cmd.args(["-f", "dvdvideo", "-title", &title.to_string(), "-i"])
                    .arg(root);
            }
        }
    }
}

/// Posições (fração da duração) onde os trechos são amostrados.
const WINDOW_POSITIONS: [f64; 5] = [0.1, 0.3, 0.5, 0.7, 0.9];
/// Frames por trecho (flags e cadência).
const FRAMES_PER_WINDOW: u32 = 300;
/// Pacotes lidos por trecho na análise de VFR.
const VFR_PACKETS_PER_WINDOW: u32 = 600;
/// Blocos de frames usados para contar duplicados (2 ciclos de telecine 3:2).
/// Janela (frames de cada lado) usada para medir o nível de movimento local.
const LOCAL_RADIUS: usize = 5;
/// Frame com movimento local (p75 da diferença na vizinhança) acima disso conta na análise.
const MOTION_THRESHOLD: f64 = 1.0;
/// Frame é duplicado se a diferença for menor que esta fração do movimento local.
/// (Granulação deixa duplicados de hard telecine em ~0.35-0.45 do movimento.)
const DUP_RATIO: f64 = 0.5;
/// Mínimo de frames com movimento para a análise ser conclusiva.
const MIN_MOVING_FRAMES: usize = 120;
/// Abaixo desta fração de intervalos com a cadência vencedora, o conteúdo é marcado como misto.
const MIXED_THRESHOLD: f64 = 0.8;
/// Converte "30000/1001", "29.970" ou "29.97" em número.
pub fn parse_fps_value(s: &str) -> Option<f64> {
    let s = s.trim();
    let v = if let Some((n, d)) = s.split_once('/') {
        let n: f64 = n.trim().parse().ok()?;
        let d: f64 = d.trim().parse().ok()?;
        if d <= 0.0 {
            return None;
        }
        n / d
    } else {
        s.parse().ok()?
    };
    (v.is_finite() && v > 0.0).then_some(v)
}

/// Formata um FPS no rótulo padrão usado pelo app (ex: 23.976, 24, 25, 29.97, 59.94).
pub fn format_fps(v: f64) -> String {
    const LABELS: [(f64, &str); 8] = [
        (24000.0 / 1001.0, "23.976"),
        (24.0, "24"),
        (25.0, "25"),
        (30000.0 / 1001.0, "29.97"),
        (30.0, "30"),
        (50.0, "50"),
        (60000.0 / 1001.0, "59.94"),
        (60.0, "60"),
    ];
    for (val, label) in LABELS {
        if (v - val).abs() < 0.01 {
            return label.to_string();
        }
    }
    format!("{v:.3}")
}

/// Texto para exibir o FPS junto com a explicação da análise, ex: `23.976 (telecine 3:2 detectado)`.
pub fn fps_with_detail(fps: Option<&str>, detail: Option<&str>) -> String {
    match (fps, detail) {
        (Some(f), Some(d)) => format!("{f} ({d})"),
        (Some(f), None) => f.to_string(),
        (None, _) => "?".to_string(),
    }
}

fn near(v: f64, target: f64) -> bool {
    (v - target).abs() < 0.02
}

/// FPS nominais que podem esconder conteúdo com outra cadência (telecine / frames repetidos).
pub fn needs_cadence_analysis(nominal: f64) -> bool {
    near(nominal, 30000.0 / 1001.0)
        || near(nominal, 30.0)
        || near(nominal, 60000.0 / 1001.0)
        || near(nominal, 60.0)
}

/// Analisa a cadência do conteúdo e retorna o FPS real.
///
/// `nominal` é o FPS do container (29.97 / 30 para conteúdo entrelaçado ou com pulldown,
/// 59.94 / 60 para progressivo de alta taxa). Retorna `None` se ffmpeg/ffprobe não puderem rodar.
pub fn analyze_cadence(
    cfg: &AppConfig,
    src: ProbeSource<'_>,
    nominal: f64,
    duration_secs: Option<f64>,
    cancel: &CancelHandle,
) -> Option<FpsVerdict> {
    if !needs_cadence_analysis(nominal) {
        return None;
    }
    let duration = duration_secs.or_else(|| probe_duration(cfg, src, cancel))?;
    let starts = window_starts(duration);
    let field_based = nominal < 40.0;

    // Atalho barato: soft telecine puro (todo frame progressivo, ~50% com RFF) dispensa
    // decodificar com filtros. Só uma chamada de ffprobe para todos os trechos.
    let mut has_rff = true;
    if field_based {
        if let Some(stats) = probe_frame_flags(cfg, src, &starts, cancel) {
            if let Some(v) = classify_flags(nominal, &stats) {
                return Some(v);
            }
            if stats.frames > 0 {
                has_rff = stats.rff > 0;
            }
        }
    }
    if cancel.is_cancelled() {
        return None;
    }

    let ffmpeg = cfg.ffmpeg_path.trim();
    if ffmpeg.is_empty() {
        return None;
    }
    // 29.97/30: aplica RFF (só se houver flags; em hard telecine o repeatfields embaralha
    // os campos) e reconstrói frames (fieldmatch) antes de comparar.
    // 59.94/60 progressivo: compara frames diretamente.
    let chain = match (field_based, has_rff) {
        (true, true) => "repeatfields,fieldmatch=combmatch=full,",
        (true, false) => "fieldmatch=combmatch=full,",
        (false, _) => "",
    };
    let vf = format!(
        "{chain}tblend=all_mode=difference,signalstats,metadata=print:key=lavfi.signalstats.YAVG:file=-"
    );

    // Os trechos são independentes: roda um ffmpeg por trecho em paralelo (cada um decodifica
    // só ~300 frames, então o custo dominante é abrir/posicionar a mídia).
    let run_window = |start: f64| -> Option<Vec<f64>> {
        if cancel.is_cancelled() {
            return None;
        }
        let mut cmd = Command::new(ffmpeg);
        cmd.args(["-hide_banner", "-nostdin", "-v", "error"]);
        cmd.args(["-ss", &format!("{start:.3}")]);
        src.add_input_args(&mut cmd);
        cmd.args([
            "-map",
            "0:v:0",
            "-frames:v",
            &FRAMES_PER_WINDOW.to_string(),
            "-an",
            "-sn",
            "-dn",
            "-fps_mode",
            "passthrough",
            "-vf",
            &vf,
            "-f",
            "null",
            "-",
        ]);
        let out = run_command_cancellable(&mut cmd, cancel).ok()?;
        let diffs = parse_yavg_lines(&String::from_utf8_lossy(&out.stdout));
        (!diffs.is_empty()).then_some(diffs)
    };
    let windows: Vec<Vec<f64>> = std::thread::scope(|s| {
        let handles: Vec<_> = starts
            .iter()
            .map(|&start| s.spawn(move || run_window(start)))
            .collect();
        handles
            .into_iter()
            .filter_map(|h| h.join().ok().flatten())
            .collect()
    });
    if windows.is_empty() || cancel.is_cancelled() {
        return None;
    }

    Some(classify_cadence(nominal, &cadence_stats(&windows)))
}

/// Contagem das flags de codificação dos frames amostrados.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct FrameFlagStats {
    pub frames: usize,
    /// Frames com repeat_first_field / pic_struct de repetição (soft pulldown).
    pub rff: usize,
    /// Frames codificados como entrelaçados.
    pub interlaced: usize,
}

fn probe_frame_flags(
    cfg: &AppConfig,
    src: ProbeSource<'_>,
    starts: &[f64],
    cancel: &CancelHandle,
) -> Option<FrameFlagStats> {
    let ffprobe = cfg.ffprobe_path.trim();
    if ffprobe.is_empty() {
        return None;
    }
    let intervals = starts
        .iter()
        .map(|s| format!("{s:.3}%+#{FRAMES_PER_WINDOW}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut cmd = Command::new(ffprobe);
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-read_intervals",
        &intervals,
        "-show_entries",
        "frame=interlaced_frame,repeat_pict",
        "-of",
        "csv=p=0",
    ]);
    match src {
        ProbeSource::File(p) => {
            cmd.arg(p);
        }
        ProbeSource::DvdTitle { root, title } => {
            cmd.args(["-f", "dvdvideo", "-title", &title.to_string()])
                .arg(root);
        }
    }
    let out = run_command_cancellable(&mut cmd, cancel).ok()?;
    Some(parse_frame_flags(&String::from_utf8_lossy(&out.stdout)))
}

/// Lê linhas `interlaced_frame,repeat_pict` do ffprobe.
pub fn parse_frame_flags(text: &str) -> FrameFlagStats {
    let mut s = FrameFlagStats::default();
    for line in text.lines() {
        let mut parts = line.trim().split(',');
        let (Some(i), Some(r)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (Ok(i), Ok(r)) = (i.trim().parse::<u32>(), r.trim().parse::<u32>()) else {
            continue;
        };
        s.frames += 1;
        if i == 1 {
            s.interlaced += 1;
        }
        if r >= 1 {
            s.rff += 1;
        }
    }
    s
}

/// Soft telecine puro → 23.976 sem precisar da análise de cadência.
/// Qualquer outro padrão retorna `None` (precisa da análise completa).
pub fn classify_flags(nominal: f64, s: &FrameFlagStats) -> Option<FpsVerdict> {
    if s.frames < 200 {
        return None;
    }
    let interlaced_frac = s.interlaced as f64 / s.frames as f64;
    let rff_frac = s.rff as f64 / s.frames as f64;
    if interlaced_frac < 0.02 && (0.4..=0.6).contains(&rff_frac) {
        let ntsc = near(nominal, 30000.0 / 1001.0);
        return Some(FpsVerdict {
            fps: if ntsc { "23.976" } else { "24" }.to_string(),
            detail: format!(
                "soft telecine (pulldown por flags; container {})",
                format_fps(nominal)
            ),
        });
    }
    None
}
/// Analisa vídeo VFR lendo apenas timestamps de pacotes (sem decodificar) e retorna a taxa dominante.
pub fn analyze_vfr(
    cfg: &AppConfig,
    path: &Path,
    duration_secs: Option<f64>,
    cancel: &CancelHandle,
) -> Option<FpsVerdict> {
    let ffprobe = cfg.ffprobe_path.trim();
    if ffprobe.is_empty() {
        return None;
    }
    let src = ProbeSource::File(path);
    let duration = duration_secs.or_else(|| probe_duration(cfg, src, cancel))?;
    let intervals = window_starts(duration)
        .iter()
        .map(|s| format!("{s:.3}%+#{VFR_PACKETS_PER_WINDOW}"))
        .collect::<Vec<_>>()
        .join(",");
    let mut cmd = Command::new(ffprobe);
    cmd.args([
        "-v",
        "error",
        "-select_streams",
        "v:0",
        "-read_intervals",
        &intervals,
        "-show_entries",
        "packet=pts_time",
        "-of",
        "csv=p=0",
    ])
    .arg(path);
    let out = run_command_cancellable(&mut cmd, cancel).ok()?;
    let pts: Vec<f64> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().trim_end_matches(',').parse::<f64>().ok())
        .collect();
    let (fps, share) = dominant_vfr_rate(&pts)?;
    if share >= 0.95 {
        // MediaInfo marca VFR, mas na prática a taxa é constante (comum em MKV com timestamps em ms).
        return Some(FpsVerdict {
            fps: format_fps(fps),
            detail: String::new(),
        });
    }
    Some(FpsVerdict {
        fps: format_fps(fps),
        detail: format!(
            "VFR; {} em {:.0}% dos frames",
            format_fps(fps),
            share * 100.0
        ),
    })
}

fn probe_duration(cfg: &AppConfig, src: ProbeSource<'_>, cancel: &CancelHandle) -> Option<f64> {
    let ffprobe = cfg.ffprobe_path.trim();
    if ffprobe.is_empty() {
        return None;
    }
    let mut cmd = Command::new(ffprobe);
    cmd.args([
        "-v",
        "error",
        "-show_entries",
        "format=duration",
        "-of",
        "csv=p=0",
    ]);
    match src {
        ProbeSource::File(p) => {
            cmd.arg(p);
        }
        ProbeSource::DvdTitle { root, title } => {
            cmd.args(["-f", "dvdvideo", "-title", &title.to_string()])
                .arg(root);
        }
    }
    let out = run_command_cancellable(&mut cmd, cancel).ok()?;
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .find_map(|l| l.trim().parse::<f64>().ok())
        .filter(|d| *d > 0.0)
}

fn window_starts(duration: f64) -> Vec<f64> {
    if duration < 60.0 {
        return vec![0.0];
    }
    WINDOW_POSITIONS.iter().map(|p| duration * p).collect()
}

fn parse_yavg_lines(text: &str) -> Vec<f64> {
    text.lines()
        .filter_map(|l| l.trim().strip_prefix("lavfi.signalstats.YAVG="))
        .filter_map(|v| v.trim().parse::<f64>().ok())
        .collect()
}

/// Estatísticas de frames duplicados após a reconstrução dos frames.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct CadenceStats {
    /// Frames em trechos com movimento.
    pub moving: usize,
    /// Frames duplicados (diferença muito menor que o movimento local).
    pub dups: usize,
    /// Distância (em frames) entre duplicados consecutivos dentro de cada trecho.
    pub gaps: Vec<usize>,
}

/// Encontra frames duplicados comparando cada diferença com o movimento local (p75 da vizinhança).
pub fn cadence_stats(windows: &[Vec<f64>]) -> CadenceStats {
    let mut st = CadenceStats::default();
    for diffs in windows {
        // O primeiro valor do tblend é sempre 0 (sem frame anterior) → descartado.
        let d = diffs.get(1..).unwrap_or(&[]);
        let mut last_dup: Option<usize> = None;
        for i in 0..d.len() {
            let lo = i.saturating_sub(LOCAL_RADIUS);
            let hi = (i + LOCAL_RADIUS + 1).min(d.len());
            if hi - lo < LOCAL_RADIUS + 1 {
                continue;
            }
            let mut local = d[lo..hi].to_vec();
            local.sort_by(|a, b| a.total_cmp(b));
            let p75 = local[(local.len() * 3) / 4];
            if p75 < MOTION_THRESHOLD {
                last_dup = None; // trecho parado quebra a sequência
                continue;
            }
            st.moving += 1;
            if d[i] < DUP_RATIO * p75 {
                st.dups += 1;
                if let Some(prev) = last_dup {
                    st.gaps.push(i - prev);
                }
                last_dup = Some(i);
            }
        }
    }
    st
}

/// Decide o FPS real pela periodicidade dos duplicados.
///
/// - 29.97/30: filme com telecine 3:2 tem 1 duplicado a cada **5** frames → 23.976.
///   Poucos duplicados → vídeo real. Duplicados sem período 5 (conversão PAL→NTSC,
///   blend) → mantém o nominal.
/// - 59.94/60p: duplicado a cada 2 frames → conteúdo 29.97; ~60% duplicados em padrão
///   3:2 → filme 23.976; quase nenhum → 59.94 real.
pub fn classify_cadence(nominal: f64, st: &CadenceStats) -> FpsVerdict {
    let nominal_label = format_fps(nominal);
    if st.moving < MIN_MOVING_FRAMES {
        return FpsVerdict {
            fps: nominal_label.clone(),
            detail: format!("análise inconclusiva (pouco movimento); mantido {nominal_label}"),
        };
    }
    let ntsc = near(nominal, 30000.0 / 1001.0) || near(nominal, 60000.0 / 1001.0);
    let film = if ntsc { 24000.0 / 1001.0 } else { 24.0 };
    let video30 = if ntsc { 30000.0 / 1001.0 } else { 30.0 };
    let density = st.dups as f64 / st.moving as f64;
    let gap_share = |pred: &dyn Fn(usize) -> bool| -> f64 {
        if st.gaps.is_empty() {
            0.0
        } else {
            st.gaps.iter().filter(|g| pred(**g)).count() as f64 / st.gaps.len() as f64
        }
    };
    let verdict = |fps: f64, detail: String| FpsVerdict {
        fps: format_fps(fps),
        detail,
    };

    if nominal < 40.0 {
        if density < 0.08 {
            return verdict(
                nominal,
                format!("vídeo {nominal_label} real (sem padrão de telecine)"),
            );
        }
        let film_share = gap_share(&|g| g == 5);
        if film_share >= 0.5 {
            let mut detail = format!("telecine 3:2 detectado (container {nominal_label})");
            if film_share < MIXED_THRESHOLD {
                detail.push_str(&format!(
                    "; conteúdo misto ({:.0}% com cadência de filme)",
                    film_share * 100.0
                ));
            }
            return verdict(film, detail);
        }
        return verdict(
            nominal,
            format!("sem cadência de filme (repetições irregulares); mantido {nominal_label}"),
        );
    }

    // 59.94 / 60 progressivo
    if density < 0.15 {
        return verdict(
            nominal,
            format!("{nominal_label} real (sem frames repetidos)"),
        );
    }
    if gap_share(&|g| g == 2) >= 0.7 {
        return verdict(
            video30,
            format!(
                "frames duplicados: conteúdo {} em {nominal_label}p",
                format_fps(video30)
            ),
        );
    }
    if (0.45..=0.75).contains(&density) && gap_share(&|g| g <= 2) >= 0.7 {
        return verdict(
            film,
            format!("filme com cadência 3:2 dentro de {nominal_label}p"),
        );
    }
    verdict(
        nominal,
        format!("frames repetidos sem padrão reconhecido; mantido {nominal_label}"),
    )
}
/// Taxa dominante de um vídeo VFR a partir dos PTS dos pacotes.
/// Retorna (fps, fração dos intervalos que seguem essa taxa).
pub fn dominant_vfr_rate(pts: &[f64]) -> Option<(f64, f64)> {
    let mut sorted: Vec<f64> = pts.iter().copied().filter(|v| v.is_finite()).collect();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let deltas: Vec<f64> = sorted
        .windows(2)
        .map(|w| w[1] - w[0])
        .filter(|d| *d > 0.001 && *d < 0.2) // descarta saltos entre trechos
        .collect();
    if deltas.len() < 50 {
        return None;
    }
    // Grupos de período (timestamps em ms não distinguem 23.976 de 24 por frame,
    // então a escolha dentro do grupo usa o período médio).
    const GROUPS: [&[f64]; 5] = [
        &[24000.0 / 1001.0, 24.0],
        &[25.0],
        &[30000.0 / 1001.0, 30.0],
        &[50.0],
        &[60000.0 / 1001.0, 60.0],
    ];
    let mut best: Option<(usize, f64, usize)> = None; // (grupo, soma dos períodos, contagem)
    for (gi, group) in GROUPS.iter().enumerate() {
        let period = 1.0 / group[0];
        let matched: Vec<f64> = deltas
            .iter()
            .copied()
            .filter(|d| (d - period).abs() < 0.0015)
            .collect();
        if matched.is_empty() {
            continue;
        }
        let sum: f64 = matched.iter().sum();
        if best.map(|(_, _, c)| matched.len() > c).unwrap_or(true) {
            best = Some((gi, sum, matched.len()));
        }
    }
    let (gi, sum, count) = best?;
    let mean_fps = count as f64 / sum;
    let fps = GROUPS[gi]
        .iter()
        .copied()
        .min_by(|a, b| (a - mean_fps).abs().total_cmp(&(b - mean_fps).abs()))?;
    Some((fps, count as f64 / deltas.len() as f64))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn telecine_window(cycles: usize) -> Vec<f64> {
        // 1º valor (sem frame anterior) + padrão 3:2 após fieldmatch: 4 frames novos + 1 duplicado
        let mut v = vec![0.0];
        for _ in 0..cycles {
            v.extend_from_slice(&[3.2, 4.1, 0.4, 5.0, 3.8]);
        }
        v
    }

    /// 25 fps convertido para 29.97 repetindo 1 campo a cada 5 frames: duplicado a cada 6.
    fn pal_conversion_window(cycles: usize) -> Vec<f64> {
        let mut v = vec![0.0];
        for _ in 0..cycles {
            v.extend_from_slice(&[3.4, 4.0, 3.5, 3.5, 3.3, 1.2]);
        }
        v
    }

    #[test]
    fn pal_to_ntsc_conversion_is_not_film() {
        let v = classify_cadence(
            30000.0 / 1001.0,
            &cadence_stats(&vec![pal_conversion_window(50); 5]),
        );
        assert_eq!(v.fps, "29.97", "{}", v.detail);
    }

    #[test]
    fn parses_fps_values() {
        assert!((parse_fps_value("30000/1001").unwrap() - 29.97).abs() < 0.001);
        assert_eq!(parse_fps_value("25"), Some(25.0));
        assert_eq!(parse_fps_value("0/0"), None);
        assert_eq!(format_fps(24000.0 / 1001.0), "23.976");
        assert_eq!(format_fps(30000.0 / 1001.0), "29.97");
        assert_eq!(format_fps(59.94), "59.94");
    }

    #[test]
    fn detects_telecine_as_23976() {
        let windows = vec![telecine_window(60); 5];
        let st = cadence_stats(&windows);
        assert!(st.gaps.iter().all(|g| *g == 5), "{:?}", st.gaps);
        let v = classify_cadence(30000.0 / 1001.0, &st);
        assert_eq!(v.fps, "23.976");
        assert!(v.detail.contains("telecine"));
    }

    #[test]
    fn keeps_true_interlaced_video_at_2997() {
        let win: Vec<f64> = std::iter::once(0.0)
            .chain((0..300).map(|i| 2.0 + (i % 7) as f64 * 0.3))
            .collect();
        let v = classify_cadence(30000.0 / 1001.0, &cadence_stats(&vec![win; 5]));
        assert_eq!(v.fps, "29.97");
    }

    #[test]
    fn static_content_is_inconclusive() {
        let win = vec![0.0; 301];
        let v = classify_cadence(30000.0 / 1001.0, &cadence_stats(&vec![win; 5]));
        assert_eq!(v.fps, "29.97");
        assert!(v.detail.contains("inconclusiva"));
    }

    #[test]
    fn detects_film_in_5994p() {
        // Filme em 59.94p: cada frame repetido 3,2,3,2 → 3 duplicados a cada 5
        let mut win = vec![0.0];
        for _ in 0..60 {
            win.extend_from_slice(&[4.0, 0.2, 0.3, 4.5, 0.2]);
        }
        let v = classify_cadence(60000.0 / 1001.0, &cadence_stats(&vec![win; 5]));
        assert_eq!(v.fps, "23.976");
    }

    #[test]
    fn grainy_hard_telecine_still_detected() {
        // Cena escura com granulação: duplicados ~0.6 contra ~1.6 de movimento
        let mut win = vec![0.0];
        for _ in 0..60 {
            win.extend_from_slice(&[1.69, 1.68, 1.89, 1.59, 0.62]);
        }
        let v = classify_cadence(30000.0 / 1001.0, &cadence_stats(&vec![win; 5]));
        assert_eq!(v.fps, "23.976");
    }

    #[test]
    fn mixed_content_is_flagged() {
        let film = telecine_window(60);
        let video = pal_conversion_window(50);
        let v = classify_cadence(
            30000.0 / 1001.0,
            &cadence_stats(&[film.clone(), film.clone(), film, video.clone(), video]),
        );
        assert_eq!(v.fps, "23.976");
        assert!(v.detail.contains("misto"), "{}", v.detail);
    }

    #[test]
    fn soft_telecine_flags_shortcut() {
        let text = "0,1\n0,0\n".repeat(150);
        let s = parse_frame_flags(&text);
        assert_eq!(s.frames, 300);
        let v = classify_flags(30000.0 / 1001.0, &s).unwrap();
        assert_eq!(v.fps, "23.976");
        // Com frames entrelaçados, precisa da análise completa
        let mixed = parse_frame_flags(&format!(
            "{}{}",
            "0,1\n0,0\n".repeat(100),
            "1,0\n".repeat(100)
        ));
        assert!(classify_flags(30000.0 / 1001.0, &mixed).is_none());
    }

    #[test]
    fn vfr_dominant_rate_from_ms_timestamps() {
        // 23.976 com timestamps arredondados a ms (padrão MKV) + trecho a 29.97
        let mut pts = Vec::new();
        for i in 0..900 {
            pts.push((i as f64 * 1001.0 / 24000.0 * 1000.0).round() / 1000.0);
        }
        for i in 0..200 {
            pts.push(100.0 + (i as f64 * 1001.0 / 30000.0 * 1000.0).round() / 1000.0);
        }
        let (fps, share) = dominant_vfr_rate(&pts).unwrap();
        assert_eq!(format_fps(fps), "23.976");
        assert!(share > 0.7);
    }
}
