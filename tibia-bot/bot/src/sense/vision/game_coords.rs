/// game_coords.rs — Posicionamiento absoluto por tile-hashing del minimap.
///
/// Compara un patch del minimap capturado (NDI) contra un índice pre-computado
/// de los archivos de minimap de Tibia (Minimap_Color_*.png) para determinar
/// la coordenada absoluta (x, y, z) del personaje.
///
/// ## Algoritmo
///
/// 1. Tomar un patch 32×32 de la esquina superior-izquierda del minimap
///    (lejos del crosshair central del jugador).
/// 2. Computar un difference hash (dHash) de 8×8 = 64 bits.
/// 3. Buscar en el `MapIndex` por match exacto o hamming distance ≤ 3.
/// 4. Validar con un segundo patch de otra esquina (anti-colisión).

use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use image::GrayImage;
use imageproc::template_matching::{match_template_parallel, MatchTemplateMethod};
use serde::{Deserialize, Serialize};

use crate::sense::perception::MinimapSnapshot;

// ── Constantes ────────────────────────────────────────────────────────────────

/// Número de TILES que un patch representa (no pixels directos).
///
/// - En los PNGs de referencia (TibiaMaps.io), 1 pixel = 1 tile,
///   así que el patch se extrae como `PATCH_TILES × PATCH_TILES` pixels.
/// - En el minimap NDI runtime, 1 tile = `ndi_tile_scale` pixels (típicamente 5),
///   así que el patch se extrae como `(PATCH_TILES * scale) × (PATCH_TILES * scale)`
///   pixels y se downsamplea a `PATCH_TILES × PATCH_TILES` antes de hashear.
///
/// Un valor de 16 tiles da 256 bits de información (16×16) reducido a 64 bits
/// de hash — suficientemente distintivo y compacto. Patches de 16 tiles son
/// suficientemente pequeños para caber en cualquier render del minimap Tibia.
pub const PATCH_TILES: usize = 16;

/// Tamaño del dHash reducido (HASH_DIM × HASH_DIM+1 → HASH_DIM² bits).
const HASH_DIM: usize = 8;

/// Máxima hamming distance aceptable para un match.
#[allow(dead_code)] // legacy dHash, superseded by MinimapMatcher
const MAX_HAMMING: u32 = 3;

/// Stride entre patches al indexar (overlap 50% con PATCH_TILES=16).
#[allow(dead_code)]
pub const INDEX_STRIDE: usize = 8;

/// Tamaño de los archivos PNG del minimap de Tibia (256×256 tiles).
#[allow(dead_code)]
const TILE_FILE_SIZE: i32 = 256;

// ── dHash ─────────────────────────────────────────────────────────────────────

/// Reduce un patch BGRA a escala de grises, redimensiona a 9×8, y computa
/// un difference hash de 64 bits (8×8 comparaciones horizontales).
pub fn dhash(data: &[u8], width: usize, height: usize) -> u64 {
    if data.len() < width * height * 4 || width == 0 || height == 0 {
        return 0;
    }
    // 1. Convertir a luma (Y = 0.299R + 0.587G + 0.114B) — asumiendo BGRA.
    let mut gray = vec![0u8; width * height];
    for i in 0..(width * height) {
        let b = data[i * 4] as f32;
        let g = data[i * 4 + 1] as f32;
        let r = data[i * 4 + 2] as f32;
        gray[i] = (0.114 * b + 0.587 * g + 0.299 * r) as u8;
    }
    // 2. Nearest-neighbor resize a (HASH_DIM+1) × HASH_DIM = 9×8.
    let rw = HASH_DIM + 1; // 9
    let rh = HASH_DIM;     // 8
    let mut resized = vec![0u8; rw * rh];
    for ry in 0..rh {
        for rx in 0..rw {
            let sx = rx * width / rw;
            let sy = ry * height / rh;
            resized[ry * rw + rx] = gray[sy * width + sx];
        }
    }
    // 3. dHash: comparar pixel[x] > pixel[x+1] por fila → 8 bits/fila × 8 filas.
    let mut hash: u64 = 0;
    for ry in 0..rh {
        for rx in 0..HASH_DIM {
            hash <<= 1;
            if resized[ry * rw + rx] > resized[ry * rw + rx + 1] {
                hash |= 1;
            }
        }
    }
    hash
}

/// Hamming distance entre dos hashes de 64 bits.
#[inline]
#[allow(dead_code)] // legacy dHash, used only by MapIndex::lookup_fuzzy + tests
pub fn hamming(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

// ── MapIndex ──────────────────────────────────────────────────────────────────

/// Posición absoluta en el mundo de Tibia.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MapPos {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

/// Índice de hashes de patches del minimap → posiciones.
/// Pre-computado offline desde los archivos Minimap_Color_*.png.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MapIndex {
    /// hash → lista de posiciones candidatas.
    entries: HashMap<u64, Vec<MapPos>>,
    /// Número total de patches indexados.
    pub total_patches: usize,
}

// NOTA 2026-04-15: MapIndex / dHash están en modo legacy. No se usan en
// Vision::tick porque dHash es frágil al anti-aliasing del cliente Tibia
// 12 (min hamming 14-20 bits vs threshold 3). Reemplazado por MinimapMatcher
// (SSDNormalized template matching) al final del módulo. Los símbolos siguen
// por compat con build_map_index bin, debug_game_coords bin, y tests.
#[allow(dead_code)]
impl MapIndex {
    pub fn new() -> Self { Self::default() }

    /// Inserta un hash → posición en el índice.
    pub fn insert(&mut self, hash: u64, pos: MapPos) {
        self.entries.entry(hash).or_default().push(pos);
        self.total_patches += 1;
    }

    /// Busca matches por hash exacto.
    pub fn lookup_exact(&self, hash: u64) -> &[MapPos] {
        self.entries.get(&hash).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Busca matches con hamming distance ≤ max_dist.
    /// Para max_dist=0 usa lookup_exact (O(1)). Para >0, itera todo (O(n)).
    pub fn lookup_fuzzy(&self, hash: u64, max_dist: u32) -> Vec<&MapPos> {
        if max_dist == 0 {
            return self.lookup_exact(hash).iter().collect();
        }
        let mut results = Vec::new();
        for (h, positions) in &self.entries {
            if hamming(hash, *h) <= max_dist {
                results.extend(positions.iter());
            }
        }
        results
    }

    /// Serializa el índice a bytes (bincode).
    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        let data = bincode::serialize(self)?;
        std::fs::write(path, data)?;
        Ok(())
    }

    /// Carga el índice desde un archivo .bin.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let data = std::fs::read(path)?;
        let index: Self = bincode::deserialize(&data)?;
        Ok(index)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Retorna un iterador sobre todas las entradas del índice.
    /// Expuesto para tools de debug (escaneo de hamming mínimo).
    pub fn all_entries(&self) -> &HashMap<u64, Vec<MapPos>> {
        &self.entries
    }
}

// ── Index builder ─────────────────────────────────────────────────────────────

/// Parsea el filename de un PNG de minimap de Tibia para extraer coords.
/// Formato: `Minimap_Color_{x}_{y}_{z}.png`
#[allow(dead_code)]
pub fn parse_minimap_filename(filename: &str) -> Option<(i32, i32, i32)> {
    let stem = filename.strip_suffix(".png")?;
    let parts: Vec<&str> = stem.split('_').collect();
    // Minimap_Color_{x}_{y}_{z}
    if parts.len() != 5 || parts[0] != "Minimap" || parts[1] != "Color" {
        return None;
    }
    let x: i32 = parts[2].parse().ok()?;
    let y: i32 = parts[3].parse().ok()?;
    let z: i32 = parts[4].parse().ok()?;
    Some((x, y, z))
}

/// Indexa un archivo PNG de minimap de Tibia.
/// Extrae patches de PATCH_TILES×PATCH_TILES pixels (= N tiles) con stride INDEX_STRIDE.
///
/// Como los reference PNGs son 1 px/tile, el patch size en píxeles iguala
/// al número de tiles: `PATCH_TILES` pixels = `PATCH_TILES` tiles.
#[allow(dead_code)]
pub fn index_minimap_png(
    path: &Path,
    file_x: i32,
    file_y: i32,
    z: i32,
    index: &mut MapIndex,
) -> anyhow::Result<usize> {
    let img = image::open(path)?;
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as usize, rgba.height() as usize);

    if w < PATCH_TILES || h < PATCH_TILES {
        return Ok(0);
    }

    let mut count = 0;
    let mut py = 0;
    while py + PATCH_TILES <= h {
        let mut px = 0;
        while px + PATCH_TILES <= w {
            // Extraer patch RGBA.
            let mut patch = vec![0u8; PATCH_TILES * PATCH_TILES * 4];
            for row in 0..PATCH_TILES {
                let src_offset = ((py + row) * w + px) * 4;
                let dst_offset = row * PATCH_TILES * 4;
                patch[dst_offset..dst_offset + PATCH_TILES * 4]
                    .copy_from_slice(&rgba.as_raw()[src_offset..src_offset + PATCH_TILES * 4]);
            }

            // Ignorar patches completamente magenta (inexplorado: #FF00FF).
            let all_magenta = patch.chunks_exact(4).all(|p| p[0] >= 250 && p[1] == 0 && p[2] >= 250);
            if !all_magenta {
                let hash = dhash(&patch, PATCH_TILES, PATCH_TILES);
                let pos = MapPos {
                    x: file_x + px as i32,
                    y: file_y + py as i32,
                    z,
                };
                index.insert(hash, pos);
                count += 1;
            }

            px += INDEX_STRIDE;
        }
        py += INDEX_STRIDE;
    }
    Ok(count)
}

// ── Runtime detection ─────────────────────────────────────────────────────────

/// Extrae un patch BGRA del minimap NDI Y lo downsamplea a la escala del index.
///
/// Lee una región de `patch_px × patch_px` pixels del snapshot (donde
/// `patch_px = PATCH_TILES * ndi_tile_scale`), y la downsamplea a
/// `PATCH_TILES × PATCH_TILES` promediando bloques de `scale × scale` pixels.
///
/// Retorna el patch downsampleado como `PATCH_TILES² * 4` bytes BGRA, listo
/// para hashear con `dhash`.
#[allow(dead_code)] // legacy dHash path, only called by detect_position
fn extract_and_downsample(
    snap: &MinimapSnapshot,
    px: usize,
    py: usize,
    ndi_tile_scale: u32,
) -> Option<Vec<u8>> {
    let scale = ndi_tile_scale.max(1) as usize;
    let patch_px = PATCH_TILES * scale;
    if px + patch_px > snap.width as usize || py + patch_px > snap.height as usize {
        return None;
    }

    let stride_bytes = snap.width as usize * 4;
    let mut out = vec![0u8; PATCH_TILES * PATCH_TILES * 4];

    // Para cada tile (i, j) en la grilla PATCH_TILES × PATCH_TILES:
    // promediar los `scale × scale` pixels del snapshot correspondiente.
    for j in 0..PATCH_TILES {
        for i in 0..PATCH_TILES {
            let mut sum_b = 0u32;
            let mut sum_g = 0u32;
            let mut sum_r = 0u32;
            let mut sum_a = 0u32;
            let n = (scale * scale) as u32;
            for dy in 0..scale {
                for dx in 0..scale {
                    let sx = px + i * scale + dx;
                    let sy = py + j * scale + dy;
                    let src_off = sy * stride_bytes + sx * 4;
                    sum_b += snap.data[src_off] as u32;
                    sum_g += snap.data[src_off + 1] as u32;
                    sum_r += snap.data[src_off + 2] as u32;
                    sum_a += snap.data[src_off + 3] as u32;
                }
            }
            let dst_off = (j * PATCH_TILES + i) * 4;
            out[dst_off]     = (sum_b / n) as u8;
            out[dst_off + 1] = (sum_g / n) as u8;
            out[dst_off + 2] = (sum_r / n) as u8;
            out[dst_off + 3] = (sum_a / n) as u8;
        }
    }
    Some(out)
}

