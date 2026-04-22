//! tick.rs — TickMetrics struct + flags + enums (POD, Copy).
//!
//! Snapshot inmutable de un tick completo. Cabe en ~96 bytes y se pasa
//! por value sin penalty. Serializable a JSON para HTTP / JSONL recorder.

use serde::{Deserialize, Serialize};

/// Tag corto del kind de acción emitido en el tick. u8 para que TickMetrics
/// sea Copy compacto. Mapea 1:1 con el dispatcher de actions.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionKindTag {
    #[default]
    None       = 0,
    Heal       = 1,
    Attack     = 2,
    Click      = 3,
    Key        = 4,
    MouseMove  = 5,
    NodeWalk   = 6,
    Verify     = 7,
}

impl ActionKindTag {
    pub fn label(self) -> &'static str {
        match self {
            ActionKindTag::None      => "none",
            ActionKindTag::Heal      => "heal",
            ActionKindTag::Attack    => "attack",
            ActionKindTag::Click     => "click",
            ActionKindTag::Key       => "key",
            ActionKindTag::MouseMove => "mouse_move",
            ActionKindTag::NodeWalk  => "node_walk",
            ActionKindTag::Verify    => "verify",
        }
    }
}

/// Indexa el array `vision_per_reader_us`. Orden estable — extender al final.
/// Si cambias el orden, los JSONL persistidos quedan ininterpretables.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReaderId {
    Anchors    = 0,
    HpMana     = 1,
    Battle     = 2,
    Status     = 3,
    Minimap    = 4,
    Loot       = 5,
    Target     = 6,
    UiMatch    = 7,
    Inventory  = 8,
    GameCoords = 9,
    Prompts    = 10,
    Other      = 11,
}

impl ReaderId {
    pub const COUNT: usize = 12;

    pub fn label(self) -> &'static str {
        match self {
            ReaderId::Anchors    => "anchors",
            ReaderId::HpMana     => "hp_mana",
            ReaderId::Battle     => "battle",
            ReaderId::Status     => "status",
            ReaderId::Minimap    => "minimap",
            ReaderId::Loot       => "loot",
            ReaderId::Target     => "target",
            ReaderId::UiMatch    => "ui_match",
            ReaderId::Inventory  => "inventory",
            ReaderId::GameCoords => "game_coords",
            ReaderId::Prompts    => "prompts",
            ReaderId::Other      => "other",
        }
    }

    pub fn all() -> [ReaderId; Self::COUNT] {
        [
            ReaderId::Anchors, ReaderId::HpMana, ReaderId::Battle, ReaderId::Status,
            ReaderId::Minimap, ReaderId::Loot, ReaderId::Target, ReaderId::UiMatch,
            ReaderId::Inventory, ReaderId::GameCoords, ReaderId::Prompts, ReaderId::Other,
        ]
    }
}

/// Bitfield manual de flags por tick (16 bits). Roleamos en vez de añadir
/// `bitflags` crate para mantener cero deps externas extra.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct TickFlags(pub u16);

impl TickFlags {
    pub const NONE:                   TickFlags = TickFlags(0);
    pub const TICK_OVERRUN:           TickFlags = TickFlags(1 << 0);
    pub const FRAME_STALE:            TickFlags = TickFlags(1 << 1);
    pub const ANCHOR_DRIFT_WARN:      TickFlags = TickFlags(1 << 2);
    pub const ANCHOR_LOST:            TickFlags = TickFlags(1 << 3);
    pub const VISION_SLOW:            TickFlags = TickFlags(1 << 4);
    pub const ACTION_FAILED:          TickFlags = TickFlags(1 << 5);
    pub const BRIDGE_RTT_HIGH:        TickFlags = TickFlags(1 << 6);
    pub const FRAME_SEQ_GAP:          TickFlags = TickFlags(1 << 7);
    pub const HEALTH_DEGRADED:        TickFlags = TickFlags(1 << 8);
    pub const SAFETY_PAUSED:          TickFlags = TickFlags(1 << 9);
    pub const RECORDER_ACTIVE:        TickFlags = TickFlags(1 << 10);

    #[inline]
    pub fn contains(self, other: TickFlags) -> bool {
        (self.0 & other.0) == other.0
    }

    #[inline]
    pub fn insert(&mut self, other: TickFlags) {
        self.0 |= other.0;
    }

    #[inline]
    pub fn remove(&mut self, other: TickFlags) {
        self.0 &= !other.0;
    }

    #[inline]
    pub fn is_empty(self) -> bool { self.0 == 0 }

