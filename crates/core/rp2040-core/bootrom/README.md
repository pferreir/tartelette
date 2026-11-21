# RP2040 bootrom (B2)

`b2.bin` is the 16 KiB bootrom image of the RP2040 B2 silicon revision,
extracted from the `b2.elf` release asset of
<https://github.com/raspberrypi/pico-bootrom-rp2040/releases/tag/b2>
(the single PT_LOAD segment at 0x0). It is mapped at address 0 so ROM
functions (memcpy, soft-float, ...) and the function tables work.

Licensed under the BSD-3-Clause terms in `LICENSE.TXT`, with the exception
stated there for the files `mufplib.S` / `mufplib-double.S` (the floating
point routines), which are licensed separately by their author.

SHA-256 of `b2.bin`: `9c19b46f068c21f90d200c514faad4a0d5cecfc978f155b8c9d25cb6bc2efd81`
