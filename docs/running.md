# Running tartelette

```
tartelette [OPTIONS] [FIRMWARE]
tartelette mkdisk | parts | schema
```

`tartelette --help` is the complete reference; this page explains how the
options fit together.

## Firmware

The firmware is an ELF or a UF2 (the file you'd drag onto a Pico in BOOTSEL
mode). Prefer the ELF when you have it: tartelette needs its symbols to find
the defmt RTT buffer, so a UF2 runs without a log.

Core 0 does not run boot2: the image is loaded straight into flash and SRAM,
and core 0 starts at the program's vector table as if boot2 had already set up
XIP. Core 1 does run the real B2 bootrom and waits there for the launch
handshake, as on the chip. Both pico-sdk style images (boot2 in the first 256
bytes) and Embassy/rp2040-hal images work.

## Boards and shields

```sh
tartelette fw.elf --board btl-01
tartelette fw.elf --board pico-w --shield pico-display-2
tartelette fw.elf --board ./my-board.toml --shield ./my-shield.toml
```

`--board` takes a built-in name (`pico`, `pico-w`, `btl-01`) or a path. Without
it you get a plain Pico, which is just the LED on GPIO25. `--shield` stacks an
add-on board on top, and can be repeated. See [boards.md](boards.md) for
writing your own.

## The window

The top bar opens a firmware or board, pauses, resets and sets the speed,
and under that shows the files, the emulated time and the speed reached. When
something goes wrong it shows **⚠ N new problems**, which opens the Problems
tab. The front panel in the middle is laid out by the board file's `[panel]`
section. The tabs at the bottom (**→ Dock right** moves them to the right,
and the edge between them and the panel resizes them) are:

