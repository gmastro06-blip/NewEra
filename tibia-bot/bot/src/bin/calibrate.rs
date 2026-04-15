/// calibrate — Herramienta de calibración visual del bot.
///
/// Uso: calibrate [frame.png] [assets_dir]
///   frame.png   — Frame de referencia. Default: frame_reference.png
///   assets_dir  — Directorio de assets donde guardar calibration.toml. Default: assets
///
/// Controles:
///   Click + drag → dibuja un ROI en la imagen
///   R → asignar ROI a HP bar
///   M → asignar ROI a mana bar
///   B → asignar ROI a battle list
///   S → asignar ROI a status icons
///   N → asignar ROI a minimap
///   G → asignar ROI a game viewport
///   Enter / Ctrl+S → guardar calibration.toml
///   Esc → salir sin guardar
// Incluimos solo calibration.rs (solo depende de crates externos, no de otros módulos).
#[path = "../sense/vision/calibration.rs"]
mod calibration;

use std::path::PathBuf;

use crate::calibration::{Calibration, RoiDef};
use eframe::egui;
use egui::{Color32, Pos2, Rect, Stroke, Vec2};

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let frame_path = args.get(1).map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("frame_reference.png"));
    let assets_dir = args.get(2).map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("assets"));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("tibia-bot calibrate")
            .with_inner_size([1280.0, 780.0]),
        ..Default::default()
    };

    eframe::run_native(
        "tibia-bot calibrate",
        options,
        Box::new(move |cc| {
            Ok(Box::new(CalibrateApp::new(cc, frame_path.clone(), assets_dir.clone())))
        }),
    )
}

#[derive(Default)]
struct DrawState {
    origin:  Option<Pos2>,
    current: Option<Pos2>,
}

#[derive(Default, Clone, Copy)]
struct SelectedRoi {
    x: f32, y: f32, w: f32, h: f32,
}

impl SelectedRoi {
    fn to_roi_def(self, scale: f32) -> RoiDef {
        RoiDef::new(
            (self.x / scale).max(0.0) as u32,
            (self.y / scale).max(0.0) as u32,
            (self.w / scale).max(0.0) as u32,
            (self.h / scale).max(0.0) as u32,
        )
    }
}

struct CalibrateApp {
    frame_path:  PathBuf,
    assets_dir:  PathBuf,
    texture:     Option<egui::TextureHandle>,
    img_size:    (u32, u32),
    calibration: Calibration,
    draw:        DrawState,
    selected:    Option<SelectedRoi>,
    status_msg:  String,
    scale:       f32,
}

impl CalibrateApp {
    fn new(_cc: &eframe::CreationContext<'_>, frame_path: PathBuf, assets_dir: PathBuf) -> Self {
        let cal_path = assets_dir.join("calibration.toml");
        let (calibration, status_msg) = match Calibration::load(&cal_path) {
            Ok(c) if c.is_usable() => {
                let msg = format!("Calibración cargada desde '{}'", cal_path.display());
                (c, msg)
            }
            _ => (
                Calibration::default(),
                "Sin calibración. Dibuja ROIs y asígnalos con R/M/B/S/N/G. Enter para guardar.".into(),
            ),
        };
        Self {
            frame_path,
            assets_dir,
            texture:  None,
            img_size: (0, 0),
            calibration,
            draw:     DrawState::default(),
            selected: None,
            status_msg,
            scale:    1.0,
        }
    }

    fn load_texture(&mut self, ctx: &egui::Context) {
        if self.texture.is_some() { return; }
        match image::open(&self.frame_path) {
            Ok(img) => {
                let rgba = img.to_rgba8();
                let (w, h) = (rgba.width(), rgba.height());
                self.img_size = (w, h);
                let color_image = egui::ColorImage::from_rgba_unmultiplied(
                    [w as usize, h as usize],
                    &rgba,
                );
                self.texture = Some(ctx.load_texture(
                    "frame_ref",
                    color_image,
                    egui::TextureOptions::default(),
                ));
            }
            Err(e) => {
                self.status_msg = format!(
                    "ERROR cargando '{}': {}",
                    self.frame_path.display(), e
                );
            }
        }
    }

    fn save(&mut self) {
        std::fs::create_dir_all(&self.assets_dir).ok();
        let path = self.assets_dir.join("calibration.toml");
        match self.calibration.save(&path) {
            Ok(())  => self.status_msg = format!("Guardado: '{}'", path.display()),
            Err(e)  => self.status_msg = format!("ERROR guardando: {}", e),
        }
    }

    fn assign(&mut self, target: &str) {
        let Some(sel) = self.selected else { return; };
        let roi = sel.to_roi_def(self.scale);
        match target {
            "hp"       => self.calibration.hp_bar        = Some(roi),
            "mana"     => self.calibration.mana_bar      = Some(roi),
            "battle"   => self.calibration.battle_list   = Some(roi),
            "status"   => self.calibration.status_icons  = Some(roi),
            "minimap"  => self.calibration.minimap       = Some(roi),
            "viewport" => self.calibration.game_viewport = Some(roi),
            _          => {}
        }
        self.status_msg = format!(
            "ROI → {}: ({},{}) {}x{}", target, roi.x, roi.y, roi.w, roi.h
        );
    }
}

