//! MILESTONE 12: a real, dynamically-definable spiking network --
//! Milestone 9/10/11's LeftKey/RightKey/Motor network is fixed Rust
//! structure; this brings the actual PROGRAMMABILITY of Spikeling's
//! own .spk language into the kernel, via shell commands instead of a
//! file (no filesystem yet), so networks can be built and rewired at
//! runtime rather than only at compile time.
//!
//! Ticked on every real timer interrupt.
//!
//! MILESTONE 21: this is now the ONLY neuron/synapse representation and
//! the ONLY STDP implementation in the kernel. Milestone 9/10/11's
//! LeftKey/RightKey/Motor network used to be a second, independently-
//! implemented copy (its own LifNeuron/Network structs, its own
//! duplicated apply_stdp, its own tick() called separately from
//! interrupts.rs) that happened to start from the same initial values
//! but shared no live state with this file -- a real keypress
//! stimulating LeftKey there was invisible to `net`/`stim`/`addneuron`
//! here, and vice versa, an honestly-disclosed gap for several
//! milestones. neurons.rs now seeds LeftKey/RightKey/Motor into THIS
//! GenericNetwork at boot (seed_fixed_network, below) and every fixed
//! command (`neurons`, bare `train`, `save`) reads/writes these same
//! Neuron/Synapse entries by name -- one engine, one STDP formula, one
//! set of live numbers, provably shared (see the M21 test evidence in
//! the shell session this milestone was verified with).
//!
//! Unifying onto this engine's tick() also means LeftKey/RightKey/Motor
//! now propagate firings with this engine's one-tick synaptic delay
//! (firing THIS tick becomes stimulus on the NEXT tick) instead of the
//! old fixed network's same-tick propagation -- a real, disclosed
//! behavioral change from having two separately-timed engines collapse
//! into one, not a silently swept-under-the-rug difference. The LIF
//! constants that Milestone 9/10 actually verified (threshold, leak,
//! refractory period, STDP rate/tau, initial weight) are unchanged --
//! see neurons.rs for the exact values preserved at seed time.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use spin::Mutex;

const SPIKE_GAIN: f32 = 50.0; // same convention as Spikeling's own weight*50.0

pub struct Neuron {
    pub name: String,
    pub v: f32,
    pub threshold: f32,
    pub leak: f32,
    pub fire_count: u32,
    pub last_fire_tick: Option<u64>,
    refractory_ticks: u32,
    refractory_remaining: u32,
}

pub struct Synapse {
    pub from: usize,
    pub to: usize,
    pub weight: f32,
}

pub struct GenericNetwork {
    pub neurons: Vec<Neuron>,
    pub synapses: Vec<Synapse>,
    pending_stimulus: Vec<f32>, // one slot per neuron, cleared every tick
    beep_ticks_remaining: u32,  // MILESTONE 14: auto-silence countdown
    tick_count: u64,            // MILESTONE 17: STDP needs real tick numbers
}

// Real STDP -- the ONE implementation in the kernel as of Milestone 21
// (previously duplicated near-verbatim in neurons.rs; that copy is
// gone, this is the only one left). Δw = rate * exp(-|dt|/tau),
// pre-before-post -> LTP, pre-after-post -> LTD, clamped [0,1].
const STDP_RATE: f32 = 0.1;
const STDP_TAU_MS: f32 = 20.0;
const TICK_PERIOD_MS: f32 = 1000.0 / 18.2;

fn apply_stdp(weight: &mut f32, pre_last_fire: Option<u64>, post_last_fire: Option<u64>) {
    if let (Some(pre_t), Some(post_t)) = (pre_last_fire, post_last_fire) {
        let dt_ticks = post_t as i64 - pre_t as i64;
        let dt_ms = dt_ticks as f32 * TICK_PERIOD_MS;
        if dt_ms == 0.0 {
            return;
        }
        let delta = STDP_RATE * libm::expf(-(dt_ms.abs()) / STDP_TAU_MS);
        if dt_ms > 0.0 {
            *weight = (*weight + delta).min(1.0);
        } else {
            *weight = (*weight - delta).max(0.0);
        }
    }
}

