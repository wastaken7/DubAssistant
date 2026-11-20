use crate::cancel::{run_command_cancellable, CancelHandle};
use crate::config::AppConfig;
use crate::media::configure_no_window;
use anyhow::{bail, Context, Result};
use directories::ProjectDirs;
use image::{Rgb, RgbImage};
use rustfft::{num_complex::Complex, FftPlanner};
use serde_json::Value;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

const FFT_SIZE: usize = 2048;
const PLOT_W: usize = 1600;
const PANEL_GAP: usize = 10;
const MARGIN_LEFT: usize = 68;
const MARGIN_RIGHT: usize = 76;
const MARGIN_TOP: usize = 42;
const MARGIN_BOTTOM: usize = 34;
const DB_MIN: f32 = -120.0;
const DB_MAX: f32 = 0.0;

#[derive(Debug, Clone)]
struct AudioProbe {
    sample_rate: u32,
    channels: usize,
    duration_secs: f64,
}

/// Gera o PNG de espectrograma em **streaming real** (`ffmpeg` decodificando PCM `f32le` via `pipe:1`
/// + `rustfft` coluna a coluna), mantendo o uso de RAM em ~30–50 MB mesmo para filmes 5.1/7.1 de 3h+.
/// - Todos os canais são plotados individualmente (FL, FR, FC, LFE, SL, SR, BL, BR).
/// - Se `duration_secs == 0`, analisa o **arquivo inteiro**.
pub fn generate_audio_spectrogram(
    cfg: &AppConfig,
    audio_path: &Path,
    channels_hint: &str,
    duration_secs: u32,
    cancel: &CancelHandle,
) -> Result<PathBuf> {
    if cancel.is_cancelled() {
        bail!("Tarefa cancelada pelo usuário.");
    }
    if !audio_path.is_file() {
        bail!(
            "Arquivo de áudio não encontrado para gerar espectrograma: {}",
            audio_path.display()
        );
    }
    let ffmpeg = cfg.ffmpeg_path.trim();
    if ffmpeg.is_empty() {
        bail!("ffmpeg não configurado nas Configurações.");
    }

    let probe = probe_audio_stream(cfg, audio_path, channels_hint, cancel)?;
    let channels = probe.channels.clamp(1, 8);
    let sample_rate = probe.sample_rate.max(8000);
    let effective_dur = if duration_secs > 0 {
        probe.duration_secs.min(duration_secs as f64).max(1.0)
    } else {
        probe.duration_secs.max(1.0)
    };

    let panel_h: usize = if channels <= 2 { 340 } else { 240 };
    let total_w = MARGIN_LEFT + PLOT_W + MARGIN_RIGHT;
    let total_h = MARGIN_TOP + channels * panel_h + (channels - 1) * PANEL_GAP + MARGIN_BOTTOM;

    let mut img = RgbImage::from_pixel(total_w as u32, total_h as u32, Rgb([14, 14, 18]));

    // Pré-computa janela Hann e planejador FFT
    let hann: Vec<f32> = (0..FFT_SIZE)
        .map(|i| {
            let x = (std::f32::consts::PI * 2.0 * i as f32) / (FFT_SIZE as f32 - 1.0);
            0.5 * (1.0 - x.cos())
        })
        .collect();
    let hann_sum: f32 = hann.iter().sum::<f32>().max(1.0);

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FFT_SIZE);
    let mut fft_buf = vec![Complex::new(0.0f32, 0.0f32); FFT_SIZE];

    // Mapeamento de cada coluna horizontal -> frame inicial alvo no fluxo PCM
    let total_frames = (effective_dur * sample_rate as f64) as u64;
    let max_start = total_frames.saturating_sub(FFT_SIZE as u64);
    let col_targets: Vec<u64> = (0..PLOT_W)
        .map(|c| {
            if PLOT_W <= 1 {
                0
            } else {
                (c as u64 * max_start) / (PLOT_W as u64 - 1)
            }
        })
        .collect();

    // Inicia ffmpeg decodificando em streaming para stdout (`f32le` intercalado)
    let mut cmd = Command::new(ffmpeg);
    configure_no_window(&mut cmd);
    cmd.args(["-v", "error", "-nostdin"]);
    if duration_secs > 0 {
        cmd.args(["-t", &duration_secs.to_string()]);
    }
    cmd.arg("-i")
        .arg(audio_path)
        .args([
            "-vn",
            "-sn",
            "-dn",
            "-ac",
            &channels.to_string(),
            "-ar",
            &sample_rate.to_string(),
            "-f",
            "f32le",
            "-acodec",
            "pcm_f32le",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let mut child = cmd
        .spawn()
        .with_context(|| format!("Falha ao iniciar ffmpeg ({ffmpeg}) para espectrograma"))?;
    let mut stdout = child
        .stdout
        .take()
        .context("Falha ao abrir pipe stdout do ffmpeg")?;

    let frame_bytes = channels * 4;
    let chunk_frames = 4096usize;
    let mut read_buf = vec![0u8; chunk_frames * frame_bytes];
    let mut leftover = Vec::<u8>::new();

    let mut cur_frame: u64 = 0;
    let mut next_col: usize = 0;
    let mut capturing = false;
    let mut capture_col: usize = 0;
    let mut capture_filled: usize = 0;
    // Buffer pequeno apenas para a janela FFT atual (~64 KB no máximo!)
    let mut window_samples: Vec<Vec<f32>> = vec![vec![0.0f32; FFT_SIZE]; channels];
    let bins = FFT_SIZE / 2 + 1;
    let mut col_db = vec![DB_MIN; panel_h];

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
        let complete_frames = leftover.len() / frame_bytes;
        let consumed = complete_frames * frame_bytes;

        for f_idx in 0..complete_frames {
            let base = f_idx * frame_bytes;

            // Inicia captura quando atingimos a posição da próxima coluna
            if !capturing && next_col < PLOT_W && cur_frame >= col_targets[next_col] {
                capturing = true;
                capture_col = next_col;
                capture_filled = 0;
                // Avança colunas que apontam para o mesmo frame (arquivos muito curtos)
                while next_col < PLOT_W && col_targets[next_col] <= cur_frame {
                    next_col += 1;
                }
            }

            if capturing {
                for ch in 0..channels {
                    let off = base + ch * 4;
                    let sample = f32::from_le_bytes([
                        leftover[off],
                        leftover[off + 1],
                        leftover[off + 2],
                        leftover[off + 3],
                    ]);
                    window_samples[ch][capture_filled] = sample;
                }
                capture_filled += 1;

                if capture_filled == FFT_SIZE {
                    // Executa FFT para cada canal e pinta diretamente a coluna no PNG
                    for ch in 0..channels {
                        for i in 0..FFT_SIZE {
                            fft_buf[i] = Complex::new(window_samples[ch][i] * hann[i], 0.0);
                        }
                        fft.process(&mut fft_buf);

                        col_db.fill(DB_MIN);
                        for b in 0..bins {
                            let mag = (fft_buf[b].norm() * 2.0) / hann_sum;
                            let db = if mag > 1e-10 {
                                (20.0 * mag.log10()).clamp(DB_MIN, DB_MAX)
                            } else {
                                DB_MIN
                            };
                            let y_from_bottom = (b * (panel_h - 1)) / (bins - 1);
                            let y_in_panel = (panel_h - 1) - y_from_bottom;
                            if db > col_db[y_in_panel] {
                                col_db[y_in_panel] = db;
                            }
                        }

                        let panel_y0 = MARGIN_TOP + ch * (panel_h + PANEL_GAP);
                        // Preenche do capture_col até next_col-1 caso arquivo seja muito curto
                        let fill_end = next_col.max(capture_col + 1).min(PLOT_W);
                        for col_x in capture_col..fill_end {
                            let px = (MARGIN_LEFT + col_x) as u32;
                            for (row, &db) in col_db.iter().enumerate() {
                                img.put_pixel(px, (panel_y0 + row) as u32, db_to_rgb(db));
                            }
                        }
                    }
                    capturing = false;
                }
            }

            cur_frame += 1;
        }

        if consumed > 0 {
            leftover.drain(..consumed);
        }
    }

    let _ = child.wait();

    // Desenha cabeçalho, nomes dos canais, réguas de frequência (kHz), tempo e barra de dB
    draw_overlays(
        &mut img,
        audio_path,
        channels,
        panel_h,
        sample_rate,
        effective_dur,
        duration_secs == 0,
    );

    let out_dir = spectrogram_temp_dir()?;
    let stem = audio_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "audio".to_string());
    let ts = chrono::Utc::now().timestamp();
    let out_path = out_dir.join(format!("{stem}_spec_{ts}.png"));

    img.save(&out_path)
        .with_context(|| format!("Falha ao salvar PNG do espectrograma: {}", out_path.display()))?;

    Ok(out_path)
}

