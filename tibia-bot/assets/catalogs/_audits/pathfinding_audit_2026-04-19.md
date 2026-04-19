# Pathfinding Audit — 16 hunts del catálogo

Fecha: 2026-04-19
Método: `path_finder` con `assets/walkability.bin` (11M tiles, 790k transitions, 373 floor files de TibiaMaps.io).

## Resultado batch (post fix Darashia z=1→7)

| Hunt                         | Estado | Detalle                         |
|------------------------------|--------|---------------------------------|
| trolls_thais                 | OK     | 39 tiles, 2 floor changes       |
| abdendriel_wasps             | OK     | 92 tiles, 0 floor changes       |
| rotworms_darashia            | OK     | 63 tiles, 2 floor changes       |
| orcs_thais                   | FAIL   | no hay path posible (~600 tiles, A* se rinde) |
| minos_plains_of_havoc        | FAIL   | goal no walkable (cost=255)     |
| cyclops_edron                | FAIL   | goal no walkable (cost=255)     |
| mutated_rats_yalahar         | FAIL   | goal no walkable (cost=255, z=10) |
| stone_golems_cormaya         | FAIL   | goal no walkable (cost=255, z=8) |
| ancient_scarabs_darashia     | FAIL   | goal no walkable (cost=255, z=8) |
| water_elementals_portimi     | FAIL   | goal no en grid (cost=None, z=8) |
| dwarves_kazordoon            | FAIL   | start no walkable (depot z=11 inválido) |
| drakens_edron_bottom         | FAIL   | goal no en grid (cost=None, z=10) |
| mutated_tigers_yalahar       | FAIL   | goal no walkable (cost=255, z=9) |
| hero_cave_edron              | FAIL   | goal no en grid (cost=None, z=8) |
| asura_palace_low             | FAIL   | goal no en grid (cost=None, z=8) |
| corym_mine_port_hope         | FAIL   | goal no en grid (cost=None, z=8) |

**Stats**: 3/16 reachable (19%), 13/16 requieren fix.

## Clasificación de fails

### `start no walkable` (1 hunt)
- **dwarves_kazordoon**: depot kazordoon declarado en z=11, pero (32647,31925,11) no es walkable.
  - Probé (32647,31917,7) → walkable.
  - Probé (32661,31918,6) y (32661,31918,8) → walkables.
  - **Sin coord exacta del Kazordoon Main Depot, no puedo fijar offline.**

### `goal no walkable (cost=255)` (5 hunts)
Tile destino existe en grid pero es pared/agua/bloqueado. Probablemente el start_coord apunta dentro del spawn interior de la cueva, no a la entrada caminable.
- minos_plains_of_havoc (z=7 surface)
- cyclops_edron (z=7 surface)
- mutated_rats_yalahar (z=10)
- stone_golems_cormaya (z=8)
- ancient_scarabs_darashia (z=8)
- mutated_tigers_yalahar (z=9)

### `goal no en grid (cost=None)` (6 hunts)
Tile destino NO está mapeado en walkability.bin — TibiaMaps.io no cubre esa área (spawns profundos privados o post-corte del mapa).
- water_elementals_portimi (z=8)
- drakens_edron_bottom (z=10)
- hero_cave_edron (z=8)
- asura_palace_low (z=8)
- corym_mine_port_hope (z=8)

### `no hay path posible` (1 hunt)
A* no encuentra ruta aunque ambos tiles existan.
- orcs_thais: Thais depot (z=7) → Orc Fortress (z=7), ~600 tiles de distancia cruzando montañas. Probablemente hay overrides de pathfinding que cortan rutas de montaña.

## Lo que requeriría arreglo completo

1. **Coords validadas externamente**: acceso a tibiamaps.io lookup interactivo o sesión live con `/vision/perception`.
2. **Walkability grid extendida**: algunas áreas (cuevas profundas, mines) no están en TibiaMaps.io y requieren data adicional.
3. **Overrides de pathfinding**: `pathfinding_overrides.toml` para rutas de montaña (orcs_thais).
4. **Extensión de path_finder**: "find nearest walkable tile" para tolerar start/goal off por 1-2 tiles.

## Probado offline sin éxito

- Probé ±3z en coords del catálogo para starts fallidos: ninguno encontró walkable.
- Probé sector notation de wiki (Mapper Coords) para Kazordoon: resultó en coords cercanas pero no el depot exacto.

## Recomendación

Este audit queda como **data de diagnóstico**. Para progresar:
- Opción A: sesión live, calibrar los 13 start_coords con `/vision/cursor` parado en cada entrada de cueva.
- Opción B: descargar mapas adicionales o usar API de tibiamaps.io para lookup batch de coords.
- Opción C: extender `path_finder` con modo "nearest walkable" (buscar tile walkable más cercano al goal dentro de radio N, y usar ese).
