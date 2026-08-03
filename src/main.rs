use ovmf_prebuilt::{Arch, FileType, Prebuilt, Source};
use std::env;
use std::process::{Command, exit};

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
