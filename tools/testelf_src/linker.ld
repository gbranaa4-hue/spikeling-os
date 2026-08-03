PHDRS
{
    seg1 PT_LOAD FLAGS(5); /* PF_R | PF_X -- segment 1: _start only */
    seg2 PT_LOAD FLAGS(5); /* PF_R | PF_X -- segment 2: seg2_entry code + MESSAGE data */
}

ENTRY(_start)

SECTIONS
{
    . = 0x0000555550000000;
    .text.start : {
        KEEP(*(.text.start))
    } :seg1

    . = 0x0000555550001000;
    .text.seg2seg : {
        *(.text.seg2)
        *(.rodata.seg2)
        *(.lrodata*)
    } :seg2

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
