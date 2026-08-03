PHDRS
{
    seg1 PT_LOAD FLAGS(7); /* PF_R | PF_W | PF_X -- one segment: code + rodata + the
                              process's own writable statics (heap marker readback
                              buffer) all together; this milestone doesn't need to
                              re-prove multi-segment loading (Milestone 36 already did) */
}

ENTRY(_start)

SECTIONS
{
    . = 0x0000555550000000;
    .text : {
        KEEP(*(.text.start))
        *(.text*)
    } :seg1

    .rodata : {
        *(.rodata*)
        *(.lrodata*)
    } :seg1

    .data : {
        *(.data*)
    } :seg1

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note*)
        *(.comment)
        *(.got*)
        *(.rela*)
        *(.debug*)
        *(.bss*)
    }
}
