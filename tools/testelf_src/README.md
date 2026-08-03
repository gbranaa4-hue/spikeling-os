Build recipe for `kernel/assets/testelf.elf`, Milestone 36's real, externally-built
ELF64 test executable (two genuine `PT_LOAD` segments, built with this project's
own pinned Rust nightly toolchain + `rust-lld`, not hand-assembled).

```
rustc --target x86_64-unknown-none -C link-arg=-Ttestelf_src/linker.ld \
    -C code-model=large -C panic=abort --crate-type bin \
    testelf_src/testelf.rs -o kernel/assets/testelf.elf
```

`linker.ld` forces two separate `PT_LOAD` segments via a `PHDRS` block so the
kernel's ELF loader has something real to exercise beyond a single trivial
segment -- segment 1 (`_start`, at `USER_CODE_ADDR`) does a real linker-resolved
cross-page `call` into segment 2 (a different page), which performs the
write+exit syscalls and holds the distinguishing message. Verify with `readelf
-l kernel/assets/testelf.elf` after rebuilding.
