#!/usr/bin/env python3
"""
train_inventory_classifier.py — Entrena un CNN classifier para inventory crops 32x32.

Toma un manifest.csv etiquetado (producido por DatasetRecorder + label_dataset CLI)
y entrena un modelo PyTorch que clasifica crops 32x32 en N clases. Exporta ONNX
para consumo por el bot Rust via crate `ort`.

USO:
    pip install -r requirements.txt
    python train_inventory_classifier.py \
        --manifest ../datasets/abdendriel/manifest.csv \
        --output models/inventory_v1.onnx \
        --epochs 30 \
        --batch-size 32

ARGUMENTOS:
    --manifest    Path al manifest.csv etiquetado.
    --output      Path destino del modelo ONNX (carpeta se crea si no existe).
    --epochs      Epochs de entrenamiento (default 30).
    --batch-size  Batch size (default 32).
    --val-split   Fracción de validación (default 0.2).
    --device      cpu / cuda (default auto).
    --seed        Random seed (default 42).
    --no-augment  Desactiva data augmentation (debug).

OUTPUT:
    - models/inventory_v1.onnx           # modelo exportado
    - models/inventory_v1.classes.json   # mapping idx → class name
    - models/inventory_v1.metrics.json   # train/val accuracy + loss curves

ARQUITECTURA:
    Input: 3×32×32 RGB (normalizado [0,1])
    Conv2d(3,16,3,p=1) + ReLU + MaxPool(2)   → 16×16×16
    Conv2d(16,32,3,p=1) + ReLU + MaxPool(2)  → 32×8×8
    Conv2d(32,64,3,p=1) + ReLU + MaxPool(2)  → 64×4×4
    Flatten                                   → 1024
    Linear(1024,128) + ReLU + Dropout(0.3)
    Linear(128, N_classes)

Total params: ~150K. ONNX ~600 KB. Inference CPU < 5ms.

DATA AUGMENTATION (default on):
    - RandomHorizontalFlip(p=0.0)             # NO flipear: items asimétricos
    - ColorJitter(brightness=0.15, contrast=0.15)
    - RandomAffine(degrees=0, translate=(0.05, 0.05))   # ±2 px shift
    - Normalización a [0, 1]

NOTA HARDCODE:
    Acepta solo crops exactos 32×32. Si manifest tiene crops de otra dimensión,
    falla con error claro.
"""

import argparse
import json
import os
import random
import sys
from pathlib import Path
from typing import List, Tuple

try:
    import pandas as pd
    import torch
    import torch.nn as nn
    import torch.nn.functional as F
    from torch.utils.data import Dataset, DataLoader, random_split
    from torchvision import transforms
    from PIL import Image
except ImportError as e:
    print(f"ERROR: dependency missing: {e}", file=sys.stderr)
    print("Run: pip install -r requirements.txt", file=sys.stderr)
    sys.exit(1)


# ── Modelo ───────────────────────────────────────────────────────────────────

class InventoryClassifier(nn.Module):
    """CNN pequeño para 32×32 → N classes."""
    def __init__(self, num_classes: int):
        super().__init__()
        self.conv1 = nn.Conv2d(3,  16, kernel_size=3, padding=1)
        self.conv2 = nn.Conv2d(16, 32, kernel_size=3, padding=1)
        self.conv3 = nn.Conv2d(32, 64, kernel_size=3, padding=1)
        self.pool  = nn.MaxPool2d(2, 2)
        self.fc1   = nn.Linear(64 * 4 * 4, 128)
        self.fc2   = nn.Linear(128, num_classes)
        self.drop  = nn.Dropout(0.3)

    def forward(self, x):
        x = self.pool(F.relu(self.conv1(x)))
        x = self.pool(F.relu(self.conv2(x)))
        x = self.pool(F.relu(self.conv3(x)))
        x = x.view(x.size(0), -1)
        x = self.drop(F.relu(self.fc1(x)))
        return self.fc2(x)


# ── Dataset ──────────────────────────────────────────────────────────────────

class InventoryDataset(Dataset):
    """Carga crops del manifest, aplica transform."""
    def __init__(self,
                 rows: List[Tuple[str, int]],
                 crops_dir: Path,
                 transform: transforms.Compose):
        self.rows      = rows
        self.crops_dir = crops_dir
        self.transform = transform

    def __len__(self):
        return len(self.rows)

    def __getitem__(self, idx):
        filename, label_idx = self.rows[idx]
        img = Image.open(self.crops_dir / filename).convert("RGB")
        if img.size != (32, 32):
            raise RuntimeError(
                f"crop {filename} tiene tamaño {img.size}, esperado (32, 32)")
        return self.transform(img), label_idx


def build_transforms(augment: bool):
    if augment:
        return transforms.Compose([
            transforms.ColorJitter(brightness=0.15, contrast=0.15),
            transforms.RandomAffine(degrees=0, translate=(0.05, 0.05)),
            transforms.ToTensor(),
        ])
    return transforms.Compose([transforms.ToTensor()])


# ── Training ─────────────────────────────────────────────────────────────────

def train_one_epoch(model, loader, optimizer, criterion, device):
    model.train()
    total_loss, correct, total = 0.0, 0, 0
    for imgs, labels in loader:
        imgs, labels = imgs.to(device), labels.to(device)
        optimizer.zero_grad()
        out = model(imgs)
        loss = criterion(out, labels)
        loss.backward()
        optimizer.step()
        total_loss += loss.item() * imgs.size(0)
        correct += (out.argmax(1) == labels).sum().item()
        total += labels.size(0)
    return total_loss / total, correct / total


