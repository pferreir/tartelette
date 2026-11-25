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
