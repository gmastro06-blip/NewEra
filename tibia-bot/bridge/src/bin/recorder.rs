/// recorder.rs — Waypoint recorder: captura movimientos del jugador y genera
/// cavebot TOML automaticamente.
///
/// Ejecutar en el gaming PC junto al bridge. Captura teclado global via rdev.
///
/// Controles:
///   F9        — Toggle grabacion on/off
///   Ctrl+L    — Insertar label (pide nombre por consola)
///   Ctrl+G    — Insertar goto (pide label por consola)
///   Ctrl+K    — Insertar stand mobs_killed(N) (pide N)
///   Ctrl+N    — Insertar npc_dialog (pide frases)
///   Escape    — Salir del recorder
///
/// Uso:
///   cargo run --release -p pico-bridge --bin recorder
///   # O directamente:
///   ./target/release/recorder.exe
use std::io::{self, Write as _};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use chrono::Local;
use rdev::{listen, Event, EventType, Key};
use serde::Serialize;

// ── Walk keys ────────────────────────────────────────────────────────────────

fn is_walk_key(key: &Key) -> bool {
    matches!(key, Key::KeyW | Key::KeyA | Key::KeyS | Key::KeyD
                | Key::UpArrow | Key::DownArrow | Key::LeftArrow | Key::RightArrow)
}

fn walk_key_name(key: &Key) -> &'static str {
    match key {
        Key::KeyW | Key::UpArrow    => "W",
        Key::KeyS | Key::DownArrow  => "S",
        Key::KeyA | Key::LeftArrow  => "A",
        Key::KeyD | Key::RightArrow => "D",
        _ => "?",
    }
}

fn hotkey_name(key: &Key) -> Option<&'static str> {
    match key {
        Key::F1  => Some("F1"),
        Key::F2  => Some("F2"),
        Key::F3  => Some("F3"),
        Key::F4  => Some("F4"),
        Key::F5  => Some("F5"),
        Key::F6  => Some("F6"),
        Key::F7  => Some("F7"),
        Key::F8  => Some("F8"),
        // F9 reserved for toggle
        Key::F10 => Some("F10"),
        Key::F11 => Some("F11"),
        Key::F12 => Some("F12"),
        Key::Space     => Some("Space"),
        Key::Return    => Some("Enter"),
        Key::Escape    => Some("Escape"),
        Key::Tab       => Some("Tab"),
        Key::Backspace => Some("Backspace"),
        Key::PageUp    => Some("PageUp"),
        Key::PageDown  => Some("PageDown"),
        Key::Home      => Some("Home"),
        Key::End       => Some("End"),
        Key::Insert    => Some("Insert"),
        Key::Delete    => Some("Delete"),
        _ => None,
    }
}

// ── TOML output ──────────────────────────────────────────────────────────────

#[derive(Serialize, Clone, Debug)]
struct StepToml {
    kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interval_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    until: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_wait_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phrases: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    wait_prompt_ms: Option<u64>,
}

impl StepToml {
    fn walk(key: &str, duration_ms: u64) -> Self {
        Self {
            kind: "walk".into(), key: Some(key.into()), duration_ms: Some(duration_ms),
            interval_ms: Some(400),
            name: None, label: None, until: None, max_wait_ms: None,
            phrases: None, wait_prompt_ms: None,
        }
    }
    fn wait(duration_ms: u64) -> Self {
        Self {
            kind: "wait".into(), duration_ms: Some(duration_ms),
            key: None, name: None, label: None, interval_ms: None,
            until: None, max_wait_ms: None, phrases: None, wait_prompt_ms: None,
        }
    }
    fn hotkey(key: &str) -> Self {
        Self {
            kind: "hotkey".into(), key: Some(key.into()),
            name: None, label: None, duration_ms: None, interval_ms: None,
            until: None, max_wait_ms: None, phrases: None, wait_prompt_ms: None,
        }
    }
    fn label(name: &str) -> Self {
        Self {
            kind: "label".into(), name: Some(name.into()),
            key: None, label: None, duration_ms: None, interval_ms: None,
            until: None, max_wait_ms: None, phrases: None, wait_prompt_ms: None,
        }
    }
    fn goto(label: &str) -> Self {
        Self {
            kind: "goto".into(), label: Some(label.into()),
            key: None, name: None, duration_ms: None, interval_ms: None,
            until: None, max_wait_ms: None, phrases: None, wait_prompt_ms: None,
        }
    }
    fn stand_mobs_killed(n: u32, max_wait: u64) -> Self {
        Self {
            kind: "stand".into(), until: Some(format!("mobs_killed({})", n)),
            max_wait_ms: Some(max_wait),
            key: None, name: None, label: None, duration_ms: None, interval_ms: None,
            phrases: None, wait_prompt_ms: None,
        }
    }
    fn npc_dialog(phrases: Vec<String>, wait_ms: u64) -> Self {
        Self {
            kind: "npc_dialog".into(), phrases: Some(phrases),
            wait_prompt_ms: Some(wait_ms),
            key: None, name: None, label: None, duration_ms: None, interval_ms: None,
            until: None, max_wait_ms: None,
        }
    }
}

