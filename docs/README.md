# Documentation

tartelette runs RP2040 firmware against an emulated board. These pages cover
how to run it, how to describe a board, and what each emulated part does.

- [Running tartelette](running.md): the command line, headless runs,
  SD cards, flash, faults and the test synth
- [Board files](boards.md): describing a PCB, the front panel and shields
- [Devices](devices/): one page per part kind, with its board-file keys,
  panel widget, events and actions
- [The RP2040 model](rp2040.md): the CPU and each on-chip peripheral
- [USB](usb.md): the emulated host, the console and the bridges
- [Development](development.md): the crates, tests, the web build and
  PGO builds
- [Firmware](../firmware/README.md): the example and test firmware
