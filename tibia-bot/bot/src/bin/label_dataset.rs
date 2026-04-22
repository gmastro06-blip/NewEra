//! label_dataset — CLI tool de etiquetado para datasets de inventory crops (Fase 2.3).
//!
//! Lee un manifest.csv producido por `DatasetRecorder`, abre cada PNG sin
//! label asignado en el viewer del OS y prompts al usuario por la clase.
//! Salva el manifest periódicamente para no perder progreso.
//!
//! ## Uso
//!
//! ```bash
//! cargo run --release --bin label_dataset -- \
//!     --manifest datasets/abdendriel/manifest.csv \
//!     --classes vial,golden_backpack,green_backpack,white_key,dragon_ham,empty
//!
//! # Resume mode (skip filas con label ya asignado):
//! # Default behavior — siempre skipea labels existentes.
//!
//! # Re-label all (ignora labels existentes):
//! cargo run --release --bin label_dataset -- \
//!     --manifest datasets/abdendriel/manifest.csv \
//!     --classes vial,golden_backpack,empty \
//!     --relabel
//! ```
//!
//! ## Comandos durante el labeling
//!
//! - `0..9 / a-z` — número/letra del menú de clases mostrado
//! - `s` — skip (deja label vacío, próxima ejecución vuelve a preguntar)
//! - `?` — repite el menú de clases
//! - `q` — quit (salva progreso)
//! - `u` — undo (re-label el crop anterior)
//!
//! ## Diseño
//!
//! - Lee CSV en memoria (manifest típico < 1 MB).
//! - Por cada fila sin label: ejecuta `start <png>` (Win) o `xdg-open` (Linux)
//!   para abrir el PNG en el viewer default. Lee del stdin la respuesta del usuario.
//! - Salva el manifest cada 10 labels asignados Y al salir (graceful o quit).
//! - Append-safe: si el usuario interrumpe (Ctrl+C), el último auto-save
//!   tiene el progreso hasta los últimos 10 labels.

use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

const SAVE_EVERY: usize = 10;

fn main() {
    let args: Vec<String> = env::args().collect();
    let manifest_path = match arg_value(&args, "--manifest") {
        Some(p) => PathBuf::from(p),
        None => {
            eprintln!("Error: --manifest <path/to/manifest.csv> requerido");
            print_usage();
            std::process::exit(1);
        }
    };
    let classes_str = match arg_value(&args, "--classes") {
        Some(s) => s,
        None => {
            eprintln!("Error: --classes <c1,c2,c3,...> requerido");
            print_usage();
            std::process::exit(1);
        }
    };
    let classes: Vec<String> = classes_str
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if classes.is_empty() {
        eprintln!("Error: --classes vacío");
        std::process::exit(1);
    }
    let relabel = args.iter().any(|a| a == "--relabel");

    if !manifest_path.exists() {
        eprintln!("Manifest no existe: {}", manifest_path.display());
        std::process::exit(1);
    }
    let crops_dir = manifest_path.parent().map(|p| p.join("crops")).unwrap_or_else(|| PathBuf::from("crops"));
    if !crops_dir.exists() {
        eprintln!("Directorio de crops no existe: {} (esperado en mismo dir que el manifest)",
            crops_dir.display());
        std::process::exit(1);
    }

    let mut manifest = match Manifest::load(&manifest_path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Error cargando manifest: {}", e);
            std::process::exit(1);
        }
    };
    let total = manifest.rows.len();
    let already_labeled = manifest.rows.iter().filter(|r| !r.label.is_empty()).count();
    let to_label = if relabel { total } else { total - already_labeled };
    println!(
        "📊 Manifest: {} filas total, {} ya etiquetadas, {} por etiquetar.",
        total, already_labeled, to_label
    );
    if to_label == 0 {
        println!("✓ Nada por etiquetar. Salir.");
        return;
    }

    println!("📋 Clases disponibles ({}):", classes.len());
    print_class_menu(&classes);
    println!("Comandos: s=skip, u=undo, ?=menu, q=quit\n");

    let stdin = io::stdin();
    let mut handle = stdin.lock();
    let mut labeled_since_save = 0usize;
    let mut session_labeled = 0usize;
    let mut last_idx: Option<usize> = None;

    let indices: Vec<usize> = (0..manifest.rows.len())
        .filter(|&i| relabel || manifest.rows[i].label.is_empty())
        .collect();

    let mut i = 0;
    while i < indices.len() {
        let row_idx = indices[i];
        let row = &manifest.rows[row_idx];
        let png_path = crops_dir.join(&row.filename);
        if !png_path.exists() {
            eprintln!("⚠ PNG missing, skip: {}", png_path.display());
            i += 1;
            continue;
        }
        // Abrir PNG en viewer del OS.
        let _ = open_png(&png_path);
        // Prompt.
        print!(
            "[{}/{}] {} → label: ",
            i + 1, indices.len(), row.filename
        );
        let _ = io::stdout().flush();
        let mut line = String::new();
        match handle.read_line(&mut line) {
            Ok(0) => break, // EOF (Ctrl+D)
            Ok(_) => {}
            Err(e) => {
                eprintln!("stdin error: {}", e);
                break;
            }
        }
        let input = line.trim().to_lowercase();
        match input.as_str() {
            "q" => { println!("👋 Salir + salvar."); break; }
            "?" => {
                print_class_menu(&classes);
                continue;
            }
            "s" => {
                println!("  ↳ skip");
                i += 1;
                continue;
            }
            "u" => {
                if let Some(prev) = last_idx {
                    println!("  ↳ undo: re-etiquetando {}", manifest.rows[prev].filename);
                    manifest.rows[prev].label.clear();
                    // re-añadir el prev al inicio iteración: queremos volver a pedirlo
                    // Búsqueda en indices desde i hacia atrás.
                    if let Some(prev_pos) = indices.iter().position(|&x| x == prev) {
                        i = prev_pos;
                    }
                    continue;
                } else {
                    println!("  ↳ no hay undo disponible");
                    continue;
                }
            }
            other => {
                // Parse menu index (números o letras).
                let class = parse_class_input(other, &classes);
                if let Some(class) = class {
                    manifest.rows[row_idx].label = class.clone();
                    println!("  ↳ {}", class);
                    last_idx = Some(row_idx);
                    labeled_since_save += 1;
                    session_labeled += 1;
                    if labeled_since_save >= SAVE_EVERY {
                        if let Err(e) = manifest.save(&manifest_path) {
                            eprintln!("⚠ no se pudo salvar: {}", e);
                        } else {
                            println!("  💾 auto-save ({} desde último save)", labeled_since_save);
                            labeled_since_save = 0;
                        }
                    }
                    i += 1;
                } else {
                    eprintln!("  ⚠ input no reconocido: '{}'. Tipea ? para ver menú.", other);
                    continue;
                }
            }
        }
    }

    // Save final.
    match manifest.save(&manifest_path) {
        Ok(()) => println!("\n✓ Salvado: {} ({} etiquetados esta sesión)",
                            manifest_path.display(), session_labeled),
        Err(e) => eprintln!("\n⚠ falló save final: {}", e),
    }
    print_distribution(&manifest);
}

