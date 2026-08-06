PHDRS
{
    seg1 PT_LOAD FLAGS(5); /* PF_R | PF_X */
}

ENTRY(_start)

SECTIONS
{
    . = 0x0000555550000000;
    .text.start : {
        KEEP(*(.text.start))
        *(.text*)
        *(.rodata*)
    } :seg1

    /DISCARD/ : {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note*)
        *(.comment)
        *(.got*)
        *(.rela*)
        *(.debug*)
    }
}
