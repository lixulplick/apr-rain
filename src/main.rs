use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Parser;
use rand::Rng;

/// Matrix rain that condenses into Alejandro Revilla's headshot.
///
/// The headshot (jPOS / Transactility founder) is mapped onto the terminal
/// grid using a subject mask (sidecar `<image>.mask.png` when present,
/// otherwise auto polarity + Otsu segmentation). Classic matrix rain falls
/// everywhere; over `--reveal-secs` the subject snaps in as bright,
/// slowly-mutating glyphs, carving the face out of the rain. After a hold
/// period the face dissolves back into the rain and the cycle repeats.
#[derive(Parser, Debug)]
#[command(
    name = "apr-rain",
    version,
    about = "Matrix rain that reveals Alejandro Revilla's headshot"
)]
struct Args {
    /// Path to the headshot image (falls back to the embedded copy if missing)
    #[arg(short, long, default_value = "assets/headshot.jpg")]
    image: PathBuf,

    /// Frames per second
    #[arg(long, default_value_t = 30.0)]
    fps: f32,

    /// Total run time in seconds (0 = run until a key is pressed)
    #[arg(short, long, default_value_t = 0.0)]
    duration: f32,

    /// Seconds for the face to condense out of the rain
    #[arg(long, default_value_t = 8.0)]
    reveal_secs: f32,

    /// Seconds to hold the revealed face before it dissolves
    #[arg(long, default_value_t = 10.0)]
    hold_secs: f32,

    /// Number of reveal/hold cycles (0 = loop forever)
    #[arg(long, default_value_t = 0)]
    loops: u32,

    /// Rain hue: green, amber, cyan, magenta, white
    #[arg(long, default_value = "green")]
    color: String,

    /// Force luminance inversion (dark subject on light background).
    /// Default: auto-detected; ignored when a sidecar mask exists.
    #[arg(long, default_value_t = false)]
    invert: bool,

    /// Otsu threshold bias, -1.0 (fewer face cells) .. 1.0 (more face cells).
    /// Ignored when a sidecar mask exists.
    #[arg(long, default_value_t = 0.0)]
    threshold: f32,

    /// Glyph alphabet: matrix (half-width katakana), katakana (full-width, CJK
    /// terminals only), jpos (hex / ISO 8583 vibes), ascii, binary
    #[arg(long, default_value = "matrix")]
    charset: String,

    /// Headless mode: simulate `--ticks` frames and write the final frame to this PNG
    #[arg(long)]
    screenshot: Option<PathBuf>,

    /// Headless mode: render one full reveal/hold/dissolve cycle to an animated GIF
    #[arg(long)]
    anim: Option<PathBuf>,

    /// Ticks to simulate before --screenshot (default: reveal + hold + 2s worth)
    #[arg(long)]
    ticks: Option<u64>,

    /// Screenshot cell block width in pixels (block is 2x tall, like a terminal cell)
    #[arg(long, default_value_t = 12)]
    cell_px: u32,
}

const EMBEDDED_HEADSHOT: &[u8] = include_bytes!("../assets/headshot.jpg");

// --------------------------------------------------------------------------
// helpers
// --------------------------------------------------------------------------

fn charset(name: &str) -> Vec<char> {
    match name {
        "jpos" => "0123456789ABCDEF".chars().collect(),
        "ascii" => (33u8..127).map(|b| b as char).collect(),
        "binary" => vec!['0', '1'],
        // full-width katakana advance 2 columns in most terminals — only
        // safe on CJK-configured terminals
        "katakana" => "アイウエオカキクケコサシスセソタチツテトナニヌネノハヒフヘホマミムメモヤユヨラリルレロワン"
            .chars()
            .collect(),
        // default: HALF-WIDTH katakana (U+FF66-FF9D) — wcwidth == 1 everywhere,
        // same trick cmatrix uses
        _ => {
            let mut v: Vec<char> = "ｦｧｨｩｪｫｬｭｮｯｱｲｳｴｵｶｷｸｹｺｻｼｽｾｿﾀﾁﾂﾃﾄﾅﾆﾇﾈﾉﾊﾋﾌﾍﾎﾏﾐﾑﾒﾓﾔﾕﾖﾗﾘﾙﾚﾛﾜﾝ"
                .chars()
                .collect();
            v.extend("0123456789".chars());
            v.extend("$#+*<>".chars());
            v
        }
    }
}

