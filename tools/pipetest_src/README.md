Build recipe for `kernel/assets/pipetest.elf`, Milestone 40's real, externally-built
ELF64 test executable exercising `pipe()`/`dup2()` (single genuine `PT_LOAD`
segment, built with this project's own pinned Rust nightly toolchain + `rust-lld`,
not hand-assembled).

```
rustc --target x86_64-unknown-none -C link-arg=-Ttools/pipetest_src/linker.ld \
    -C link-arg=-zmax-page-size=16 -C code-model=large -C panic=abort \
    --crate-type bin tools/pipetest_src/pipetest.rs -o kernel/assets/pipetest.elf
```

Real bug found and fixed while building this: the first attempt (same recipe as
`testelf_src`'s own README, minus `-zmax-page-size=16`) produced a 7016-byte file
that failed `seedpipetest` with a real, honest error from the kernel itself --
`fs::MAX_FILE_BYTES` caps on-disk files at 4096 bytes, and this build blew past it.
Root cause, confirmed via `readelf -S`: the default linker page-size alignment
left a 4096-byte gap of pure padding between the ELF/program headers and the
first real section (`.text.start` landing at file offset `0x1000` instead of
immediately after the headers). `-zmax-page-size=16` removes that padding --
same fix already needed once elsewhere in this project's own toolchain work for
the identical byte-cap collision. Rebuilding after the fix produced a real
3048-byte file, comfortably under the cap. Verify with `readelf -S
kernel/assets/pipetest.elf` after rebuilding -- `.text.start`'s file offset
should land right after the program headers, not at a page boundary.
