//! click_live — herramienta interactiva para validar clicks contra Tibia
//! con feedback rápido.
//!
//! Dispara **un click** vía el endpoint HTTP del bot (`POST /test/click`),
//! luego polea frames con `GET /test/grab` y hace template matching contra
//! un PNG para confirmar que el click tuvo el efecto esperado. Reporta
//! pass/fail en <1s.
//!
//! Reemplaza el ciclo `edit → build → restart → observar` de ~15 minutos
//! con un ciclo `click_live → leer output` de ~10 segundos.
//!
//! ## Ejemplo: validar que cerrar el chat de NPC funciona
//!
//! ```bash
//! cargo run --release --bin click_live -- \
//!     --coord 117,298 --button L \
//!     --verify-template npc_trade \
//!     --verify-roi 50,25,420,650 \
//!     --template-dir tibia-bot/assets/templates/prompts \
//!     --baseline-first
//! ```
//!
//! Con `--baseline-first`: captura un frame PRE-click y mide el baseline;
//! si el template ya matchea, el verify espera que **desaparezca** tras el
//! click. Si no matchea pre-click, espera que **aparezca**.
//!
//! ## Requisitos
//!
//! - El bot debe estar corriendo y en `is_paused = true` (el endpoint
//!   `/test/click` requiere pausa para no interferir con la FSM).
//! - El template PNG debe existir en `--template-dir`.
//! - Para que el verify sea significativo, el ROI del verify-template debe
//!   corresponder a la zona del frame donde el template aparecería.
//!
//! ## Exit codes
//! - 0 → PASS (verify observado dentro del timeout)
//! - 1 → FAIL (timeout sin observar el cambio esperado)
//! - 2 → error de configuración / bot unreachable / template faltante

use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, ValueEnum};
use image::GrayImage;
use imageproc::template_matching::{match_template, MatchTemplateMethod};
use serde_json::json;

// ── CLI ──────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(name = "click_live", about = "Dispara 1 click y verifica el efecto vía template matching sobre frames NDI")]
struct Cli {
    /// Dirección del bridge TCP (sólo informativa, los clicks van vía /test/click del bot).
    #[arg(long, default_value = "127.0.0.1:9000")]
    bridge: String,

    /// Coordenada viewport del click `x,y`.
    #[arg(long, value_parser = parse_xy)]
    coord: (i32, i32),

    /// Botón a presionar (L=left, R=right, M=middle).
    #[arg(long, default_value = "L", value_parser = parse_button)]
    button: String,

    /// Nombre del template PNG sin extensión (buscado en --template-dir).
    #[arg(long)]
    verify_template: String,

    /// ROI donde buscar el template, formato `x,y,w,h`. Si se omite, frame completo.
    #[arg(long, value_parser = parse_roi)]
    verify_roi: Option<Roi>,

    /// Timeout del verify en ms (cuánto tiempo polea antes de reportar fail).
    #[arg(long, default_value_t = 1500)]
    verify_within_ms: u64,

    /// Directorio que contiene `<verify-template>.png`.
    #[arg(long)]
    template_dir: PathBuf,

    /// Threshold SSE normalizado. Un match es válido cuando `best_score <= threshold`.
    #[arg(long, default_value_t = 0.15)]
    threshold: f32,

    /// URL HTTP del bot (expone /status, /test/click, /test/grab).
    #[arg(long, default_value = "http://localhost:8080")]
    bot_url: String,

    /// Captura un frame PRE-click para decidir si esperar aparición o desaparición.
    #[arg(long)]
    baseline_first: bool,

    /// Fuerza el modo de verify en vez de decidirlo desde el baseline.
    #[arg(long, value_enum)]
    expect: Option<Expect>,