fn print_usage() {
    eprintln!("Uso:");
    eprintln!("  label_dataset --manifest <path> --classes <c1,c2,...> [--relabel]");
}

fn arg_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn print_class_menu(classes: &[String]) {
    for (i, c) in classes.iter().enumerate() {
        let key = menu_key(i);
        println!("  [{}] {}", key, c);
    }
    println!();
}

/// Devuelve la tecla del menú para el índice dado: 0..9 → '0'..'9', 10+ → 'a'..'z'.
fn menu_key(idx: usize) -> char {
    if idx < 10 {
        std::char::from_digit(idx as u32, 10).unwrap()
    } else {
        ((b'a' + (idx - 10) as u8) as char)
    }
}

/// Parsea input de menú: "0".."9" → idx 0..9, "a".."z" → idx 10..35.
/// También acepta el nombre completo de la clase para typo-tolerance.
fn parse_class_input(input: &str, classes: &[String]) -> Option<String> {
    let input = input.trim();
    if input.len() == 1 {
        let c = input.chars().next().unwrap();
        if let Some(d) = c.to_digit(10) {
            return classes.get(d as usize).cloned();
        }
        if c.is_ascii_lowercase() && c >= 'a' && c <= 'z' {
            let idx = 10 + (c as usize - 'a' as usize);
            return classes.get(idx).cloned();
        }
    }
    // Match completo de nombre de clase.
    classes.iter().find(|c| c.eq_ignore_ascii_case(input)).cloned()
}

fn open_png(path: &Path) -> io::Result<()> {
    #[cfg(target_os = "windows")]
    {
        Command::new("cmd")
            .args(["/C", "start", "", path.to_str().unwrap_or("")])
            .spawn()?;
    }
    #[cfg(target_os = "linux")]
    {
        Command::new("xdg-open").arg(path).spawn()?;
    }
    #[cfg(target_os = "macos")]
    {
        Command::new("open").arg(path).spawn()?;
    }
    Ok(())
}

fn print_distribution(manifest: &Manifest) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for r in &manifest.rows {
        let key = if r.label.is_empty() { "(unlabeled)" } else { r.label.as_str() };
        *counts.entry(key.to_string()).or_insert(0) += 1;
    }
    let mut entries: Vec<_> = counts.into_iter().collect();
    entries.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    println!("\n📊 Distribución actual:");
    for (label, n) in entries {
        let pct = 100.0 * n as f64 / manifest.rows.len() as f64;
        println!("  {:<25} {:>5}  ({:>5.1}%)", label, n, pct);
    }
}

