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

impl MapIndex {
    #[allow(dead_code)]
    pub fn new() -> Self { Self::default() }

    /// Inserta un hash → posición en el índice.
    #[allow(dead_code)]
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
    #[allow(dead_code)]
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
}
