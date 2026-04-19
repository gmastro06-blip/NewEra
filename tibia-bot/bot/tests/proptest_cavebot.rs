//! proptest_cavebot — property-based tests del parser de cavebot.
//!
//! Complementan los 62 unit tests de `cavebot/parser.rs` generando inputs
//! random para capturar edge cases. Corren `cargo test --release
//! --test proptest_cavebot` con default 256 cases por property.
//!
//! ## Properties verificadas
//!
//! 1. **no_panic_on_random_toml**: un string random (TOML-like chars) nunca
//!    causa panic — solo `Err`. Captura bugs en el parser donde un TOML
//!    malformado dispara `unwrap()` o `expect()` silencioso.
//! 2. **no_panic_on_truncated_valid_script**: empezar con un script real,
//!    truncarlo en un punto random → parsearlo. Nunca debe panic. Captura
//!    bugs donde un TOML incompleto (fd cerrado prematuramente) crashea.
//! 3. **valid_assets_round_trip**: todos los scripts en `assets/cavebot/`
//!    deben parsear sin error. Regresión contra cambios en el parser.

use proptest::prelude::*;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use tibia_bot::cavebot::parser;

/// Tmp dir único per test invocation para no colisionar en CI paralelo.
fn tmp_dir() -> PathBuf {
    static C: AtomicU32 = AtomicU32::new(0);
    let seq = C.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir()
        .join(format!("proptest_cavebot_{}_{}", std::process::id(), seq));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Root del workspace (parent de `bot/`). `CARGO_MANIFEST_DIR` resuelve a
/// `tibia-bot/bot/` cuando cargo corre los tests; los assets están un nivel
/// arriba. Path absoluto evita que tests fallen según el cwd del runner.
fn assets_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("CARGO_MANIFEST_DIR sin parent")
        .join("assets")
}

proptest! {
    #![proptest_config(ProptestConfig {
        // 64 cases por property para mantener el suite <5s total.
        cases: 64,
        .. ProptestConfig::default()
    })]

    /// Property 1: TOML random nunca causa panic. Siempre `Err` o `Ok` —
    /// nada de `unwrap` panic en el path crítico.
    #[test]
    fn no_panic_on_random_toml(
        s in "[a-zA-Z0-9_= \\n\\[\\]\"\\.,\\-+/:]{0,500}"
    ) {
        let dir = tmp_dir();
        let path = dir.join("random.toml");
        fs::write(&path, &s).unwrap();
        // El parser puede retornar Err (caso esperado cuando el TOML es
        // inválido), pero NO debe panic.
        let _ = parser::load(&path, 30);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Property 2: truncar un script real en un byte random nunca panic.
    /// Simula un disk full mid-write o un fd cerrado prematuramente.
    #[test]
    fn no_panic_on_truncated_valid_script(
        trunc_ratio in 0.0f32..1.0f32
    ) {
        let source_path = assets_dir().join("cavebot/abdendriel_wasps.toml");
        let source = fs::read_to_string(&source_path)
            .unwrap_or_else(|e| panic!("{}: {}", source_path.display(), e));
        // Truncar a un byte boundary (importante porque UTF-8 multibyte chars
        // podrían partir en medio — file read en Rust valida UTF-8 on demand).
        let len = source.len();
        let cut = ((len as f32) * trunc_ratio) as usize;
        // Encontrar el char boundary más cercano <= cut.
        let mut boundary = cut.min(len);
        while boundary > 0 && !source.is_char_boundary(boundary) {
            boundary -= 1;
        }
        let truncated = &source[..boundary];

        let dir = tmp_dir();
        let path = dir.join("trunc.toml");
        fs::write(&path, truncated).unwrap();
        let _ = parser::load(&path, 30);
        let _ = fs::remove_dir_all(&dir);
    }
}

/// Property 3 (deterministic, no proptest needed): todos los scripts en
/// `assets/cavebot/` deben parsear al menos exitosamente (parseo TOML +
/// construcción Cavebot, excluyendo resolución de hunt_profile/templates).
/// Esto es regression contra cambios en el parser — si añadís un campo
/// required y olvidás default, algún script rompe aquí.
#[test]
fn all_asset_cavebots_parse() {
    let dir = assets_dir().join("cavebot");
    let mut failures = Vec::new();
    let mut checked = 0;

    for entry in fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {}", dir.display(), e))
    {
        let entry = entry.unwrap();
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        checked += 1;
        match parser::load(&p, 30) {
            Ok(_) => {}
            Err(e) => failures.push(format!("{}: {}", p.display(), e)),
        }
    }

    assert!(checked > 0, "no se encontraron .toml en assets/cavebot");
    assert!(
        failures.is_empty(),
        "{} script(s) fallan al parsear:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
