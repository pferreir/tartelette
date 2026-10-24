MEMORY
{
    FLASH : ORIGIN = 0x10000000, LENGTH = 2M
    RAM   : ORIGIN = 0x20000000, LENGTH = 256K
}

ENTRY(reset)

SECTIONS
{
    .boot2 ORIGIN(FLASH) : { KEEP(*(.boot2)) } > FLASH
    .vectors : { KEEP(*(.vectors)) } > FLASH
    .text : { *(.text .text.*) } > FLASH
    .rodata : { *(.rodata .rodata.*) . = ALIGN(4); } > FLASH
    .data : { _sdata = .; *(.data .data.*) . = ALIGN(4); _edata = .; } > RAM AT > FLASH
    _sidata = LOADADDR(.data);
    .bss (NOLOAD) : { _sbss = .; *(.bss .bss.*) *(COMMON) . = ALIGN(4); _ebss = .; } > RAM
    /DISCARD/ : { *(.ARM.exidx*) *(.eh_frame*) }
}

_stack_top = ORIGIN(RAM) + LENGTH(RAM);