fn base_color(name: &str) -> (u8, u8, u8) {
    match name {
        "amber" => (255, 176, 0),
        "cyan" => (0, 255, 209),
        "magenta" => (255, 0, 153),
        "white" => (208, 208, 208),
        _ => (0, 255, 70), // classic matrix green
    }
}

const WHITE: (u8, u8, u8) = (255, 255, 255);

#[inline]
fn lerp(a: (u8, u8, u8), b: (u8, u8, u8), t: f32) -> (u8, u8, u8) {
    (
        (a.0 as f32 + (b.0 as f32 - a.0 as f32) * t) as u8,
        (a.1 as f32 + (b.1 as f32 - a.1 as f32) * t) as u8,
        (a.2 as f32 + (b.2 as f32 - a.2 as f32) * t) as u8,
    )
}

#[inline]
fn scale(c: (u8, u8, u8), k: f32) -> (u8, u8, u8) {
    (
        (c.0 as f32 * k) as u8,
        (c.1 as f32 * k) as u8,
        (c.2 as f32 * k) as u8,
    )
}

fn percentile(mut sorted: Vec<f32>, p: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let idx = ((sorted.len() as f32 * p) as usize).min(sorted.len() - 1);
    sorted[idx]
}

/// Otsu's method: optimal threshold separating two classes in a 64-bin histogram.
fn otsu(values: &[f32]) -> f32 {
    const BINS: usize = 64;
    let mut hist = [0u64; BINS];
    for &v in values {
        let b = ((v.clamp(0.0, 1.0) * (BINS as f32 - 1.0)) as usize).min(BINS - 1);
        hist[b] += 1;
    }
    let total = values.len() as f64;
    let mut sum_all = 0.0f64;
    for (i, &h) in hist.iter().enumerate() {
        sum_all += (i as f64) * (h as f64);
    }
    let (mut w_b, mut sum_b) = (0.0f64, 0.0f64);
    let mut best = (0.0f64, 0usize);
    for (i, &h) in hist.iter().enumerate() {
        w_b += h as f64;
        if w_b == 0.0 {
            continue;
        }
        let w_f = total - w_b;
        if w_f == 0.0 {
            break;
        }
        sum_b += (i as f64) * (h as f64);
        let m_b = sum_b / w_b;
        let m_f = (sum_all - sum_b) / w_f;
        let between = w_b * w_f * (m_b - m_f) * (m_b - m_f);
        if between > best.0 {
            best = (between, i);
        }
    }
    (best.1 as f32 + 0.5) / BINS as f32
}

// --------------------------------------------------------------------------
// face mask
// --------------------------------------------------------------------------

/// Face mask: which cells belong to the subject, and each cell's internal
/// detail intensity (0..1) after shadow recovery.
struct FaceMask {
    active: Vec<bool>,
    inten: Vec<f32>,
}

/// Letterbox geometry: scale source to fit the grid's screen aspect
/// (cells are ~1:2, so the grid-pixel space is cols x rows*2).
fn fit_geometry(sw: u32, sh: u32, cols: usize, rows: usize) -> (u32, u32, u32, u32) {
    let gw = cols.max(1) as u32;
    let gh = (rows.max(1) as u32).saturating_mul(2);
    if sw == 0 || sh == 0 {
        return (gw, gh, 0, 0);
    }
    let s = (gw as f32 / sw as f32).min(gh as f32 / sh as f32);
    let w = ((sw as f32 * s).round() as u32).clamp(1, gw);
    let h = ((sh as f32 * s).round() as u32).clamp(1, gh);
    (w, h, (gw - w) / 2, (gh - h) / 2)
}