fn probe_audio_stream(
    cfg: &AppConfig,
    audio_path: &Path,
    channels_hint: &str,
    cancel: &CancelHandle,
) -> Result<AudioProbe> {
    let fallback_ch = parse_channels_count(channels_hint).clamp(1, 8) as usize;
    let ffprobe = cfg.ffprobe_path.trim();
    if ffprobe.is_empty() {
        return Ok(AudioProbe {
            sample_rate: 48000,
            channels: fallback_ch,
            duration_secs: 600.0,
        });
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
    .arg(audio_path);

    let out = run_command_cancellable(&mut cmd, cancel)?;
    let json: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);

    let audio_stream = json
        .get("streams")
        .and_then(|v| v.as_array())
        .and_then(|arr| {
            arr.iter()
                .find(|s| s.get("codec_type").and_then(|t| t.as_str()) == Some("audio"))
        });

    let sample_rate = audio_stream
        .and_then(|s| s.get("sample_rate"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(48000);

    let channels = audio_stream
        .and_then(|s| s.get("channels"))
        .and_then(|v| v.as_u64())
        .map(|c| c as usize)
        .unwrap_or(fallback_ch);

    let duration_secs = audio_stream
        .and_then(|s| s.get("duration"))
        .and_then(|v| v.as_str())
        .and_then(|s| s.parse::<f64>().ok())
        .or_else(|| {
            json.pointer("/format/duration")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<f64>().ok())
        })
        .unwrap_or(600.0);

    Ok(AudioProbe {
        sample_rate,
        channels,
        duration_secs,
    })
}

/// Mapa de cores perceptual (estilo Inferno/SoX) de -120 dB (preto) até 0 dB (amarelo claro).
fn db_to_rgb(db: f32) -> Rgb<u8> {
    let t = ((db - DB_MIN) / (DB_MAX - DB_MIN)).clamp(0.0, 1.0);
    const STOPS: [(f32, [u8; 3]); 7] = [
        (0.00, [0, 0, 4]),
        (0.18, [26, 12, 65]),
        (0.36, [87, 16, 110]),
        (0.55, [159, 42, 99]),
        (0.72, [219, 80, 59]),
        (0.88, [249, 155, 15]),
        (1.00, [252, 255, 164]),
    ];

    for w in STOPS.windows(2) {
        let (t0, c0) = w[0];
        let (t1, c1) = w[1];
        if t <= t1 {
            let f = ((t - t0) / (t1 - t0)).clamp(0.0, 1.0);
            let lerp = |a: u8, b: u8| -> u8 { (a as f32 + (b as f32 - a as f32) * f).round() as u8 };
            return Rgb([lerp(c0[0], c1[0]), lerp(c0[1], c1[1]), lerp(c0[2], c1[2])]);
        }
    }
    Rgb([252, 255, 164])
}

fn channel_labels(channels: usize) -> &'static [&'static str] {
    match channels {
        1 => &["MONO"],
        2 => &["FL", "FR"],
        3 => &["FL", "FR", "LFE"],
        4 => &["FL", "FR", "SL", "SR"],
        5 => &["FL", "FR", "FC", "SL", "SR"],
        6 => &["FL", "FR", "FC", "LFE", "SL", "SR"],
        7 => &["FL", "FR", "FC", "LFE", "BC", "SL", "SR"],
        _ => &["FL", "FR", "FC", "LFE", "BL", "BR", "SL", "SR"],
    }
}

