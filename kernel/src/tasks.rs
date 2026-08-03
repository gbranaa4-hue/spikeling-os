//! MILESTONE 5, STAGE C: real per-task stacks and a genuine preemptive
//! context switch, driven by the timer interrupt (stage B) and
//! selecting which task runs next via the SAME TopologicalScheduler
//! from Milestone 4 -- closing the loop the README named as the actual
//! goal: "this is where Spikeling's spiking-network logic becomes a
//! real candidate to replace a conventional scheduler, once there's a
//! scheduler slot to replace." Now there is one, and it's real: the
//! three worker tasks below never call back into scheduling code
//! themselves -- they just spin, incrementing their own counter,
//! entirely at the mercy of the timer interrupt firing asynchronously
//! mid-loop. That's the actual distinction from Milestone 4's
//! cooperative closures.
//!
//! MILESTONE 25: real dynamic task lifecycle on top of the above --
//! spawn() and kill() let the shell create and destroy worker tasks at
//! runtime instead of the fixed 3 booted once by run_preemption_demo().
//! Two things had to change to make that real rather than cosmetic:
//! (1) TopologicalScheduler's slot bank, fixed-size since Milestone 4,
//! needed a genuine "grow" operation (add_slot) and a way to bring a
//! killed slot back to life (revive), not just kill(); (2) the original
//! demo's timer_tick_switch() stopped switching entirely once its
//! bounded MAX_TICKS window closed (by design, so kernel_main could
//! regain control and finish booting) -- spawned tasks would never
//! actually run if that stayed permanent, so kernel_main now flips on
//! BACKGROUND scheduling once, right before its own final hlt_loop(),
//! at which point it has nothing left to do and losing control to a
//! worker task forever is harmless. A killed task's stack is freed
//! immediately UNLESS it's the one currently running (a real case: kill
//! runs nested inside the keyboard ISR, on top of whatever task's stack
//! happened to be current when the key was pressed, including possibly
//! its own target) -- that case is queued in ZOMBIE and reaped lazily,
//! the first tick a real switch has actually carried execution off it.

use crate::scheduler::TopologicalScheduler;
use alloc::boxed::Box;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use spin::Mutex;

const STACK_SIZE: usize = 4096 * 4; // 16 KiB per task
const N_TASKS: usize = 3; // original Milestone 5c fixed task count
const MAX_TASKS: usize = 8; // Milestone 25: hard cap on concurrent task slots
const MAX_TICKS: u64 = 60; // ~3.3s of real time at the default ~18.2Hz PIT rate

pub type TaskId = usize;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct TaskContext {
    rsp: u64,
}

struct Task {
    ctx: TaskContext,
    _stack: Box<[u8]>, // kept alive for the task's lifetime; never read directly after setup
}

impl Task {
    /// Pre-populates the new stack to look as if it already called
    /// switch_to() once, so the FIRST switch_to() INTO this task pops
    /// harmless zeroed register values and `ret`s straight into
    /// `entry` -- exactly like resuming a previously-interrupted task,
    /// just with a synthetic first "interruption".
    fn new(entry: extern "C" fn() -> !) -> Task {
        let mut stack = alloc::vec![0u8; STACK_SIZE].into_boxed_slice();
        let stack_top = unsafe { stack.as_mut_ptr().add(STACK_SIZE) };
        unsafe {
            let mut sp = stack_top as *mut u64;
            sp = sp.sub(1);
            *sp = entry as u64; // what switch_to's `ret` will jump to
            for _ in 0..6 {
                // rbx, rbp, r12, r13, r14, r15 -- matches switch_to's push order
                sp = sp.sub(1);
                *sp = 0;
            }
            Task {
                ctx: TaskContext { rsp: sp as u64 },
                _stack: stack,
            }
        }
    }
}