const BEEP_FREQ_HZ: u32 = 600;
// DIAGNOSED (real test): an earlier 3-tick (~165ms) duration was
// shorter than the time it takes to even TYPE the word
// "speakerstatus" via a real keystroke-by-keystroke test harness --
// the blip had already auto-silenced before the observation command
// finished being typed, showing enabled=false both times despite the
// neuron genuinely firing (fires=1). Not a bug in the mechanism, a
// mismatch between blip duration and the observation method's own
// overhead. Extended to ~2.2s so it's reliably observable.
const BEEP_DURATION_TICKS: u32 = 40;

impl GenericNetwork {
    fn find(&self, name: &str) -> Option<usize> {
        self.neurons.iter().position(|n| n.name == name)
    }

    /// refractory_ticks=0 (the default for every DSL-added neuron,
    /// unchanged since Milestone 12/14/17) means the refractory check
    /// below never triggers -- only the Milestone 21-seeded
    /// LeftKey/RightKey/Motor neurons get a non-zero value, so this
    /// addition is behavior-preserving for every network built with
    /// `addneuron` before this milestone.
    fn add_neuron(&mut self, name: &str, threshold: f32, leak: f32, refractory_ticks: u32) -> Result<(), String> {
        if self.find(name).is_some() {
            return Err(format!("neuron '{name}' already exists"));
        }
        self.neurons.push(Neuron {
            name: name.to_string(),
            v: 0.0,
            threshold,
            leak,
            fire_count: 0,
            last_fire_tick: None,
            refractory_ticks,
            refractory_remaining: 0,
        });
        self.pending_stimulus.push(0.0);
        Ok(())
    }

    fn add_synapse(&mut self, from: &str, to: &str, weight: f32) -> Result<(), String> {
        let from_i = self.find(from).ok_or_else(|| format!("no such neuron '{from}'"))?;
        let to_i = self.find(to).ok_or_else(|| format!("no such neuron '{to}'"))?;
        self.synapses.push(Synapse { from: from_i, to: to_i, weight });
        Ok(())
    }

    fn stimulate(&mut self, name: &str, amount: f32) -> Result<(), String> {
        let i = self.find(name).ok_or_else(|| format!("no such neuron '{name}'"))?;
        self.pending_stimulus[i] += amount;
        Ok(())
    }

    /// A controlled STDP trial on a named synapse, bypassing real
    /// command-typing timing entirely -- DIAGNOSED (real test): a real
    /// `stim`-then-`stim` sequence with a ~1s gap between them (the
    /// realistic floor for two separately-typed shell commands)
    /// produced a dt so far outside STDP_TAU_MS (20ms) that the
    /// resulting weight change genuinely underflowed to zero in f32.
    ///
    /// MILESTONE 21: `pre_first` was added so this same function can
    /// drive BOTH the generic `train FROM TO GAP` DSL command (always
    /// pre-before-post, i.e. LTP) and neurons.rs's bare `train` command
    /// (which needs both directions: 5x LTP then 5x LTD on
    /// LeftKey->Motor, exactly as Milestone 10 verified) -- one function
    /// instead of two separate copies of "set two fire ticks gap_ticks
    /// apart and call apply_stdp".
    fn train_synapse(&mut self, from: &str, to: &str, gap_ticks: u64, pre_first: bool) -> Result<(f32, f32), String> {
        let from_i = self.find(from).ok_or_else(|| format!("no such neuron '{from}'"))?;
        let to_i = self.find(to).ok_or_else(|| format!("no such neuron '{to}'"))?;
        let syn_i = self
            .synapses
            .iter()
            .position(|s| s.from == from_i && s.to == to_i)
            .ok_or_else(|| format!("no synapse {from} -> {to}"))?;

        let before = self.synapses[syn_i].weight;
        let t_a = self.tick_count;
        let t_b = t_a + gap_ticks;
        self.tick_count = t_b + 1;
        if pre_first {
            self.neurons[from_i].last_fire_tick = Some(t_a);
            self.neurons[from_i].fire_count += 1;
            self.neurons[to_i].last_fire_tick = Some(t_b);
            self.neurons[to_i].fire_count += 1;
        } else {
            self.neurons[to_i].last_fire_tick = Some(t_a);
            self.neurons[to_i].fire_count += 1;
            self.neurons[from_i].last_fire_tick = Some(t_b);
            self.neurons[from_i].fire_count += 1;
        }
        apply_stdp(&mut self.synapses[syn_i].weight, self.neurons[from_i].last_fire_tick, self.neurons[to_i].last_fire_tick);
        Ok((before, self.synapses[syn_i].weight))
    }