/// Sample a grayscale source into the grid-pixel space (cols x rows*2),
/// centered, with `pad` outside the placed image. Also returns per-pixel validity.
fn sample_into_grid(
    src: &image::GrayImage,
    cols: usize,
    rows: usize,
    pad: f32,
) -> (Vec<f32>, Vec<bool>) {
    let (w, h, x, y) = fit_geometry(src.width(), src.height(), cols, rows);
    let gw = cols.max(1);
    let gh = rows.max(1) * 2;
    let resized = image::imageops::resize(src, w, h, image::imageops::FilterType::Triangle);

    let mut vals = vec![pad; gw * gh];
    let mut valid = vec![false; gw * gh];
    for r in 0..h as usize {
        for c in 0..w as usize {
            let idx = (y as usize + r) * gw + (x as usize + c);
            vals[idx] = resized.get_pixel(c as u32, r as u32)[0] as f32 / 255.0;
            valid[idx] = true;
        }
    }
    (vals, valid)
}

/// Average grid-pixel rows into cell rows (2 grid pixels per cell).
fn cells_from_pixels(pixels: &[f32], cols: usize, rows: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(cols * rows);
    for r in 0..rows {
        for c in 0..cols {
            let a = pixels[(r * 2) * cols + c];
            let b = pixels[(r * 2 + 1) * cols + c];
            out.push((a + b) / 2.0);
        }
    }
    out
}

/// Build the face mask from the photo and optional pre-segmented subject mask.
fn build_face_mask(
    img: &image::DynamicImage,
    sidecar: Option<&image::GrayImage>,
    cols: usize,
    rows: usize,
    args: &Args,
) -> FaceMask {
    let gray = img.to_luma8();

    // subject sidecar mask (white = subject)
    let (mut active, luma) = if let Some(mask) = sidecar {
        let (mvals, _) = sample_into_grid(mask, cols, rows, 0.0);
        let mask_cells = cells_from_pixels(&mvals, cols, rows);
        let active: Vec<bool> = mask_cells.iter().map(|&m| m > 0.5).collect();
        let (luma, _) = sample_into_grid(&gray, cols, rows, 0.0);
        (active, cells_from_pixels(&luma, cols, rows))
    } else {
        // auto polarity: is the image center darker than the whole image?
        let (luma_px, valid) = sample_into_grid(&gray, cols, rows, 0.0);
        let luma_cells = cells_from_pixels(&luma_px, cols, rows);
        let mut valid_cells = vec![false; cols * rows];
        for r in 0..rows {
            for c in 0..cols {
                valid_cells[r * cols + c] =
                    valid[(r * 2) * cols + c] && valid[(r * 2 + 1) * cols + c];
            }
        }

        let valid_vals: Vec<f32> = luma_cells
            .iter()
            .zip(valid_cells.iter())
            .filter(|(_, v)| **v)
            .map(|(l, _)| *l)
            .collect();
        let global = valid_vals.iter().sum::<f32>() / valid_vals.len().max(1) as f32;

        // center region mean over valid cells
        let (r0, r1) = (rows * 3 / 8, rows * 5 / 8);
        let (c0, c1) = (cols * 3 / 8, cols * 5 / 8);
        let mut s = 0.0;
        let mut n = 0;
        for r in r0..r1 {
            for c in c0..c1 {
                if valid_cells[r * cols + c] {
                    s += luma_cells[r * cols + c];
                    n += 1;
                }
            }
        }
        let center = if n > 0 { s / n as f32 } else { global };
        let inverted = args.invert || center < global * 0.92;

        let pol: Vec<f32> = luma_cells
            .iter()
            .zip(valid_cells.iter())
            .map(|(v, val)| {
                if *val {
                    if inverted {
                        1.0 - *v
                    } else {
                        *v
                    }
                } else {
                    // padding must never be active
                    if inverted {
                        0.0
                    } else {
                        0.0
                    }
                }
            })
            .collect();

        let valid_pol: Vec<f32> = pol
            .iter()
            .zip(valid_cells.iter())
            .filter(|(_, v)| **v)
            .map(|(p, _)| *p)
            .collect();
        let lo = percentile(valid_pol.clone(), 0.02);
        let hi = percentile(valid_pol.clone(), 0.98);
        let stretched: Vec<f32> = pol
            .iter()
            .map(|v| {
                if hi - lo > 1e-4 {
                    ((v - lo) / (hi - lo)).clamp(0.0, 1.0)
                } else {
                    *v
                }
            })
            .collect();

        let valid_stretched: Vec<f32> = stretched
            .iter()
            .zip(valid_cells.iter())
            .filter(|(_, v)| **v)
            .map(|(s, _)| *s)
            .collect();
        let thr = (otsu(&valid_stretched) + args.threshold * 0.25).clamp(0.02, 0.98);
        let active: Vec<bool> = stretched
            .iter()
            .zip(valid_cells.iter())
            .map(|(s, v)| *v && *s >= thr)
            .collect();
        (active, luma_cells)
    };

    smooth_mask(&mut active, cols, rows);

    // Shadow recovery: stretch ORIGINAL luminance over subject pixels only,
    // so smile / skin highlights render bright inside the silhouette.
    let subject: Vec<f32> = luma
        .iter()
        .zip(active.iter())
        .filter(|(_, a)| **a)
        .map(|(v, _)| *v)
        .collect();
    let slo = percentile(subject.clone(), 0.05);
    let shi = percentile(subject.clone(), 0.95);
    let inten: Vec<f32> = luma
        .iter()
        .zip(active.iter())
        .map(|(v, a)| {
            if *a && shi - slo > 1e-4 {
                ((v - slo) / (shi - slo)).clamp(0.0, 1.0).powf(0.85)
            } else {
                0.0
            }
        })
        .collect();

    FaceMask { active, inten }
}