    /// Lista los flags activos como labels para logs/JSON.
    pub fn labels(self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.contains(Self::TICK_OVERRUN)      { out.push("tick_overrun"); }
        if self.contains(Self::FRAME_STALE)       { out.push("frame_stale"); }
        if self.contains(Self::ANCHOR_DRIFT_WARN) { out.push("anchor_drift_warn"); }
        if self.contains(Self::ANCHOR_LOST)       { out.push("anchor_lost"); }
        if self.contains(Self::VISION_SLOW)       { out.push("vision_slow"); }
        if self.contains(Self::ACTION_FAILED)     { out.push("action_failed"); }
        if self.contains(Self::BRIDGE_RTT_HIGH)   { out.push("bridge_rtt_high"); }
        if self.contains(Self::FRAME_SEQ_GAP)     { out.push("frame_seq_gap"); }
        if self.contains(Self::HEALTH_DEGRADED)   { out.push("health_degraded"); }
        if self.contains(Self::SAFETY_PAUSED)     { out.push("safety_paused"); }
        if self.contains(Self::RECORDER_ACTIVE)   { out.push("recorder_active"); }
        out
    }
}

impl std::ops::BitOr for TickFlags {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self { TickFlags(self.0 | rhs.0) }
}

impl std::ops::BitOrAssign for TickFlags {
    fn bitor_assign(&mut self, rhs: Self) { self.0 |= rhs.0; }
}

/// Snapshot de timings + counts + flags de UN tick. POD, Copy. ~120 bytes.
///
/// Todos los timings en µs (microsegundos) — precisión sub-ms necesaria para
/// etapas como filter/fsm que pueden ser <100 µs. Usamos u32 que cubre hasta
/// ~71 minutos de latencia (más que suficiente para single-tick events).
///
/// `vision_per_reader_us` es u16 (cubre hasta 65 ms por reader; si un reader
/// individual excede esto, satura — y deberías investigar urgentemente).
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct TickMetrics {
    pub tick:                     u64,
    pub frame_seq:                u64,
    pub ts_unix_ms:               u64,

    // Timings (µs)
    pub frame_age_us:             u32,
    pub acquire_us:               u32,
    pub vision_total_us:          u32,
    pub filter_us:                u32,
    pub fsm_us:                   u32,
    pub dispatch_us:              u32,
    pub state_write_us:           u32,
    pub tick_total_us:            u32,

    pub vision_per_reader_us:     [u16; ReaderId::COUNT],

    // Action observada en este tick
    pub last_action_kind:         ActionKindTag,
    pub last_action_rtt_us:       u32,

    // Counts vision
    pub valid_anchors:            u8,
    pub total_anchors:            u8,
    pub anchor_confidence_bp:     u16,        // basis points 0..10000
    pub vitals_confidence_bp:     u16,
    pub target_confidence_bp:     u16,
    pub enemies_visible:          u8,
    pub inventory_items:          u8,

    pub flags:                    TickFlags,
}

impl Default for TickMetrics {
    fn default() -> Self {
        Self {
            tick: 0, frame_seq: 0, ts_unix_ms: 0,
            frame_age_us: 0, acquire_us: 0, vision_total_us: 0,
            filter_us: 0, fsm_us: 0, dispatch_us: 0,
            state_write_us: 0, tick_total_us: 0,
            vision_per_reader_us: [0; ReaderId::COUNT],
            last_action_kind: ActionKindTag::None,
            last_action_rtt_us: 0,
            valid_anchors: 0, total_anchors: 0,
            anchor_confidence_bp: 0, vitals_confidence_bp: 0, target_confidence_bp: 0,
            enemies_visible: 0, inventory_items: 0,
            flags: TickFlags::NONE,
        }
    }
}

impl TickMetrics {
    /// E2E desde captura NDI hasta dispatch (no incluye RTT del bridge).
    pub fn e2e_capture_to_emit_us(&self) -> u32 {
        self.frame_age_us.saturating_add(self.tick_total_us)
    }

    /// E2E completo incluyendo round-trip al bridge (si hay action ack-able).
    pub fn e2e_capture_to_ack_us(&self) -> u32 {
        self.e2e_capture_to_emit_us().saturating_add(self.last_action_rtt_us)
    }