- **UART0** and **UART1**, with a line to send text back (and a choice of line
  ending; it waits until the firmware turns the UART's receiver on);
- **USB serial**, the emulated device's CDC-ACM port, if it has one;
- **USB**, the traffic log: enumeration, decoded control transfers, packets;
- **Log**, the firmware's defmt output;
- **Synth**, the test synth's patch bay (see [below](#test-synth));
- **SD *name***, a card's browser, opened by **Browse…** on its widget and
  closed with its **×**;
- **Problems**: errors starting the run, ignored panel options, and warnings
  from the run (faults, bus wiring, an expander's pins).

Firmware, board and shield files are watched; save one and tartelette reloads
it and starts over.

## Headless

`--headless` runs without a window, as fast as it can unless `--speed` paces
it (`--speed 1` is real time; useful when the firmware talks to a real network
or Bluetooth device).

Output:

| Stream | Goes to | Change with |
|--------|---------|-------------|
| UART0 | stdout (none if bridged) | `--uart0 stdout\|stderr\|none\|FILE` |
| UART1 | stderr (none if bridged) | `--uart1 ...` |
| USB serial | stdout (and stdin to it) | `--usb-bridge serial` |
| board events | stderr | |
| defmt log | stderr | `--log` |
| USB traffic | stderr | `--usb-log` |

When MicroPython's REPL is on the USB serial port and something else prints on
UART0, `--uart0 FILE` keeps the two apart.

Stdin goes to the USB serial port; `--stdin uart0` (or `uart1`) types it into
a UART instead. The bytes wait until the firmware turns the UART's receiver
on, then arrive at its baud rate, and the run doesn't end on the firmware
falling asleep while stdin is open or bytes are still on their way:

```sh
echo 'help' | tartelette fw.elf --headless --stdin uart0
```

Board events are printed with the emulated time they happened at, the part's
name and the event, with its value when it carries one:

```text
[   14600.0 us] dac dac=147455
[ 1001200.0 us] knob detent=1
[ 1200000.0 us] enc_sw pressed
```

Each device's page in [devices/](devices/) lists the events it emits.

### Stopping

`--max-cycles N` stops after N cycles. The clock is fixed at 125 MHz, so one
second is 125000000 cycles. Without it, a headless run goes on until you
interrupt it or the firmware executes a `bkpt`.

### Scripting inputs

```sh
--press NAME@US[+HOLD]          # press button NAME at US µs, release HOLD µs later
--input NAME:ACTION[=VALUE]@US  # any device action
```

For example `--press enc_sw@1000000+50000` holds the BTL-01's encoder switch
for 50 ms after one second, `--input knob:turn=3@2000000` turns the knob three
detents clockwise and `--input expander:gp3=0@500` pulls pin 3 of the port
expander low. The actions each device takes are on its page.

### Capturing output

- `--dump-screen NAME=FILE` saves display NAME at the end of the run, as PNG
  or PPM depending on the extension.
- `--dump-paper NAME=FILE` saves what the mock cat printer printed (with `--bt
  mock`), PNG or PBM.
- `--wav FILE` records the test synth (see below).
- `--wifi-trace NAME` prints the last 1024 bus transactions with the Wi-Fi
  chip at the end, for debugging drivers.

## Flash

The flash chip is 2 MiB and starts erased with the firmware on top. With
`--flash FILE` (headless) it is kept in a file instead: the run starts from
the file's contents with the firmware written over them, and the file is
saved at the end if the firmware wrote to flash. That is how MicroPython's
filesystem survives from one run to the next. The file is created if it
doesn't exist.

## SD cards

```sh
tartelette mkdisk card.img --dir data
tartelette os.elf --board btl-01 --sd sd=card.img
```

`mkdisk` makes a sparse image with an MBR and one empty FAT32 partition
starting at 1 MiB, the way `embedded-sdmmc` likes it. `--size-mb` sets the
size (default 64 MiB). FAT32 needs at least 34 MiB, because readers tell it
from FAT16 by the cluster count alone; `--fat 16` makes FAT16 instead, from
4 MiB up to 2000. `--dir` adds empty top-level directories; `--label` sets
the volume label.

`--sd NAME=FILE` loads an image into the `sdcard` part called NAME; without
it, the slot is empty (unless the board gives the part a `size_mb`). The image
lives in memory while the firmware runs. Headless, it is written back at the
end if the firmware changed it; with a window, use the card's **Save…**
button.

To look inside a card while the firmware runs:

- **Browse…** on the card's widget (native and web) opens a copy of the card
  in a tab and shows its files, with a text or hex preview and **Save file…**. It reads
  FAT12, FAT16 and FAT32, whole-disk or in an MBR partition. **Refresh** takes
  a new copy.
- `--sd-live` (native) writes every block to the `--sd` file as the firmware
  writes it, so host tools can read the card while it runs:

  ```sh
  mdir -/ -i card.img@@1M ::
  mtype -i card.img@@1M ::/LOG.TXT
  fsck.fat -n card.img
  ```

  (`@@1M` is the partition offset `mkdisk` uses.)

Either way you see what is on the card, not what the firmware still has in
its buffers. A copy taken in the middle of a write can be inconsistent; the
browser reports that as an error.

## Faults

An unaligned access, an undefined instruction, a branch to an ARM-state
address or a bad exception return is reported as a warning naming the
instruction and the reason, and then the firmware's
HardFault handler runs, as on the chip. `--stop-on-fault` stops right there
instead, before the handler gets a chance to reset or spin.

Headless, a run that stops on a fault, or on a `bkpt` inside a handler (which
is what pico-sdk's fault handlers do), ends by printing the core's registers,
the exception frame and the cause.

## Network and Bluetooth

These only matter for boards with a `cyw43439` (the Pico W). See
[devices/cyw43439.md](devices/cyw43439.md).

```sh
--net user|loopback|none   # default user: the host's network through a NAT
--forward 8080:80          # host 127.0.0.1:8080 -> guest port 80
--bt none|mock|hci0        # default none
```

## UART bridges

On Linux, `--uart-bridge 0` (or `1`, or `0,1`) hands a UART to the host as a
pseudo-terminal, as if a USB-serial adapter were wired to its pins: terminal
programs, pyserial and the like open it. The firmware's baud rate paces the
line, whatever the terminal is set to, and a program writing faster than that
is held back by the terminal. What the firmware sends while nobody has it open
is dropped, as with a real port. `--uart-tty-link 0=/tmp/ttyUART0` adds a
symlink with a fixed name. It works with a window (the console tab says where
the UART went) and headless.

```sh
tartelette fw.elf --uart-bridge 0 --uart-tty-link 0=/tmp/ttyUART0
picocom -b 115200 /tmp/ttyUART0
```

## USB

See [usb.md](usb.md).

```sh
--usb-bridge serial,hid,midi
--usb-tty-link /tmp/ttyACM
--usb-log
```

## Test synth

If the board has CV and gate outputs (a `dac8165`, and `mcp23008` pins or
LEDs used as gates), tartelette can play them through test voices so you hear
what a sequencer is doing:

- oscillators (`sine`, `triangle`, `saw`, `square`) follow a DAC channel at
  1 V/octave, with 0 V at C4, and sound while their gate is high;
- drums (`kick`, `snare`, `hat`, `openhat`, `clap`, all synthesized) play a hit
  on each rising edge of their gate, however short the trigger.

In the window, the **Synth** tab is the patch bay. Tick **Sound** to
play through the sound card (native and web) and **Drum kit** to put kick,
snare and hats on the first four gates. Each voice shows its gate, note and
pitch.

On the command line:

```sh
--osc WAVE[:CV[:GATE]]   # e.g. saw:dac.A:expander.0
--osc DRUM[:GATE]        # e.g. kick:expander.0
--drums                  # kick, snare, closed and open hat on the first four gates
--wav out.wav            # headless: record the voices, 48 kHz mono
```

CV is a DAC channel (`dac.A` .. `dac.D`). GATE is an expander pin, by
number or name (`expander.0`, `expander.gate_a`), an LED part (`clk_out`), or `open` for always on. Left out,
they are the board's first DAC channel and first gate. Headless, the WAV is
rendered in emulated time and is sample-exact, however fast or slow the run
was:

```sh
tartelette os.elf --board btl-01 --headless --max-cycles 1250000000 \
    --osc saw:dac.A:expander.0 --osc square:dac.B:expander.1 --wav out.wav
```

The voltage at each jack comes from the DAC part's output-stage settings; see
[devices/dac8165.md](devices/dac8165.md).

## Other subcommands

- `tartelette parts` lists the device kinds; `tartelette parts KIND` lists the
  keys a part of that kind takes, with types and defaults.
- `tartelette schema` prints the JSON schema for board files.
- `tartelette mkdisk` is described above.