/// Detecta la posición absoluta del jugador comparando el minimap contra el índice.
///
/// Extrae patches del NDI minimap (downsampleados a escala 1 px/tile), computa
/// dHash y busca en el MapIndex. Con `ndi_tile_scale`, el patch extraído del
/// minimap tiene `PATCH_TILES * ndi_tile_scale` pixels cada lado y se downsamplea
/// a `PATCH_TILES × PATCH_TILES` antes de hashear, matching la escala del index.
///
/// Retorna `None` si no hay match, el minimap es demasiado pequeño, o el index
/// está vacío.
#[allow(dead_code)] // legacy dHash-based detect, superseded by MinimapMatcher
pub fn detect_position(
    snap: &MinimapSnapshot,
    index: &MapIndex,
    ndi_tile_scale: u32,
) -> Option<(i32, i32, i32)> {
    let scale = ndi_tile_scale.max(1) as usize;
    let patch_px = PATCH_TILES * scale;
    if index.is_empty()
        || snap.width < patch_px as u32
        || snap.height < patch_px as u32
    {
        return None;
    }

    // Patch de esquina superior-izquierda (offset 0,0) — lejos del crosshair central.
    let patch = extract_and_downsample(snap, 0, 0, ndi_tile_scale)?;
    let hash = dhash(&patch, PATCH_TILES, PATCH_TILES);

    // Buscar match exacto primero, luego fuzzy.
    let exact = index.lookup_exact(hash);
    let candidates = if !exact.is_empty() {
        exact.iter().collect::<Vec<_>>()
    } else {
        index.lookup_fuzzy(hash, MAX_HAMMING)
    };

    if candidates.is_empty() {
        return None;
    }

    // Si hay múltiples candidatos, validar con un segundo patch (esquina opuesta).
    // El offset en TILES entre los 2 patches es `(width-patch_px)/scale` = tiles.
    if candidates.len() > 1
        && snap.width >= (patch_px * 2) as u32
        && snap.height >= (patch_px * 2) as u32
    {
        let px2 = snap.width as usize - patch_px;
        let py2 = snap.height as usize - patch_px;
        if let Some(patch2) = extract_and_downsample(snap, px2, py2, ndi_tile_scale) {
            let hash2 = dhash(&patch2, PATCH_TILES, PATCH_TILES);
            // El offset en tiles (no pixels) entre los 2 patches.
            let offset_tiles_x = (px2 / scale) as i32;
            let offset_tiles_y = (py2 / scale) as i32;
            for cand in &candidates {
                let expected_x = cand.x + offset_tiles_x;
                let expected_y = cand.y + offset_tiles_y;
                let exact2 = index.lookup_exact(hash2);
                if exact2.iter().any(|p| p.x == expected_x && p.y == expected_y && p.z == cand.z) {
                    // El centro del minimap es el jugador.
                    // En tiles: cand.x + (snap.width/2) / scale
                    let center_x = cand.x + (snap.width as i32 / 2) / scale as i32;
                    let center_y = cand.y + (snap.height as i32 / 2) / scale as i32;
                    return Some((center_x, center_y, cand.z));
                }
            }
        }
    }

    // Single candidate o fallback: usar el primer match.
    let pos = candidates[0];
    let center_x = pos.x + (snap.width as i32 / 2) / scale as i32;
    let center_y = pos.y + (snap.height as i32 / 2) / scale as i32;
    Some((center_x, center_y, pos.z))
}

// ── Tracking híbrido helpers ─────────────────────────────────────────────────

/// Aplica un displacement (en pixels del minimap NDI) a un coord de mundo,
/// usando un acumulador sub-tile para precisión cuando el displacement es
/// menor que `ndi_tile_scale` pixels.
///
/// Retorna `(new_coord, new_accum_px)` donde:
/// - `new_coord` es el coord actualizado (solo cambia al completar 1+ tiles)
/// - `new_accum_px` es el remainder pixel-offset que NO completó 1 tile
///
/// # Ejemplos
///
/// ```
/// use tibia_bot::sense::vision::game_coords::apply_displacement;
///
/// // scale=2 px/tile, shift de 2 px al este → +1 tile east
/// let (coord, accum) = apply_displacement(
///     (100, 200, 7),
///     (0, 0),
///     (2, 0),
///     2,
/// );
/// assert_eq!(coord, (101, 200, 7));
/// assert_eq!(accum, (0, 0));
///
/// // scale=2, shift de solo 1 px → no cambia coord, acumula
/// let (coord, accum) = apply_displacement(
///     (100, 200, 7),
///     (0, 0),
///     (1, 0),
///     2,
/// );
/// assert_eq!(coord, (100, 200, 7));
/// assert_eq!(accum, (1, 0));
///
/// // Accumulated de 1 + nuevo shift 1 = 2 → +1 tile east
/// let (coord, accum) = apply_displacement(
///     (100, 200, 7),
///     (1, 0),
///     (1, 0),
///     2,
/// );
/// assert_eq!(coord, (101, 200, 7));
/// assert_eq!(accum, (0, 0));
/// ```
pub fn apply_displacement(
    last_coord: (i32, i32, i32),
    accum_px: (i32, i32),
    displacement_px: (i32, i32),
    ndi_tile_scale: u32,
) -> ((i32, i32, i32), (i32, i32)) {
    let scale = ndi_tile_scale.max(1) as i32;
    let new_accum_x = accum_px.0 + displacement_px.0;
    let new_accum_y = accum_px.1 + displacement_px.1;
    let delta_tiles_x = new_accum_x / scale;
    let delta_tiles_y = new_accum_y / scale;
    let new_coord = (
        last_coord.0 + delta_tiles_x,
        last_coord.1 + delta_tiles_y,
        last_coord.2,
    );
    let remainder = (new_accum_x % scale, new_accum_y % scale);
    (new_coord, remainder)
}

// ── MinimapMatcher (CCORR fallback) ───────────────────────────────────────────
//
// dHash-based detection falla con el cliente Tibia 12 porque el anti-aliasing
// del renderer altera suficientes pixels para que 2 patches "iguales" en área
// del mundo produzcan hashes diferentes (observado min hamming 14-20 bits vs
// threshold 3). SSD normalized template matching es MUCHO más robusto porque:
// - Opera en luma (ignora color shifts del NDI/OBS)
// - Promedia sobre un área grande (107×110 pixels → ~21×22 tiles) que absorbe
//   ruido local
// - Retorna un score continuo en [0, 1] con decisión clara por threshold
//
// **Costo**: 1 match_template sobre un reference 256×256 es ~5-10ms. Con
// brute-force sobre todos los PNGs de un piso (~220), 1-2 seg por detección.
// Inaceptable para 30Hz. MITIGACIÓN: usar `last_known` para limitar el search
// a ~9 PNGs adyacentes (el sector actual + 8 vecinos), bajando a ~50-100ms por
// detección.
//
// **Uso**: se carga desde `assets/minimap/minimap/` vía `load_dir(floors)` al
// boot. Luego `detect` se llama como fallback del dHash en Vision::tick.

/// Máximo desplazamiento XY en tiles permitido entre dos detecciones
/// consecutivas en el mismo piso. Usado por `validate_jump` como
/// physical-motion sanity filter para rechazar false positives.
///
/// Valor: 30 tiles / 500ms ≈ 60 tiles/s. Referencias de movimiento:
/// - Walking: ~10 tiles/s (sin haste)
/// - Utani hur: ~15 tiles/s
/// - Utani gran hur: ~25 tiles/s
///
/// Margen ~2× sobre el máximo esperado para absorber jitter de timing
/// + drift del detect interval bajo carga.
pub const MAX_JUMP_PER_DETECT: i32 = 30;

/// Physical-motion sanity filter sobre el output del `MinimapMatcher::detect()`.
///
/// Rechaza detecciones inconsistentes con el movimiento real del personaje:
/// un char no puede saltar >`MAX_JUMP_PER_DETECT` tiles entre dos detecciones
/// (intervalo ~500ms) caminando normal. Si el matcher devuelve un coord
/// físicamente imposible, probablemente es un false positive (validado
/// live 2026-04-17: matcher elegía un sector diagonal ~370 tiles off del
/// seed en Ab'dendriel depot).
///
/// Reglas:
/// - Si `detected` es `None` → retorna `None` (passthrough).
/// - Si `force_full=true` → bypassa el filtro (re-validación explícita,
///   acepta drift grande tras muerte/despawn).
/// - Si `last` es `None` → no hay baseline, acepta (cold boot sin seed).
/// - Si `detected.z != last.z` → cross-floor. Ladder/rope drops al char
///   cerca del mismo XY (dx,dy ≤ MAX_XY_ON_FLOOR_CHANGE tiles). Si el
///   jump XY es grande + z diff → false positive del matcher (validado
///   live 2026-04-18: saltó (32679,31684,6) → (32565,31779,7), dx=114,
///   dy=95, imposible sin teleport). Solo aceptar cross-floor si XY
///   está cerca del último known.
/// - Si `|dx| > MAX_JUMP OR |dy| > MAX_JUMP` en el mismo piso → rechaza (`None`).
/// - Sino → acepta (`Some(detected)`).
pub fn validate_jump(
    last:      Option<(i32, i32, i32)>,
    detected:  Option<(i32, i32, i32)>,
    force_full: bool,
) -> Option<(i32, i32, i32)> {
    let d = detected?;
    // 2026-04-18 fix live: removido el `if force_full { return Some(d); }`
    // — la auto-revalidación cada N detects generaba un hole en el filter:
    // el matcher podía jumpear a un sector false-positive sin validation,
    // actualizar last_game_coords, y los narrow siguientes se quedaban ahí.
    //
    // Ahora: si last es Some, SIEMPRE validamos. El `force_full` solo
    // controla el scope del SEARCH en el matcher (narrow vs brute force),
    // pero el FILTER se aplica igual. Cold boot (last=None) sigue bypass
    // porque no hay baseline contra qué validar.
    //
    // Trade-off: si el char verdaderamente teleporta (hot_portal, rescue
    // post-death), el filter bloquea hasta que el bot reinicie o el user
    // actualice el seed. Para farming en area fija (Ab'dendriel), strict
    // es la ventana correcta.
    let _ = force_full; // kept para backwards compat de la API
    let Some(l) = last else {
        return Some(d);
    };
    if d.2 != l.2 {
        // Cross-floor: ladder/rope deja al char casi en el mismo XY. Un
        // jump grande de XY al cambiar piso es señal de false positive
        // del matcher, no navegación real.
        const MAX_XY_ON_FLOOR_CHANGE: i32 = 3;
        let dx = (d.0 - l.0).abs();
        let dy = (d.1 - l.1).abs();
        if dx <= MAX_XY_ON_FLOOR_CHANGE && dy <= MAX_XY_ON_FLOOR_CHANGE {
            return Some(d); // legitimate ladder/rope
        }
        return None; // reject: z change + big xy jump = false positive
    }
    let dx = (d.0 - l.0).abs();
    let dy = (d.1 - l.1).abs();
    if dx > MAX_JUMP_PER_DETECT || dy > MAX_JUMP_PER_DETECT {
        return None;
    }
    Some(d)
}