    /// Intervalo entre polls en ms (cuánto esperar entre cada GET /test/grab).
    #[arg(long, default_value_t = 100)]
    poll_interval_ms: u64,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum Expect {
    /// Esperar a que el template APAREZCA (score <= threshold).
    Present,
    /// Esperar a que el template DESAPAREZCA (score > threshold).
    Absent,
}

#[derive(Debug, Clone, Copy)]
struct Roi {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

fn parse_xy(s: &str) -> Result<(i32, i32), String> {
    let mut it = s.split(',');
    let x: i32 = it.next().ok_or("missing x")?.trim().parse().map_err(|e: std::num::ParseIntError| e.to_string())?;
    let y: i32 = it.next().ok_or("missing y")?.trim().parse().map_err(|e: std::num::ParseIntError| e.to_string())?;
    if it.next().is_some() {
        return Err("esperaba exactamente 2 valores 'x,y'".into());
    }
    Ok((x, y))
}

fn parse_roi(s: &str) -> Result<Roi, String> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        return Err("ROI debe ser 'x,y,w,h'".into());
    }
    let parse_u32 = |p: &str| p.trim().parse::<u32>().map_err(|e| e.to_string());
    Ok(Roi {
        x: parse_u32(parts[0])?,
        y: parse_u32(parts[1])?,
        w: parse_u32(parts[2])?,
        h: parse_u32(parts[3])?,
    })
}

fn parse_button(s: &str) -> Result<String, String> {
    match s.to_uppercase().as_str() {
        "L" | "R" | "M" => Ok(s.to_uppercase()),
        other => Err(format!("botón inválido '{}', espera L|R|M", other)),
    }
}

// ── HTTP client (blocking) ───────────────────────────────────────────────────

struct BotClient {
    base:   String,
    client: reqwest::blocking::Client,
}

impl BotClient {
    fn new(base_url: &str) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .context("construyendo HTTP client")?;
        Ok(Self { base: base_url.trim_end_matches('/').to_string(), client })
    }

    /// GET /status con hasta 3 intentos de reintento (200/400/800 ms backoff).
    fn wait_status(&self) -> Result<serde_json::Value> {
        let mut last_err: Option<anyhow::Error> = None;
        let backoffs = [200u64, 400, 800];
        for (idx, wait) in backoffs.iter().enumerate() {
            match self.status_once() {
                Ok(v) => return Ok(v),
                Err(e) => {
                    last_err = Some(e);
                    if idx + 1 < backoffs.len() {
                        std::thread::sleep(Duration::from_millis(*wait));
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow!("status unreachable")))
    }

    fn status_once(&self) -> Result<serde_json::Value> {
        let url = format!("{}/status", self.base);
        let resp = self.client.get(&url).send().context("GET /status")?;
        if !resp.status().is_success() {
            bail!("GET /status → HTTP {}", resp.status());
        }
        let v: serde_json::Value = resp.json().context("parsing /status body")?;
        Ok(v)
    }

    /// POST /test/click con JSON body `{"x": .., "y": .., "button": ..}`.
    /// Retorna `ok` y la latencia reportada por el bot.
    fn click(&self, x: i32, y: i32, button: &str) -> Result<ClickReply> {
        let url = format!("{}/test/click", self.base);
        let body = json!({ "x": x, "y": y, "button": button });
        let resp = self.client.post(&url).json(&body).send().context("POST /test/click")?;
        if !resp.status().is_success() {
            bail!("POST /test/click → HTTP {}", resp.status());
        }
        let v: serde_json::Value = resp.json().context("parsing /test/click body")?;
        let ok = v.get("ok").and_then(|x| x.as_bool()).unwrap_or(false);
        let latency_ms = v.get("latency_ms").and_then(|x| x.as_f64()).unwrap_or(0.0);
        Ok(ClickReply { ok, latency_ms })
    }

    /// GET /test/grab — retorna PNG bytes del último frame NDI.
    fn grab(&self) -> Result<Vec<u8>> {
        let url = format!("{}/test/grab", self.base);
        let resp = self.client.get(&url).send().context("GET /test/grab")?;
        if !resp.status().is_success() {
            bail!("GET /test/grab → HTTP {}", resp.status());
        }
        let bytes = resp.bytes().context("reading /test/grab body")?;
        Ok(bytes.to_vec())
    }
}

struct ClickReply {
    ok:         bool,
    #[allow(dead_code)]
    latency_ms: f64,
}

// ── Template matching ────────────────────────────────────────────────────────

/// Carga un PNG como GrayImage (luma).
fn load_template_png(path: &Path) -> Result<GrayImage> {
    let img = image::open(path)
        .with_context(|| format!("abriendo template '{}'", path.display()))?;
    Ok(img.to_luma8())
}

/// Decodifica los bytes PNG del frame grabeado y convierte a GrayImage.
fn decode_png_to_gray(bytes: &[u8]) -> Result<GrayImage> {
    let img = image::load(Cursor::new(bytes), image::ImageFormat::Png)
        .context("decodificando PNG del frame")?;
    Ok(img.to_luma8())
}

/// Recorta un GrayImage al ROI dado (None = frame completo).
fn crop_gray(img: &GrayImage, roi: Option<Roi>) -> Option<GrayImage> {
    let Some(r) = roi else { return Some(img.clone()); };
    if r.w == 0 || r.h == 0 {
        return None;
    }
    if r.x + r.w > img.width() || r.y + r.h > img.height() {
        return None;
    }
    let mut out = GrayImage::new(r.w, r.h);
    for row in 0..r.h {
        for col in 0..r.w {
            let px = img.get_pixel(r.x + col, r.y + row);
            out.put_pixel(col, row, *px);
        }
    }
    Some(out)
}

/// Ejecuta template matching y retorna el mejor score (menor = mejor match
/// bajo SumOfSquaredErrorsNormalized).
fn best_score(patch: &GrayImage, template: &GrayImage) -> Option<f32> {
    if patch.width() < template.width() || patch.height() < template.height() {
        return None;
    }
    let result = match_template(patch, template, MatchTemplateMethod::SumOfSquaredErrorsNormalized);
    let best = result.iter().cloned().fold(f32::MAX, f32::min);
    Some(best)
}

/// Devuelve true si el score del template matchea por debajo del threshold.
fn matches(score: Option<f32>, threshold: f32) -> bool {
    score.map(|s| s <= threshold).unwrap_or(false)
}

// ── Main flow ────────────────────────────────────────────────────────────────

fn main() {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("error: {:#}", e);
            std::process::exit(2);
        }
    }
}