/// 3x3 majority filter to de-speckle the face mask.
fn smooth_mask(mask: &mut Vec<bool>, cols: usize, rows: usize) {
    let orig = mask.clone();
    let idx = |r: isize, c: isize| r as usize * cols + c as usize;
    for r in 0..rows as isize {
        for c in 0..cols as isize {
            let mut on = 0;
            let mut n = 0;
            for dr in -1..=1 {
                for dc in -1..=1 {
                    let (rr, cc) = (r + dr, c + dc);
                    if rr >= 0 && rr < rows as isize && cc >= 0 && cc < cols as isize {
                        n += 1;
                        if orig[idx(rr, cc)] {
                            on += 1;
                        }
                    }
                }
            }
            mask[idx(r, c)] = on * 2 > n;
        }
    }
}

// --------------------------------------------------------------------------
// simulation
// --------------------------------------------------------------------------

struct Drop {
    y: f32,     // head position, in rows
    speed: f32, // rows / second
    trail: f32, // trail length in rows
    wait: f32,  // respawn delay
}

struct Sim {
    cols: usize,
    rows: usize,
    active: Vec<bool>,
    inten: Vec<f32>,
    reveal: Vec<f32>,
    glyph: Vec<char>,
    chars: Vec<char>,
    drops: Vec<Drop>,
    rng: rand::rngs::ThreadRng,

    reveal_secs: f32,
    hold_secs: f32,
    dissolve_secs: f32,
    cycle_len: f32,
    base: (u8, u8, u8),

    elapsed: f32, // within current cycle
    cycles: u32,
}

impl Sim {
    fn new(
        img: &image::DynamicImage,
        sidecar: Option<&image::GrayImage>,
        args: &Args,
        cols: usize,
        rows: usize,
    ) -> Self {
        let face = build_face_mask(img, sidecar, cols, rows, args);
        let chars = charset(&args.charset);

        let mut sim = Sim {
            cols,
            rows,
            active: face.active,
            inten: face.inten,
            reveal: vec![0.0; cols * rows],
            glyph: vec![' '; cols * rows],
            chars,
            drops: Vec::with_capacity(cols),
            rng: rand::thread_rng(),
            reveal_secs: args.reveal_secs.max(0.1),
            hold_secs: args.hold_secs.max(0.0),
            dissolve_secs: 0.0,
            cycle_len: 0.0,
            base: base_color(&args.color),
            elapsed: 0.0,
            cycles: 0,
        };
        sim.dissolve_secs = (sim.reveal_secs * 0.4).min(3.0);
        sim.cycle_len = sim.reveal_secs + sim.hold_secs + sim.dissolve_secs;

        let mut rng = rand::thread_rng();
        for _ in 0..cols {
            sim.drops.push(Drop {
                y: rng.gen_range(-(rows as f32)..(rows as f32)),
                speed: rng.gen_range(6.0..28.0),
                trail: rng.gen_range(6.0..18.0),
                wait: 0.0,
            });
        }
        sim.reset_cycle();
        sim
    }

