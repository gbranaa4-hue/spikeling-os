//! MILESTONE 8: a minimal interactive shell tying together everything
//! built so far -- keyboard input (M6), console rendering (M7), the
//! heap (M3, for the line buffer), and real introspection into the
//! kernel's own state (M4's scheduler, M5's tasks) -- the first way to
//! actually TALK to spikeling-os, rather than just watch it run a
//! fixed demo sequence once and halt.

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use spin::Mutex;

static LINE: Mutex<String> = Mutex::new(String::new());

// MILESTONE 32: a real current-working-directory, layered entirely in
// the shell on top of fs.rs's now arbitrary-depth-capable, always
// root-relative path API -- fs.rs itself has no notion of "current
// directory", it only ever resolves a full path starting at root
// (DIR_LBA). Stored as the chain of path components from root (empty
// == root itself) rather than a single pre-joined string, so `cd ..`
// is a plain `pop()` with no filesystem access needed at all -- popping
// to a shorter existing prefix is always valid, since every component
// currently in CWD was itself validated as a real directory at the
// time it was pushed.
static CWD: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Joins CWD into the root-relative path string fs.rs's API expects
/// (no leading slash, empty string means root).
fn cwd_path() -> String {
    CWD.lock().join("/")
}

/// MILESTONE 32: resolves a shell argument against CWD -- a leading
/// `/` means "absolute, starting at root" (the leading slash itself is
/// stripped since fs.rs's paths are always root-relative already);
/// anything else is relative to CWD. This is the ONLY place CWD gets
/// combined with a typed argument -- every command below calls this
/// once on its path argument(s) and then talks to fs.rs in terms of
/// plain root-relative paths, same as before Milestone 32 existed.
fn resolve_arg(arg: &str) -> String {
    if let Some(rest) = arg.strip_prefix('/') {
        rest.to_string()
    } else {
        let cwd = cwd_path();
        if cwd.is_empty() {
            arg.to_string()
        } else {
            format!("{cwd}/{arg}")
        }
    }
}

/// Sets CWD from an already-resolved, root-relative path (empty means
/// root) -- splits back into components for storage.
fn set_cwd(resolved: &str) {
    let mut cwd = CWD.lock();
    *cwd = if resolved.is_empty() { Vec::new() } else { resolved.split('/').map(|s| s.to_string()).collect() };
}

/// MILESTONE 32: the prompt now shows the real current directory
/// (`/> ` at root, `/a/b/c> ` three levels deep) rather than a fixed
/// `"> "` -- this doubles as live, continuous confirmation that `cd`
/// actually moved somewhere, not just a cosmetic addition; every
/// verification transcript for this milestone leans on it.
fn prompt_string() -> String {
    let cwd = cwd_path();
    if cwd.is_empty() { "/> ".to_string() } else { format!("/{cwd}> ") }
}

pub fn init() {
    crate::console::write_str(&prompt_string());
}

/// MILESTONE 29: re-prints the prompt after drawing mode exits itself
/// asynchronously via a real right-click, handled inside mouse.rs's own
/// IRQ12 packet handler -- outside the normal Enter-key flow in
/// on_char below that every other command completion goes through.
pub fn show_prompt() {
    crate::console::write_str(&prompt_string());
}

/// Called from keyboard.rs for every decoded character, in place of a
/// plain console echo -- a shell needs to intercept Enter/Backspace
/// specially (run a command / erase a character) rather than render
/// every character verbatim.
pub fn on_char(c: char) {
    match c {
        '\n' | '\r' => {
            crate::console::write_char('\n');
            let line = {
                let mut l = LINE.lock();
                let taken = l.clone();
                l.clear();
                taken
            };
            run_command(line.trim());
            crate::console::write_str(&prompt_string());
        }
        // pc-keyboard's Backspace key decodes to Unicode('\u{8}') with
        // HandleControl::Ignore; '\x7f' (DEL) handled too for safety,
        // in case that assumption is ever wrong on different hardware.
        '\u{8}' | '\x7f' => {
            let mut l = LINE.lock();
            if l.pop().is_some() {
                crate::console::backspace();
            }
        }
        c if !c.is_control() => {
            LINE.lock().push(c);
            crate::console::write_char(c);
        }
        _ => {}
    }
}