/// Minimal context switch: only the callee-saved registers + stack
/// pointer, following the standard "switch_to" pattern (Redox, xv6,
/// countless minimal-kernel tutorials). Correct specifically because
/// it's called as an ORDINARY nested function call from within the
/// timer interrupt handler's Rust body -- the compiler-generated
/// x86-interrupt prologue/epilogue already protects the interrupted
/// task's caller-saved registers on ITS OWN stack; this only needs to
/// additionally preserve what a normal call boundary requires anyway.
///
/// DIAGNOSED AND FIXED: the very first version of this hung completely
/// after exactly one real switch, never returning to kernel_main. Root
/// cause: switching stacks via a plain `ret` (not a real `iretq`) means
/// the CPU's interrupt flag -- automatically cleared on interrupt
/// entry -- never gets restored. Once the first switch happened from
/// inside the timer handler, interrupts stayed permanently disabled,
/// so no further timer tick could ever fire to drive another switch --
/// whichever task got landed on just ran forever. Fixed with an
/// explicit `sti` before `ret`, on every switch (harmless/idempotent
/// when interrupts are already enabled, e.g. the initial bootstrap
/// switch from ordinary kernel_main code).
#[unsafe(naked)]
unsafe extern "C" fn switch_to(_old_rsp_ptr: *mut u64, _new_rsp: u64) {
    core::arch::naked_asm!(
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbp",
        "pop rbx",
        "sti",
        "ret",
    );
}

pub static TASK_COUNTERS: [AtomicU64; MAX_TASKS] = [const { AtomicU64::new(0) }; MAX_TASKS];

static TASKS: Mutex<Vec<Option<Task>>> = Mutex::new(Vec::new());
static ZOMBIE: Mutex<Vec<TaskId>> = Mutex::new(Vec::new()); // MILESTONE 25: self-killed tasks awaiting a safe reap
static KERNEL_RSP: AtomicU64 = AtomicU64::new(0);
static CURRENT: AtomicUsize = AtomicUsize::new(usize::MAX); // MAX = "kernel_main/shell is running, no worker active"
static SCHED: Mutex<Option<TopologicalScheduler>> = Mutex::new(None);
static TOTAL_SWITCHES: AtomicU64 = AtomicU64::new(0);
static DEMO_ACTIVE: AtomicBool = AtomicBool::new(false); // bounded Milestone 5c window only
static BACKGROUND: AtomicBool = AtomicBool::new(false); // MILESTONE 25: perpetual post-boot scheduling

/// BUGFIX (found while root-causing the Milestone 36 disclosed ELF-loader
/// page fault): usertest.rs's ring-3 excursions (usertest/runproc/
/// runfile/runelf, ALL funneled through enter_ring3()/enter_ring3_now())
/// run with interrupts enabled, deliberately, so the excursion's own
/// syscalls work -- but that means a background-scheduler timer tick
/// CAN legitimately fire while nested deep inside one, with CURRENT ==
/// usize::MAX ("kernel_main/shell is running"). If the scheduler then
/// picks a different task, timer_tick_switch() below calls switch_to()
/// on THAT nested, mid-excursion rsp -- which stomps THIS module's own
/// KERNEL_RSP (a completely different static than usertest.rs's own
/// same-named KERNEL_RSP, despite sharing a name) with a stack pointer
/// sitting mid-excursion, and a later scheduler switch back to
/// "kernel_main" resumes it via switch_to()'s callee-saved-register
/// convention -- a protocol mismatch with resume_kernel()'s actual
/// int-0x80-return convention, corrupting execution once it unwinds.
/// This is the SAME general failure class the earlier, already-fixed
/// `sti`-in-resume_kernel bug was (a nested timer tick hijacking
/// execution mid-call-chain) -- just triggered here by the excursion's
/// OWN interrupts-enabled design (a real, disclosed, documented
/// limitation in usertest::run()'s own comment) rather than a premature
/// `sti`. Fixed the same way real kernels handle this class of hazard:
/// a real, minimal "preemption disabled" guard around exactly the
/// excursion's duration, checked here and set/cleared by
/// usertest::run()/enter_ring3_now() around their enter_ring3() calls.
/// Scoped deliberately narrow -- it only skips SWITCHING a task away
/// mid-tick, not the timer interrupt itself, so PIT-driven timing
/// (Milestone 5b) and reap_zombies() below are unaffected.
pub static RING3_EXCURSION_ACTIVE: AtomicBool = AtomicBool::new(false);