@torch.no_grad()
def eval_epoch(model, loader, criterion, device):
    model.eval()
    total_loss, correct, total = 0.0, 0, 0
    for imgs, labels in loader:
        imgs, labels = imgs.to(device), labels.to(device)
        out = model(imgs)
        loss = criterion(out, labels)
        total_loss += loss.item() * imgs.size(0)
        correct += (out.argmax(1) == labels).sum().item()
        total += labels.size(0)
    return total_loss / total, correct / total


# ── Main ─────────────────────────────────────────────────────────────────────

def main():
    p = argparse.ArgumentParser(description=__doc__,
                                formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--manifest", required=True, type=Path,
                   help="Path al manifest.csv etiquetado")
    p.add_argument("--output", required=True, type=Path,
                   help="Path destino del modelo ONNX")
    p.add_argument("--epochs", type=int, default=30)
    p.add_argument("--batch-size", type=int, default=32)
    p.add_argument("--val-split", type=float, default=0.2)
    p.add_argument("--lr", type=float, default=1e-3)
    p.add_argument("--device", default="auto", choices=["auto", "cpu", "cuda"])
    p.add_argument("--seed", type=int, default=42)
    p.add_argument("--no-augment", action="store_true")
    args = p.parse_args()

    # Setup determinismo
    random.seed(args.seed)
    torch.manual_seed(args.seed)

    # Resolver device
    if args.device == "auto":
        device = "cuda" if torch.cuda.is_available() else "cpu"
    else:
        device = args.device
    print(f"Device: {device}")

    # Cargar manifest
    if not args.manifest.exists():
        print(f"ERROR: manifest no existe: {args.manifest}", file=sys.stderr)
        sys.exit(1)
    df = pd.read_csv(args.manifest)
    crops_dir = args.manifest.parent / "crops"
    if not crops_dir.exists():
        print(f"ERROR: crops dir no existe: {crops_dir}", file=sys.stderr)
        sys.exit(1)

    # Filtrar etiquetadas
    df = df[df["label"].notna() & (df["label"].astype(str).str.strip() != "")]
    if len(df) == 0:
        print("ERROR: 0 filas etiquetadas en manifest. Etiquetar primero con label_dataset.",
              file=sys.stderr)
        sys.exit(1)
    print(f"Filas etiquetadas: {len(df)}")

    # Mapping label → idx
    classes = sorted(df["label"].unique())
    class_to_idx = {c: i for i, c in enumerate(classes)}
    print(f"Clases ({len(classes)}): {classes}")

    # Distribución
    print("\nDistribución por clase:")
    for c in classes:
        n = (df["label"] == c).sum()
        print(f"  {c:<25} {n:>5}")

    # Build dataset rows
    rows = [(row["filename"], class_to_idx[row["label"]]) for _, row in df.iterrows()]
    random.shuffle(rows)

    # Train/val split
    n_val = max(1, int(len(rows) * args.val_split))
    train_rows = rows[n_val:]
    val_rows   = rows[:n_val]
    print(f"\nTrain: {len(train_rows)}, Val: {len(val_rows)}")

    train_ds = InventoryDataset(train_rows, crops_dir,
                                build_transforms(augment=not args.no_augment))
    val_ds   = InventoryDataset(val_rows, crops_dir,
                                build_transforms(augment=False))
    train_loader = DataLoader(train_ds, batch_size=args.batch_size, shuffle=True,
                              num_workers=0)
    val_loader   = DataLoader(val_ds,   batch_size=args.batch_size, shuffle=False,
                              num_workers=0)

    # Model
    model = InventoryClassifier(len(classes)).to(device)
    n_params = sum(p.numel() for p in model.parameters())
    print(f"Modelo: {n_params:,} parameters")

    optimizer = torch.optim.Adam(model.parameters(), lr=args.lr)
    criterion = nn.CrossEntropyLoss()

    # Training loop
    metrics = {"epochs": [], "train_loss": [], "train_acc": [],
               "val_loss": [], "val_acc": []}
    best_val_acc = 0.0
    print("\n=== Training ===")
    for epoch in range(1, args.epochs + 1):
        tl, ta = train_one_epoch(model, train_loader, optimizer, criterion, device)
        vl, va = eval_epoch(model, val_loader, criterion, device)
        metrics["epochs"].append(epoch)
        metrics["train_loss"].append(tl)
        metrics["train_acc"].append(ta)
        metrics["val_loss"].append(vl)
        metrics["val_acc"].append(va)
        marker = ""
        if va > best_val_acc:
            best_val_acc = va
            marker = "  *"
        print(f"Epoch {epoch:>3}  "
              f"train_loss={tl:.4f} acc={ta:.3f}  "
              f"val_loss={vl:.4f} acc={va:.3f}{marker}")

    print(f"\nBest val acc: {best_val_acc:.3f}")

    # Export ONNX
    args.output.parent.mkdir(parents=True, exist_ok=True)
    model.eval()
    dummy = torch.zeros(1, 3, 32, 32, device=device)
    torch.onnx.export(
        model, dummy, str(args.output),
        input_names=["input"], output_names=["logits"],
        dynamic_axes={"input": {0: "batch"}, "logits": {0: "batch"}},
        opset_version=17,
    )
    print(f"✓ ONNX exportado: {args.output}")

    # Sidecar files
    classes_path = args.output.with_suffix(".classes.json")
    with open(classes_path, "w") as f:
        json.dump({"classes": classes, "input_size": [3, 32, 32]}, f, indent=2)
    print(f"✓ Classes: {classes_path}")

    metrics_path = args.output.with_suffix(".metrics.json")
    with open(metrics_path, "w") as f:
        json.dump({
            "best_val_acc": best_val_acc,
            "n_train": len(train_rows),
            "n_val":   len(val_rows),
            "n_classes": len(classes),
            "history": metrics,
        }, f, indent=2)
    print(f"✓ Metrics: {metrics_path}")


if __name__ == "__main__":
    main()