    /// Reader que más tiempo consumió en este tick.
    pub fn dominant_reader(&self) -> Option<(ReaderId, u16)> {
        ReaderId::all()
            .iter()
            .map(|r| (*r, self.vision_per_reader_us[*r as usize]))
            .max_by_key(|(_, us)| *us)
            .filter(|(_, us)| *us > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_insert_contains() {
        let mut f = TickFlags::NONE;
        assert!(!f.contains(TickFlags::TICK_OVERRUN));
        f.insert(TickFlags::TICK_OVERRUN);
        assert!(f.contains(TickFlags::TICK_OVERRUN));
        assert!(!f.contains(TickFlags::FRAME_STALE));
    }

    #[test]
    fn flags_combine_via_bitor() {
        let f = TickFlags::TICK_OVERRUN | TickFlags::VISION_SLOW;
        assert!(f.contains(TickFlags::TICK_OVERRUN));
        assert!(f.contains(TickFlags::VISION_SLOW));
        assert!(!f.contains(TickFlags::FRAME_STALE));
    }

    #[test]
    fn flags_remove_works() {
        let mut f = TickFlags::TICK_OVERRUN | TickFlags::FRAME_STALE;
        f.remove(TickFlags::TICK_OVERRUN);
        assert!(!f.contains(TickFlags::TICK_OVERRUN));
        assert!(f.contains(TickFlags::FRAME_STALE));
    }

    #[test]
    fn flags_labels_lists_active() {
        let f = TickFlags::TICK_OVERRUN | TickFlags::ANCHOR_LOST;
        let labels = f.labels();
        assert!(labels.contains(&"tick_overrun"));
        assert!(labels.contains(&"anchor_lost"));
        assert_eq!(labels.len(), 2);
    }

    #[test]
    fn empty_flags_no_labels() {
        assert!(TickFlags::NONE.labels().is_empty());
    }

    #[test]
    fn reader_id_all_complete_and_unique() {
        let all = ReaderId::all();
        assert_eq!(all.len(), ReaderId::COUNT);
        // Cada uno tiene índice único == valor del enum.
        for (i, r) in all.iter().enumerate() {
            assert_eq!(*r as usize, i);
        }
    }

    #[test]
    fn tick_metrics_default_is_zero() {
        let m = TickMetrics::default();
        assert_eq!(m.tick, 0);
        assert_eq!(m.tick_total_us, 0);
        assert!(m.flags.is_empty());
        assert_eq!(m.last_action_kind, ActionKindTag::None);
    }

    #[test]
    fn tick_metrics_e2e_calculations() {
        let m = TickMetrics {
            frame_age_us: 80_000,
            tick_total_us: 20_000,
            last_action_rtt_us: 15_000,
            ..Default::default()
        };
        assert_eq!(m.e2e_capture_to_emit_us(), 100_000);
        assert_eq!(m.e2e_capture_to_ack_us(), 115_000);
    }

    #[test]
    fn tick_metrics_e2e_saturates_on_overflow() {
        let m = TickMetrics {
            frame_age_us: u32::MAX,
            tick_total_us: 1000,
            ..Default::default()
        };
        // saturating_add evita overflow.
        assert_eq!(m.e2e_capture_to_emit_us(), u32::MAX);
    }

    #[test]
    fn dominant_reader_picks_max() {
        let mut m = TickMetrics::default();
        m.vision_per_reader_us[ReaderId::HpMana as usize]    = 100;
        m.vision_per_reader_us[ReaderId::Battle as usize]    = 5000;
        m.vision_per_reader_us[ReaderId::Inventory as usize] = 2000;
        let (id, us) = m.dominant_reader().unwrap();
        assert_eq!(id, ReaderId::Battle);
        assert_eq!(us, 5000);
    }

    #[test]
    fn dominant_reader_none_when_all_zero() {
        let m = TickMetrics::default();
        assert!(m.dominant_reader().is_none());
    }

    #[test]
    fn tick_metrics_serializes_to_json() {
        let m = TickMetrics {
            tick: 42,
            frame_age_us: 80_000,
            tick_total_us: 18_500,
            last_action_kind: ActionKindTag::Heal,
            flags: TickFlags::TICK_OVERRUN,
            ..Default::default()
        };
        let json = serde_json::to_string(&m).expect("serialize");
        assert!(json.contains("\"tick\":42"));
        assert!(json.contains("\"frame_age_us\":80000"));
        assert!(json.contains("\"last_action_kind\":\"heal\""));
        // flags se serializa como número (transparent over u16).
        assert!(json.contains(&format!("\"flags\":{}", TickFlags::TICK_OVERRUN.0)));
    }

    #[test]
    fn action_kind_tag_labels_match_serde() {
        assert_eq!(ActionKindTag::Heal.label(), "heal");
        assert_eq!(ActionKindTag::NodeWalk.label(), "node_walk");
        // Roundtrip serde.
        let s = serde_json::to_string(&ActionKindTag::NodeWalk).unwrap();
        assert_eq!(s, "\"node_walk\"");
        let parsed: ActionKindTag = serde_json::from_str(&s).unwrap();
        assert_eq!(parsed, ActionKindTag::NodeWalk);
    }
}