/// Entry de reference en el atlas: el GrayImage del PNG + sus coords world.
#[allow(dead_code)]
pub struct ReferenceSector {
    pub file_x:  i32,
    pub file_y:  i32,
    pub z:       i32,
    pub image:   GrayImage,
}

/// Stats runtime del matcher, con interior mutability para que detect()
/// pueda actualizarlos con `&self` desde Vision::tick sin lock.
///
/// Accesibles vía `MinimapMatcher::stats_snapshot()` para diagnósticos
/// (HTTP endpoint, Prometheus export).
#[derive(Default)]
pub struct MatcherStats {
    /// Cantidad de narrow searches completados (con last_known, 9 sectores).
    pub narrow_searches: AtomicU64,
    /// Cantidad de full brute force completados (cold start + re-validations).
    pub full_searches:   AtomicU64,
    /// Cantidad de llamadas a detect() que retornaron None (no match).
    pub misses:          AtomicU64,
    /// Cantidad de candidatos top-K descartados por disambiguation
    /// (segundo patch no confirmó). Útil para monitorear false positives.
    pub disambiguation_rejects: AtomicU64,
    /// Cantidad de llamadas a detect() donde disambiguation devolvió None
    /// tras rechazar TODOS los candidatos top-K.
    pub disambiguation_misses: AtomicU64,
    /// Última duración del detect, en microsegundos (para gauge).
    pub last_duration_us: AtomicU64,
    /// Último score SSD del match (bits de f32). 0 si nunca detectó.
    pub last_score_bits: AtomicU32,
}

/// Snapshot serializable de stats para exponer vía HTTP/Prometheus.
#[derive(Debug, Clone, serde::Serialize)]
pub struct MatcherStatsSnapshot {
    pub narrow_searches:  u64,
    pub full_searches:    u64,
    pub misses:           u64,
    /// Candidates rechazados por segunda verificación (sum sobre todos los detect()).
    pub disambiguation_rejects: u64,
    /// Detect() que retornaron None exclusivamente por disambiguation.
    pub disambiguation_misses:  u64,
    pub total_detects:    u64,
    pub last_duration_ms: f64,
    pub last_score:       f32,
    pub sectors_loaded:   usize,
    pub floors_loaded:    Vec<i32>,
    pub match_threshold:  f32,
    pub disambiguation_enabled: bool,
}

/// Atlas de reference PNGs indexado por piso. Para cada piso, mantiene la
/// lista de sectores (cada sector = 1 PNG del minimap de Tibia).
///
/// # Uso básico
///
/// ```no_run
/// use std::path::Path;
/// use tibia_bot::sense::vision::game_coords::MinimapMatcher;
/// use tibia_bot::sense::perception::MinimapSnapshot;
///
/// let mut matcher = MinimapMatcher::new();
///
/// // Cargar reference PNGs para los pisos 6, 7, 8 (Ab'dendriel area).
/// // Cada piso consume ~15 MB de RAM.
/// matcher.load_dir(
///     Path::new("assets/minimap/minimap"),
///     &[6, 7, 8],
/// ).expect("load reference PNGs");
///
/// // En runtime (cada detect_interval frames), detectar posición.
/// // snap es un MinimapSnapshot capturado del NDI frame.
/// # let snap = MinimapSnapshot { width: 107, height: 110, data: vec![0; 107*110*4] };
/// # let last_known = None;
/// # let force_full = false;
/// let ndi_tile_scale = 2; // empírico para Tibia 12
/// if let Some((x, y, z)) = matcher.detect(&snap, ndi_tile_scale, last_known, force_full) {
///     println!("char at ({}, {}, {})", x, y, z);
/// }
/// ```
///
/// # Narrow vs Full search
///
/// - **Narrow** (fast, ~80-160ms): solo matchea contra el sector del
///   `last_known` y sus 8 vecinos. Usa `force_full=false` + `last_known=Some(...)`.
/// - **Full** (slow, ~1-4s): brute force sobre todos los sectores cargados.
///   Se usa en cold boot (`last_known=None`) y en re-validación periódica
///   (`force_full=true`) para recuperar falsos positivos.
///
/// # Disambiguation de falsos positivos
///
/// El SSD raw puede producir múltiples candidatos con score similar en
/// regiones visualmente parecidas del mapa (ej. 2 depots con la misma
/// estructura). Para rechazar esos falsos positivos, `detect()` puede
/// aplicar una segunda verificación: extrae un segundo patch de una
/// esquina opuesta del minimap y verifica que también matchee en el
/// sector ganador en la posición esperada (preservando la geometría del
/// viewport). Si el segundo patch no concuerda, el candidate se rechaza.
///
/// Cuando el top-K de candidatos está muy empatado, se evalúan todos y
/// solo el primero que pase disambiguation es retornado; si ninguno
/// valida, `detect()` retorna `None` — preferimos no-match sobre
/// wrong-match.
///
/// Controlado por [`MinimapMatcher::disambiguation_enabled`] (ON por default).
///
/// # Observabilidad
///
/// Los contadores internos (narrow/full/misses/last_duration) se exponen via
/// [`MinimapMatcher::stats_snapshot`] para diagnostics y Prometheus.
#[derive(Default)]
#[allow(dead_code)]
pub struct MinimapMatcher {
    /// Sector list por piso. Usar HashMap para lookup O(1) por piso.
    sectors_by_floor: HashMap<i32, Vec<ReferenceSector>>,
    /// Threshold SSD (CCORR_NORMED lower=better). Default 0.05 = match muy fuerte.
    /// Si no hay matches bajo este threshold, retornamos None.
    pub match_threshold: f32,
    /// Si true, detect() aplica segunda verificación con un patch de la
    /// esquina opuesta del minimap para rechazar falsos positivos (ver
    /// docs de `MinimapMatcher`). Default ON.
    pub disambiguation_enabled: bool,
    /// Stats con interior mutability para tracking durante detect().
    pub stats: MatcherStats,
}

impl MinimapMatcher {
    pub fn new() -> Self {
        Self {
            sectors_by_floor: HashMap::new(),
            match_threshold: 0.05,
            disambiguation_enabled: true,
            stats: MatcherStats::default(),
        }
    }

    /// Inyecta un ReferenceSector directamente en el matcher. Solo para
    /// tests de integración (evita dependencia de archivos PNG en disk).
    ///
    /// # Uso
    ///
    /// ```ignore
    /// let mut matcher = MinimapMatcher::new();
    /// let reference = image::GrayImage::new(256, 256);
    /// matcher.push_sector_for_test(32000, 31000, 7, reference);
    /// ```
    #[allow(dead_code)]
    pub fn push_sector_for_test(&mut self, file_x: i32, file_y: i32, z: i32, image: GrayImage) {
        self.sectors_by_floor
            .entry(z)
            .or_default()
            .push(ReferenceSector { file_x, file_y, z, image });
    }

    /// Retorna un snapshot de las stats para diagnóstico.
    /// Safe de llamar desde cualquier thread (usa atomic loads).
    ///
    /// # Ejemplo
    ///
    /// ```
    /// use tibia_bot::sense::vision::game_coords::MinimapMatcher;
    ///
    /// let matcher = MinimapMatcher::new();
    /// let stats = matcher.stats_snapshot();
    /// assert_eq!(stats.narrow_searches, 0);
    /// assert_eq!(stats.full_searches, 0);
    /// assert_eq!(stats.misses, 0);
    /// assert_eq!(stats.sectors_loaded, 0);
    /// ```
    pub fn stats_snapshot(&self) -> MatcherStatsSnapshot {
        let narrow = self.stats.narrow_searches.load(Ordering::Relaxed);
        let full = self.stats.full_searches.load(Ordering::Relaxed);
        let misses = self.stats.misses.load(Ordering::Relaxed);
        let disamb_rejects = self.stats.disambiguation_rejects.load(Ordering::Relaxed);
        let disamb_misses  = self.stats.disambiguation_misses.load(Ordering::Relaxed);
        let dur_us = self.stats.last_duration_us.load(Ordering::Relaxed);
        let score_bits = self.stats.last_score_bits.load(Ordering::Relaxed);
        let mut floors: Vec<i32> = self.sectors_by_floor.keys().copied().collect();
        floors.sort();
        let sectors: usize = self.sectors_by_floor.values().map(|v| v.len()).sum();
        MatcherStatsSnapshot {
            narrow_searches:       narrow,
            full_searches:         full,
            misses,
            disambiguation_rejects: disamb_rejects,
            disambiguation_misses:  disamb_misses,
            total_detects:         narrow + full,
            last_duration_ms:      dur_us as f64 / 1000.0,
            last_score:            f32::from_bits(score_bits),
            sectors_loaded:        sectors,
            floors_loaded:         floors,
            match_threshold:       self.match_threshold,
            disambiguation_enabled: self.disambiguation_enabled,
        }
    }