fn run(cli: Cli) -> Result<i32> {
    // 1. Validar template.
    let template_path = cli.template_dir.join(format!("{}.png", cli.verify_template));
    if !template_path.exists() {
        let hint = suggest_template(&cli.template_dir, &cli.verify_template);
        bail!(
            "template no encontrado: '{}'{}",
            template_path.display(),
            hint
        );
    }
    let template = load_template_png(&template_path)?;
    println!(
        "template: {} ({}×{} luma)",
        template_path.display(), template.width(), template.height()
    );

    // 2. Validar ROI: template debe caber.
    if let Some(r) = cli.verify_roi {
        if r.w < template.width() || r.h < template.height() {
            bail!(
                "ROI {}×{} más pequeño que template {}×{}",
                r.w, r.h, template.width(), template.height()
            );
        }
    }

    // 3. Bot up?
    let bot = BotClient::new(&cli.bot_url)?;
    let status = bot.wait_status().with_context(|| format!("bot en {} no responde (3 intentos)", cli.bot_url))?;
    let is_paused = status.get("is_paused").and_then(|x| x.as_bool()).unwrap_or(false);
    let has_frame = status.get("has_frame").and_then(|x| x.as_bool()).unwrap_or(false);
    println!(
        "bot: tick={} paused={} has_frame={} fsm={}",
        status.get("tick").and_then(|x| x.as_u64()).unwrap_or(0),
        is_paused, has_frame,
        status.get("fsm_state").and_then(|x| x.as_str()).unwrap_or("?"),
    );
    if !is_paused {
        bail!("bot NO está paused — POST /test/click lo rechaza. Llama POST /pause antes.");
    }
    if !has_frame {
        bail!("bot no tiene frame NDI — verify no podría comparar. Revisa OBS/DistroAV.");
    }
    println!("bridge (informativo): {}", cli.bridge);

    // 4. Baseline (opcional) — decide qué esperar post-click.
    let mut baseline_matched = false;
    if cli.baseline_first {
        println!("capturando baseline...");
        let bytes = bot.grab().context("grab baseline")?;
        let gray  = decode_png_to_gray(&bytes)?;
        let patch = crop_gray(&gray, cli.verify_roi)
            .ok_or_else(|| anyhow!("ROI fuera del frame"))?;
        let score = best_score(&patch, &template);
        baseline_matched = matches(score, cli.threshold);
        println!(
            "  baseline score={} match={}",
            fmt_score(score), baseline_matched
        );
    }

    // Decide expectation.
    let expect = cli.expect.unwrap_or(
        if cli.baseline_first && baseline_matched {
            Expect::Absent
        } else {
            Expect::Present
        }
    );
    println!("expect: {:?}", expect);

    // 5. Dispatch click.
    let reply = bot.click(cli.coord.0, cli.coord.1, &cli.button)?;
    if !reply.ok {
        bail!("POST /test/click retornó ok=false (revisa que el bot siga paused)");
    }
    let click_ts = Instant::now();
    println!(
        "click ENVIADO: ({}, {}) button={} bot_latency_ms={:.1}",
        cli.coord.0, cli.coord.1, cli.button, reply.latency_ms
    );

    // 6. Poll loop.
    let deadline      = click_ts + Duration::from_millis(cli.verify_within_ms);
    let poll_interval = Duration::from_millis(cli.poll_interval_ms.max(10));
    let mut best_seen: Option<f32> = None;
    let mut polls     = 0u32;

    println!();
    println!("poll (threshold={:.3}, timeout={}ms, interval={}ms)",
        cli.threshold, cli.verify_within_ms, cli.poll_interval_ms);

    loop {
        polls += 1;
        let t = click_ts.elapsed().as_millis() as u64;
        let bytes = bot.grab().context("grab durante poll")?;
        let gray  = decode_png_to_gray(&bytes)?;
        let patch = crop_gray(&gray, cli.verify_roi)
            .ok_or_else(|| anyhow!("ROI fuera del frame en poll"))?;
        let score = best_score(&patch, &template);
        let match_now = matches(score, cli.threshold);

        // Track best score seen (the smallest — closest to matching).
        if let Some(s) = score {
            if best_seen.map(|b| s < b).unwrap_or(true) {
                best_seen = Some(s);
            }
        }

        let condition_met = match expect {
            Expect::Present => match_now,
            Expect::Absent  => !match_now,
        };

        println!(
            "  t+{:4}ms  score={}  match={}  condition_met={}",
            t, fmt_score(score), match_now, condition_met
        );

        if condition_met {
            let latency_ms = click_ts.elapsed().as_millis();
            println!();
            println!(
                "RESULT: PASS — verify {:?} observado en {}ms ({} polls, best_score={})",
                expect, latency_ms, polls, fmt_score(best_seen)
            );
            return Ok(0);
        }

        let now = Instant::now();
        if now >= deadline {
            println!();
            println!(
                "RESULT: FAIL — {:?} NO observado en {}ms. best_score={} threshold={:.3} ({} polls)",
                expect, cli.verify_within_ms, fmt_score(best_seen), cli.threshold, polls
            );
            return Ok(1);
        }
        std::thread::sleep(poll_interval);
    }
}