    /// Re-randomize per-cell reveal times and glyphs for a new cycle.
    fn reset_cycle(&mut self) {
        let rs = self.reveal_secs;
        for i in 0..self.active.len() {
            // Detailed cells snap in earlier; randomize within the window.
            let jitter = self.rng.gen::<f32>().powi(2);
            let bias = 1.15 - 0.45 * self.inten[i];
            self.reveal[i] = (jitter * rs * bias).clamp(0.0, rs);
            self.glyph[i] = self.chars[self.rng.gen_range(0..self.chars.len())];
        }
        self.elapsed = 0.0;
    }

    /// Advance the simulation. Returns true when all requested loops finished.
    fn step(&mut self, dt: f32, loops: u32) -> bool {
        self.elapsed += dt;

        // rain drops
        let rows = self.rows as f32;
        for d in &mut self.drops {
            if d.wait > 0.0 {
                d.wait -= dt;
                if d.wait <= 0.0 {
                    d.y = -self.rng.gen_range(0.0..5.0);
                    d.speed = self.rng.gen_range(6.0..28.0);
                    d.trail = self.rng.gen_range(6.0..18.0);
                }
            } else {
                d.y += d.speed * dt;
                if d.y - d.trail > rows {
                    d.wait = self.rng.gen_range(0.0..3.0);
                }
            }
        }

        if self.elapsed >= self.cycle_len {
            self.cycles += 1;
            if loops > 0 && self.cycles >= loops {
                return true;
            }
            self.reset_cycle();
        }
        false
    }

    /// Compute one frame: per-cell color + glyph, or None for blank cells.
    fn frame(&mut self) -> Vec<Option<((u8, u8, u8), char)>> {
        let n = self.cols * self.rows;
        let mut out: Vec<Option<((u8, u8, u8), char)>> = vec![None; n];

        let phase = self.elapsed;
        let reveal_end = self.reveal_secs + self.hold_secs;
        let dissolve = if phase > reveal_end {
            1.0 - (phase - reveal_end) / self.dissolve_secs
        } else {
            1.0
        };

        // ---- rain layer ----
        let base = self.base;
        for (c, d) in self.drops.iter().enumerate() {
            if d.wait > 0.0 {
                continue;
            }
            let head = d.y;
            let top = (head - d.trail).max(0.0) as usize;
            let bottom = (head + 1.0).min(self.rows as f32) as usize;
            for r in top..bottom {
                let idx = r * self.cols + c;
                if self.active[idx] {
                    continue; // face layer owns this cell
                }
                let dy = head - r as f32;
                if dy < 0.0 || dy >= d.trail {
                    continue;
                }
                let fade = 1.0 - dy / d.trail;
                let col = if dy < 1.0 {
                    lerp(base, WHITE, 0.75)
                } else {
                    scale(base, 0.15 + 0.75 * fade)
                };
                let ch = self.chars[self.rng.gen_range(0..self.chars.len())];
                out[idx] = Some((col, ch));
            }
        }

        // ---- face layer ----
        for idx in 0..n {
            if !self.active[idx] {
                continue;
            }
            let t_reveal = self.reveal[idx];
            if phase < t_reveal || dissolve <= 0.0 {
                continue;
            }
            // snap-in ease (~0.35s)
            let t = ((phase - t_reveal) / 0.35).min(1.0);

            // occasional glyph mutation keeps it matrixy
            if self.rng.gen::<f32>() < 0.02 {
                self.glyph[idx] = self.chars[self.rng.gen_range(0..self.chars.len())];
            }
            let sparkle = self.rng.gen::<f32>() < 0.004;

            // base silhouette glow + internal detail
            let inten = 0.3 + 0.7 * self.inten[idx];
            let mut col = scale(base, inten);
            if self.inten[idx] > 0.9 {
                col = lerp(col, WHITE, (self.inten[idx] - 0.9) * 4.5);
            }
            if sparkle {
                col = WHITE;
            }
            col = scale(col, t * dissolve);
            if col == (0, 0, 0) {
                out[idx] = None;
            } else {
                out[idx] = Some((col, self.glyph[idx]));
            }
        }
        out
    }
}