/// TEMPORARY DIAGNOSTIC (root-causing the Milestone 36 page fault):
/// counts how many timer/keyboard interrupts landed WHILE
/// RING3_EXCURSION_ACTIVE was true, to directly correlate interrupt
/// activity during the excursion window against pass/fail -- same
/// diagnostic style this project already used elsewhere (instrumenting
/// Cr3::read() inside the timer handler for an earlier bug). Not meant
/// to stay long-term.
pub static TIMER_DURING_EXCURSION: AtomicU64 = AtomicU64::new(0);
pub static KEYBOARD_DURING_EXCURSION: AtomicU64 = AtomicU64::new(0);

extern "C" fn worker_entry_0() -> ! {
    worker_loop(0)
}
extern "C" fn worker_entry_1() -> ! {
    worker_loop(1)
}
extern "C" fn worker_entry_2() -> ! {
    worker_loop(2)
}

/// Slots 0-2 always spin at the original Milestone 5c rate (kept exact,
/// so the fixed demo's reported counters are unchanged); spawned slots
/// (>= N_TASKS) alternate a faster/slower spin count by id parity --
/// two genuinely distinguishable demo bodies, per Milestone 25, rather
/// than clones differing only by which counter they touch.
fn worker_loop(id: usize) -> ! {
    let spin_count = if id < N_TASKS || id % 2 == 0 { 2000 } else { 6000 };
    loop {
        TASK_COUNTERS[id].fetch_add(1, Ordering::Relaxed);
        for _ in 0..spin_count {
            core::hint::spin_loop();
        }
    }
}

/// MILESTONE 25: one entry stub per possible task slot -- switch_to's
/// `ret` needs a plain fn pointer with no captured state, so the id a
/// spawned task counts under has to be baked in at compile time via a
/// distinct fn item per slot, not passed as an argument.
static DEMO_ENTRIES: [extern "C" fn() -> !; MAX_TASKS] = [
    { extern "C" fn e0() -> ! { worker_loop(0) } e0 },
    { extern "C" fn e1() -> ! { worker_loop(1) } e1 },
    { extern "C" fn e2() -> ! { worker_loop(2) } e2 },
    { extern "C" fn e3() -> ! { worker_loop(3) } e3 },
    { extern "C" fn e4() -> ! { worker_loop(4) } e4 },
    { extern "C" fn e5() -> ! { worker_loop(5) } e5 },
    { extern "C" fn e6() -> ! { worker_loop(6) } e6 },
    { extern "C" fn e7() -> ! { worker_loop(7) } e7 },
];

/// Bootstraps the 3 worker tasks, switches into the first one, and
/// blocks (from kernel_main's point of view) until the timer-driven
/// switcher below has counted MAX_TICKS timer interrupts and switched
/// back here. None of the worker loops above ever call this module's
/// scheduling code themselves -- the only thing moving execution
/// between them is the timer interrupt.
pub fn run_preemption_demo() {
    {
        let mut tasks = TASKS.lock();
        tasks.push(Some(Task::new(worker_entry_0)));
        tasks.push(Some(Task::new(worker_entry_1)));
        tasks.push(Some(Task::new(worker_entry_2)));
    }
    *SCHED.lock() = Some(TopologicalScheduler::new(N_TASKS, 0.6));
    TOTAL_SWITCHES.store(0, Ordering::SeqCst);
    CURRENT.store(0, Ordering::SeqCst);
    DEMO_ACTIVE.store(true, Ordering::SeqCst);

    let first_rsp = TASKS.lock()[0].as_ref().unwrap().ctx.rsp;
    unsafe {
        switch_to(KERNEL_RSP.as_ptr(), first_rsp);
    }
    // execution resumes HERE once the timer handler's "return to
    // kernel_main" branch below switches back -- reached via the
    // return address switch_to's `ret` restores from this call's own
    // stack frame, exactly as if this function call had simply
    // returned normally after some delay.
}