impl eframe::App for CalibrateApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.load_texture(ctx);

        egui::SidePanel::right("panel").min_width(260.0).show(ctx, |ui| {
            ui.heading("Calibración");
            ui.separator();
            ui.label("Atajos (después de dibujar ROI):");
            for (key, label) in [("R","HP bar"),("M","Mana bar"),("B","Battle list"),
                                  ("S","Status icons"),("N","Minimap"),("G","Viewport")] {
                ui.monospace(format!("{key} → {label}"));
            }
            ui.monospace("Enter / Ctrl+S → Guardar");
            ui.separator();

            macro_rules! roi_line {
                ($label:expr, $field:expr) => {
                    match $field {
                        Some(r) => ui.colored_label(Color32::GREEN,
                            format!("{}: ({},{}) {}x{}", $label, r.x, r.y, r.w, r.h)),
                        None    => ui.colored_label(Color32::GRAY, format!("{}: —", $label)),
                    };
                };
            }

            roi_line!("HP bar",       self.calibration.hp_bar);
            roi_line!("Mana bar",     self.calibration.mana_bar);
            roi_line!("Battle list",  self.calibration.battle_list);
            roi_line!("Status icons", self.calibration.status_icons);
            roi_line!("Minimap",      self.calibration.minimap);
            roi_line!("Viewport",     self.calibration.game_viewport);

            ui.separator();
            if ui.button("Guardar calibration.toml").clicked() {
                self.save();
            }
            ui.separator();
            ui.label(format!("Scale: {:.2}x", self.scale));
            ui.label(&self.status_msg);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            let Some(tex) = &self.texture else {
                ui.label("Cargando imagen de referencia...");
                return;
            };

            let avail = ui.available_size();
            let (iw, ih) = self.img_size;
            let sx = avail.x / iw as f32;
            let sy = avail.y / ih as f32;
            self.scale = sx.min(sy).min(1.0);
            let disp = Vec2::new(iw as f32 * self.scale, ih as f32 * self.scale);

            let (resp, painter) = ui.allocate_painter(disp, egui::Sense::drag());
            let orig = resp.rect.min;

            painter.image(
                tex.id(),
                Rect::from_min_size(orig, disp),
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );

            // Drag input.
            if resp.drag_started() {
                self.draw.origin  = ctx.input(|i| i.pointer.press_origin());
                self.draw.current = self.draw.origin;
            }
            if resp.dragged() {
                self.draw.current = ctx.input(|i| i.pointer.interact_pos());
            }
            if resp.drag_stopped() {
                if let (Some(a), Some(b)) = (self.draw.origin, self.draw.current) {
                    let x0 = (a.x - orig.x).min(b.x - orig.x).max(0.0);
                    let y0 = (a.y - orig.y).min(b.y - orig.y).max(0.0);
                    let x1 = (a.x - orig.x).max(b.x - orig.x);
                    let y1 = (a.y - orig.y).max(b.y - orig.y);
                    if x1 - x0 > 4.0 && y1 - y0 > 4.0 {
                        self.selected = Some(SelectedRoi { x: x0, y: y0, w: x1-x0, h: y1-y0 });
                    }
                }
                self.draw.origin  = None;
                self.draw.current = None;
            }

            // Draw existing calibrated ROIs.
            let s = self.scale;
            let draw_roi = |roi: RoiDef, color: Color32, label: &str| {
                let r = Rect::from_min_size(
                    Pos2::new(orig.x + roi.x as f32 * s, orig.y + roi.y as f32 * s),
                    Vec2::new(roi.w as f32 * s, roi.h as f32 * s),
                );
                painter.rect_stroke(r, 0.0, Stroke::new(1.5, color));
                painter.text(r.min + Vec2::new(2.0, 2.0), egui::Align2::LEFT_TOP,
                    label, egui::FontId::proportional(11.0), color);
            };

            if let Some(r) = self.calibration.hp_bar        { draw_roi(r, Color32::GREEN,      "HP"); }
            if let Some(r) = self.calibration.mana_bar      { draw_roi(r, Color32::BLUE,       "MP"); }
            if let Some(r) = self.calibration.battle_list   { draw_roi(r, Color32::RED,        "Battle"); }
            if let Some(r) = self.calibration.status_icons  { draw_roi(r, Color32::YELLOW,     "Status"); }
            if let Some(r) = self.calibration.minimap       { draw_roi(r, Color32::LIGHT_BLUE, "Map"); }
            if let Some(r) = self.calibration.game_viewport { draw_roi(r, Color32::WHITE,      "Viewport"); }

            // Current selection.
            if let Some(sel) = self.selected {
                let r = Rect::from_min_size(
                    Pos2::new(orig.x + sel.x, orig.y + sel.y),
                    Vec2::new(sel.w, sel.h),
                );
                painter.rect_stroke(r, 0.0, Stroke::new(2.0, Color32::WHITE));
            }

            // Live drag preview.
            if let (Some(a), Some(b)) = (self.draw.origin, self.draw.current) {
                painter.rect_stroke(
                    Rect::from_two_pos(a, b),
                    0.0,
                    Stroke::new(1.5, Color32::from_rgba_unmultiplied(255, 255, 200, 180)),
                );
            }
        });

        // Keyboard shortcuts.
        ctx.input(|i| {
            if i.key_pressed(egui::Key::R)     { self.assign("hp"); }
            if i.key_pressed(egui::Key::M)     { self.assign("mana"); }
            if i.key_pressed(egui::Key::B)     { self.assign("battle"); }
            if i.key_pressed(egui::Key::S)     { self.assign("status"); }
            if i.key_pressed(egui::Key::N)     { self.assign("minimap"); }
            if i.key_pressed(egui::Key::G)     { self.assign("viewport"); }
            if i.key_pressed(egui::Key::Enter) || (i.modifiers.ctrl && i.key_pressed(egui::Key::S)) {
                self.save_from_input();
            }
        });
    }
}

impl CalibrateApp {
    // Wrapper needed because `self.save()` can't be called inside ctx.input closure.
    fn save_from_input(&mut self) {
        self.save();
    }
}