fn draw_overlays(
    img: &mut RgbImage,
    audio_path: &Path,
    channels: usize,
    panel_h: usize,
    sample_rate: u32,
    duration_secs: f64,
    is_full: bool,
) {
    let white = Rgb([235, 235, 240]);
    let gray = Rgb([160, 165, 175]);
    let border = Rgb([65, 70, 82]);

    let fname = audio_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Audio".to_string());
    let dur_fmt = format_hms(duration_secs as u64);
    let scope = if is_full { "FULL" } else { "PARTIAL" };
    let header = format!(
        "{}  |  {:.1} kHz  |  {} CH  |  {} ({})",
        fname,
        sample_rate as f32 / 1000.0,
        channels,
        dur_fmt,
        scope
    );
    draw_text(img, MARGIN_LEFT as i32, 14, &header, white, 2);

    let labels = channel_labels(channels);
    let nyquist_khz = (sample_rate as f32) / 2000.0;

    for ch in 0..channels {
        let y0 = MARGIN_TOP + ch * (panel_h + PANEL_GAP);
        let y1 = y0 + panel_h - 1;
        let x0 = MARGIN_LEFT;
        let x1 = MARGIN_LEFT + PLOT_W - 1;

        // Borda do painel
        for x in x0..=x1 {
            img.put_pixel(x as u32, y0 as u32, border);
            img.put_pixel(x as u32, y1 as u32, border);
        }
        for y in y0..=y1 {
            img.put_pixel(x0 as u32, y as u32, border);
            img.put_pixel(x1 as u32, y as u32, border);
        }

        // Nome do canal no canto superior esquerdo do painel
        let ch_name = labels.get(ch).copied().unwrap_or("CH");
        draw_text(img, (x0 + 8) as i32, (y0 + 8) as i32, ch_name, white, 2);

        // Régua de frequência à esquerda (4 divisões: 0, 1/4, 2/4, 3/4, Nyquist)
        for step in 0..=4 {
            let frac = step as f32 / 4.0;
            let khz = nyquist_khz * frac;
            let y = y1 - ((panel_h - 1) as f32 * frac).round() as usize;
            let lbl = format!("{khz:.0}K");
            draw_text(img, 10, (y as i32) - 4, &lbl, gray, 1);
            for tx in (x0 - 4)..x0 {
                img.put_pixel(tx as u32, y as u32, gray);
            }
        }
    }

    // Régua de tempo na base
    let bottom_y = MARGIN_TOP + channels * panel_h + (channels - 1) * PANEL_GAP;
    let time_ticks = 8usize;
    for i in 0..=time_ticks {
        let frac = i as f64 / time_ticks as f64;
        let x = MARGIN_LEFT + ((PLOT_W - 1) as f64 * frac).round() as usize;
        let t_lbl = format_hms((duration_secs * frac).round() as u64);
        draw_text(img, (x as i32) - 18, (bottom_y + 10) as i32, &t_lbl, gray, 1);
    }

    // Barra de legenda de dB à direita
    let bar_x = MARGIN_LEFT + PLOT_W + 14;
    let bar_w = 14usize;
    let bar_y0 = MARGIN_TOP;
    let bar_h = bottom_y - MARGIN_TOP;
    for row in 0..bar_h {
        let frac = 1.0 - (row as f32 / (bar_h.max(1) - 1) as f32);
        let db = DB_MIN + frac * (DB_MAX - DB_MIN);
        let c = db_to_rgb(db);
        for dx in 0..bar_w {
            img.put_pixel((bar_x + dx) as u32, (bar_y0 + row) as u32, c);
        }
    }
    for db_tick in [0, -30, -60, -90, -120] {
        let frac = (db_tick as f32 - DB_MIN) / (DB_MAX - DB_MIN);
        let y = bar_y0 + ((1.0 - frac) * (bar_h - 1) as f32).round() as usize;
        let lbl = format!("{db_tick}");
        draw_text(
            img,
            (bar_x + bar_w + 5) as i32,
            (y as i32) - 4,
            &lbl,
            gray,
            1,
        );
    }
}

