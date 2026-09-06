//! Cross-thread shared state: the window thread renders it, the protocol and
//! audio threads update it. Lock-free on purpose — every field is read every
//! frame on the UI side.

use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    /// Standby, no connection.
    Idle,
    /// Opening the WebSocket / waiting for the server hello.
    Connecting,
    /// Command held, streaming mic audio.
    Listening,
    /// Command released, `listen stop` sent, waiting for the reply.
    Waiting,
    /// Playing server TTS audio.
    Speaking,
}

impl Phase {
    fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::Connecting,
            2 => Self::Listening,
            3 => Self::Waiting,
            4 => Self::Speaking,
            _ => Self::Idle,
        }
    }
}

/// UI events produced by the window thread, consumed by the protocol thread.
#[derive(Debug)]
pub enum UiEvent {
    CmdDown,
    CmdUp,
    Quit,
}

/// Requested app action from MCP tools (0 = none, 1 = restart, 2 = quit).
pub const ACTION_RESTART: u8 = 1;
pub const ACTION_QUIT: u8 = 2;

#[derive(Default)]
pub struct SharedState {
    phase: AtomicU8,
    /// Playback loudness (f32 bits of the last decoded packet's RMS).
    level: AtomicU32,
    /// Mic loudness (f32 bits), mirrors what is being encoded.
    mic_level: AtomicU32,
    /// True while mic audio should be encoded and shipped to the server.
    mic_enabled: AtomicU8,
    /// Last server emotion (index into ui::EMOTIONS).
    emotion: AtomicU8,
    /// Action requested via MCP tools.
    action: AtomicU8,
}

impl SharedState {
    pub fn phase(&self) -> Phase {
        Phase::from_u8(self.phase.load(Ordering::Relaxed))
    }

    pub fn set_phase(&self, p: Phase) {
        self.phase.store(p as u8, Ordering::Relaxed);
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    pub fn set_level(&self, v: f32) {
        self.level.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn mic_level(&self) -> f32 {
        f32::from_bits(self.mic_level.load(Ordering::Relaxed))
    }

    pub fn set_mic_level(&self, v: f32) {
        self.mic_level.store(v.to_bits(), Ordering::Relaxed);
    }

    pub fn mic_enabled(&self) -> bool {
        self.mic_enabled.load(Ordering::Relaxed) != 0
    }

    pub fn set_mic_enabled(&self, on: bool) {
        self.mic_enabled.store(on as u8, Ordering::Relaxed);
    }

    pub fn emotion(&self) -> u8 {
        self.emotion.load(Ordering::Relaxed)
    }

    pub fn set_emotion(&self, e: u8) {
        self.emotion.store(e, Ordering::Relaxed);
    }

    pub fn action(&self) -> u8 {
        self.action.load(Ordering::Relaxed)
    }

    pub fn set_action(&self, a: u8) {
        self.action.store(a, Ordering::Relaxed);
    }
}