    /// Carga todos los reference PNGs del dir para los floors dados.
    /// Si `floors` está vacío, carga todos los floors (consume más RAM).
    ///
    /// Retorna (total_sectors, total_bytes_estimate_mb).
    #[allow(dead_code)]
    pub fn load_dir(&mut self, dir: &Path, floors: &[i32]) -> anyhow::Result<(usize, usize)> {
        self.sectors_by_floor.clear();
        if !dir.exists() {
            anyhow::bail!("MinimapMatcher dir no existe: {}", dir.display());
        }

        let mut total_sectors = 0;
        let mut total_bytes: usize = 0;
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            let Some((fx, fy, fz)) = parse_minimap_filename(&name_str) else { continue };

            if !floors.is_empty() && !floors.contains(&fz) {
                continue;
            }

            let img = match image::open(entry.path()) {
                Ok(i) => i.to_luma8(),
                Err(e) => {
                    tracing::warn!("MinimapMatcher: skip '{}': {}", entry.path().display(), e);
                    continue;
                }
            };

            total_bytes += (img.width() * img.height()) as usize;
            self.sectors_by_floor
                .entry(fz)
                .or_default()
                .push(ReferenceSector {
                    file_x: fx,
                    file_y: fy,
                    z:      fz,
                    image:  img,
                });
            total_sectors += 1;
        }
        tracing::info!(
            "MinimapMatcher: {} sectores cargados desde '{}' ({:.1} MB RAM)",
            total_sectors, dir.display(), total_bytes as f64 / 1_048_576.0
        );
        Ok((total_sectors, total_bytes / 1_048_576))
    }

    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.sectors_by_floor.is_empty()
    }

    /// Extrae el template luma del minimap NDI downsamplealo por `scale`.
    /// Retorna (template, downsampled_width, downsampled_height).
    ///
    /// **IMPORTANTE**: `MinimapSnapshot.data` está en orden **RGBA** (byte[0]=R,
    /// byte[2]=B), confirmado por:
    /// - `/test/grab` pasa `frame.data` directo a `ImageBuffer<Rgba<u8>>` y el
    ///   PNG resultante tiene colores correctos
    /// - `CLAUDE.md` memory: "NDI frame format is RGBA"
    ///
    /// Luma formula (BT.601): Y = 0.299R + 0.587G + 0.114B
    #[allow(dead_code)]
    fn build_template(snap: &MinimapSnapshot, scale: u32) -> Option<GrayImage> {
        let scale = scale.max(1);
        let dw = snap.width / scale;
        let dh = snap.height / scale;
        if dw == 0 || dh == 0 {
            return None;
        }
        let mut out = GrayImage::new(dw, dh);
        let stride = snap.width as usize * 4;
        let n = (scale * scale) as u32;
        for dy in 0..dh {
            for dx in 0..dw {
                let mut sum_r = 0u32;
                let mut sum_g = 0u32;
                let mut sum_b = 0u32;
                for by in 0..scale {
                    for bx in 0..scale {
                        let sx = dx * scale + bx;
                        let sy = dy * scale + by;
                        let off = sy as usize * stride + sx as usize * 4;
                        sum_r += snap.data[off] as u32;       // R byte 0
                        sum_g += snap.data[off + 1] as u32;   // G byte 1
                        sum_b += snap.data[off + 2] as u32;   // B byte 2
                    }
                }
                let r = (sum_r / n) as f32;
                let g = (sum_g / n) as f32;
                let b = (sum_b / n) as f32;
                let luma = (0.299 * r + 0.587 * g + 0.114 * b) as u8;
                out.put_pixel(dx, dy, image::Luma([luma]));
            }
        }
        Some(out)
    }

    /// Matchea el template contra un sector individual. Retorna (best_score_ssd,
    /// local_x, local_y) del mejor match encontrado dentro del sector.
    #[allow(dead_code)]
    fn match_sector(sector: &ReferenceSector, template: &GrayImage) -> Option<(f32, u32, u32)> {
        let (iw, ih) = sector.image.dimensions();
        let (tw, th) = template.dimensions();
        if iw < tw || ih < th {
            return None;
        }
        let result = match_template_parallel(
            &sector.image,
            template,
            MatchTemplateMethod::SumOfSquaredErrorsNormalized,
        );
        let mut best = f32::MAX;
        let mut best_x = 0u32;
        let mut best_y = 0u32;
        for y in 0..result.height() {
            for x in 0..result.width() {
                let s = result.get_pixel(x, y).0[0];
                if s < best {
                    best = s;
                    best_x = x;
                    best_y = y;
                }
            }
        }
        Some((best, best_x, best_y))
    }

    /// Extrae una sub-región del template ya construido.
    ///
    /// Usado por la fase de disambiguation para verificar con un patch
    /// de una esquina distinta del minimap ya downsampleado.
    ///
    /// Retorna `None` si la región pedida se sale del template.
    fn crop_template(template: &GrayImage, tl_x: u32, tl_y: u32, w: u32, h: u32) -> Option<GrayImage> {
        let (iw, ih) = template.dimensions();
        if w == 0 || h == 0 || tl_x + w > iw || tl_y + h > ih {
            return None;
        }
        let mut out = GrayImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let p = template.get_pixel(tl_x + x, tl_y + y);
                out.put_pixel(x, y, *p);
            }
        }
        Some(out)
    }

    /// Computa el SSD normalizado de un sub-template contra un sector en
    /// una posición LOCAL específica (sin hacer full sliding match).
    ///
    /// Equivalente a leer `match_template(...).get_pixel(local_x, local_y)`
    /// pero O(sub_template.area) en vez de O(sector.area * sub.area).
    ///
    /// Fórmula: `sum((s - t)^2) / sqrt(sum(s^2) * sum(t^2))`.
    ///
    /// Retorna `None` si la posición + sub-template se sale del sector.
    fn ssd_normalized_at(
        sector: &ReferenceSector,
        sub_template: &GrayImage,
        local_x: i32,
        local_y: i32,
    ) -> Option<f32> {
        let (iw, ih) = sector.image.dimensions();
        let (tw, th) = sub_template.dimensions();
        if local_x < 0 || local_y < 0 {
            return None;
        }
        let lx = local_x as u32;
        let ly = local_y as u32;
        if lx + tw > iw || ly + th > ih {
            return None;
        }
        let mut sum_sq_diff: f64 = 0.0;
        let mut sum_s_sq: f64 = 0.0;
        let mut sum_t_sq: f64 = 0.0;
        for y in 0..th {
            for x in 0..tw {
                let s = sector.image.get_pixel(lx + x, ly + y).0[0] as f64;
                let t = sub_template.get_pixel(x, y).0[0] as f64;
                let d = s - t;
                sum_sq_diff += d * d;
                sum_s_sq    += s * s;
                sum_t_sq    += t * t;
            }
        }
        let denom = (sum_s_sq * sum_t_sq).sqrt();
        if denom <= f64::EPSILON {
            // Todo negro en ambos → considerar match perfecto (0).
            // Protege contra division-by-zero en áreas totalmente vacías.
            return Some(0.0);
        }
        Some((sum_sq_diff / denom) as f32)
    }

    /// Detecta la posición del char vía template matching.
    ///
    /// Modos de search:
    /// - **Narrow** (default, `force_full=false`): si `last_known` presente,
    ///   matchea solo contra el sector actual + 8 vecinos (9 sectores =
    ///   ~50-100ms). Fast path.
    /// - **Full** (`force_full=true` O `last_known=None`): brute force sobre
    ///   TODOS los sectores de TODOS los floors cargados. Slow path (~1-4s)
    ///   usado en cold boot y para re-validación periódica.
    ///
    /// La re-validación periódica es el mecanismo clave contra
    /// "stuck in false positive": si el narrow search cae en un sector
    /// equivocado en el cold start (ej char en login screen), sin
    /// re-validación el bot queda pegado ahí. Vision llama con
    /// `force_full=true` cada `COORDS_REVALIDATE_INTERVAL` detecciones.
    ///
    /// ## Disambiguation (OPT-IN)
    ///
    /// Si `disambiguation_enabled=true` (default), tras el match primario
    /// se valida el candidato cortando un segundo patch de una esquina
    /// opuesta del template y verificando que también cae en el mismo
    /// sector en la posición geométricamente esperada. Si el top-1 no
    /// confirma, se prueba el siguiente de top-K hasta encontrar uno
    /// consistente (o retornar `None`). Ver rationale en docstring de
    /// [`MinimapMatcher`].
    ///
    /// Retorna `None` si:
    /// - El matcher está vacío
    /// - El minimap NDI es demasiado pequeño
    /// - Ningún match pasó el threshold
    /// - Ningún candidato top-K pasó la disambiguation
    pub fn detect(
        &self,
        snap: &MinimapSnapshot,
        ndi_tile_scale: u32,
        last_known: Option<(i32, i32, i32)>,
        force_full: bool,
    ) -> Option<(i32, i32, i32)> {
        if self.sectors_by_floor.is_empty() {
            return None;
        }
        let template = Self::build_template(snap, ndi_tile_scale)?;
        let (tw, th) = template.dimensions();
        if tw == 0 || th == 0 {
            return None;
        }

        let detect_start = std::time::Instant::now();

        // Use narrow search only when we have last_known AND force_full=false.
        // Otherwise: full brute force over all loaded floors.
        let use_narrow = !force_full && last_known.is_some();

        // Determine which floors to search.
        let candidate_floors: Vec<i32> = if use_narrow {
            // Narrow: only the current floor of last_known.
            vec![last_known.map(|(_, _, z)| z).unwrap_or(0)]
        } else {
            // Full brute force: all loaded floors.
            self.sectors_by_floor.keys().copied().collect()
        };

        // Collect candidate sectors.
        let mut candidates: Vec<&ReferenceSector> = Vec::new();
        for floor in candidate_floors {
            if let Some(list) = self.sectors_by_floor.get(&floor) {
                if use_narrow {
                    // Narrow to current sector + 8 neighbors (3×3 grid of 256-tile sectors).
                    let (lx, ly) = last_known.map(|(x, y, _)| (x, y)).unwrap_or((0, 0));
                    let cur_fx = (lx / 256) * 256;
                    let cur_fy = (ly / 256) * 256;
                    for sector in list {
                        let dx = (sector.file_x - cur_fx).abs();
                        let dy = (sector.file_y - cur_fy).abs();
                        if dx <= 256 && dy <= 256 {
                            candidates.push(sector);
                        }
                    }
                } else {
                    // Full brute force over this floor.
                    candidates.extend(list.iter());
                }
            }
        }

        if candidates.is_empty() {
            return None;
        }

        // Match against each candidate, collect TODOS los resultados (top-K
        // ordenados por score) para que disambiguation pueda probarlos en
        // orden si el top-1 no valida.
        //
        // MatchResult: (score, local_x, local_y, sector_idx)
        let mut per_sector: Vec<(f32, u32, u32, usize)> = Vec::with_capacity(candidates.len());
        for (idx, sector) in candidates.iter().enumerate() {
            let Some((score, lx, ly)) = Self::match_sector(sector, &template) else { continue };
            per_sector.push((score, lx, ly, idx));
        }
        if per_sector.is_empty() {
            self.update_common_stats(detect_start, use_narrow);
            self.stats.misses.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        per_sector.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        let global_best = per_sector[0].0;

        // Update duración y search-type antes de ramificar.
        self.update_common_stats(detect_start, use_narrow);

        if global_best > self.match_threshold {
            self.stats.misses.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                "MinimapMatcher: best SSD {:.4} > threshold {:.4}, no match",
                global_best, self.match_threshold
            );
            return None;
        }

        // ── Disambiguation ─────────────────────────────────────────────
        // Si está ON, validamos top-K con un segundo patch de la esquina
        // opuesta del template. El primero que valide gana. Si ninguno
        // valida, retornamos None (preferimos no-match sobre wrong-match).
        //
        // top-K se restringe a candidates "cercanos" al mejor: solo los
        // scores ≤ max(best * 2.0, threshold). Esto acota el costo (los
        // candidates muy lejos del best son descartes obvios) sin perder
        // false-positive resolvers.
        if !self.disambiguation_enabled {
            let (score, lx, ly, idx) = per_sector[0];
            self.stats.last_score_bits.store(score.to_bits(), Ordering::Relaxed);
            let sector = candidates[idx];
            let world_x = sector.file_x + lx as i32 + (tw / 2) as i32;
            let world_y = sector.file_y + ly as i32 + (th / 2) as i32;
            return Some((world_x, world_y, sector.z));
        }

        // Construir sub-template de la esquina opuesta al sub-template
        // "primary" (que implícitamente representa el top-left).
        //
        // Elegimos el cuadrante bottom-right del template como sub-patch,
        // de tamaño ~1/3 del template cada lado (evita overlap con la
        // zona dominante del primary y mantiene área suficiente para que
        // el SSD sea discriminativo).
        let sub_w = (tw / 3).max(4);
        let sub_h = (th / 3).max(4);
        let sub_tl_x = tw.saturating_sub(sub_w);
        let sub_tl_y = th.saturating_sub(sub_h);
        let sub_template = match Self::crop_template(&template, sub_tl_x, sub_tl_y, sub_w, sub_h) {
            Some(st) => st,
            None => {
                // No podemos disambiguar (template muy pequeño). Fallback:
                // retornar el top-1 como antes.
                let (score, lx, ly, idx) = per_sector[0];
                self.stats.last_score_bits.store(score.to_bits(), Ordering::Relaxed);
                let sector = candidates[idx];
                let world_x = sector.file_x + lx as i32 + (tw / 2) as i32;
                let world_y = sector.file_y + ly as i32 + (th / 2) as i32;
                return Some((world_x, world_y, sector.z));
            }
        };

        // Umbral del sub-patch: idéntico al del template completo. El SSD
        // normalizado es invariante al tamaño del patch (es un ratio), así
        // que el mismo threshold funciona razonablemente.
        let sub_threshold = self.match_threshold;

        // Cota del top-K a probar: todos con score ≤ 2 * best (caps).
        // Cap duro de 8 candidates para que el cost esté acotado (full
        // search con miles de candidates similares no explota).
        const MAX_TOP_K: usize = 8;
        let best_score = per_sector[0].0;
        let score_cap = (best_score * 2.0).max(best_score + 0.01);

        let mut checked = 0;
        for &(score, lx, ly, idx) in per_sector.iter() {
            if checked >= MAX_TOP_K || score > score_cap {
                break;
            }
            checked += 1;
            let sector = candidates[idx];

            // La posición esperada del sub-patch dentro del sector:
            // el primary template está anclado en (lx, ly); el sub-template
            // está a offset (sub_tl_x, sub_tl_y) dentro del primary.
            let expected_sub_x = lx as i32 + sub_tl_x as i32;
            let expected_sub_y = ly as i32 + sub_tl_y as i32;

            let Some(sub_score) = Self::ssd_normalized_at(
                sector,
                &sub_template,
                expected_sub_x,
                expected_sub_y,
            ) else {
                // Sub-patch fuera del sector (edge case). Rechazamos este.
                self.stats.disambiguation_rejects.fetch_add(1, Ordering::Relaxed);
                continue;
            };

            if sub_score <= sub_threshold {
                // Confirmación: primary + secondary patches coherentes.
                self.stats.last_score_bits.store(score.to_bits(), Ordering::Relaxed);
                let world_x = sector.file_x + lx as i32 + (tw / 2) as i32;
                let world_y = sector.file_y + ly as i32 + (th / 2) as i32;
                tracing::debug!(
                    "MinimapMatcher disamb OK: primary {:.4} + sub {:.4} @ sector ({},{},z={})",
                    score, sub_score, sector.file_x, sector.file_y, sector.z
                );
                return Some((world_x, world_y, sector.z));
            }

            // No matchea en la esquina opuesta → false positive candidate.
            self.stats.disambiguation_rejects.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                "MinimapMatcher disamb REJECT: primary {:.4} OK pero sub {:.4} > {:.4} @ sector ({},{},z={})",
                score, sub_score, sub_threshold, sector.file_x, sector.file_y, sector.z
            );
        }

        // Ningún candidato top-K pasó disambiguation: preferimos no-match.
        self.stats.disambiguation_misses.fetch_add(1, Ordering::Relaxed);
        self.stats.misses.fetch_add(1, Ordering::Relaxed);
        tracing::debug!(
            "MinimapMatcher disamb: ninguno de top-{} pasó (best primary {:.4})",
            checked, best_score
        );
        None
    }

    /// Helper común para actualizar stats de duración y search-type.
    fn update_common_stats(&self, start: std::time::Instant, use_narrow: bool) {
        let elapsed_us = start.elapsed().as_micros() as u64;
        self.stats.last_duration_us.store(elapsed_us, Ordering::Relaxed);
        if use_narrow {
            self.stats.narrow_searches.fetch_add(1, Ordering::Relaxed);
        } else {
            self.stats.full_searches.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── validate_jump ─────────────────────────────────────────────────────

    #[test]
    fn validate_jump_none_detected_passes_through() {
        assert_eq!(validate_jump(Some((100, 100, 6)), None, false), None);
        assert_eq!(validate_jump(None, None, false), None);
        assert_eq!(validate_jump(None, None, true), None);
    }

    #[test]
    fn validate_jump_no_last_accepts_any_detect() {
        // Cold boot sin seed: sin baseline, aceptamos lo que devuelva el matcher.
        assert_eq!(
            validate_jump(None, Some((99999, 99999, 6)), false),
            Some((99999, 99999, 6))
        );
    }

    #[test]
    fn validate_jump_force_full_still_validates_with_baseline() {
        // 2026-04-18 fix: force_full ya NO bypassa el filter si hay last_known.
        // Era un hole que permitía false positives del matcher post-revalidation.
        // Ahora: si last es Some, validamos siempre, independiente de force_full.
        let last     = Some((1000, 1000, 6));
        let detected = Some((5000, 5000, 6));
        // Drift enorme mismo piso → rechazado incluso con force_full=true.
        assert_eq!(validate_jump(last, detected, true), None);

        // Si last es None (verdadero cold boot), pasa cualquier cosa.
        assert_eq!(
            validate_jump(None, Some((5000, 5000, 6)), true),
            Some((5000, 5000, 6))
        );
    }

    #[test]
    fn validate_jump_cross_floor_accepts_small_xy_jump() {
        // Ladder/rope legítimo: z cambia pero xy queda cerca (dx/dy ≤ 3).
        let last = Some((1000, 1000, 6));
        // Mismo XY, piso distinto → ladder típico
        assert_eq!(
            validate_jump(last, Some((1000, 1000, 7)), false),
            Some((1000, 1000, 7))
        );
        // XY ±2 tiles, piso distinto → ladder con drift permitido
        assert_eq!(
            validate_jump(last, Some((1002, 998, 7)), false),
            Some((1002, 998, 7))
        );
        // XY ±3, piso distinto → borde (3 es max permitido)
        assert_eq!(
            validate_jump(last, Some((1003, 997, 7)), false),
            Some((1003, 997, 7))
        );
    }

    #[test]
    fn validate_jump_cross_floor_rejects_big_xy_jump() {
        // Hardening 2026-04-18 post live bug: z cambia + xy jump grande =
        // false positive del matcher (imposible físicamente sin teleport).
        // Caso real observado: (32679,31684,6) → (32565,31779,7)
        // dx=114, dy=95, z=1 → rechazar.
        let last = Some((32679, 31684, 6));
        let bogus = Some((32565, 31779, 7));
        assert_eq!(
            validate_jump(last, bogus, false), None,
            "cross-floor + xy jump >3 tiles debe rechazarse como false positive"
        );

        // dx=4 en z change → borde excedido, rechaza
        let last2 = Some((1000, 1000, 6));
        assert_eq!(
            validate_jump(last2, Some((1004, 1000, 7)), false), None,
            "dx=4 en cross-floor excede MAX_XY_ON_FLOOR_CHANGE=3"
        );

        // Teleports legítimos (mage spells, etc) NO pasan el filter sin
        // force_full — aceptable trade-off: tras un teleport, el proximo
        // force_full (cada N detects) re-sincroniza.
    }

    #[test]
    fn validate_jump_same_floor_within_max_accepts() {
        let last = Some((1000, 1000, 6));
        // dx=30, dy=0 → justo en el borde (<=30 acepta)
        assert_eq!(
            validate_jump(last, Some((1030, 1000, 6)), false),
            Some((1030, 1000, 6))
        );
        // dx=-30, dy=-30 → esquina, borde ambos
        assert_eq!(
            validate_jump(last, Some((970, 970, 6)), false),
            Some((970, 970, 6))
        );
        // Movimiento pequeño normal
        assert_eq!(
            validate_jump(last, Some((1005, 1003, 6)), false),
            Some((1005, 1003, 6))
        );
    }

    #[test]
    fn validate_jump_same_floor_exceeds_max_rejects_x() {
        let last = Some((1000, 1000, 6));
        // dx=31 > MAX_JUMP=30
        assert_eq!(validate_jump(last, Some((1031, 1000, 6)), false), None);
    }

    #[test]
    fn validate_jump_same_floor_exceeds_max_rejects_y() {
        let last = Some((1000, 1000, 6));
        // dy=31 > MAX_JUMP=30
        assert_eq!(validate_jump(last, Some((1000, 1031, 6)), false), None);
    }

    #[test]
    fn validate_jump_large_jump_rejected() {
        // Caso real observado 2026-04-17: matcher devolvía coord ~370 tiles off
        // del seed por false positive en sector diagonal vecino.
        let seed     = Some((32681, 31686, 6));
        let false_fp = Some((32980, 31460, 6));
        assert_eq!(validate_jump(seed, false_fp, false), None);
    }

    #[test]
    fn validate_jump_max_jump_constant_is_30() {
        // Documenta la constante para reviewers / regresion detection.
        assert_eq!(MAX_JUMP_PER_DETECT, 30);
    }

    fn make_patch(w: usize, h: usize, seed: u8) -> Vec<u8> {
        let mut data = vec![0u8; w * h * 4];
        for i in 0..(w * h) {
            let v = ((i as u8).wrapping_mul(seed.wrapping_add(7))).wrapping_add(seed);
            data[i * 4] = v;                          // B
            data[i * 4 + 1] = v.wrapping_add(50);     // G
            data[i * 4 + 2] = v.wrapping_add(100);    // R
            data[i * 4 + 3] = 255;                    // A
        }
        data
    }

    #[test]
    fn dhash_identical_patches_same_hash() {
        let patch = make_patch(32, 32, 42);
        let h1 = dhash(&patch, 32, 32);
        let h2 = dhash(&patch, 32, 32);
        assert_eq!(h1, h2);
    }

    #[test]
    fn dhash_different_patches_different_hash() {
        let p1 = make_patch(32, 32, 42);
        let p2 = make_patch(32, 32, 99);
        let h1 = dhash(&p1, 32, 32);
        let h2 = dhash(&p2, 32, 32);
        assert_ne!(h1, h2);
    }

    #[test]
    fn dhash_empty_returns_zero() {
        assert_eq!(dhash(&[], 0, 0), 0);
        assert_eq!(dhash(&[0; 16], 1, 1), 0); // too small for 9×8 resize
    }

    #[test]
    fn hamming_distance_correct() {
        assert_eq!(hamming(0, 0), 0);
        assert_eq!(hamming(0b1111, 0b0000), 4);
        assert_eq!(hamming(0xFF, 0xFE), 1);
        assert_eq!(hamming(u64::MAX, 0), 64);
    }

    #[test]
    fn map_index_insert_and_lookup() {
        let mut index = MapIndex::new();
        let pos = MapPos { x: 32000, y: 31000, z: 7 };
        index.insert(0xDEADBEEF, pos);

        assert_eq!(index.lookup_exact(0xDEADBEEF).len(), 1);
        assert_eq!(index.lookup_exact(0xDEADBEEF)[0], pos);
        assert_eq!(index.lookup_exact(0xCAFEBABE).len(), 0);
    }

    #[test]
    fn map_index_fuzzy_lookup() {
        let mut index = MapIndex::new();
        let pos = MapPos { x: 100, y: 200, z: 7 };
        index.insert(0b1111_0000, pos);

        // Hamming distance 1
        let results = index.lookup_fuzzy(0b1111_0001, 1);
        assert_eq!(results.len(), 1);

        // Hamming distance 5 → no match at max_dist=3
        let results = index.lookup_fuzzy(0b1010_0101, 3);
        assert!(results.is_empty());
    }

    #[test]
    fn parse_minimap_filename_valid() {
        let r = parse_minimap_filename("Minimap_Color_31744_30976_7.png");
        assert_eq!(r, Some((31744, 30976, 7)));
    }

    #[test]
    fn parse_minimap_filename_invalid() {
        assert_eq!(parse_minimap_filename("not_a_minimap.png"), None);
        assert_eq!(parse_minimap_filename("Minimap_Color_abc_123_7.png"), None);
    }

    #[test]
    fn detect_position_empty_index_returns_none() {
        let index = MapIndex::new();
        let snap = MinimapSnapshot {
            width: 107,
            height: 110,
            data: vec![128u8; 107 * 110 * 4],
        };
        assert_eq!(detect_position(&snap, &index, 5), None);
    }

    #[test]
    fn detect_position_with_matching_index_scale1() {
        // Caso scale=1: el minimap tiene 1 px/tile (como los reference PNGs).
        // PATCH_TILES = 16, así que el patch es 16×16.
        let patch_data = make_patch(PATCH_TILES, PATCH_TILES, 42);
        let hash = dhash(&patch_data, PATCH_TILES, PATCH_TILES);

        let mut index = MapIndex::new();
        index.insert(hash, MapPos { x: 32000, y: 31000, z: 7 });

        // Minimap 40×40 con el patch en esquina (0,0).
        let w = 40usize;
        let h = 40usize;
        let mut snap_data = vec![0u8; w * h * 4];
        for row in 0..PATCH_TILES {
            let src_start = row * PATCH_TILES * 4;
            let dst_start = row * w * 4;
            snap_data[dst_start..dst_start + PATCH_TILES * 4]
                .copy_from_slice(&patch_data[src_start..src_start + PATCH_TILES * 4]);
        }

        let snap = MinimapSnapshot { width: w as u32, height: h as u32, data: snap_data };
        let result = detect_position(&snap, &index, 1);
        assert!(result.is_some(), "expected match at scale=1");
        let (x, y, z) = result.unwrap();
        // Centro del minimap: 32000 + 40/2 = 32020, 31000 + 40/2 = 31020
        assert_eq!((x, y, z), (32020, 31020, 7));
    }

    #[test]
    fn detect_position_with_matching_index_scale5() {
        // Caso scale=5: el minimap NDI tiene 5 px/tile. Un patch de 16 tiles
        // son 80×80 pixels en el snapshot, downsampleado a 16×16 antes del hash.
        //
        // Para que el test sea determinista, generamos un patch 16×16 base y
        // lo "upscaleamos" 5× por nearest-neighbor (cada pixel del patch → bloque
        // 5×5 en el snapshot). Downsamplear con averaging vuelve a dar el mismo
        // patch 16×16 original → mismo hash.
        let base = make_patch(PATCH_TILES, PATCH_TILES, 77);
        let hash = dhash(&base, PATCH_TILES, PATCH_TILES);

        let mut index = MapIndex::new();
        index.insert(hash, MapPos { x: 32100, y: 31200, z: 7 });

        // Snapshot 107×110 (tamaño real del minimap NDI del usuario).
        let w = 107usize;
        let h = 110usize;
        let mut snap_data = vec![0u8; w * h * 4];
        // Upscale el patch 16×16 → 80×80 por nearest-neighbor en (0,0).
        for j in 0..PATCH_TILES {
            for i in 0..PATCH_TILES {
                let src_off = (j * PATCH_TILES + i) * 4;
                for dy in 0..5 {
                    for dx in 0..5 {
                        let sx = i * 5 + dx;
                        let sy = j * 5 + dy;
                        let dst_off = (sy * w + sx) * 4;
                        snap_data[dst_off..dst_off + 4]
                            .copy_from_slice(&base[src_off..src_off + 4]);
                    }
                }
            }
        }

        let snap = MinimapSnapshot { width: w as u32, height: h as u32, data: snap_data };
        let result = detect_position(&snap, &index, 5);
        assert!(result.is_some(), "expected match at scale=5");
        let (x, y, z) = result.unwrap();
        // Centro del minimap en tiles: 32100 + (107/2)/5 = 32100 + 10 = 32110
        //                              31200 + (110/2)/5 = 31200 + 11 = 31211
        assert_eq!((x, y, z), (32110, 31211, 7));
    }

    #[test]
    fn extract_and_downsample_averages_blocks() {
        // Crear un snapshot 10×10 (scale=5, patch_tiles efectivo=2) con 2 bloques:
        // - bloque (0,0): todo rojo
        // - bloque (1,0): todo verde
        // etc.
        //
        // Usamos PATCH_TILES fijo en 16, así que hacemos un caso small-scale
        // manualmente sin usar la función real. Solo testeamos la lógica de
        // averaging con scale=2 y patch mini.
        //
        // Este test verifica que para scale=1, el downsample es identity.
        let snap = MinimapSnapshot {
            width: 16,
            height: 16,
            data: make_patch(16, 16, 123),
        };
        let patch = extract_and_downsample(&snap, 0, 0, 1).unwrap();
        // Con scale=1, el patch debe ser idéntico al snapshot (ambos 16×16).
        assert_eq!(patch.len(), 16 * 16 * 4);
        assert_eq!(patch, snap.data);
    }

    #[test]
    fn map_index_serialize_roundtrip() {
        let mut index = MapIndex::new();
        index.insert(0xAAAA, MapPos { x: 1, y: 2, z: 3 });
        index.insert(0xBBBB, MapPos { x: 4, y: 5, z: 6 });

        let serialized = bincode::serialize(&index).unwrap();
        let deserialized: MapIndex = bincode::deserialize(&serialized).unwrap();

        assert_eq!(deserialized.lookup_exact(0xAAAA).len(), 1);
        assert_eq!(deserialized.lookup_exact(0xBBBB)[0], MapPos { x: 4, y: 5, z: 6 });
    }

    // ── MinimapMatcher tests ─────────────────────────────────────────────

    /// Crea un reference PNG sintético 256×256 con patrones únicos globales
    /// (no periódicos), útil para tests determinísticos del matcher donde
    /// cada posición del template tiene UN solo mejor match.
    ///
    /// Formula: combina términos lineales (x*7, y*13) con un término no-lineal
    /// (x*y) para romper cualquier periodicidad diagonal o axial. El resultado
    /// es un patrón pseudo-aleatorio determinístico sin false positives.
    fn make_synthetic_reference() -> GrayImage {
        let mut img = GrayImage::new(256, 256);
        for y in 0..256u32 {
            for x in 0..256u32 {
                let v = (x.wrapping_mul(7)
                    .wrapping_add(y.wrapping_mul(13))
                    .wrapping_add(x.wrapping_mul(y) % 256)
                    .wrapping_add(((x / 4) ^ (y / 4)) * 17)) % 256;
                img.put_pixel(x, y, image::Luma([v as u8]));
            }
        }
        img
    }

    /// Crea un MinimapSnapshot sintético que representa un recorte 107×110
    /// de un reference PNG en una posición específica, con scale=1 (1 px/tile).
    /// Usado para validar que detect() retorna el coord esperado.
    fn make_synthetic_minimap_from_ref(
        reference: &GrayImage,
        center_x: u32,
        center_y: u32,
        view_w: u32,
        view_h: u32,
    ) -> MinimapSnapshot {
        // Calcular la esquina top-left del view en la reference.
        let tl_x = center_x.saturating_sub(view_w / 2);
        let tl_y = center_y.saturating_sub(view_h / 2);
        let mut data = vec![0u8; (view_w * view_h * 4) as usize];
        for y in 0..view_h {
            for x in 0..view_w {
                let sx = tl_x + x;
                let sy = tl_y + y;
                let luma = if sx < reference.width() && sy < reference.height() {
                    reference.get_pixel(sx, sy).0[0]
                } else {
                    0
                };
                let off = ((y * view_w + x) * 4) as usize;
                // RGBA: byte[0]=R, byte[1]=G, byte[2]=B (luma en los 3)
                data[off] = luma;
                data[off + 1] = luma;
                data[off + 2] = luma;
                data[off + 3] = 255;
            }
        }
        MinimapSnapshot { width: view_w, height: view_h, data }
    }

    #[test]
    fn matcher_empty_returns_none() {
        let matcher = MinimapMatcher::new();
        assert!(matcher.is_empty());
        let snap = MinimapSnapshot { width: 107, height: 110, data: vec![0u8; 107 * 110 * 4] };
        assert_eq!(matcher.detect(&snap, 1, None, false), None);
        assert_eq!(matcher.detect(&snap, 1, Some((100, 100, 7)), false), None);
    }

    #[test]
    fn matcher_finds_known_position_scale1() {
        // Given: un reference sintético + snapshot que captura la misma
        // región en el centro del quadrante GRADIENT (top-left), donde cada
        // pixel tiene luma única (x+y) % 256. Esto garantiza que match_template
        // encuentre una sola posición óptima, sin falsos positivos periódicos
        // como pasa con el checkerboard quadrant.
        let reference = make_synthetic_reference();
        let mut matcher = MinimapMatcher::new();
        // Insertar manualmente 1 sector en el atlas
        matcher.sectors_by_floor.insert(
            7,
            vec![ReferenceSector {
                file_x: 32000,
                file_y: 31000,
                z:      7,
                image:  reference.clone(),
            }],
        );
        matcher.match_threshold = 0.15;

        // Snapshot centrado en (64, 64) del reference = tile del mundo
        // (32000+64, 31000+64, 7) = (32064, 31064, 7). View 60×60 cubre
        // (34-93, 34-93) todo en quadrante gradient.
        let snap = make_synthetic_minimap_from_ref(&reference, 64, 64, 60, 60);

        // Full brute force (force_full=true).
        let detected = matcher.detect(&snap, 1, None, true);
        assert!(detected.is_some(), "matcher debe encontrar match en reference sintético");
        let (x, y, z) = detected.unwrap();
        // Tolerancia ±2 por rounding del centro del template.
        assert!(
            (x - 32064).abs() <= 2,
            "x esperado ~32064, obtenido {}",
            x
        );
        assert!(
            (y - 31064).abs() <= 2,
            "y esperado ~31064, obtenido {}",
            y
        );
        assert_eq!(z, 7);
    }

    #[test]
    fn matcher_narrow_search_uses_only_current_floor() {
        // Insert 2 floors, each with 1 sector. Narrow should only match the one
        // matching last_known.z.
        let mut matcher = MinimapMatcher::new();
        let ref7 = make_synthetic_reference();
        let mut ref8 = GrayImage::new(256, 256);
        // ref8 = patrón distinto (luma fija) para que NO matchee el snap
        for y in 0..256u32 {
            for x in 0..256u32 {
                ref8.put_pixel(x, y, image::Luma([128]));
            }
        }
        matcher.sectors_by_floor.insert(
            7,
            vec![ReferenceSector { file_x: 32000, file_y: 31000, z: 7, image: ref7.clone() }],
        );
        matcher.sectors_by_floor.insert(
            8,
            vec![ReferenceSector { file_x: 32000, file_y: 31000, z: 8, image: ref8 }],
        );
        matcher.match_threshold = 0.15;

        let snap = make_synthetic_minimap_from_ref(&ref7, 64, 64, 60, 60);
        // Narrow search con last_known en piso 7 (coord aproximado).
        let detected = matcher.detect(&snap, 1, Some((32064, 31064, 7)), false);
        assert!(detected.is_some());
        assert_eq!(detected.unwrap().2, 7, "narrow debe mantener piso 7");
    }

    #[test]
    fn matcher_force_full_searches_all_floors() {
        // Con force_full=true, el matcher debe considerar sectores de todos
        // los pisos cargados, no solo el de last_known.
        let mut matcher = MinimapMatcher::new();
        let ref7 = make_synthetic_reference();
        matcher.sectors_by_floor.insert(
            7,
            vec![ReferenceSector { file_x: 32000, file_y: 31000, z: 7, image: ref7.clone() }],
        );
        matcher.match_threshold = 0.15;

        let snap = make_synthetic_minimap_from_ref(&ref7, 64, 64, 60, 60);
        // last_known dice piso 8 (incorrecto), force_full=true debe encontrar piso 7 real.
        let detected = matcher.detect(&snap, 1, Some((33000, 32000, 8)), true);
        assert!(detected.is_some());
        assert_eq!(detected.unwrap().2, 7, "force_full debe recuperar piso correcto");
    }

    #[test]
    fn matcher_rejects_above_threshold() {
        // Un snap totalmente distinto al reference debe producir SSD alto
        // y retornar None cuando está por encima del threshold.
        let mut matcher = MinimapMatcher::new();
        let reference = make_synthetic_reference();
        matcher.sectors_by_floor.insert(
            7,
            vec![ReferenceSector { file_x: 32000, file_y: 31000, z: 7, image: reference }],
        );
        matcher.match_threshold = 0.01; // MUY estricto

        // Snap con patrón random (no viene del reference).
        let mut data = vec![0u8; 60 * 60 * 4];
        for i in 0..(60 * 60) {
            let v = ((i * 137) % 256) as u8;
            data[i * 4]     = v;
            data[i * 4 + 1] = v.wrapping_add(80);
            data[i * 4 + 2] = v.wrapping_add(160);
            data[i * 4 + 3] = 255;
        }
        let snap = MinimapSnapshot { width: 60, height: 60, data };

        let detected = matcher.detect(&snap, 1, None, true);
        assert_eq!(detected, None, "snap unrelated no debe matchear con threshold 0.01");
    }

    #[test]
    fn matcher_build_template_rgba_luma_correct() {
        // Verifica que build_template calcula luma BT.601 correctamente
        // asumiendo RGBA byte order (byte[0]=R, byte[1]=G, byte[2]=B).
        let mut data = vec![0u8; 4 * 4 * 4];
        for i in 0..16 {
            data[i * 4]     = 255; // R
            data[i * 4 + 1] = 128; // G
            data[i * 4 + 2] = 64;  // B
            data[i * 4 + 3] = 255; // A
        }
        let snap = MinimapSnapshot { width: 4, height: 4, data };
        let template = MinimapMatcher::build_template(&snap, 1).unwrap();
        assert_eq!(template.dimensions(), (4, 4));
        // Luma = 0.299*255 + 0.587*128 + 0.114*64 = 76.245 + 75.136 + 7.296 = 158.677
        let expected = (0.299 * 255.0 + 0.587 * 128.0 + 0.114 * 64.0) as u8;
        let actual = template.get_pixel(0, 0).0[0];
        assert!(
            (actual as i16 - expected as i16).abs() <= 1,
            "luma esperada ~{}, obtenida {}",
            expected, actual
        );
    }

    #[test]
    fn matcher_stats_track_narrow_vs_full() {
        // Verify stats are incremented correctly per detection mode.
        let mut matcher = MinimapMatcher::new();
        let reference = make_synthetic_reference();
        matcher.sectors_by_floor.insert(
            7,
            vec![ReferenceSector { file_x: 32000, file_y: 31000, z: 7, image: reference.clone() }],
        );
        matcher.match_threshold = 0.15;

        let snap = make_synthetic_minimap_from_ref(&reference, 64, 64, 60, 60);

        // Initial: stats all zero.
        let s0 = matcher.stats_snapshot();
        assert_eq!(s0.narrow_searches, 0);
        assert_eq!(s0.full_searches, 0);
        assert_eq!(s0.misses, 0);

        // Full search (last_known=None forces full).
        let _ = matcher.detect(&snap, 1, None, false);
        let s1 = matcher.stats_snapshot();
        assert_eq!(s1.full_searches, 1);
        assert_eq!(s1.narrow_searches, 0);

        // Narrow search (last_known provided, force_full=false).
        let _ = matcher.detect(&snap, 1, Some((32064, 31064, 7)), false);
        let s2 = matcher.stats_snapshot();
        assert_eq!(s2.full_searches, 1);
        assert_eq!(s2.narrow_searches, 1);

        // Force full override.
        let _ = matcher.detect(&snap, 1, Some((32064, 31064, 7)), true);
        let s3 = matcher.stats_snapshot();
        assert_eq!(s3.full_searches, 2);
        assert_eq!(s3.narrow_searches, 1);
    }

    #[test]
    fn matcher_stats_track_misses() {
        // Snap unrelated al reference → miss incrementa.
        let mut matcher = MinimapMatcher::new();
        let reference = make_synthetic_reference();
        matcher.sectors_by_floor.insert(
            7,
            vec![ReferenceSector { file_x: 32000, file_y: 31000, z: 7, image: reference }],
        );
        matcher.match_threshold = 0.0001; // imposible de matchear

        let mut data = vec![255u8; 60 * 60 * 4];
        for i in 0..(60 * 60) {
            let v = (i * 11 % 256) as u8;
            data[i * 4]     = v;
            data[i * 4 + 1] = v;
            data[i * 4 + 2] = v;
            data[i * 4 + 3] = 255;
        }
        let snap = MinimapSnapshot { width: 60, height: 60, data };

        let r = matcher.detect(&snap, 1, None, true);
        assert_eq!(r, None);
        let s = matcher.stats_snapshot();
        assert_eq!(s.misses, 1);
        assert_eq!(s.full_searches, 1, "aún cuenta el search aunque haya fallado");
    }

    #[test]
    fn matcher_stats_snapshot_reflects_loaded_sectors() {
        let mut matcher = MinimapMatcher::new();
        assert_eq!(matcher.stats_snapshot().sectors_loaded, 0);

        matcher.sectors_by_floor.insert(
            7,
            vec![
                ReferenceSector { file_x: 0,   file_y: 0, z: 7, image: GrayImage::new(256, 256) },
                ReferenceSector { file_x: 256, file_y: 0, z: 7, image: GrayImage::new(256, 256) },
            ],
        );
        matcher.sectors_by_floor.insert(
            8,
            vec![ReferenceSector { file_x: 0, file_y: 0, z: 8, image: GrayImage::new(256, 256) }],
        );

        let snap = matcher.stats_snapshot();
        assert_eq!(snap.sectors_loaded, 3);
        assert_eq!(snap.floors_loaded, vec![7, 8]);
    }

    // ── apply_displacement tests ──────────────────────────────────────────

    #[test]
    fn apply_displacement_zero_shift_does_not_change_coord() {
        let (c, a) = apply_displacement((100, 200, 7), (0, 0), (0, 0), 2);
        assert_eq!(c, (100, 200, 7));
        assert_eq!(a, (0, 0));
    }

    #[test]
    fn apply_displacement_exact_tile_shift_updates_coord() {
        // scale=2, shift 2px east → +1 tile east
        let (c, a) = apply_displacement((100, 200, 7), (0, 0), (2, 0), 2);
        assert_eq!(c, (101, 200, 7));
        assert_eq!(a, (0, 0));
    }

    #[test]
    fn apply_displacement_sub_tile_accumulates() {
        // scale=2, shift 1px → accum=1, coord no cambia
        let (c, a) = apply_displacement((100, 200, 7), (0, 0), (1, 0), 2);
        assert_eq!(c, (100, 200, 7));
        assert_eq!(a, (1, 0));
    }

    #[test]
    fn apply_displacement_accumulator_completes_tile() {
        // accum=1, new shift=1 → total=2 → +1 tile, accum reset
        let (c, a) = apply_displacement((100, 200, 7), (1, 0), (1, 0), 2);
        assert_eq!(c, (101, 200, 7));
        assert_eq!(a, (0, 0));
    }

    #[test]
    fn apply_displacement_multi_tile_shift() {
        // scale=2, shift 5px east → 2 tiles east + 1px accum
        let (c, a) = apply_displacement((100, 200, 7), (0, 0), (5, 0), 2);
        assert_eq!(c, (102, 200, 7));
        assert_eq!(a, (1, 0));
    }

    #[test]
    fn apply_displacement_negative_shift_works() {
        // scale=2, shift -4px west → -2 tiles west
        let (c, a) = apply_displacement((100, 200, 7), (0, 0), (-4, 0), 2);
        assert_eq!(c, (98, 200, 7));
        assert_eq!(a, (0, 0));
    }

    #[test]
    fn apply_displacement_diagonal_shift() {
        // scale=2, shift (2, 4) → +1 east, +2 south
        let (c, a) = apply_displacement((100, 200, 7), (0, 0), (2, 4), 2);
        assert_eq!(c, (101, 202, 7));
        assert_eq!(a, (0, 0));
    }

    #[test]
    fn apply_displacement_preserves_z() {
        // z no cambia con displacement (solo template match puede cambiar piso)
        let (c, _) = apply_displacement((100, 200, 14), (0, 0), (2, 0), 2);
        assert_eq!(c.2, 14);
    }

    #[test]
    fn apply_displacement_multi_frame_walk_simulation() {
        // Simula caminar 5 tiles al este a 30 Hz con scale=2 (shift 2px/tile).
        // Durante 5 frames consecutivos, cada uno con +2 px displacement.
        let scale = 2u32;
        let mut coord = (100, 200, 7);
        let mut accum = (0, 0);
        for _ in 0..5 {
            let (c, a) = apply_displacement(coord, accum, (2, 0), scale);
            coord = c;
            accum = a;
        }
        assert_eq!(coord, (105, 200, 7), "5 shifts de 2px deben dar +5 tiles");
        assert_eq!(accum, (0, 0));
    }

    #[test]
    fn apply_displacement_scale_1_every_shift_is_a_tile() {
        // scale=1 (1 px/tile): cada pixel de shift = 1 tile
        let (c, a) = apply_displacement((100, 200, 7), (0, 0), (3, -2), 1);
        assert_eq!(c, (103, 198, 7));
        assert_eq!(a, (0, 0));
    }

    #[test]
    fn apply_displacement_handles_scale_zero() {
        // scale=0 debe tratarse como 1 (max prevención)
        let (c, _) = apply_displacement((100, 200, 7), (0, 0), (1, 0), 0);
        // No panic, coord cambia por 1 tile (como scale=1)
        assert_eq!(c, (101, 200, 7));
    }

    #[test]
    fn matcher_build_template_downsamples_correctly() {
        // Scale=2 debe promediar bloques 2×2 → output tiene la mitad de dimensiones.
        let w = 8u32;
        let h = 8u32;
        let mut data = vec![0u8; (w * h * 4) as usize];
        // Pattern: todos los pixels tienen luma 100 (R=100, G=100, B=100)
        for i in 0..(w * h) as usize {
            data[i * 4]     = 100;
            data[i * 4 + 1] = 100;
            data[i * 4 + 2] = 100;
            data[i * 4 + 3] = 255;
        }
        let snap = MinimapSnapshot { width: w, height: h, data };
        let template = MinimapMatcher::build_template(&snap, 2).unwrap();
        assert_eq!(template.dimensions(), (4, 4), "downsample by 2 → 4×4");
        // Promedio de bloques uniformes sigue siendo 100.
        assert_eq!(template.get_pixel(0, 0).0[0], 100);
        assert_eq!(template.get_pixel(3, 3).0[0], 100);
    }

    // ── Disambiguation tests ─────────────────────────────────────────

    /// Genera una region de patrón único (no-periódico) de tamaño `w×h`
    /// con seed determinístico. Derivado de make_synthetic_reference para
    /// los tests de disambiguation.
    fn make_pattern_region(w: u32, h: u32, seed: u32) -> GrayImage {
        let mut img = GrayImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let v = (x.wrapping_mul(7)
                    .wrapping_add(y.wrapping_mul(13))
                    .wrapping_add(seed.wrapping_mul(23))
                    .wrapping_add(((x * y) ^ seed) % 256)
                    .wrapping_add(((x / 3) ^ (y / 3)) * 31)) % 256;
                img.put_pixel(x, y, image::Luma([v as u8]));
            }
        }
        img
    }

    #[test]
    fn crop_template_extracts_subregion() {
        // Given un template 10×10 con luma linear, crop 3×3 desde (2,2)
        // debe coincidir con los pixels correspondientes del original.
        let mut t = GrayImage::new(10, 10);
        for y in 0..10u32 {
            for x in 0..10u32 {
                t.put_pixel(x, y, image::Luma([(x * 10 + y) as u8]));
            }
        }
        let sub = MinimapMatcher::crop_template(&t, 2, 2, 3, 3).unwrap();
        assert_eq!(sub.dimensions(), (3, 3));
        for y in 0..3u32 {
            for x in 0..3u32 {
                assert_eq!(
                    sub.get_pixel(x, y).0[0],
                    t.get_pixel(x + 2, y + 2).0[0],
                    "crop pixel ({},{})", x, y
                );
            }
        }
    }

    #[test]
    fn crop_template_rejects_out_of_bounds() {
        let t = GrayImage::new(5, 5);
        assert!(MinimapMatcher::crop_template(&t, 3, 3, 3, 3).is_none());
        assert!(MinimapMatcher::crop_template(&t, 0, 0, 6, 1).is_none());
        assert!(MinimapMatcher::crop_template(&t, 0, 0, 0, 1).is_none());
    }

    #[test]
    fn ssd_normalized_at_perfect_match_returns_zero() {
        // Si el sub-template matchea exactamente los pixels del sector en
        // la posición dada, SSD normalizado debe ser 0.
        let sector_img = make_pattern_region(64, 64, 42);
        let sector = ReferenceSector {
            file_x: 32000,
            file_y: 31000,
            z: 7,
            image: sector_img.clone(),
        };
        // Sub-template = exact crop del sector en (10, 10) tamaño 8×8.
        let sub = MinimapMatcher::crop_template(&sector_img, 10, 10, 8, 8).unwrap();
        let score = MinimapMatcher::ssd_normalized_at(&sector, &sub, 10, 10).unwrap();
        assert!(score < 1e-5, "exact match SSD debe ser ~0, got {}", score);
    }

    #[test]
    fn ssd_normalized_at_returns_high_score_for_mismatch() {
        // Sub-template extraído de un patrón distinto → SSD alto.
        let sector_img = make_pattern_region(64, 64, 42);
        let sector = ReferenceSector {
            file_x: 0, file_y: 0, z: 7,
            image: sector_img,
        };
        // Sub-template = patrón totalmente distinto (seed 999).
        let sub = make_pattern_region(8, 8, 999);
        let score = MinimapMatcher::ssd_normalized_at(&sector, &sub, 10, 10).unwrap();
        assert!(score > 0.01, "mismatch SSD debe ser grande, got {}", score);
    }

    #[test]
    fn ssd_normalized_at_out_of_bounds_returns_none() {
        let sector_img = GrayImage::new(20, 20);
        let sector = ReferenceSector {
            file_x: 0, file_y: 0, z: 7, image: sector_img,
        };
        let sub = GrayImage::new(8, 8);
        // Position (15, 15) + sub 8×8 se sale del sector 20×20.
        assert!(MinimapMatcher::ssd_normalized_at(&sector, &sub, 15, 15).is_none());
        // Negative position.
        assert!(MinimapMatcher::ssd_normalized_at(&sector, &sub, -1, 0).is_none());
    }

    #[test]
    fn disambiguation_unique_match_returns_some() {
        // Given un solo sector que contiene la vista exacta → disambiguation
        // debe confirmar con ambos patches y retornar Some.
        let reference = make_synthetic_reference();
        let mut matcher = MinimapMatcher::new();
        matcher.disambiguation_enabled = true;
        matcher.match_threshold = 0.15;
        matcher.sectors_by_floor.insert(
            7,
            vec![ReferenceSector {
                file_x: 32000, file_y: 31000, z: 7,
                image: reference.clone(),
            }],
        );

        let snap = make_synthetic_minimap_from_ref(&reference, 80, 80, 60, 60);
        let result = matcher.detect(&snap, 1, None, true);
        assert!(result.is_some(), "match único debe pasar disambiguation");
        let (x, y, z) = result.unwrap();
        assert!((x - 32080).abs() <= 2);
        assert!((y - 31080).abs() <= 2);
        assert_eq!(z, 7);

        // No se incrementan contadores de disambiguation (no rechazos).
        let stats = matcher.stats_snapshot();
        assert_eq!(stats.disambiguation_misses, 0);
    }

    #[test]
    fn disambiguation_rejects_false_positive_with_corrupted_corner() {
        // Construimos 1 sector donde:
        // - La vista (60×60) está pegada en (10, 10) → primary debería
        //   matchear SSD bajo en esa posición.
        // - PERO el corner bottom-right del sector (zona del sub-template)
        //   está CORRUPTO (luma SATURADA a blanco 255), muy distinto del
        //   patrón original.
        // → primary matchea OK, secondary NO matchea en la pos geométrica
        //   esperada → disambiguation rechaza.
        //
        // Con view 60×60 y scale 1, template es 60×60, sub es 20×20 @ (40, 40).
        // Si matchea en (10, 10) del sector, sub cae en (50, 50) del sector.
        // Corrompemos esa zona con luma 255 plana (contraste máximo vs patrón).
        let mut reference = make_synthetic_reference();
        for y in 50..70u32 {
            for x in 50..70u32 {
                reference.put_pixel(x, y, image::Luma([255]));
            }
        }

        let mut matcher = MinimapMatcher::new();
        matcher.disambiguation_enabled = true;
        // Threshold MODERADO: primary debe passar (la zona corrupta es
        // 20×20 sobre 60×60 = ~11% del template, no es enough para pushear
        // primary SSD > 0.15) pero el sub-patch DE ESA MISMA zona sí debe
        // fallar (ratio 100% de corrupción dentro del sub).
        matcher.match_threshold = 0.15;
        matcher.sectors_by_floor.insert(
            7,
            vec![ReferenceSector {
                file_x: 32000, file_y: 31000, z: 7,
                image: reference.clone(),
            }],
        );

        // Snap generado desde la versión LIMPIA: simula el caso live donde
        // el char ve el mundo limpio pero el sector indexado está dañado.
        let clean_reference = make_synthetic_reference();
        let snap = make_synthetic_minimap_from_ref(&clean_reference, 40, 40, 60, 60);

        // Con disambiguation ON, el top-1 (sector único) falla el check del
        // sub-patch y detect retorna None.
        let result = matcher.detect(&snap, 1, None, true);
        assert_eq!(
            result, None,
            "disambiguation debe rechazar el único candidato cuyo sub-patch no matchea"
        );
        let stats = matcher.stats_snapshot();
        assert!(
            stats.disambiguation_rejects >= 1,
            "expected disamb_rejects ≥ 1, got {}", stats.disambiguation_rejects
        );
        assert_eq!(stats.disambiguation_misses, 1);
    }

    #[test]
    fn disambiguation_disabled_accepts_top1_legacy() {
        // Con disambiguation OFF, el mismo escenario del test anterior
        // (sector con bottom-right corrupto) SÍ retorna Some.
        // Esto verifica que el flag preserva backward compat.
        let mut reference = make_synthetic_reference();
        for y in 50..70u32 {
            for x in 50..70u32 {
                if x < 256 && y < 256 {
                    let v = (x.wrapping_mul(199).wrapping_add(y.wrapping_mul(211))) as u8;
                    reference.put_pixel(x, y, image::Luma([v ^ 0xA5]));
                }
            }
        }

        let mut matcher = MinimapMatcher::new();
        matcher.disambiguation_enabled = false; // <<< OFF
        matcher.match_threshold = 0.50;
        matcher.sectors_by_floor.insert(
            7,
            vec![ReferenceSector {
                file_x: 32000, file_y: 31000, z: 7,
                image: reference,
            }],
        );

        let clean_reference = make_synthetic_reference();
        let snap = make_synthetic_minimap_from_ref(&clean_reference, 40, 40, 60, 60);

        let result = matcher.detect(&snap, 1, None, true);
        assert!(
            result.is_some(),
            "con disambiguation=OFF debe retornar Some (legacy behavior)"
        );
        let stats = matcher.stats_snapshot();
        assert_eq!(
            stats.disambiguation_rejects, 0,
            "disambiguation OFF no debe incrementar rejects"
        );
    }

    #[test]
    fn disambiguation_picks_correct_candidate_from_top_k() {
        // Given dos sectores:
        //   A: contiene la vista LIMPIA en (40, 40) → primary OK + sub OK.
        //   B: contiene la vista COPIADA pero con corner BR corrupto → primary
        //      podría ganar por casualidad pero sub-patch no matchea.
        // La lógica de top-K debe probar B primero si tiene mejor primary score,
        // rechazarlo por sub-check, y caer al 2do candidato (A) que sí valida.
        //
        // Para forzar que B tenga mejor primary score que A, hacemos B un
        // clone de A pero con LEVE ruido global (menor que el threshold) EXCEPTO
        // en la zona del sub-patch (donde pondremos algo MUY distinto).
        // En la práctica B no tendrá mejor score que A, pero construimos las
        // assertions para ser tolerantes: el resultado final DEBE ser A.

        let clean = make_synthetic_reference();

        // Sector A: vista limpia.
        let sector_a_img = clean.clone();

        // Sector B: clone de A pero con bottom-right CORRUPTO (donde caerá
        // el sub-patch si match_sector elige la misma posición que A).
        let mut sector_b_img = clean.clone();
        for y in 40..120u32 {
            for x in 40..120u32 {
                // Solo corrompemos zona bottom-right del área de match
                // (centro visto = 60, bottom-right ~ 80-120).
                if x >= 80 && y >= 80 {
                    let v = (x.wrapping_mul(17).wrapping_add(y.wrapping_mul(23))) as u8;
                    sector_b_img.put_pixel(x, y, image::Luma([v ^ 0xFF]));
                }
            }
        }

        let mut matcher = MinimapMatcher::new();
        matcher.disambiguation_enabled = true;
        matcher.match_threshold = 0.50;
        matcher.sectors_by_floor.insert(
            7,
            vec![
                ReferenceSector { file_x: 32000, file_y: 31000, z: 7, image: sector_a_img },
                ReferenceSector { file_x: 33000, file_y: 32000, z: 7, image: sector_b_img },
            ],
        );

        // Snap desde la vista LIMPIA: solo sector A debe matchear ambos patches.
        let snap = make_synthetic_minimap_from_ref(&clean, 60, 60, 60, 60);

        let result = matcher.detect(&snap, 1, None, true);
        assert!(result.is_some(), "al menos sector A debe matchear");
        let (x, _y, _z) = result.unwrap();
        // Debe caer en sector A (file_x 32000) no en B (33000).
        assert!(
            x >= 32000 && x < 33000,
            "detect debe resolver en sector A (32000..33000), got x={}", x
        );
    }

    #[test]
    fn disambiguation_stats_snapshot_exposes_flags() {
        let matcher = MinimapMatcher::new();
        let s = matcher.stats_snapshot();
        assert!(s.disambiguation_enabled, "default es ON");
        assert_eq!(s.disambiguation_rejects, 0);
        assert_eq!(s.disambiguation_misses, 0);
    }
}
