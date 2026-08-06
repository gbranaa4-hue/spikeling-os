use ovmf_prebuilt::{Arch, FileType, Prebuilt, Source};
use std::env;
use std::fs;
use std::process::{Command, exit};

// MILESTONE 40 investigation: ata.rs (since Milestone 11) has always
// targeted the SECONDARY ATA bus (ports 0x170-0x177) for weight
// persistence, on the documented assumption that "a separate, dedicated
// disk image is attached there... in the verification harness" -- but no
// such harness ever actually existed in this runner. Every `cargo run`
// since Milestone 11 has launched QEMU with exactly one drive (the boot
// image, implicitly primary-master), leaving the secondary bus
// unclaimed. Reading/writing an unclaimed IDE bus in QEMU reads back
// with the ERR status bit set, which is exactly the "ATA error bit set"
// fs::self_test_disk_write() started catching once something finally
// checked write_sector()'s real return value instead of only silently
// discarding it via `.ok()?` in load_weights() (see ata.rs). This
// creates and attaches a real, stable-across-runs persistence disk so
// ata.rs finally has a real secondary-bus device to talk to, matching
// what its own module doc always claimed.
fn ensure_persist_disk() -> String {
    let path = "target/persist.img";
    if !std::path::Path::new(path).exists() {
        // 1 MiB is far more than the single 512-byte sector ata.rs
        // currently uses, but leaves real headroom without any real
        // cost -- an all-zero image reads back as a blank disk (no
        // magic match), the same honest "no saved weights" state
        // load_weights() already handles correctly.
        let blank = vec![0u8; 1024 * 1024];
        fs::write(path, blank).expect("failed to create target/persist.img");
    }
    path.to_string()
}

// Runs the built kernel disk image in QEMU -- this is the "run it
// virtually" piece: iterate against a real emulated x86_64 machine with
// no risk to physical hardware, same approach every serious hobby-OS
// project uses before anything ever touches real bare metal.
fn main() {
    // read env variables that were set in build.rs
    let uefi_path = env!("UEFI_PATH");
    let bios_path = env!("BIOS_PATH");

    let args: Vec<String> = env::args().collect();
    let prog = &args[0];

    let uefi = match args.get(1).map(|s| s.to_lowercase()) {
        Some(ref s) if s == "uefi" => true,
        Some(ref s) if s == "bios" => false,
        Some(ref s) if s == "-h" || s == "--help" => {
            println!("Usage: {prog} [uefi|bios]");
            println!("  uefi  - boot using OVMF (UEFI)");
            println!("  bios  - boot using legacy BIOS");
            exit(0);
        }
        _ => {
            eprintln!("Usage: {prog} [uefi|bios]");
            exit(1);
        }
    };

    let mut cmd = Command::new("qemu-system-x86_64");
    // print serial output (writeln! calls in the kernel) to this shell
    cmd.arg("-serial").arg("mon:stdio");
    // no graphical window needed for milestone 1 -- serial output only
    cmd.arg("-display").arg("none");
    // lets the kernel request a clean QEMU exit on panic (see
    // exit_qemu() in kernel/src/main.rs)
    cmd.arg("-device")
        .arg("isa-debug-exit,iobase=0xf4,iosize=0x04");
    // MILESTONE 33: an additional QEMU human-monitor instance on a fixed
    // TCP port, alongside (not replacing) the existing `-serial
    // mon:stdio` monitor -- lets test tooling drive `sendkey`/`quit`
    // over a plain socket independently of whatever is/isn't attached to
    // this process's own stdio. Opt-in via an env var, unset by default,
    // so ordinary `cargo run` (every other milestone's workflow) is
    // completely unaffected.
    if let Ok(port) = env::var("SPIKELING_QEMU_MONITOR_PORT") {
        cmd.arg("-monitor")
            .arg(format!("tcp:127.0.0.1:{port},server,nowait"));
    }
    // Diagnostic-only, opt-in via env var (same pattern as the monitor port
    // above) -- full interrupt/exception cascade + CPU-reset trace to a
    // file, and --no-reboot so a triple fault halts QEMU instead of
    // silently rebooting past it. Same technique this project's own M41
    // diagnostic history already used successfully.
    if env::var("SPIKELING_QEMU_TRACE").is_ok() {
        cmd.arg("-d").arg("int,cpu_reset");
        cmd.arg("-D").arg("qemu_trace.log");
        cmd.arg("--no-reboot");
    }

    if uefi {
        let prebuilt =
            Prebuilt::fetch(Source::LATEST, "target/ovmf").expect("failed to fetch OVMF firmware");
        let code = prebuilt.get_file(Arch::X64, FileType::Code);
        let vars = prebuilt.get_file(Arch::X64, FileType::Vars);

        cmd.arg("-drive").arg(format!("format=raw,file={uefi_path}"));
        cmd.arg("-drive").arg(format!(
            "if=pflash,format=raw,unit=0,file={},readonly=on",
            code.display()
        ));
        cmd.arg("-drive").arg(format!(
            "if=pflash,format=raw,unit=1,file={},snapshot=on",
            vars.display()
        ));
    } else {
        cmd.arg("-drive").arg(format!("format=raw,file={bios_path}"));
    }

    // ata.rs's dedicated secondary-bus persistence disk -- explicit
    // bus=ide.1 (secondary controller) unit=0 (master) so it lands at
    // exactly the ports (0x170-0x177) ata.rs targets, regardless of
    // QEMU version-specific implicit-index defaults for the boot drive
    // above.
    let persist_path = ensure_persist_disk();
    cmd.arg("-drive")
        .arg(format!("id=persist,format=raw,file={persist_path},if=none"));
    cmd.arg("-device").arg("ide-hd,drive=persist,bus=ide.1,unit=0");

    let mut child = cmd.spawn().expect(
        "failed to start qemu-system-x86_64 -- install QEMU first (e.g. apt install qemu-system-x86 \
         on Linux, or the Windows build from qemu.org)",
    );
    let status = child.wait().expect("failed to wait on qemu");
    let code = match status.code().unwrap_or(1) {
        0x10 => 0, // kernel requested a clean exit
        0x11 => 1, // kernel panicked
        _ => 2,    // unexpected
    };
    exit(code);
}