// --------------------------------------------------------------------------
// renderers
// --------------------------------------------------------------------------

fn render_ansi(frame: &[Option<((u8, u8, u8), char)>], cols: usize, rows: usize) -> String {
    let mut out = String::with_capacity(cols * rows * 12 + 16);
    out.push_str("\x1b[H");
    let mut last: Option<(u8, u8, u8)> = None;
    for r in 0..rows {
        for c in 0..cols {
            match frame[r * cols + c] {
                Some((col, ch)) => {
                    if last != Some(col) {
                        out.push_str(&format!("\x1b[38;2;{};{};{}m", col.0, col.1, col.2));
                        last = Some(col);
                    }
                    out.push(ch);
                }
                None => out.push(' '),
            }
        }
        if r + 1 < rows {
            out.push_str("\r\n");
        }
    }
    out.push_str("\x1b[0m");
    out
}

/// Screenshot renderer: each cell becomes a block cell_px wide and
/// cell_px*2 tall, mirroring terminal cell geometry.
fn render_png(
    frame: &[Option<((u8, u8, u8), char)>],
    cols: usize,
    rows: usize,
    cell_px: u32,
) -> image::RgbImage {
    let cw = cell_px.max(1);
    let ch = cw * 2;
    let mut img =
        image::RgbImage::from_pixel(cols as u32 * cw, rows as u32 * ch, image::Rgb([0u8, 0, 0]));
    for r in 0..rows {
        for c in 0..cols {
            let col = match frame[r * cols + c] {
                Some((col, _)) => col,
                None => continue,
            };
            let rgb = image::Rgb([col.0, col.1, col.2]);
            for dy in 0..ch {
                for dx in 0..cw {
                    img.put_pixel(c as u32 * cw + dx, r as u32 * ch + dy, rgb);
                }
            }
        }
    }
    img
}

// --------------------------------------------------------------------------
// main
// --------------------------------------------------------------------------

fn load_image(path: &Path) -> io::Result<image::DynamicImage> {
    if path.exists() {
        image::ImageReader::open(path)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
            .decode()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    } else {
        image::load_from_memory(EMBEDDED_HEADSHOT)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }
}

/// Sidecar subject mask: `<image-stem>.mask.png` next to the image.
fn load_sidecar_mask(image_path: &Path) -> Option<image::GrayImage> {
    let stem = image_path.file_stem()?;
    let mask_path = image_path.with_file_name(format!("{}.mask.png", stem.to_string_lossy()));
    let bytes = if mask_path.exists() {
        std::fs::read(&mask_path).ok()?
    } else {
        return None;
    };
    image::load_from_memory(&bytes)
        .ok()
        .map(|d| d.to_luma8())
        .filter(|m| mask_is_meaningful(m))
}

/// Reject degenerate masks (all or nothing) that would break the effect.
fn mask_is_meaningful(mask: &image::GrayImage) -> bool {
    let total = (mask.width() * mask.height()) as f32;
    let mut on = 0.0f32;
    for p in mask.pixels() {
        if p[0] > 127 {
            on += 1.0;
        }
    }
    let f = on / total;
    f > 0.02 && f < 0.98
}

fn run_headless(
    args: &Args,
    img: &image::DynamicImage,
    sidecar: Option<&image::GrayImage>,
    out: &Path,
) -> io::Result<()> {
    let cols = 140usize;
    let rows = 70usize;
    let mut sim = Sim::new(img, sidecar, args, cols, rows);
    let ticks = args
        .ticks
        .unwrap_or(((args.reveal_secs + args.hold_secs + 2.0) * args.fps) as u64);
    let dt = 1.0 / args.fps;
    for _ in 0..ticks {
        sim.step(dt, 0);
    }
    let frame = sim.frame();
    let png = render_png(&frame, cols, rows, args.cell_px.max(1));
    png.save(out)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    println!(
        "wrote {} (grid {}x{}, {} ticks, sidecar mask: {})",
        out.display(),
        cols,
        rows,
        ticks,
        sidecar.is_some()
    );
    Ok(())
}

