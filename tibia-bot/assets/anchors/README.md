# assets/anchors/

Coloca aquí los templates PNG usados como anclas de referencia.

Un ancla es un recorte de un elemento estático de la UI de Tibia
(borde del marco de HP, ícono fijo, etc.) que el bot usa para detectar
si la ventana de Tibia se desplazó y corregir todos los ROIs.

## Cómo crear un ancla

1. Abre `frame_reference.png` en un editor de imagen.
2. Recorta una región pequeña (20-40px) de un elemento **estático**
   que esté en los bordes de la UI (no en el área de juego).
3. Guarda como `nombre_ancla.png` en este directorio.
4. Agrega la definición en `calibration.toml`:

```toml
[[anchors]]
name          = "nombre_ancla"
template_path = "nombre_ancla.png"
expected_roi  = { x = X, y = Y, w = W, h = H }
```

Donde `expected_roi` es la posición de la región en `frame_reference.png`.

## Recomendaciones

- Usa elementos con textura única (no fondos lisos ni degradados).
- Tamaño recomendado: 20x20 a 50x50 px.
- Evita zonas que cambien entre frames (animaciones, HP bars, etc.).
- 1-2 anclas es suficiente para la mayoría de setups.