fn fmt_score(s: Option<f32>) -> String {
    s.map(|v| format!("{:.4}", v)).unwrap_or_else(|| "n/a".into())
}

/// Genera un hint con los templates disponibles si el nombre está mal escrito.
fn suggest_template(dir: &Path, name: &str) -> String {
    if !dir.exists() {
        return format!(" (directorio '{}' no existe)", dir.display());
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return String::new();
    };
    let mut available: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("png") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                available.push(stem.to_string());
            }
        }
    }
    if available.is_empty() {
        return format!(" (no hay .png en '{}')", dir.display());
    }
    available.sort();
    // Hint: match parcial por prefijo/substring.
    let close: Vec<&String> = available.iter()
        .filter(|n| n.contains(name) || name.contains(n.as_str()))
        .collect();
    if !close.is_empty() {
        let hints: Vec<String> = close.iter().map(|s| s.to_string()).collect();
        format!(" (similar: {})", hints.join(", "))
    } else {
        format!(" (disponibles: {})", available.join(", "))
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GrayImage, Luma};

    #[test]
    fn parse_xy_ok() {
        assert_eq!(parse_xy("10,20").unwrap(), (10, 20));
        assert_eq!(parse_xy("-5, 100").unwrap(), (-5, 100));
    }

    #[test]
    fn parse_xy_err() {
        assert!(parse_xy("10").is_err());
        assert!(parse_xy("10,20,30").is_err());
        assert!(parse_xy("foo,bar").is_err());
    }

    #[test]
    fn parse_roi_ok() {
        let r = parse_roi("50,25,420,650").unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (50, 25, 420, 650));
    }

    #[test]
    fn parse_roi_err() {
        assert!(parse_roi("1,2,3").is_err());
        assert!(parse_roi("a,b,c,d").is_err());
    }

    #[test]
    fn parse_button_normalizes() {
        assert_eq!(parse_button("l").unwrap(), "L");
        assert_eq!(parse_button("R").unwrap(), "R");
        assert!(parse_button("X").is_err());
    }

    #[test]
    fn crop_gray_returns_full_when_no_roi() {
        let img = GrayImage::from_pixel(100, 100, Luma([128]));
        let out = crop_gray(&img, None).unwrap();
        assert_eq!(out.dimensions(), (100, 100));
    }

    #[test]
    fn crop_gray_returns_none_when_roi_out_of_bounds() {
        let img = GrayImage::from_pixel(100, 100, Luma([128]));
        let bad = Roi { x: 90, y: 0, w: 50, h: 10 }; // overflow
        assert!(crop_gray(&img, Some(bad)).is_none());
    }

    #[test]
    fn crop_gray_correct_region() {
        let mut img = GrayImage::new(10, 10);
        // poner (5,5) en luma 200
        img.put_pixel(5, 5, Luma([200]));
        let roi = Roi { x: 4, y: 4, w: 3, h: 3 };
        let out = crop_gray(&img, Some(roi)).unwrap();
        assert_eq!(out.dimensions(), (3, 3));
        // En coords locales, (5,5) del original está en (1,1) del crop.
        assert_eq!(out.get_pixel(1, 1).0[0], 200);
    }

    #[test]
    fn best_score_self_match_is_zero() {
        // SumOfSquaredErrorsNormalized: template contra sí mismo → 0.
        let template = GrayImage::from_pixel(8, 8, Luma([150]));
        let score = best_score(&template, &template).unwrap();
        assert!(score < 1e-5, "score={}", score);
    }

    #[test]
    fn best_score_returns_none_when_template_bigger() {
        let patch = GrayImage::from_pixel(5, 5, Luma([0]));
        let template = GrayImage::from_pixel(10, 10, Luma([0]));
        assert!(best_score(&patch, &template).is_none());
    }

    #[test]
    fn matches_respects_threshold() {
        assert!(matches(Some(0.05), 0.15));
        assert!(matches(Some(0.15), 0.15));
        assert!(!matches(Some(0.20), 0.15));
        assert!(!matches(None, 0.15));
    }

    /// Synthetic frame where the template APPEARS inside a larger patch.
    /// Verifies template matching finds it and best_score is ~0.
    #[test]
    fn template_present_in_frame() {
        // Frame 50×50 con valor base 100.
        let mut frame = GrayImage::from_pixel(50, 50, Luma([100]));
        // Stamp un rectángulo 10×10 con valor 200 en (20,20).
        for dy in 0..10 {
            for dx in 0..10 {
                frame.put_pixel(20 + dx, 20 + dy, Luma([200]));
            }
        }
        // Template = exactamente ese rectángulo.
        let template = GrayImage::from_pixel(10, 10, Luma([200]));
        let score = best_score(&frame, &template).unwrap();
        assert!(score < 1e-5, "expected ~0, got {}", score);
        assert!(matches(Some(score), 0.15));
    }

    /// Frame que NO contiene el template — score debe ser alto.
    #[test]
    fn template_absent_in_frame() {
        let frame    = GrayImage::from_pixel(50, 50, Luma([100]));
        let template = GrayImage::from_pixel(10, 10, Luma([200]));
        let score    = best_score(&frame, &template).unwrap();
        assert!(score > 0.1, "expected high SSE, got {}", score);
    }

    /// Verifica que `suggest_template` no panic con dir inexistente.
    #[test]
    fn suggest_template_handles_missing_dir() {
        let hint = suggest_template(Path::new("/nonexistent/xyz"), "foo");
        assert!(hint.contains("no existe"));
    }

    /// Verifica el fmt_score helper.
    #[test]
    fn fmt_score_format() {
        assert_eq!(fmt_score(Some(0.1234)), "0.1234");
        assert_eq!(fmt_score(None), "n/a");
    }
}
