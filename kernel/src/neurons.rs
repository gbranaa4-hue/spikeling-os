//! MILESTONE 9: real LIF (leaky integrate-and-fire) neuron dynamics --
//! ported from Spikeling's own core/runtime/runtime.py. Network topology
//! mirrors Spikeling's own core/examples/sound_localizer.spk
//! (LeftMic/RightMic -> Motor), adapted to keyboard keys as the input
//! sensor ('a'/'d') since this kernel has no microphone driver.
//!
//! MILESTONE 10: real STDP learning on the LeftKey/RightKey -> Motor
//! synapses. MILESTONE 11: learned weights persisted to disk (ata.rs)
//! across a real reboot.
//!
//! MILESTONE 21: this file no longer holds ANY neuron/synapse state of
//! its own. It used to own a completely separate LifNeuron/Network
//! struct pair, its own copy of the STDP formula, and its own tick(),
//! ticked independently from network.rs's GenericNetwork -- two
//! parallel engines that happened to start from the same constants but
//! shared no live state, an honestly-disclosed gap for several
//! milestones (stimulating LeftKey via a real keypress had zero effect
//! on anything the `net`/`stim`/`addneuron` commands could see).
//!
//! LeftKey, RightKey and Motor now live as three ordinary named
//! neurons inside network.rs's single GenericNetwork, seeded once at
//! boot by init() (below) with the EXACT constants Milestone 9/10
//! originally used -- threshold, leak, refractory period and initial
//! weight are all preserved unchanged, listed here for the record.
//! Everything below is a thin, named VIEW over that one shared network:
//! a fixed-format report and the fixed-command entry points shell.rs
//! already calls (`neurons`, bare `train`, `save`), now reading and
//! writing the SAME synapses `net`/`stim`/`train FROM TO GAP` also
//! reach. There is exactly one apply_stdp() left in the kernel, in
//! network.rs; the copy that used to live here is gone.

use alloc::format;
use alloc::string::String;

pub const LEFT_KEY: &str = "LeftKey";
pub const RIGHT_KEY: &str = "RightKey";
pub const MOTOR: &str = "Motor";

const KEY_THRESHOLD: f32 = 100.0; // unchanged since Milestone 9
const KEY_LEAK: f32 = 5.0; // unchanged since Milestone 9
const MOTOR_THRESHOLD: f32 = 80.0; // unchanged since Milestone 9
const MOTOR_LEAK: f32 = 3.0; // unchanged since Milestone 9
const REFRACTORY_TICKS: u32 = 7; // ~400ms at the default ~18.2Hz PIT rate -- unchanged since Milestone 9
const INITIAL_WEIGHT: f32 = 0.5; // neutral starting point -- unchanged since Milestone 10
const KEY_STIM_AMOUNT: f32 = 120.0; // unchanged since Milestone 9

/// Seeds the shared GenericNetwork (network.rs) with LeftKey, RightKey
/// and Motor and their synapses, loading learned weights from disk
/// (Milestone 11) if present. Returns true if weights were loaded from
/// disk, false if the neutral defaults were used -- reported honestly
/// by main.rs either way, same as before Milestone 21.
pub fn init() -> bool {
    let (left_w, right_w, loaded) = match crate::ata::load_weights() {
        Some((l, r)) => (l, r, true),
        None => (INITIAL_WEIGHT, INITIAL_WEIGHT, false),
    };
    crate::network::seed_fixed_network(
        LEFT_KEY,
        RIGHT_KEY,
        MOTOR,
        KEY_THRESHOLD,
        KEY_LEAK,
        MOTOR_THRESHOLD,
        MOTOR_LEAK,
        REFRACTORY_TICKS,
        left_w,
        right_w,
    );
    loaded
}

/// Called from keyboard.rs -- 'a' stimulates LeftKey, 'd' stimulates
/// RightKey, standing in for sound_localizer.spk's LeftMic/RightMic.
/// This now calls straight into network.rs's stimulate() on the named
/// neuron -- the SAME neuron `stim LeftKey ...` or `net` would see.
pub fn on_key_stimulus(c: char) {
    match c {
        'a' => {
            let _ = crate::network::stimulate(LEFT_KEY, KEY_STIM_AMOUNT);
        }
        'd' => {
            let _ = crate::network::stimulate(RIGHT_KEY, KEY_STIM_AMOUNT);
        }
        _ => {}
    }
}

/// MILESTONE 11: writes the current learned weights to the dedicated
/// persistence disk (ata.rs), so they survive a real reboot -- sourced
/// from the shared network's live synapse weights, not a separate copy.
pub fn save_weights_to_disk() -> Result<(), &'static str> {
    let left = crate::network::synapse_weight(LEFT_KEY, MOTOR).ok_or("network not initialized")?;
    let right = crate::network::synapse_weight(RIGHT_KEY, MOTOR).ok_or("network not initialized")?;
    crate::ata::save_weights(left, right)
}

pub fn left_to_motor_weight() -> f32 {
    crate::network::synapse_weight(LEFT_KEY, MOTOR).unwrap_or(0.0)
}

/// A controlled, repeatable STDP trial on the LeftKey->Motor synapse,
/// called from the shell's bare `train` command -- now just a named
/// call into network.rs's ONE train_synapse()/apply_stdp()
/// implementation (the same one `train FROM TO GAP_TICKS` uses), on the
/// SAME synapse `net` reports.
pub fn run_training_trial(ltp: bool, gap_ticks: u64) {
    let _ = crate::network::train_synapse(LEFT_KEY, MOTOR, gap_ticks, ltp);
}

/// Fixed-format report, kept distinct from `net`'s generic table for
/// continuity with Milestone 9-11's existing shell output -- reads the
/// SAME underlying neurons network.rs owns, not a separate copy.
pub fn report() -> String {
    let (lv, lf) = crate::network::neuron_state(LEFT_KEY).unwrap_or((0.0, 0));
    let (rv, rf) = crate::network::neuron_state(RIGHT_KEY).unwrap_or((0.0, 0));
    let (mv, mf) = crate::network::neuron_state(MOTOR).unwrap_or((0.0, 0));
    let lw = crate::network::synapse_weight(LEFT_KEY, MOTOR).unwrap_or(0.0);
    let rw = crate::network::synapse_weight(RIGHT_KEY, MOTOR).unwrap_or(0.0);
    format!(
        "LeftKey  V={lv:5.1} fires={lf}\nRightKey V={rv:5.1} fires={rf}\nMotor    V={mv:5.1} fires={mf}  (threshold=80; MILESTONE 21: firing now propagates to Motor on the NEXT tick, via network.rs's shared one-tick synaptic delay, not the same tick)\nweights: LeftKey->Motor={lw:.4}  RightKey->Motor={rw:.4}\n"
    )
}
