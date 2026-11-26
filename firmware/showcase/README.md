# Showcase demos

The firmware on tartelette's demo page (`site/`, built by
`scripts/build-site.sh`). One crate, one binary per demo, all for a plain
RP2040 (no Wi-Fi, so they need no firmware blobs and run in the browser).
They are ordinary Embassy firmware: the same ELF runs on a real Pico wired
as the board file says.

| Demo        | Board | What it shows |
|-------------|-------|---------------|
| `groovebox` | [`boards/groovebox.toml`](boards/groovebox.toml): ST7789 screen, MCP23008 (gates and keys), DAC8165 (pitch CV), encoder | A 16-step drum machine and bass sequencer, heard through the test synth. Two async tasks; I2C, SPI with DMA, a PIO encoder, a bit-banged DAC |
| `groovebox-audio` | [`boards/groovebox-audio.toml`](boards/groovebox-audio.toml) + `pico-display-2`: a speaker on GPIO16 | A drum machine and bass synth making its own sound, on rings with a sweeping playhead. Drums and bass synthesized on the Pico and played as 25 kHz PWM, DMA paced by the PWM's wrap; the sequencer runs in the audio interrupt, to the sample |
| `stardust`  | `pico` + `pico-display-2` | An arcade game: fall around a star, collect stardust, dodge comets. Full-screen rendering at 25 fps |
| `lumen`     | [`boards/lumen.toml`](boards/lumen.toml): 64 WS2812 in an 8x8 grid, encoder, two buttons | Generative light: plasma, fire, rain, life, ripples, fireflies. PIO WS2812 with DMA, a PIO encoder |
| `spectra`   | [`boards/spectra.toml`](boards/spectra.toml) + `pico-display-2`: a WAV player (line in) and a pot (gain) on the ADC | A scope, spectrum analyser and waterfall of a song. ADC round robin with DMA, a fixed-point FFT, strip rendering |
| `picosh`    | `pico` | A shell on UART0: an ASCII Mandelbrot on one core and on both, `peek` at the bootrom, primes, the LED |
| `varvara`   | [`boards/varvara.toml`](boards/varvara.toml): ST7789 screen, D-pad, A, B, Select, Start, a speaker on GPIO16, an SD card on SPI1, a PS/2 mouse | A [Uxn/Varvara](https://wiki.xxiivv.com/site/varvara.html) virtual machine running a ROM built in or one from the card, picked from a menu: screen, controller, mouse, console on UART0, four-voice audio, files on the card. At 200 MHz, as the interpreter needs the cycles |

Each binary's `src/bin/*.rs` starts with its controls. `src/lib.rs` has
what the screen demos share: a strip renderer for a 320x240 ST7789 that
sends only the strips that changed, overlapping drawing with DMA.

`boards/picodexed.toml` is the board for a firmware that isn't built here:
[picodexed](https://github.com/diyelectromusic/picodexed), a DX7-style synth,
run from its own prebuilt UF2 (in the `build/` folder of its repository):
an SSD1306 OLED, a PCM5102 on PIO I2S with a speaker, MIDI in on UART1 with
a keyboard on the panel, and the voice knob. It overclocks to 250 MHz and
takes about five seconds to start.

`boards/spectra.wav` is the song `spectra` listens to: an 8-second loop made
for it (drums, bass, pads and an arpeggio), so it is free to use like the
rest of the repository.

## Varvara ROMs

`varvara` runs a Uxn ROM built into the firmware, or one from its SD card.
ROMs aren't kept in the repository: `roms/fetch.sh` downloads
[fourtette](https://github.com/nf/fourtette), a block game by nf
(Apache-2.0), to `roms/fourtette.rom`, which the build picks up, with its
licence. Another ROM
can be built in with `VARVARA_ROM=path/to/game.rom cargo build ...`. Without
a ROM the demo still builds, and runs from a card alone.

`roms/card.sh` makes `roms/card.img`, a card of 46 ROMs, which
`./run` puts in the slot when it's there. They are assembled from their
sources, downloaded at pinned commits (and an assembler, uxnasm, built from
uxn's with `cc`): fourtette,
[donsol](https://git.sr.ht/~rabbits/donsol) (a card game) and
[catclock](https://git.sr.ht/~rabbits/catclock) at the top, by Hundred
Rabbits (MIT), and [uxn](https://git.sr.ht/~rabbits/uxn)'s examples (MIT)
in `demos/`, `devices/`, `exercises/`, `gui/` and `software/`. Their licences are in `licences/` on the card. The
console ones (`exercises/`, most of `devices/`) print to UART0, which is the
terminal headless.

With a card in, the demo starts on a menu of the built-in ROM and the
card's directories and `.rom` files: Up and Down to choose, A (or Start, or
Right) to open, B (or Left) to go back up. Holding Select and Start for a
second goes back to the menu from a ROM, and so does a ROM halting. Without
a card, the built-in ROM starts straight away (after a second spent looking
for one), and Select and Start start it again, or the menu if a card has
been put in since.

Both File devices work on the card's first FAT volume, with the card's root
as the program's working directory. Files are found by their long or 8.3
names, regardless of case, and directory listings give long names where
there are any. `embedded-sdmmc` can't write long names, so a program that
makes a file whose name isn't 8.3 gets an 8.3 one made from it
(`file-output.txt` becomes `FILE~B7D.TXT`), which its own name still finds;
and deleting a file leaves its clusters allocated (`fsck.fat` gets them
back).

It suits ROMs whose screen fits 320x256: the panel shows 320x240 of it,
centred, and cuts off what is below. On the panel the D-pad is on the arrow
keys, A on X, B on Z, Select on Space and Start on Enter.

The Mouse device is a PS/2 mouse on GPIO20 (CLK) and GPIO21 (DATA), with its
wheel: on a real board, wire one through a level shifter, as PS/2 is 5 V. On
the panel, move the pointer across the Mouse pad, and click and scroll on it;
the pointer starts in the middle of the screen and stays on it. The firmware
sets the mouse up at boot and carries on without one if none answers. The emulator models no flash wait states, so on a real Pico, running
from XIP flash, the VM may be slower than it is here.

## Running

```sh
cd firmware/showcase
cargo run --release --bin groovebox
cargo run --release --bin stardust -- --headless --log --max-cycles 1000000000
```

`./run` (the cargo runner) picks each demo's board and hands over to
`../examples/run`, so `TARTELETTE_RUNNER=probe-rs` or `picotool` put it on
a real board instead, and options after `--` go to tartelette. `groovebox`
also gets its synth patch, `--drums --osc saw:dac.A:expander.bass`. Tick
**Sound** in the toolbar to hear it, and `groovebox-audio`'s speaker.

## On the web page

`scripts/build-site.sh` builds these, strips their debug info (keeping the
defmt symbols) and links each from the page as
`emu/?firmware=../demos/NAME.elf&board=...`; see the README's "In the
browser" for the parameters. The screenshots in `site/img/` are of the
web build.
