# assets/templates/status/

Coloca aquí los templates PNG para detección de condiciones de estado.

El bot busca cada template dentro del ROI `status_icons` del frame usando
template matching (SSD normalizado de imageproc).

## Nombres de archivo → condición

| Archivo          | Condición       |
|------------------|-----------------|
| poisoned.png     | Poisoned        |
| burning.png      | Burning         |
| electrified.png  | Electrified     |
| drowning.png     | Drowning        |
| freezing.png     | Freezing        |
| dazzled.png      | Dazzled         |
| cursed.png       | Cursed          |
| bleeding.png     | Bleeding        |
| haste.png        | Haste           |
| protection.png   | Protection      |
| strengthened.png | Strengthened    |
| infight.png      | InFight         |
| hungry.png       | Hungry          |
| drunk.png        | Drunk           |
| magic_shield.png | MagicShield     |
| slowed.png       | SlowedDown      |

## Cómo capturar un template

1. Activa la condición en Tibia (o encuentra un screenshot con ella).
2. Recorta el ícono exacto (~11x11 px) de `frame_reference.png`.
3. Guarda con el nombre de la tabla (sin extensión = nombre del enum).

## Notas

- El tamaño exacto importa: un template más grande que el ROI no se busca.
- Los templates se convierten a escala de grises internamente.
- Ajusta el umbral `MATCH_THRESHOLD` en `status_icons.rs` según la precisión
  (default: 0.15 SSD normalizado — menor = más estricto).