/// MILESTONE 23: shared arg-parser for the fixed-arity graphics DSL
/// commands (line/rect/fillrect) -- splits on whitespace like the rest
/// of the shell's simple space-separated commands, requiring exactly N
/// integers so malformed input hits the caller's usage message instead
/// of silently drawing garbage coordinates.
fn parse_usize_args<const N: usize>(rest: &str) -> Option<[usize; N]> {
    let mut out = [0usize; N];
    let mut parts = rest.split_whitespace();
    for slot in out.iter_mut() {
        *slot = parts.next()?.parse().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(out)
}

// MILESTONE 28: shared by the "ls" (root) and "ls DIR" (subdirectory)
// arms -- formats each entry with a trailing '/' for subdirectories so
// they're visibly distinct from files, rather than looking identical
// to a same-named file in the listing.
fn print_listing(result: Result<alloc::vec::Vec<(String, bool, u16)>, String>) {
    match result {
        Ok(entries) if entries.is_empty() => crate::console::write_str("(empty)\n"),
        Ok(entries) => {
            for (name, is_dir, len) in entries {
                if is_dir {
                    crate::console::write_str(&format!("{}/\n", name));
                } else {
                    crate::console::write_str(&format!("{name:<16} {len} bytes\n"));
                }
            }
        }
        Err(e) => crate::console::write_str(&format!("ls FAILED: {e}\n")),
    }
}

fn run_command(cmd: &str) {
    match cmd {
        "" => {}
        "help" => crate::console::write_str(
            "commands: help, about, tasks, spawn, kill, neurons, train, save, net, addneuron, addsynapse, stim, beep, silence, date, mouse, ls, cd, mkdir, rmdir, write, read, rm, clear, lspci, nic, nicinfo, sendpacket, recvpacket, pixel, line, rect, fillrect, draw, stopdraw, usertest, runproc, seedtestprog, runfile, seedfdtest, runfdtest, seedtestelf, runelf\n",
        ),
        "usertest" => {
            // MILESTONE 27: drops to real CPL=3 and back. setup() at
            // boot already mapped the user pages, so this should always
            // succeed once the kernel has finished booting -- reported
            // honestly either way, not assumed.
            match crate::usertest::run() {
                Ok(()) => crate::console::write_str(
                    "usertest: entered ring 3, ran the write + exit syscalls, returned cleanly -- see serial log for the hardware-recorded CPL confirmation and the raw write() bytes\n",
                ),
                Err(e) => crate::console::write_str(&format!("usertest FAILED: {e}\n")),
            }
        }
        "ls" => {
            // MILESTONE 32: lists CWD, not always root -- None (root's
            // own listing) only when CWD actually IS root.
            let cwd = cwd_path();
            if cwd.is_empty() {
                print_listing(crate::fs::list(None));
            } else {
                print_listing(crate::fs::list(Some(&cwd)));
            }
        }
        "cd" => {
            // MILESTONE 32: bare `cd`, no argument -- goes to root,
            // same convention as a real shell's `cd` with no argument.
            *CWD.lock() = Vec::new();
        }
        "about" => crate::console::write_str(
            "spikeling-os -- Spikeling's spiking-network runtime as the kernel's own control logic\n",
        ),
        "tasks" => {
            // MILESTONE 25: reports every currently-live task (the
            // original 3 fixed ones plus whatever spawn/kill has since
            // added or removed), not a hardcoded 3-counter line -- the
            // point is that this list can genuinely grow and shrink.
            let live = crate::tasks::live_tasks();
            if live.is_empty() {
                crate::console::write_str("(no live tasks)\n");
            } else {
                let mut out = String::from("live task fire counts: ");
                for t in live {
                    out.push_str(&format!("task{}={} ", t.id, t.counter));
                }
                out.push('\n');
                crate::console::write_str(&out);
            }
        }
        "spawn" => {
            // MILESTONE 25: creates one new demo counting task and
            // reports the id it was assigned -- the id is whatever
            // TopologicalScheduler slot was reused or newly added, not
            // guaranteed to be sequential once kills start freeing
            // slots for reuse.
            match crate::tasks::spawn() {
                Some(id) => crate::console::write_str(&format!("spawned task{id}\n")),
                None => crate::console::write_str(
                    "spawn FAILED -- scheduler not running yet or task capacity reached\n",
                ),
            }
        }
        "neurons" => crate::console::write_str(&crate::neurons::report()),
        "train" => {
            // MILESTONE 10: 5 real LTP trials (LeftKey fires, Motor
            // fires 2 ticks later -- pre-before-post) followed by 5
            // real LTD trials (Motor fires, LeftKey fires 2 ticks
            // later -- pre-after-post), on the LeftKey->Motor synapse.
            // Reports the weight after each phase so the direction AND
            // magnitude of real learning is directly visible.
            let before = crate::neurons::left_to_motor_weight();
            for _ in 0..5 {
                crate::neurons::run_training_trial(true, 2);
            }
            let after_ltp = crate::neurons::left_to_motor_weight();
            for _ in 0..5 {
                crate::neurons::run_training_trial(false, 2);
            }
            let after_ltd = crate::neurons::left_to_motor_weight();
            crate::console::write_str(&format!(
                "LeftKey->Motor weight: {before:.4} -> (5x LTP) -> {after_ltp:.4} -> (5x LTD) -> {after_ltd:.4}\n"
            ));
        }
        "save" => {
            // MILESTONE 11: writes the real, currently-learned weights
            // to the dedicated persistence disk -- reported honestly,
            // including the failure case (e.g. no persistence disk
            // attached).
            match crate::neurons::save_weights_to_disk() {
                Ok(()) => crate::console::write_str("weights saved to disk\n"),
                Err(e) => crate::console::write_str(&format!("save FAILED: {e}\n")),
            }
        }
        "net" => crate::console::write_str(&crate::network::report()),
        "mouse" => {
            let m = crate::mouse::state();
            crate::console::write_str(&format!(
                "x={} y={} left={} right={} middle={} packets={}\n",
                m.x, m.y, m.left, m.right, m.middle, m.packet_count
            ));
        }
        "date" => {
            let dt = crate::rtc::now();
            crate::console::write_str(&format!(
                "{:04}-{:02}-{:02} {:02}:{:02}:{:02} (real CMOS RTC, not the PIT tick count)\n",
                dt.year, dt.month, dt.day, dt.hour, dt.minute, dt.second
            ));
        }
        "speakerstatus" => crate::console::write_str(&format!(
            "speaker gate register enabled={}\n",
            crate::speaker::is_enabled()
        )),
        "silence" => {
            crate::speaker::stop();
            crate::console::write_str(&format!(
                "silenced -- speaker gate register enabled={}\n",
                crate::speaker::is_enabled()
            ));
        }
        "lspci" => {
            // MILESTONE 20: lists every device found by the real PCI
            // config-space scan in pci.rs (run once at boot) -- not a
            // live re-scan, same as `tasks`/`neurons` reporting
            // already-collected state rather than recomputing it.
            let devices = crate::pci::devices();
            if devices.is_empty() {
                crate::console::write_str("(no PCI devices found)\n");
            } else {
                for d in devices {
                    crate::console::write_str(&format!(
                        "{:02x}:{:02x}.{} {:04x}:{:04x} class {:02x}{:02x}\n",
                        d.bus, d.device, d.function, d.vendor_id, d.device_id, d.class_code, d.subclass
                    ));
                }
            }
        }
        "nic" => match crate::pci::find_nic() {
            Some(nic) => crate::console::write_str(&format!(
                "network controller found -- vendor={:04x} device={:04x}\n",
                nic.vendor_id, nic.device_id
            )),
            None => crate::console::write_str("no network controller found on PCI bus 0\n"),
        },
        "nicinfo" => {
            // MILESTONE 24: live register reads (MAC via RAL0/RAH0 read
            // at init, link status via a fresh STATUS register read
            // right now) -- not a cached/assumed value.
            match crate::nic::mac_address() {
                Some(mac) => {
                    let valid = crate::nic::mac_is_valid().unwrap_or(false);
                    let link = match crate::nic::link_up() {
                        Some(true) => "up",
                        Some(false) => "down",
                        None => "unknown",
                    };
                    crate::console::write_str(&format!(
                        "e1000 NIC -- MAC {} (address-valid bit: {valid}), link: {link}\n",
                        crate::nic::format_mac(mac)
                    ));
                }
                None => crate::console::write_str("no e1000 NIC initialized (see boot log for why)\n"),
            }
        }
        "sendpacket" => match crate::nic::send_test_packet() {
            Ok(true) => crate::console::write_str(
                "packet transmitted -- descriptor DD (descriptor done) bit confirmed set by hardware\n",
            ),
            Ok(false) => crate::console::write_str(
                "packet queued and TDT advanced, but DD bit never set within timeout -- transmission NOT confirmed\n",
            ),
            Err(e) => crate::console::write_str(&format!("sendpacket FAILED: {e}\n")),
        },
        "recvpacket" => match crate::nic::recv_packet() {
            // MILESTONE 26: recv_packet() itself does one bounded poll
            // of the RX ring's next descriptor -- this arm just reports
            // whichever of the three real outcomes actually happened.
            Ok(Some(frame)) => crate::console::write_str(&format!(
                "packet received -- src {} dst {} ethertype {:#06x} length {} bytes, payload matches test packet: {}\n",
                crate::nic::format_mac(frame.src_mac),
                crate::nic::format_mac(frame.dest_mac),
                frame.ethertype,
                frame.length,
                frame.payload_matches_test
            )),
            Ok(None) => crate::console::write_str("no packet received within timeout\n"),
            Err(e) => crate::console::write_str(&format!("recvpacket FAILED: {e}\n")),
        },
        "draw" => {
            // MILESTONE 29: real mouse-driven drawing -- movement while
            // the left button is held draws live on the framebuffer,
            // handled directly inside mouse.rs's IRQ12 packet handler.
            crate::mouse::enter_draw_mode();
            crate::console::write_str(
                "drawing mode ON -- hold left mouse button and move to draw, right-click (or 'stopdraw') to exit\n",
            );
        }
        "stopdraw" => {
            if crate::mouse::drawing_active() {
                crate::mouse::exit_draw_mode();
                crate::console::write_str("drawing mode OFF\n");
            } else {
                crate::console::write_str("not currently in drawing mode\n");
            }
        }
        "seedtestprog" => {
            // MILESTONE 34: writes this milestone's hand-assembled test
            // payload (loader::build_test_program_image()) to a REAL
            // file ("testprog") on the real on-disk filesystem -- not
            // something a real user would type, just the one piece of
            // test setup that needs genuine non-typeable machine-code
            // bytes on disk, which the keyboard-driven `write` shell
            // command uses.
            match crate::loader::seed_test_program() {
                Ok(len) => crate::console::write_str(&format!(
                    "seedtestprog: wrote {len} real bytes to 'testprog' on disk (hand-assembled syscalls + a distinguishing message) -- try 'runfile testprog'\n"
                )),
                Err(e) => crate::console::write_str(&format!("seedtestprog FAILED: {e}\n")),
            }
        }
        "seedfdtest" => {
            // MILESTONE 35: same reasoning as seedtestprog above, for
            // this milestone's own open/read/fdwrite/close test payload
            // (loader::FDTEST_PROGRAM). Real usage: `write fdtest ...`
            // (some known content) first, then `seedfdtest`, then
            // `runfile fdtestprog`, then `read fdout` to confirm the
            // fdwrite+close round trip actually persisted.
            match crate::loader::seed_fdtest_program() {
                Ok(len) => crate::console::write_str(&format!(
                    "seedfdtest: wrote {len} real bytes to 'fdtestprog' on disk (hand-assembled open/read/fdwrite/close syscalls) -- write a 'fdtest' file first, then 'runfile fdtestprog', then 'read fdout'\n"
                )),
                Err(e) => crate::console::write_str(&format!("seedfdtest FAILED: {e}\n")),
            }
        }
        "runfdtest" => {
            // MILESTONE 35: runs FDTEST_PROGRAM via process.rs's THIRD
            // hardcoded process slot (FDTEST_PROCESS) -- the SAME
            // open/read/fdwrite/close syscall test `runfile fdtestprog`
            // runs, but through the `runproc`-style boot-time-created
            // path instead of the file-loaded path. See process.rs's
            // FDTEST_PROCESS doc comment for why both exist: `runfile
            // fdtestprog` DOES exercise the real syscalls correctly
            // (verified in the serial log), but hits a separate,
            // pre-existing, unrelated Milestone 34 bug shortly
            // afterward; this command reuses the already-proven-safe
            // `runproc` mechanism so the fd syscalls can be verified
            // end to end without that bug in the way. Usage: `write
            // fdtest ...` (known content) first, then `runfdtest`, then
            // `read fdout` to confirm the fdwrite+close round trip
            // persisted.
            match crate::process::run(crate::process::FDTEST_PROCESS_ID) {
                Ok(()) => crate::console::write_str(
                    "runfdtest: entered ring 3 under FDTEST_PROCESS's own private page table, ran open/read/fdwrite/close syscalls against the real filesystem, returned cleanly -- see serial log for the full syscall trace\n",
                ),
                Err(e) => crate::console::write_str(&format!("runfdtest FAILED: {e}\n")),
            }
        }
        "seedtestelf" => {
            // MILESTONE 36: writes this milestone's REAL, externally-
            // built ELF64 test executable (loader::TEST_ELF_BYTES,
            // built with this machine's actual Rust toolchain --
            // rustc+rust-lld, NOT hand-assembled) to a real file
            // ("testelf") on the real on-disk filesystem, same
            // "genuine non-typeable bytes" reasoning as seedtestprog
            // above.
            match crate::loader::seed_test_elf() {
                Ok(len) => crate::console::write_str(&format!(
                    "seedtestelf: wrote {len} real bytes to 'testelf' on disk (a genuine ELF64 executable, not hand-assembled) -- try 'runelf testelf'\n"
                )),
                Err(e) => crate::console::write_str(&format!("seedtestelf FAILED: {e}\n")),
            }
        }
        "clear" => crate::console::clear_screen(),
        other => {
            // MILESTONE 12/17: variable-argument DSL commands can't be
            // matched as exact string literals like the fixed commands
            // above -- routed by prefix to network::run_dsl_line
            // instead. "train " (with args, M17's generic-network
            // trainer) is distinct from the exact-match "train" case
            // above (M10's LeftKey-specific trainer) since Rust's exact
            // match only matches the literal string with no arguments.
            if other.starts_with("addneuron ")
                || other.starts_with("addsynapse ")
                || other.starts_with("stim ")
                || other.starts_with("train ")
            {
                crate::console::write_str(&crate::network::run_dsl_line(other));
            } else if let Some(freq_str) = other.strip_prefix("beep ") {
                // MILESTONE 13: real PC speaker output -- the analogue
                // of Spikeling's own `action Motor -> [MOTOR_FIRE]`,
                // now a genuine physical/audible effect.
                match freq_str.trim().parse::<u32>() {
                    Ok(freq) => {
                        crate::speaker::beep(freq);
                        crate::console::write_str(&format!(
                            "beeping at {freq}Hz -- speaker gate register enabled={} (use 'silence' to stop)\n",
                            crate::speaker::is_enabled()
                        ));
                    }
                    Err(_) => crate::console::write_str("usage: beep FREQ_HZ\n"),
                }
            } else if let Some(rest) = other.strip_prefix("cd ") {
                // MILESTONE 32: real CWD navigation. `..` is handled
                // specially with no filesystem access (see CWD's own
                // doc comment for why that's always safe); `/` or an
                // empty argument goes to root; anything else is
                // resolved (relative to the CURRENT CWD, via
                // resolve_arg) and validated against the real on-disk
                // tree via fs::list before CWD is actually updated, so
                // a failed `cd` never leaves CWD pointing somewhere
                // nonexistent.
                let target = rest.trim();
                if target == ".." {
                    let popped = CWD.lock().pop().is_some();
                    if !popped {
                        crate::console::write_str("cd FAILED: already at root\n");
                    }
                } else if target.is_empty() || target == "/" {
                    *CWD.lock() = Vec::new();
                } else {
                    let resolved = resolve_arg(target);
                    let list_result =
                        if resolved.is_empty() { crate::fs::list(None) } else { crate::fs::list(Some(&resolved)) };
                    match list_result {
                        Ok(_) => set_cwd(&resolved),
                        Err(e) => crate::console::write_str(&format!("cd FAILED: {e}\n")),
                    }
                }
            } else if let Some(rest) = other.strip_prefix("write ") {
                // MILESTONE 18: real named-file storage on the same
                // ATA disk Milestone 11 already proved persists across
                // a genuine reboot.
                match rest.split_once(' ') {
                    Some((name, text)) => {
                        let target = resolve_arg(name);
                        match crate::fs::write_file(&target, text.as_bytes()) {
                            Ok(()) => crate::console::write_str(&format!("wrote {} bytes to '{name}'\n", text.len())),
                            Err(e) => crate::console::write_str(&format!("write FAILED: {e}\n")),
                        }
                    }
                    None => crate::console::write_str("usage: write NAME TEXT...\n"),
                }
            } else if let Some(name) = other.strip_prefix("read ") {
                let target = resolve_arg(name.trim());
                match crate::fs::read_file(&target) {
                    Ok(data) => {
                        let text = String::from_utf8_lossy(&data);
                        crate::console::write_str(&format!("{text}\n"));
                    }
                    Err(e) => crate::console::write_str(&format!("read FAILED: {e}\n")),
                }
            } else if let Some(name) = other.strip_prefix("rm ") {
                // MILESTONE 22: real delete -- frees the directory slot
                // AND its allocated data-pool sectors, so they're
                // genuinely available to a later write, not just
                // hidden from ls forever.
                let target = resolve_arg(name.trim());
                match crate::fs::delete_file(&target) {
                    Ok(()) => crate::console::write_str(&format!("removed '{}'\n", name.trim())),
                    Err(e) => crate::console::write_str(&format!("rm FAILED: {e}\n")),
                }
            } else if let Some(name) = other.strip_prefix("rmdir ") {
                // MILESTONE 32: real rmdir -- refuses on a non-empty
                // directory (fs::remove_dir's own real check), and the
                // shell additionally refuses to remove CWD itself
                // (comparing resolved, root-relative paths) since
                // fs.rs has no notion of "the shell's current
                // directory" to protect that invariant on its own --
                // succeeding would leave CWD pointing at sectors that
                // were just freed back into the pool.
                let target = resolve_arg(name.trim());
                if target == cwd_path() {
                    crate::console::write_str("rmdir FAILED: cannot remove the current directory\n");
                } else {
                    match crate::fs::remove_dir(&target) {
                        Ok(()) => crate::console::write_str(&format!("removed directory '{}'\n", name.trim())),
                        Err(e) => crate::console::write_str(&format!("rmdir FAILED: {e}\n")),
                    }
                }
            } else if let Some(name) = other.strip_prefix("mkdir ") {
                // MILESTONE 28: creates a real subdirectory -- its own
                // one-sector entry table allocated from the same pool
                // files use (see fs.rs doc comment for the full design
                // and its honest limits). MILESTONE 32: no longer
                // restricted to directly under root -- resolved against
                // CWD like every other path argument now.
                let target = resolve_arg(name.trim());
                match crate::fs::make_dir(&target) {
                    Ok(()) => crate::console::write_str(&format!("created directory '{}'\n", name.trim())),
                    Err(e) => crate::console::write_str(&format!("mkdir FAILED: {e}\n")),
                }
            } else if let Some(dir) = other.strip_prefix("ls ") {
                let target = resolve_arg(dir.trim());
                print_listing(crate::fs::list(Some(&target)));
            } else if let Some(rest) = other.strip_prefix("kill ") {
                // MILESTONE 25: terminates a live task for real -- its
                // stack gets freed (immediately, or deferred until it's
                // safely switched away from if it's the task this very
                // command happens to be running nested on top of).
                match rest.trim().parse::<usize>() {
                    Ok(id) => {
                        if crate::tasks::kill(id) {
                            crate::console::write_str(&format!("killed task{id}\n"));
                        } else {
                            crate::console::write_str(&format!(
                                "kill FAILED -- task{id} not found or already dead\n"
                            ));
                        }
                    }
                    Err(_) => crate::console::write_str("usage: kill ID\n"),
                }
            } else if let Some(rest) = other.strip_prefix("runproc ") {
                // MILESTONE 30: enters ring 3 under process ID's OWN,
                // genuinely separate PML4 (created at boot by
                // process::init_test_processes) rather than the
                // kernel's shared page tables `usertest` still uses --
                // real per-process isolation, not just a second
                // hardcoded program. MILESTONE 33: the program it runs
                // now also calls the sbrk syscall against its own
                // private per-process heap before writing/exiting --
                // see serial log for the heap marker byte proof.
                match rest.trim().parse::<u8>() {
                    Ok(id) => match crate::process::run(id) {
                        Ok(()) => crate::console::write_str(&format!(
                            "runproc {id}: entered ring 3 under its own private page table, ran sbrk+write+exit syscalls, returned cleanly -- see serial log for its message + private heap marker + hardware CPL confirmation\n"
                        )),
                        Err(e) => crate::console::write_str(&format!("runproc FAILED: {e}\n")),
                    },
                    Err(_) => crate::console::write_str("usage: runproc ID (1 or 2)\n"),
                }
            } else if let Some(path) = other.strip_prefix("runfile ") {
                // MILESTONE 34: real general program loader -- reads
                // `path`'s bytes off the actual on-disk filesystem
                // (fs.rs) and runs them under a fresh private PML4 via
                // process::create_loaded_process()/run_loaded_process()
                // (MILESTONE 35: split from one original
                // load_and_run_image() to fix a real bug -- see
                // run_loaded_process()'s own doc comment), the same
                // page-table mechanism Milestone 30 built for
                // PROCESS_A/PROCESS_B, just fed from a real file read
                // instead of a compiled-in array.
                match crate::loader::run_file(path.trim()) {
                    Ok(()) => crate::console::write_str(
                        "runfile: loaded from disk and ran under its own private page table -- write+exit syscalls returned cleanly, see serial log for the message it printed (proof it came from the file) + hardware CPL confirmation\n",
                    ),
                    Err(e) => crate::console::write_str(&format!("{e}\n")),
                }
            } else if let Some(path) = other.strip_prefix("runelf ") {
                // MILESTONE 36: real ELF64 loader -- reads `path`'s
                // bytes off the actual on-disk filesystem (same as
                // `runfile` above), genuinely PARSES them as ELF64
                // (kernel/src/elf.rs -- real magic/e_type/e_machine
                // validation, real program header table walk, real
                // PT_LOAD segment extraction) instead of assuming
                // they're already flat code at offset 0, then maps each
                // PT_LOAD segment at its own real p_vaddr and runs the
                // ELF's own e_entry under a fresh private PML4 (see
                // process::create_process_from_elf() for this
                // milestone's honest scoping decision: e_entry must
                // equal USER_CODE_ADDR exactly, since the ring-3 entry
                // trampoline's jump target was deliberately kept fixed
                // rather than made dynamic this milestone).
                match crate::loader::run_elf(path.trim()) {
                    Ok(()) => crate::console::write_str(
                        "runelf: parsed a REAL ELF64 file, mapped its PT_LOAD segments at their own vaddrs, and ran it under its own private page table -- write+exit syscalls returned cleanly, see serial log for the parsed e_entry/segments + the message it printed (proof execution reached a non-zero-offset segment) + hardware CPL confirmation\n",
                    ),
                    Err(e) => crate::console::write_str(&format!("{e}\n")),
                }
            } else if let Some(rest) = other.strip_prefix("pixel ") {
                match parse_usize_args::<2>(rest) {
                    Some([x, y]) => {
                        crate::console::draw_pixel(x, y, true);
                        crate::console::write_str(&format!("pixel set at ({x},{y})\n"));
                    }
                    None => crate::console::write_str("usage: pixel X Y\n"),
                }
            } else if let Some(rest) = other.strip_prefix("line ") {
                // MILESTONE 23: real framebuffer graphics -- raw pixel
                // coordinates, independent of the text cursor.
                match parse_usize_args::<4>(rest) {
                    Some([x0, y0, x1, y1]) => {
                        crate::console::draw_line(x0, y0, x1, y1);
                        crate::console::write_str(&format!("line drawn from ({x0},{y0}) to ({x1},{y1})\n"));
                    }
                    None => crate::console::write_str("usage: line X0 Y0 X1 Y1\n"),
                }
            } else if let Some(rest) = other.strip_prefix("fillrect ") {
                match parse_usize_args::<4>(rest) {
                    Some([x, y, w, h]) => {
                        crate::console::draw_rect(x, y, w, h, true);
                        crate::console::write_str(&format!("filled rect drawn at ({x},{y}) {w}x{h}\n"));
                    }
                    None => crate::console::write_str("usage: fillrect X Y W H\n"),
                }
            } else if let Some(rest) = other.strip_prefix("rect ") {
                match parse_usize_args::<4>(rest) {
                    Some([x, y, w, h]) => {
                        crate::console::draw_rect(x, y, w, h, false);
                        crate::console::write_str(&format!("rect drawn at ({x},{y}) {w}x{h}\n"));
                    }
                    None => crate::console::write_str("usage: rect X Y W H\n"),
                }
            } else {
                crate::console::write_str(&format!("unknown command: {other}\n"));
            }
        }
    }
}