// ── Manifest CSV (parser / serializer) ────────────────────────────────────────

#[derive(Debug, Clone)]
struct ManifestRow {
    filename:           String,
    tick:               String,
    slot_index:         String,
    frame_id:           String,
    captured_at_unix_ms:String,
    tag:                String,
    label:              String,
}

struct Manifest {
    header:  String,
    rows:    Vec<ManifestRow>,
}

impl Manifest {
    fn load(path: &Path) -> Result<Self, String> {
        let content = fs::read_to_string(path).map_err(|e| e.to_string())?;
        let mut lines = content.lines();
        let header = lines.next().unwrap_or("").to_string();
        if header.is_empty() {
            return Err("manifest vacío".into());
        }
        let mut rows = Vec::new();
        for (i, line) in lines.enumerate() {
            if line.trim().is_empty() { continue; }
            let cols: Vec<&str> = line.splitn(7, ',').collect();
            if cols.len() < 6 {
                return Err(format!("línea {}: columnas insuficientes ({} < 6)", i + 2, cols.len()));
            }
            // label puede no existir si el manifest fue producido por versión vieja.
            let label = cols.get(6).map(|s| s.to_string()).unwrap_or_default();
            rows.push(ManifestRow {
                filename:           cols[0].to_string(),
                tick:               cols[1].to_string(),
                slot_index:         cols[2].to_string(),
                frame_id:           cols[3].to_string(),
                captured_at_unix_ms:cols[4].to_string(),
                tag:                cols[5].to_string(),
                label,
            });
        }
        Ok(Self { header, rows })
    }

    fn save(&self, path: &Path) -> Result<(), String> {
        let mut out = String::new();
        out.push_str(&self.header);
        out.push('\n');
        for r in &self.rows {
            out.push_str(&format!(
                "{},{},{},{},{},{},{}\n",
                r.filename, r.tick, r.slot_index, r.frame_id,
                r.captured_at_unix_ms, r.tag, r.label
            ));
        }
        // Atomic write: temp + rename.
        let tmp = path.with_extension("csv.tmp");
        fs::write(&tmp, out).map_err(|e| e.to_string())?;
        fs::rename(&tmp, path).map_err(|e| e.to_string())?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_key_for_first_10_is_digit() {
        assert_eq!(menu_key(0), '0');
        assert_eq!(menu_key(9), '9');
    }

    #[test]
    fn menu_key_for_above_10_is_letter() {
        assert_eq!(menu_key(10), 'a');
        assert_eq!(menu_key(11), 'b');
        assert_eq!(menu_key(35), 'z');
    }

    #[test]
    fn parse_digit_input_returns_class_at_index() {
        let classes = vec!["a".into(), "b".into(), "c".into()];
        assert_eq!(parse_class_input("0", &classes), Some("a".into()));
        assert_eq!(parse_class_input("2", &classes), Some("c".into()));
        assert_eq!(parse_class_input("9", &classes), None); // out of range
    }

    #[test]
    fn parse_letter_input_returns_class_at_offset() {
        let classes: Vec<String> = (0..15).map(|i| format!("class_{}", i)).collect();
        assert_eq!(parse_class_input("a", &classes), Some("class_10".into()));
        assert_eq!(parse_class_input("e", &classes), Some("class_14".into()));
    }

    #[test]
    fn parse_full_name_input_works_case_insensitive() {
        let classes = vec!["VIAL".into(), "golden_backpack".into()];
        assert_eq!(parse_class_input("vial", &classes), Some("VIAL".into()));
        assert_eq!(parse_class_input("GOLDEN_BACKPACK", &classes), Some("golden_backpack".into()));
        assert_eq!(parse_class_input("unknown", &classes), None);
    }

    #[test]
    fn manifest_load_save_roundtrip() {
        let dir = std::env::temp_dir().join(format!("manifest_test_{}",
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("manifest.csv");
        let content = "filename,tick,slot_index,frame_id,captured_at_unix_ms,tag,label\n\
                       a.png,1,0,1,1000,test,vial\n\
                       b.png,2,1,2,2000,test,\n\
                       c.png,3,2,3,3000,test,empty\n";
        std::fs::write(&path, content).unwrap();
        let m = Manifest::load(&path).unwrap();
        assert_eq!(m.rows.len(), 3);
        assert_eq!(m.rows[0].label, "vial");
        assert_eq!(m.rows[1].label, "");
        assert_eq!(m.rows[2].label, "empty");
        // Roundtrip
        m.save(&path).unwrap();
        let m2 = Manifest::load(&path).unwrap();
        assert_eq!(m2.rows.len(), 3);
        assert_eq!(m2.rows[0].filename, "a.png");
        assert_eq!(m2.rows[1].label, "");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