/// MILESTONE 25: called once, right before kernel_main's own final
/// hlt_loop() -- from that point on kernel_main has nothing left to
/// do, so it's safe for the timer tick to start permanently favoring
/// whichever worker task the scheduler picks over resuming "kernel".
pub fn enable_background_scheduling() {
    BACKGROUND.store(true, Ordering::SeqCst);
}

/// Locates the raw stack-pointer-storage location for `current`
/// (either a real task's ctx.rsp, or the dedicated KERNEL_RSP static
/// standing in for kernel_main/the shell) -- shared by every switch
/// site below instead of duplicating the usize::MAX special case.
fn current_rsp_ptr(current: usize) -> *mut u64 {
    if current == usize::MAX {
        KERNEL_RSP.as_ptr()
    } else {
        let mut tasks = TASKS.lock();
        &mut tasks[current]
            .as_mut()
            .expect("current task slot missing")
            .ctx
            .rsp as *mut u64
    }
}

/// MILESTONE 25: allocates a new task (reusing a dead slot's index if
/// one exists, otherwise growing the scheduler by one live slot) and
/// registers it to run the demo counting loop under its assigned id.
/// Returns None if the scheduler hasn't been bootstrapped yet or the
/// MAX_TASKS cap has been reached.
pub fn spawn() -> Option<TaskId> {
    let mut tasks = TASKS.lock();
    let mut sched_guard = SCHED.lock();
    let sched = sched_guard.as_mut()?;

    let id = match tasks.iter().position(|t| t.is_none()) {
        Some(id) => {
            sched.revive(id);
            id
        }
        None => {
            if tasks.len() >= MAX_TASKS {
                return None;
            }
            let id = tasks.len();
            tasks.push(None);
            let added = sched.add_slot();
            debug_assert_eq!(added, id);
            id
        }
    };

    TASK_COUNTERS[id].store(0, Ordering::Relaxed);
    tasks[id] = Some(Task::new(DEMO_ENTRIES[id]));
    Some(id)
}

/// MILESTONE 25: terminates a live task for real -- the scheduler
/// slot goes dead immediately (it will never be picked again), and the
/// stack is freed immediately too UNLESS `id` is the task currently
/// executing (e.g. this call is running nested inside the keyboard ISR
/// on top of task `id`'s own stack, because task `id` was current when
/// the key was pressed) -- freeing THAT stack out from under ourselves
/// would corrupt the very frame we're about to iretq back into. That
/// case is deferred to reap_zombies(), safe once a later tick's real
/// switch has actually carried execution off this stack for good.
pub fn kill(id: TaskId) -> bool {
    let mut sched_guard = SCHED.lock();
    let Some(sched) = sched_guard.as_mut() else {
        return false;
    };
    if id >= sched.slots.len() || !sched.slots[id].alive {
        return false;
    }
    sched.kill(id);
    drop(sched_guard);

    if CURRENT.load(Ordering::SeqCst) == id {
        ZOMBIE.lock().push(id);
    } else {
        TASKS.lock()[id] = None; // drops the Task, freeing its Box<[u8]> stack now
    }
    true
}

/// Frees any zombie's stack once it's no longer the currently-running
/// task -- called every tick so a deferred self-kill gets reaped the
/// first real opportunity, not left leaking forever.
fn reap_zombies() {
    let current = CURRENT.load(Ordering::SeqCst);
    let mut zombies = ZOMBIE.lock();
    zombies.retain(|&id| {
        if id == current {
            true
        } else {
            TASKS.lock()[id] = None;
            false
        }
    });
}

pub struct TaskReport {
    pub id: TaskId,
    pub counter: u64,
}