fn run_anim(
    args: &Args,
    img: &image::DynamicImage,
    sidecar: Option<&image::GrayImage>,
    out: &Path,
) -> io::Result<()> {
    use image::codecs::gif::GifEncoder;
    use image::{DynamicImage, Frame};
    use std::fs::File;

    let cols = 140usize;
    let rows = 70usize;
    let mut sim = Sim::new(img, sidecar, args, cols, rows);

    let sim_fps = 30.0f32;
    let out_fps = 15.0f32;
    let every = (sim_fps / out_fps).round().max(1.0) as u32;
    let dt = 1.0 / sim_fps;

    let file = File::create(out).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    let mut enc = GifEncoder::new(file);

    let mut ticks: u32 = 0;
    loop {
        let done = sim.step(dt, 1); // exactly one cycle
        if ticks % every == 0 {
            let frame = sim.frame();
            let rgb = render_png(&frame, cols, rows, args.cell_px.max(1));
            let f = Frame::from_parts(
                DynamicImage::ImageRgb8(rgb).to_rgba8(),
                0,
                0,
                image::Delay::from_numer_denom_ms((1000.0 / out_fps).round() as u32, 1),
            );
            enc.encode_frame(f)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        }
        ticks += 1;
        if done {
            break;
        }
    }
    println!(
        "wrote {} (grid {}x{}, {} frames @ ~{}fps)",
        out.display(),
        cols,
        rows,
        ticks / every,
        out_fps
    );
    Ok(())
}

fn run_terminal(
    args: &Args,
    img: &image::DynamicImage,
    sidecar: Option<&image::GrayImage>,
) -> io::Result<()> {
    use crossterm::cursor::{Hide, Show};
    use crossterm::event::{poll, read, Event};
    use crossterm::execute;
    use crossterm::terminal::{
        disable_raw_mode, enable_raw_mode, size, EnterAlternateScreen, LeaveAlternateScreen,
    };

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, Hide)?;
    let mut writer = BufWriter::with_capacity(1 << 20, stdout.lock());

    // guard against degenerate terminal sizes (0x0 ptys, etc.)
    let norm = |w: u16, h: u16| -> (u16, u16) { (w.max(20), h.max(10)) };
    let (mut cols, mut rows) = {
        let (w, h) = size().unwrap_or((100, 40));
        norm(w, h)
    };
    let mut sim = Sim::new(img, sidecar, args, cols as usize, rows as usize);

    let frame_time = Duration::from_secs_f32(1.0 / args.fps.max(1.0));
    let start = Instant::now();
    let mut quit = false;

    'main: loop {
        let t0 = Instant::now();

        // drain events
        while poll(Duration::from_millis(1))? {
            match read()? {
                Event::Key(_) => {
                    quit = true;
                }
                Event::Resize(w, h) => {
                    let (w, h) = norm(w, h);
                    cols = w;
                    rows = h;
                    sim = Sim::new(img, sidecar, args, cols as usize, rows as usize);
                }
                _ => {}
            }
        }

        let dt = t0.elapsed().as_secs_f32().min(0.05);
        if sim.step(dt, args.loops) {
            break;
        }

        let frame = sim.frame();
        writer.write_all(render_ansi(&frame, sim.cols, sim.rows).as_bytes())?;
        writer.flush()?;

        if quit {
            break 'main;
        }
        if args.duration > 0.0 && start.elapsed().as_secs_f32() >= args.duration {
            break;
        }
        let spent = t0.elapsed();
        if spent < frame_time {
            std::thread::sleep(frame_time - spent);
        }
    }

    drop(writer);
    execute!(stdout, LeaveAlternateScreen, Show)?;
    disable_raw_mode()?;
    Ok(())
}

fn main() -> io::Result<()> {
    let args = Args::parse();
    let img = load_image(&args.image)?;
    let sidecar = load_sidecar_mask(&args.image);

    if let Some(out) = &args.anim {
        run_anim(&args, &img, sidecar.as_ref(), out)
    } else if let Some(out) = &args.screenshot {
        run_headless(&args, &img, sidecar.as_ref(), out)
    } else {
        run_terminal(&args, &img, sidecar.as_ref())
    }
}
