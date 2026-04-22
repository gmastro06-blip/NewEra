# Training pipeline ML para tibia-bot (Fase 2)

Pipeline Python para entrenar un classifier que reemplace el matcher SSE
de inventory por un modelo ML. Output: ONNX consumible desde Rust con
`ort` crate (Fase 2.5).

## Workflow completo

### 1. Capturar dataset (Rust, sesión live)

Con el bot corriendo + Tibia/OBS activos:

```bash
TOKEN="<bearer del config>"

# Iniciar capture
curl -X POST -H "Authorization: Bearer $TOKEN" \
    "http://localhost:8080/dataset/start?dir=datasets/abdendriel_v1&interval=15&tag=hunt1"

# Sesión hunt normal — abrí distintos backpacks, variá items, mové el
# char por zonas con distintos mobs. Más variedad = mejor modelo.

# Verificar progreso
curl -H "Authorization: Bearer $TOKEN" http://localhost:8080/dataset/status
# {"active":true,"crops_total":234,"dir":"datasets/abdendriel_v1"}

# Detener
curl -X POST -H "Authorization: Bearer $TOKEN" \
    http://localhost:8080/dataset/stop
```

**Target volumen**: 500-2000 crops con balance entre clases. Sesión de
~30 min con `interval=15` produce ~3000 crops.

### 2. Etiquetar (Rust CLI, offline)

```bash
cargo run --release --bin label_dataset -- \
    --manifest datasets/abdendriel_v1/manifest.csv \
    --classes vial,golden_backpack,green_backpack,white_key,dragon_ham,empty
```

El tool abre cada PNG en el viewer del OS y prompts por clase. Atajos:
- `0..9` / `a..z` para selección rápida del menú
- `s` skip, `u` undo, `?` re-mostrar menú, `q` quit con auto-save

Auto-save cada 10 labels para no perder progreso.

### 3. Entrenar (Python, este directorio)

```bash
cd tibia-bot/ml
python -m venv .venv
.venv\Scripts\activate  # Windows
# source .venv/bin/activate  # Linux/Mac

pip install -r requirements.txt

python train_inventory_classifier.py \
    --manifest ../datasets/abdendriel_v1/manifest.csv \
    --output models/inventory_v1.onnx \
    --epochs 30
```

**Outputs**:
- `models/inventory_v1.onnx` — modelo
- `models/inventory_v1.classes.json` — mapping idx → label
- `models/inventory_v1.metrics.json` — train/val accuracy curves

**Target accuracy**: > 95% val accuracy con ≥ 100 crops por clase.
Si está bajo, capturar más data y volver al paso 1.

### 4. Integrar al bot (Rust, Fase 2.5)

```toml
# bot/config.toml
[ml]
inventory_classifier = "ml/models/inventory_v1.onnx"
classes_file         = "ml/models/inventory_v1.classes.json"
confidence_threshold = 0.80
use_ml               = true   # false = fallback a SSE matcher
```

(Esta integración será commiteada en Fase 2.5.)

## Arquitectura del modelo

CNN simple — overkill evitado:

```
Input: 3×32×32 RGB
Conv2d(3, 16, 3, pad=1) + ReLU + MaxPool(2)  → 16×16×16
Conv2d(16, 32, 3, pad=1) + ReLU + MaxPool(2) → 32×8×8
Conv2d(32, 64, 3, pad=1) + ReLU + MaxPool(2) → 64×4×4
Flatten                                       → 1024
Linear(1024, 128) + ReLU + Dropout(0.3)
Linear(128, N_classes)
```

~150K params, ONNX ~600 KB, inferencia CPU < 5ms.

## Data augmentation

Aplicada solo al train set:
- ColorJitter (brightness, contrast ±0.15)
- RandomAffine translate ±5% (≈ ±1.6 px shift)

**NO** se aplica horizontal flip — los items de Tibia son asimétricos
(ej. el wand_of_inferno apunta a la derecha; flippeado parecería otro
item). El bot recibe slots en orientación fija.

## Troubleshooting

### Val acc bajo (<70%)

- **Insuficientes crops por clase**: revisar distribución (script lo imprime
  al inicio). Apuntar a ≥50 crops por clase, ideal ≥100.
- **Clases muy similares**: dragon_ham vs green_backpack pueden parecerse.
  Capturar más diversidad o considerar fusionar clases.
- **Crops mal alineados**: verificar calibration `inventory_backpack_strip`
  con `tune_inventory_strip` primero.

### "0 filas etiquetadas"

El manifest aún no fue procesado por `label_dataset`. Etiquetar primero.

### CUDA no disponible

Por defecto el script detecta auto. Para CPU explícito:
```bash
python train_inventory_classifier.py --device cpu ...
```

CPU training de 1000 crops × 30 epochs ≈ 5-10 min.

### ONNX inference lento

El modelo es simple — debería estar < 5ms en CPU. Si está >50ms en runtime
Rust, verificar que `ort` no usa providers no acelerados (debug logging).