/// MILESTONE 25: every task the scheduler currently considers alive --
/// what the shell's `tasks` command reports, so spawn/kill are visible
/// as genuine growth/shrinkage of this list, not just a command that
/// returned success.
pub fn live_tasks() -> Vec<TaskReport> {
    let tasks = TASKS.lock();
    let sched_guard = SCHED.lock();
    let mut out = Vec::new();
    if let Some(sched) = sched_guard.as_ref() {
        for (id, slot) in tasks.iter().enumerate() {
            if slot.is_some() && sched.slots[id].alive {
                out.push(TaskReport {
                    id,
                    counter: TASK_COUNTERS[id].load(Ordering::Relaxed),
                });
            }
        }
    }
    out
}

/// Called from the timer interrupt handler (after EOI). Uses the same
/// TopologicalScheduler design as Milestone 4 to pick which worker
/// runs next, or switches back to kernel_main once MAX_TICKS is
/// reached (the original bounded Milestone 5c demo window). Once
/// Milestone 25's enable_background_scheduling() has been called, the
/// same selection logic keeps running indefinitely afterward with no
/// tick budget -- kernel_main/the shell is just another context that
/// can lose the CPU to a worker task and, since it has nothing left to
/// do but hlt forever at that point, never needs it back.
///
/// DIAGNOSED AND FIXED (Milestone 5c): step() legitimately returns
/// None on most ticks (no slot has crossed threshold yet -- accumulation
/// takes several ticks) -- that used to be conflated with the SEPARATE
/// "time budget exhausted, return to kernel_main" condition, both
/// falling into one `None` match arm. The very first ordinary "no
/// winner yet" tick was ending the whole demo immediately, which is
/// why only the first task ever ran. Fixed by checking the tick budget
/// unconditionally first, independent of whatever the scheduler does
/// or doesn't return.
pub fn timer_tick_switch() {
    reap_zombies();

    // BUGFIX: see RING3_EXCURSION_ACTIVE's own doc comment above -- never
    // switch a task away while a ring-3 excursion (usertest/runproc/
    // runfile/runelf) is mid-flight; its own nested rsp is not a valid
    // task context to save/restore through this mechanism. The timer
    // interrupt itself still fires and returns normally (PIT timing,
    // Milestone 5b, is unaffected) -- only the SWITCH decision is
    // skipped for this one tick.
    if RING3_EXCURSION_ACTIVE.load(Ordering::SeqCst) {
        return;
    }

    let current = CURRENT.load(Ordering::SeqCst);

    if DEMO_ACTIVE.load(Ordering::SeqCst) {
        let ticks = TOTAL_SWITCHES.fetch_add(1, Ordering::SeqCst);
        if ticks + 1 >= MAX_TICKS {
            DEMO_ACTIVE.store(false, Ordering::SeqCst);
            CURRENT.store(usize::MAX, Ordering::SeqCst);
            let new_rsp = KERNEL_RSP.load(Ordering::SeqCst);
            let old_rsp_ptr = current_rsp_ptr(current);
            unsafe {
                switch_to(old_rsp_ptr, new_rsp);
            }
            return;
        }
    } else if !BACKGROUND.load(Ordering::SeqCst) {
        return; // neither the bounded demo nor Milestone 25 background mode is active
    }

    let next = SCHED.lock().as_mut().and_then(|s| s.step());
    if let Some(next_idx) = next {
        if current != next_idx {
            let old_rsp_ptr = current_rsp_ptr(current);
            let new_rsp = TASKS.lock()[next_idx]
                .as_ref()
                .expect("scheduled task slot missing")
                .ctx
                .rsp;
            CURRENT.store(next_idx, Ordering::SeqCst);
            unsafe {
                switch_to(old_rsp_ptr, new_rsp);
            }
        }
        // else: scheduler re-picked the currently running task -- nothing to do
    }
    // else: no slot crossed threshold yet this tick -- try again next tick
}