    /// One tick over the WHOLE network: leak + stimulus + threshold
    /// check for every neuron, then propagate any firings through
    /// synapses for the NEXT tick.
    ///
    /// MILESTONE 21: refractory handling ported in from the old fixed
    /// network's LifNeuron::step -- stimulus arriving during refractory
    /// is dropped entirely (not queued), the correct LIF semantics M9
    /// already established. refractory_ticks defaults to 0 for every
    /// DSL-added neuron, so this is a no-op for networks that predate
    /// this milestone; it only actually engages for the seeded
    /// LeftKey/RightKey/Motor neurons.
    ///
    /// MILESTONE 14: any firing also triggers a real, automatic,
    /// self-silencing speaker blip.
    fn tick(&mut self) {
        let tick_n = self.tick_count;
        self.tick_count += 1;

        let mut fired = Vec::new();
        for (i, neuron) in self.neurons.iter_mut().enumerate() {
            if neuron.refractory_remaining > 0 {
                neuron.refractory_remaining -= 1;
                continue;
            }
            neuron.v = (neuron.v - neuron.leak).max(0.0);
            neuron.v += self.pending_stimulus[i];
            if neuron.v >= neuron.threshold {
                neuron.v = 0.0;
                neuron.fire_count += 1;
                neuron.last_fire_tick = Some(tick_n);
                neuron.refractory_remaining = neuron.refractory_ticks;
                fired.push(i);
            }
        }
        for s in self.pending_stimulus.iter_mut() {
            *s = 0.0;
        }
        // propagate: firings THIS tick become stimulus for the NEXT
        // tick (a real one-tick synaptic delay -- see the Milestone 21
        // header comment for why this now also applies to
        // LeftKey/RightKey/Motor, not just DSL-built networks)
        for src in fired.iter() {
            for syn in self.synapses.iter().filter(|s| s.from == *src) {
                self.pending_stimulus[syn.to] += syn.weight * SPIKE_GAIN;
            }
        }

        // MILESTONE 17: real STDP on every synapse touching a neuron
        // that just fired this tick.
        for syn_idx in 0..self.synapses.len() {
            let (from, to) = (self.synapses[syn_idx].from, self.synapses[syn_idx].to);
            if fired.contains(&from) || fired.contains(&to) {
                let pre = self.neurons[from].last_fire_tick;
                let post = self.neurons[to].last_fire_tick;
                apply_stdp(&mut self.synapses[syn_idx].weight, pre, post);
            }
        }

        if !fired.is_empty() {
            crate::speaker::beep(BEEP_FREQ_HZ);
            self.beep_ticks_remaining = BEEP_DURATION_TICKS;
        } else if self.beep_ticks_remaining > 0 {
            self.beep_ticks_remaining -= 1;
            if self.beep_ticks_remaining == 0 {
                crate::speaker::stop();
            }
        }
    }

    fn report(&self) -> String {
        if self.neurons.is_empty() {
            return String::from("(no neurons defined -- use 'addneuron NAME threshold=T leak=L')\n");
        }
        let mut out = String::new();
        for n in &self.neurons {
            out.push_str(&format!(
                "{:<10} V={:6.1} threshold={:5.1} leak={:4.1} refractory={:<3} fires={}\n",
                n.name, n.v, n.threshold, n.leak, n.refractory_ticks, n.fire_count
            ));
        }
        if !self.synapses.is_empty() {
            out.push_str("synapses:\n");
            for s in &self.synapses {
                out.push_str(&format!(
                    "  {} -> {} weight={:.3}\n",
                    self.neurons[s.from].name, self.neurons[s.to].name, s.weight
                ));
            }
        }
        out
    }
}

static NET: Mutex<GenericNetwork> = Mutex::new(GenericNetwork {
    neurons: Vec::new(),
    synapses: Vec::new(),
    pending_stimulus: Vec::new(),
    beep_ticks_remaining: 0,
    tick_count: 0,
});

pub fn tick() {
    NET.lock().tick();
}

pub fn report() -> String {
    NET.lock().report()
}

