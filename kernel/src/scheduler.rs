//! MILESTONE 4: the scheduler's SELECTION policy (which task-slot runs
//! next) driven by an SNN with SSH-topological dimerized coupling
//! between adjacent slots -- the same v=1-g / w=1+g bond pattern
//! validated in Spikeling's topological_bank.py, ported here to test
//! the identical "self-healing" question at the kernel's own control
//! layer: does killing one slot's neuron crash/starve the rest, or does
//! a topologically-dimerized (g>0) bank degrade more gracefully than a
//! trivial (g<0) one?
//!
//! SCOPE, STATED HONESTLY: this is cooperative scheduling -- task-slots
//! are plain closures invoked in-place when selected, not independent
//! execution contexts with their own stacks. Real preemptive
//! multitasking (timer interrupts, context switching, separate stacks)
//! is real, separate, future work -- this milestone proves the
//! topological SELECTION policy itself, which is the piece "give it
//! topological redundancy" was actually asking for.

use alloc::vec::Vec;

/// One bond per adjacent pair of slots, alternating v=1-g / w=1+g --
/// identical convention to ssh_bonds() in topological_bank.py.
fn ssh_bonds(n: usize, g: f32) -> Vec<f32> {
    let v = 1.0 - g;
    let w = 1.0 + g;
    (0..n.saturating_sub(1))
        .map(|i| if i % 2 == 0 { v } else { w })
        .collect()
}

pub struct TaskSlot {
    pub alive: bool,
    potential: f32,
    pub fire_count: u32,
}

/// Coupled task-slot bank: each tick, every alive slot's potential
/// grows from its own bias (models "readiness" accruing, the same role
/// STDP-driven or reflex accumulation plays elsewhere in Spikeling's
/// LIF runtime) plus lateral input from alive neighbors through the SSH
/// bonds. The slot with the highest potential above THRESHOLD fires
/// (gets scheduled), then resets to 0 -- exactly the fire-and-reset
/// rule core/runtime/runtime.py's LIF neurons use, just applied to
/// task selection instead of spike dispatch.
pub struct TopologicalScheduler {
    pub slots: Vec<TaskSlot>,
    bonds: Vec<f32>,
    bias: f32,
    threshold: f32,
    g: f32, // MILESTONE 25: kept so add_slot() can recompute bonds for a grown bank
}

const THRESHOLD: f32 = 1.0;
const BIAS: f32 = 0.15;

impl TopologicalScheduler {
    pub fn new(n: usize, g: f32) -> Self {
        let slots = (0..n)
            .map(|_| TaskSlot {
                alive: true,
                potential: 0.0,
                fire_count: 0,
            })
            .collect();
        TopologicalScheduler {
            slots,
            bonds: ssh_bonds(n, g),
            bias: BIAS,
            threshold: THRESHOLD,
            g,
        }
    }

    /// Marks a slot dead -- the defect-injection test. Matches
    /// topological_bank.py's step_bank() defect handling: zeroed state,
    /// contributes nothing, receives nothing.
    pub fn kill(&mut self, id: usize) {
        if let Some(slot) = self.slots.get_mut(id) {
            slot.alive = false;
            slot.potential = 0.0;
        }
    }

    /// MILESTONE 25: grows the bank by one live slot (for a freshly
    /// spawned task with no dead slot to reuse) -- bonds are recomputed
    /// from scratch at the new size so the alternating v/w SSH pattern
    /// stays consistent with ssh_bonds() rather than just appending a
    /// bond that might land on the wrong parity. Returns the new slot's
    /// index.
    pub fn add_slot(&mut self) -> usize {
        self.slots.push(TaskSlot {
            alive: true,
            potential: 0.0,
            fire_count: 0,
        });
        self.bonds = ssh_bonds(self.slots.len(), self.g);
        self.slots.len() - 1
    }

    /// MILESTONE 25: brings a previously-killed slot back to life for
    /// reuse by a new spawn -- the mirror of kill(), resetting exactly
    /// the state kill() zeroed plus fire_count, so a reused id doesn't
    /// inherit its predecessor's fire history.
    pub fn revive(&mut self, id: usize) {
        if let Some(slot) = self.slots.get_mut(id) {
            slot.alive = true;
            slot.potential = 0.0;
            slot.fire_count = 0;
        }
    }

    /// Advances one tick. Returns the id of the task-slot selected to
    /// run this round, or None if nothing crossed threshold yet.
    pub fn step(&mut self) -> Option<usize> {
        let n = self.slots.len();
        let snapshot: Vec<f32> = self.slots.iter().map(|s| s.potential).collect();

        for i in 0..n {
            if !self.slots[i].alive {
                continue;
            }
            let mut coupling = 0.0;
            if i > 0 && self.slots[i - 1].alive {
                coupling += self.bonds[i - 1] * snapshot[i - 1];
            }
            if i + 1 < n && self.slots[i + 1].alive {
                coupling += self.bonds[i] * snapshot[i + 1];
            }
            // scale coupling down relative to bias so lateral input
            // nudges timing/fairness without letting one loud neighbor
            // permanently starve another -- coupling as a tiebreaker
            // signal, not the dominant term
            self.slots[i].potential += self.bias + 0.05 * coupling;
        }

        let winner = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.alive && s.potential >= self.threshold)
            .max_by(|(_, a), (_, b)| a.potential.partial_cmp(&b.potential).unwrap())
            .map(|(i, _)| i);

        if let Some(i) = winner {
            self.slots[i].potential = 0.0;
            self.slots[i].fire_count += 1;
        }
        winner
    }
}
