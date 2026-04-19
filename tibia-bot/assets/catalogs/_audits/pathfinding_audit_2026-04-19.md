# Pathfinding Audit — 16 hunts del catálogo

Fecha: 2026-04-19
Método: `path_finder` con `assets/walkability.bin` (11M tiles, 790k transitions, 373 floor files de TibiaMaps.io).

## Resultado final (post fix Darashia + extensión `--nearest-walkable 20`)

| Hunt                         | Estado | Path | Remap |
|------------------------------|--------|------|-------|
| trolls_thais                 | OK     | 39 tiles, 2 floor changes   | — |
| abdendriel_wasps             | OK     | 92 tiles, 0 floor changes   | — |
| rotworms_darashia            | OK     | 63 tiles, 2 floor changes   | — |
| ancient_scarabs_darashia     | OK     | 273 tiles, 7 floor changes  | goal +1 tile |
| cyclops_edron                | OK     | 258 tiles, 5 floor changes  | goal +3 tiles (z=7→8) |
| stone_golems_cormaya         | OK     | 183 tiles, 2 floor changes  | goal +1 tile (z=8→9) |
| drakens_edron_bottom         | OK     | 226 tiles, 6 floor changes  | goal +3 tiles (z=10→7) |
| hero_cave_edron              | OK     | 449 tiles, 12 floor changes | goal +5 tiles (z=8→9) |
| mutated_rats_yalahar         | OK     | 84 tiles, 4 floor changes   | goal +1 tile (z=10→8) |
| mutated_tigers_yalahar       | OK     | 87 tiles, 5 floor changes   | goal +1 tile |
| dwarves_kazordoon            | OK     | 175 tiles, 3 floor changes  | start +1 tile (z=11→10) |
| corym_mine_port_hope         | OK     | 86 tiles, 2 floor changes   | goal +1 tile (z=8→7) |
| orcs_thais                   | FAIL   | no hay path (~600 tiles)    | A* max_nodes exceeded |
| minos_plains_of_havoc        | FAIL   | no hay path (~500 tiles)    | A* max_nodes exceeded |
| water_elementals_portimi     | FAIL   | no hay path (~1200 tiles)   | cruza agua entre islas |
| asura_palace_low             | FAIL   | no hay path (~500 tiles)    | Port Hope → Asura distance |

**Stats finales**: 12/16 reachable (75%) — **up from 3/16 (19%) pre-extensión**.

## Evolución de la auditoría

1. **Batch inicial**: 2/16 OK (trolls, abdendriel). 14 FAIL por coord inválida.
2. **Post-fix Darashia z=1→z=7**: 3/16 OK (+rotworms).
3. **Post-extensión `--nearest-walkable 20`**: 12/16 OK (+9 hunts via goal/start remap).

## Fixes aplicados

### 1. `cities.toml` (commit `d6e37f6`)
- Darashia depot/temple/NPCs: z=1 → z=7 (error tipográfico).

### 2. `bot/src/bin/path_finder.rs` (nueva flag `--nearest-walkable N`)
- Si start o goal no son walkable y N > 0, busca tile walkable más cercano por Chebyshev distance.
- BFS esférico shell-by-shell (radio 1,2,...,N).
- Log explícito cada remap: `"goal remapped: (X,Y,Z) → (X',Y',Z') (distance=D tiles)"`.
- Tests: `chebyshev_returns_max_axis_delta`.

## Hunts restantes — fallan por `no hay path`

Los 4 hunts restantes NO fallan por coord inválida (coords son válidas después del remap). Fallan porque A* no encuentra ruta. Causas probables:

| Hunt | Depot → Spawn | Distancia XY | Causa |
|------|---------------|--------------|-------|
| orcs_thais | Thais → Orc Fortress | ~600 tiles | A* max_nodes exceeded o bridges de montaña |
| minos_plains_of_havoc | Carlin → Plains of Havoc | ~470 tiles | A* max_nodes exceeded |
| water_elementals_portimi | Liberty Bay → Portimi | ~1170 tiles | cruza agua entre islas (requiere boat route) |
| asura_palace_low | Port Hope → Asura Palace | ~490 tiles | A* max_nodes exceeded |

**Posibles soluciones** (fuera de scope offline):
- Subir `MAX_NODES` del A* en `pathfinding::find_path` (ver `bot/src/pathfinding/`).
- Agregar waypoints intermedios como check-points (depot → waypoint1 → waypoint2 → spawn).
- Pathfinding jerárquico (HPA*) para distancias largas.
- Usar `pathfinding_overrides.toml` para habilitar rutas de mar/boat.

## Semantic note sobre remap

El `--nearest-walkable 20` hace que path_finder tolere hasta **20 tiles de error** en el coord original. Esto es útil para el catálogo (coords aproximadas) pero **no se debe usar en producción del cavebot real**, porque el runtime del cavebot ejecuta nodes tile-exact. El flag es solo para generar los primeros waypoints viables; una vez generado el snippet, el usuario debe validar el path completo y ajustar las coords finales en sesión live.

## Recomendación operativa

- Usar el snippet generado con `--nearest-walkable` como **punto de partida** para cavebot scripts.
- Validar cada waypoint en sesión live (`/vision/cursor`) antes de subir `enabled=true`.
- Los 4 hunts que fallan por `no hay path` requieren o bien una sesión live donde se calibren waypoints intermedios, o bien la mejora de `find_path` para long-distance.