fn format_hms(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// Fonte bitmap 5x7 embutida (zero dependências externas, 100% cross-platform).
fn draw_text(img: &mut RgbImage, x0: i32, y0: i32, text: &str, color: Rgb<u8>, scale: i32) {
    let mut x = x0;
    let (w, h) = img.dimensions();
    for ch in text.chars() {
        let glyph = glyph_5x7(ch.to_ascii_uppercase());
        for (row, &bits) in glyph.iter().enumerate() {
            for col in 0..5 {
                if (bits >> (4 - col)) & 1 == 1 {
                    for sy in 0..scale {
                        for sx in 0..scale {
                            let px = x + col * scale + sx;
                            let py = y0 + (row as i32) * scale + sy;
                            if px >= 0 && py >= 0 && (px as u32) < w && (py as u32) < h {
                                img.put_pixel(px as u32, py as u32, color);
                            }
                        }
                    }
                }
            }
        }
        x += 6 * scale;
    }
}

fn glyph_5x7(c: char) -> [u8; 7] {
    match c {
        'A' => [0x0E, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'B' => [0x1E, 0x11, 0x11, 0x1E, 0x11, 0x11, 0x1E],
        'C' => [0x0E, 0x11, 0x10, 0x10, 0x10, 0x11, 0x0E],
        'D' => [0x1E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x1E],
        'E' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x1F],
        'F' => [0x1F, 0x10, 0x10, 0x1E, 0x10, 0x10, 0x10],
        'G' => [0x0E, 0x11, 0x10, 0x17, 0x11, 0x11, 0x0F],
        'H' => [0x11, 0x11, 0x11, 0x1F, 0x11, 0x11, 0x11],
        'I' => [0x0E, 0x04, 0x04, 0x04, 0x04, 0x04, 0x0E],
        'J' => [0x07, 0x02, 0x02, 0x02, 0x12, 0x12, 0x0C],
        'K' => [0x11, 0x12, 0x14, 0x18, 0x14, 0x12, 0x11],
        'L' => [0x10, 0x10, 0x10, 0x10, 0x10, 0x10, 0x1F],
        'M' => [0x11, 0x1B, 0x15, 0x15, 0x11, 0x11, 0x11],
        'N' => [0x11, 0x19, 0x15, 0x13, 0x11, 0x11, 0x11],
        'O' => [0x0E, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'P' => [0x1E, 0x11, 0x11, 0x1E, 0x10, 0x10, 0x10],
        'Q' => [0x0E, 0x11, 0x11, 0x11, 0x15, 0x12, 0x0D],
        'R' => [0x1E, 0x11, 0x11, 0x1E, 0x14, 0x12, 0x11],
        'S' => [0x0F, 0x10, 0x10, 0x0E, 0x01, 0x01, 0x1E],
        'T' => [0x1F, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        'U' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0E],
        'V' => [0x11, 0x11, 0x11, 0x11, 0x11, 0x0A, 0x04],
        'W' => [0x11, 0x11, 0x11, 0x15, 0x15, 0x1B, 0x11],
        'X' => [0x11, 0x11, 0x0A, 0x04, 0x0A, 0x11, 0x11],
        'Y' => [0x11, 0x11, 0x0A, 0x04, 0x04, 0x04, 0x04],
        'Z' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x10, 0x1F],
        '0' => [0x0E, 0x11, 0x13, 0x15, 0x19, 0x11, 0x0E],
        '1' => [0x04, 0x0C, 0x04, 0x04, 0x04, 0x04, 0x0E],
        '2' => [0x0E, 0x11, 0x01, 0x02, 0x04, 0x08, 0x1F],
        '3' => [0x1E, 0x01, 0x01, 0x0E, 0x01, 0x01, 0x1E],
        '4' => [0x02, 0x06, 0x0A, 0x12, 0x1F, 0x02, 0x02],
        '5' => [0x1F, 0x10, 0x1E, 0x01, 0x01, 0x11, 0x0E],
        '6' => [0x06, 0x08, 0x10, 0x1E, 0x11, 0x11, 0x0E],
        '7' => [0x1F, 0x01, 0x02, 0x04, 0x08, 0x08, 0x08],
        '8' => [0x0E, 0x11, 0x11, 0x0E, 0x11, 0x11, 0x0E],
        '9' => [0x0E, 0x11, 0x11, 0x0F, 0x01, 0x02, 0x0C],
        '.' => [0x00, 0x00, 0x00, 0x00, 0x00, 0x0C, 0x0C],
        ':' => [0x00, 0x0C, 0x0C, 0x00, 0x0C, 0x0C, 0x00],
        '-' => [0x00, 0x00, 0x00, 0x1F, 0x00, 0x00, 0x00],
        '|' => [0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04],
        '(' => [0x02, 0x04, 0x08, 0x08, 0x08, 0x04, 0x02],
        ')' => [0x08, 0x04, 0x02, 0x02, 0x02, 0x04, 0x08],
        '/' => [0x01, 0x02, 0x04, 0x04, 0x08, 0x10, 0x00],
        _ => [0x00; 7],
    }
}

fn spectrogram_temp_dir() -> Result<PathBuf> {
    let dir = if let Some(proj) = ProjectDirs::from("com", "CapybaraBR", "DubAssistant") {
        proj.data_local_dir().join("spectrograms")
    } else {
        std::env::temp_dir().join("DubAssistant_spectrograms")
    };
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn parse_channels_count(channels_hint: &str) -> u32 {
    let s = channels_hint.trim();
    if let Some((main, lfe)) = s.split_once('.') {
        let a: u32 = main.parse().unwrap_or(2);
        let b: u32 = lfe.parse().unwrap_or(0);
        return a + b;
    }
    s.parse::<u32>().unwrap_or(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_channels_count() {
        assert_eq!(parse_channels_count("2.0"), 2);
        assert_eq!(parse_channels_count("5.1"), 6);
        assert_eq!(parse_channels_count("7.1"), 8);
    }

    #[test]
    fn test_db_to_rgb_bounds() {
        assert_eq!(db_to_rgb(-120.0), Rgb([0, 0, 4]));
        assert_eq!(db_to_rgb(0.0), Rgb([252, 255, 164]));
    }
}