/// MILESTONE 21: called once from neurons::init() at boot -- seeds this
/// single shared network with the fixed LeftKey/RightKey/Motor neurons
/// and their synapses, so every later interface (real keypresses via
/// neurons::on_key_stimulus, the `net`/`addneuron`/`stim`/`train FROM
/// TO GAP` DSL commands, and the fixed `neurons`/`train`/`save`
/// commands) all operate on these SAME entries. Guarded against
/// double-seeding (harmless in practice since boot only calls this
/// once, but cheap insurance against a hypothetical re-init).
pub fn seed_fixed_network(
    left_name: &str,
    right_name: &str,
    motor_name: &str,
    key_threshold: f32,
    key_leak: f32,
    motor_threshold: f32,
    motor_leak: f32,
    refractory_ticks: u32,
    left_weight: f32,
    right_weight: f32,
) {
    let mut net = NET.lock();
    if net.find(left_name).is_some() {
        return;
    }
    let _ = net.add_neuron(left_name, key_threshold, key_leak, refractory_ticks);
    let _ = net.add_neuron(right_name, key_threshold, key_leak, refractory_ticks);
    let _ = net.add_neuron(motor_name, motor_threshold, motor_leak, refractory_ticks);
    let _ = net.add_synapse(left_name, motor_name, left_weight);
    let _ = net.add_synapse(right_name, motor_name, right_weight);
}

/// MILESTONE 21: named lookups so neurons.rs's fixed-command layer
/// (report formatting, disk persistence) can read this network's live
/// state without reaching into GenericNetwork's internals.
pub fn neuron_state(name: &str) -> Option<(f32, u32)> {
    let net = NET.lock();
    net.neurons.iter().find(|n| n.name == name).map(|n| (n.v, n.fire_count))
}

pub fn synapse_weight(from: &str, to: &str) -> Option<f32> {
    let net = NET.lock();
    let from_i = net.find(from)?;
    let to_i = net.find(to)?;
    net.synapses.iter().find(|s| s.from == from_i && s.to == to_i).map(|s| s.weight)
}

pub fn stimulate(name: &str, amount: f32) -> Result<(), String> {
    NET.lock().stimulate(name, amount)
}

pub fn train_synapse(from: &str, to: &str, gap_ticks: u64, pre_first: bool) -> Result<(f32, f32), String> {
    NET.lock().train_synapse(from, to, gap_ticks, pre_first)
}

/// Parses and runs one DSL line -- shell.rs's `net` command family.
/// Grammar, deliberately small:
///   addneuron NAME threshold=T leak=L [refractory=R]
///   addsynapse FROM TO weight=W
///   stim NAME AMOUNT
///   train FROM TO GAP_TICKS
pub fn run_dsl_line(line: &str) -> String {
    let parts: Vec<&str> = line.split_whitespace().collect();
    let result = match parts.as_slice() {
        ["addneuron", name, rest @ ..] => {
            let mut threshold = 100.0f32;
            let mut leak = 5.0f32;
            let mut refractory_ticks = 0u32;
            for kv in rest {
                if let Some(v) = kv.strip_prefix("threshold=") {
                    threshold = v.parse().unwrap_or(threshold);
                } else if let Some(v) = kv.strip_prefix("leak=") {
                    leak = v.parse().unwrap_or(leak);
                } else if let Some(v) = kv.strip_prefix("refractory=") {
                    refractory_ticks = v.parse().unwrap_or(refractory_ticks);
                }
            }
            NET.lock().add_neuron(name, threshold, leak, refractory_ticks)
        }
        ["addsynapse", from, to, rest @ ..] => {
            let mut weight = 0.5f32;
            for kv in rest {
                if let Some(v) = kv.strip_prefix("weight=") {
                    weight = v.parse().unwrap_or(weight);
                }
            }
            NET.lock().add_synapse(from, to, weight)
        }
        ["stim", name, amount] => {
            let amount: f32 = amount.parse().unwrap_or(0.0);
            NET.lock().stimulate(name, amount)
        }
        ["train", from, to, gap] => {
            let gap: u64 = gap.parse().unwrap_or(2);
            return match NET.lock().train_synapse(from, to, gap, true) {
                Ok((before, after)) => format!("{from}->{to} weight: {before:.4} -> {after:.4} (gap={gap} ticks)\n"),
                Err(e) => format!("error: {e}\n"),
            };
        }
        _ => Err(String::from(
            "usage: addneuron NAME [threshold=T] [leak=L] [refractory=R] | addsynapse FROM TO [weight=W] | stim NAME AMOUNT | train FROM TO GAP_TICKS",
        )),
    };
    match result {
        Ok(()) => String::from("ok\n"),
        Err(e) => format!("error: {e}\n"),
    }
}