#[derive(Serialize)]
struct CavebotFile {
    cavebot: CavebotHeader,
    step: Vec<StepToml>,
}

#[derive(Serialize)]
struct CavebotHeader {
    r#loop: bool,
}

// ── Recorder state ───────────────────────────────────────────────────────────

struct ActiveWalk {
    key_name: &'static str,
    started:  Instant,
}

struct RecorderState {
    recording:      bool,
    steps:          Vec<StepToml>,
    active_walk:    Option<ActiveWalk>,
    last_action_at: Instant,
    ctrl_held:      bool,
    should_exit:    bool,
    /// Pending stdin prompts (processed in main thread, not in callback)
    pending_prompt: Option<PendingPrompt>,
}

enum PendingPrompt {
    Label,
    Goto,
    StandKills,
    NpcDialog,
}

impl RecorderState {
    fn new() -> Self {
        Self {
            recording: false,
            steps: Vec::new(),
            active_walk: None,
            last_action_at: Instant::now(),
            ctrl_held: false,
            should_exit: false,
            pending_prompt: None,
        }
    }

    fn flush_idle(&mut self, now: Instant) {
        let elapsed = now.duration_since(self.last_action_at).as_millis() as u64;
        if elapsed > 500 {
            self.steps.push(StepToml::wait(elapsed));
        }
    }

    fn flush_walk(&mut self, now: Instant) {
        if let Some(walk) = self.active_walk.take() {
            let ms = now.duration_since(walk.started).as_millis() as u64;
            if ms >= 150 {
                self.steps.push(StepToml::walk(walk.key_name, ms));
                println!("  [+] walk {} {}ms", walk.key_name, ms);
            }
            // else too short, discard (accidental tap)
        }
    }

    fn on_key_down(&mut self, key: Key, now: Instant) {
        // Track Ctrl
        if matches!(key, Key::ControlLeft | Key::ControlRight) {
            self.ctrl_held = true;
            return;
        }

        // F9: toggle recording
        if matches!(key, Key::F9) {
            self.recording = !self.recording;
            if self.recording {
                self.last_action_at = now;
                println!("\n>>> GRABANDO — mueve tu personaje. F9 para parar.");
            } else {
                self.flush_walk(now);
                println!(">>> PARADO — {} steps grabados.", self.steps.len());
            }
            return;
        }

        // Escape: exit
        if matches!(key, Key::Escape) && !self.recording {
            self.should_exit = true;
            return;
        }

        if !self.recording { return; }

        // Ctrl combos (set pending prompt for main thread)
        if self.ctrl_held {
            match key {
                Key::KeyL => { self.pending_prompt = Some(PendingPrompt::Label); }
                Key::KeyG => { self.pending_prompt = Some(PendingPrompt::Goto); }
                Key::KeyK => { self.pending_prompt = Some(PendingPrompt::StandKills); }
                Key::KeyN => { self.pending_prompt = Some(PendingPrompt::NpcDialog); }
                _ => {}
            }
            return;
        }

        // Walk keys
        if is_walk_key(&key) {
            // If already walking same direction, ignore (key repeat)
            if let Some(ref w) = self.active_walk {
                if w.key_name == walk_key_name(&key) {
                    return;
                }
                // Different direction: flush previous walk
                self.flush_walk(now);
            }
            self.flush_idle(now);
            self.active_walk = Some(ActiveWalk {
                key_name: walk_key_name(&key),
                started: now,
            });
            self.last_action_at = now;
            return;
        }

        // Hotkeys
        if let Some(name) = hotkey_name(&key) {
            self.flush_walk(now);
            self.flush_idle(now);
            self.steps.push(StepToml::hotkey(name));
            println!("  [+] hotkey {}", name);
            self.last_action_at = now;
        }
    }

