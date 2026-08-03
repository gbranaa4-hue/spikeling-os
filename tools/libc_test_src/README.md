Build recipe for `kernel/assets/libctest.elf`, Milestone 39's real minimal-libc
test program -- the first spikeling-os user program built as ordinary Rust
calling real syscall wrappers (`libc.rs`), rather than hand-assembled `int 0x80`
machine code (every prior test program did that).

```
rustc --target x86_64-unknown-none -C link-arg=-Tlinker.ld \
    -C link-arg=--gc-sections -C link-arg=-znoseparate-code \
    -C link-arg=-zmax-page-size=16 \
    -C code-model=large -C relocation-model=static \
    -C panic=abort -C opt-level=z --crate-type bin \
    main.rs -o kernel/assets/libctest.elf
```

Notes on the flags, each earning its place for a real reason (not copied
blind from `testelf_src`'s recipe, which didn't need most of these since it
had no separate module to strip and no dynamic-linking overhead to avoid):
- `--gc-sections`: without it, `libc.rs`'s unused syscall wrappers (this
  program only exercises write/sbrk/fork/wait/exit, not open/read/fdwrite/
  close/exec) stay linked in dead, alone pushing the file well past
  `fs.rs`'s 4096-byte cap.
- `relocation-model=static`: without it, this build defaults to a `DYN`
  (position-independent/shared-object-style) ELF, pulling in real dynamic-
  linking metadata (`.dynsym`/`.hash`/`.dynstr`/`.dynamic`) this kernel's
  loader has no use for and never asked for -- confirmed the difference
  by comparing `readelf -h` against `testelf.elf` (`EXEC`, not `DYN`) and
  matching that.
- `-zmax-page-size=16`: same real reason `testelf_src`'s own recipe needed
  it -- without it, the linker pads the file to its default (much larger)
  page-size alignment even for one segment, wasting most of the file as
  padding this kernel's own byte-copying loader (`process.rs`'s
  `create_process_from_elf()`, which never mmaps the file) doesn't need.
- `opt-level=z`: minimizes code size on top of the above; the actual
  syscall-wrapper functions are tiny, so this mostly avoids incidental
  bloat from unoptimized debug codegen.

Final size: 1064 bytes (well under the 4096-byte on-disk file cap), `EXEC`
type, entry point `0x555550000000` (== `usertest::USER_CODE_ADDR`, required
exactly), one `RWE` `PT_LOAD` segment. Verify with `readelf -h`/`readelf -l`
after rebuilding.