    fn on_key_up(&mut self, key: Key, now: Instant) {
        if matches!(key, Key::ControlLeft | Key::ControlRight) {
            self.ctrl_held = false;
            return;
        }

        if !self.recording { return; }

        // Walk key released
        if is_walk_key(&key) {
            if let Some(ref w) = self.active_walk {
                if w.key_name == walk_key_name(&key) {
                    self.flush_walk(now);
                    self.last_action_at = now;
                }
            }
        }
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

fn prompt_line(msg: &str) -> String {
    print!("  {} > ", msg);
    io::stdout().flush().ok();
    let mut buf = String::new();
    io::stdin().read_line(&mut buf).ok();
    buf.trim().to_string()
}

fn save_toml(steps: &[StepToml]) {
    let ts = Local::now().format("%Y%m%d_%H%M%S");
    let filename = format!("recorded_{}.toml", ts);
    // Try assets/cavebot/ first, fallback to current dir
    let dir = std::path::Path::new("assets/cavebot");
    let path = if dir.exists() {
        dir.join(&filename)
    } else {
        std::path::PathBuf::from(&filename)
    };

    let file = CavebotFile {
        cavebot: CavebotHeader { r#loop: true },
        step: steps.to_vec(),
    };

    match toml::to_string_pretty(&file) {
        Ok(content) => {
            let header = format!(
                "# Generado por waypoint_recorder — {}\n# {} steps\n\n",
                Local::now().format("%Y-%m-%d %H:%M:%S"),
                steps.len()
            );
            match std::fs::write(&path, format!("{}{}", header, content)) {
                Ok(()) => println!("\n>>> Guardado: {}", path.display()),
                Err(e) => eprintln!("\n>>> Error escribiendo {}: {}", path.display(), e),
            }
        }
        Err(e) => eprintln!("\n>>> Error serializando TOML: {}", e),
    }
}

fn main() {
    println!("=== Waypoint Recorder ===");
    println!("F9        = iniciar/parar grabacion");
    println!("Ctrl+L    = insertar label");
    println!("Ctrl+G    = insertar goto");
    println!("Ctrl+K    = insertar stand mobs_killed(N)");
    println!("Ctrl+N    = insertar npc_dialog");
    println!("Escape    = salir (cuando no esta grabando)");
    println!();

    let state = Arc::new(Mutex::new(RecorderState::new()));
    let state_clone = Arc::clone(&state);

    // rdev::listen blocks, so run it in a thread
    std::thread::spawn(move || {
        listen(move |event: Event| {
            let mut s = state_clone.lock().unwrap();
            let now = Instant::now();
            match event.event_type {
                EventType::KeyPress(key)   => s.on_key_down(key, now),
                EventType::KeyRelease(key) => s.on_key_up(key, now),
                _ => {}
            }
        }).expect("rdev listen failed");
    });

    // Main thread: handle stdin prompts and exit
    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));

        let mut s = state.lock().unwrap();

        if s.should_exit {
            if !s.steps.is_empty() {
                drop(s);
                let steps = state.lock().unwrap().steps.clone();
                save_toml(&steps);
            }
            println!("Bye.");
            break;
        }

        // Handle pending prompts (requires stdin, can't do in rdev callback)
        if let Some(prompt) = s.pending_prompt.take() {
            let now = Instant::now();
            s.flush_walk(now);
            s.flush_idle(now);
            // Drop lock before reading stdin
            drop(s);

            match prompt {
                PendingPrompt::Label => {
                    let name = prompt_line("Nombre del label");
                    if !name.is_empty() {
                        let mut s = state.lock().unwrap();
                        s.steps.push(StepToml::label(&name));
                        s.last_action_at = Instant::now();
                        println!("  [+] label '{}'", name);
                    }
                }
                PendingPrompt::Goto => {
                    let label = prompt_line("Label destino del goto");
                    if !label.is_empty() {
                        let mut s = state.lock().unwrap();
                        s.steps.push(StepToml::goto(&label));
                        s.last_action_at = Instant::now();
                        println!("  [+] goto '{}'", label);
                    }
                }
                PendingPrompt::StandKills => {
                    let n_str = prompt_line("Kills para stand (numero)");
                    if let Ok(n) = n_str.parse::<u32>() {
                        let max_str = prompt_line("Max wait ms (default 90000)");
                        let max = max_str.parse().unwrap_or(90000u64);
                        let mut s = state.lock().unwrap();
                        s.steps.push(StepToml::stand_mobs_killed(n, max));
                        s.last_action_at = Instant::now();
                        println!("  [+] stand mobs_killed({}), max {}ms", n, max);
                    }
                }
                PendingPrompt::NpcDialog => {
                    let raw = prompt_line("Frases separadas por coma (ej: hi,mana potion,yes)");
                    let phrases: Vec<String> = raw.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                    if !phrases.is_empty() {
                        let wait_str = prompt_line("Wait prompt ms (default 1500)");
                        let wait = wait_str.parse().unwrap_or(1500u64);
                        let count = phrases.len();
                        let mut s = state.lock().unwrap();
                        s.steps.push(StepToml::npc_dialog(phrases, wait));
                        s.last_action_at = Instant::now();
                        println!("  [+] npc_dialog {} frases, wait {}ms", count, wait);
                    }
                }
            }
            continue;
        }

        // Auto-save when recording stops and there are steps
        if !s.recording && !s.steps.is_empty() {
            let steps = s.steps.clone();
            s.steps.clear();
            drop(s);
            save_toml(&steps);
        }
    }
}
