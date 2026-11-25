# `speaker`: a speaker on a DAC output or a GPIO

A speaker, behind a coupling capacitor and an amplifier. It plays one of:

- a channel of a [`dac8165`](dac8165.md), for firmware that streams audio
  samples to the DAC (from a timer, or DMA into a PIO program): the inverse
  of a [`wav`](wav.md) player;
- a [`pcm5102`](pcm5102.md) I2S audio DAC: its left or right channel, or
  both mixed;
- a GPIO, for firmware that plays audio as PWM or PDM on a pin (from a PWM
  slice, PIO or software), through the low-pass filter such a pin feeds on a
  real board.

## Keys

| Key | Type | Default | |
|-----|------|---------|---|
| `dac` | `PART[.CHANNEL]` | | the DAC it plays: `dac.A` on a `dac8165`; `pcm`, `pcm.L` or `pcm.R` on a `pcm5102` |
| `gpio` | GPIO | | the pin it plays |
| `volume` | 0..100 | `100` | the starting volume, in percent |
| `rate` | 1000..192000 | `48000` | on a GPIO: how often the pin's average level is taken, in Hz |
| `mhz` | 1..1000 | clk_sys | on a GPIO: core clock, to turn time into cycles |

A speaker takes either `dac` or `gpio`. `dac` must name a DAC on the board
or one of its shields: a `dac8165` with a channel `A` to `D`, or a `pcm5102`
alone (both channels mixed) or with `L` or `R`.

```toml
[parts.dac]
kind = "dac8165"
sdi = 4
cs = 5
clk = 6

[parts.spk]
kind = "speaker"
dac = "dac.A"
```

```toml
[parts.spk]
kind = "speaker"
gpio = 18
```

## What's heard

On a `dac8165`, the DAC's code, not the voltage at the jack: a swing across
the DAC's whole range (code 0 to 16383) is full scale at 100 % volume,
whatever `vref_mv` and the output stage are. On a `pcm5102`, its samples:
full scale is full scale.

On a GPIO, the pin's average level over each window of 1/`rate` seconds:
the fraction of the window it was high, from 0 to 1, with the pin's edges
timed to the cycle (PWM and PIO edges keep their own cycle). That is the
filter: a PWM carrier well above `rate` averages to its duty cycle, and the
duty cycle is the sample. A pin switching between 0 and 1 swings full scale.
When a window holds a whole number of PWM periods, the carrier leaves no
trace at all: [`groovebox-audio`](../../firmware/showcase/src/bin/groovebox-audio.rs)
plays one sample per period of a 25 kHz PWM, with `rate = 25000`.

Either way, each value is held until the next one, and a 20 Hz high-pass,
the coupling capacitor, takes out the DC level, so a signal biased to
mid-scale (a 50 % duty cycle) plays centred. Moving the DC level, as the DAC
does when it first leaves 0 or the PWM when it starts, makes a thump that
dies away in a few tens of milliseconds, as on a real board.

The speakers play through the test synth, mixed with its voices (see
[running.md](../running.md#test-synth)): in the window while **🔊 Sound** is
ticked in the top bar, natively and in the browser, and headless into
`--wav`, sample-exact in emulated time. A board with a speaker has no
default synth voice.

## Panel

A speaker icon with one wave per third of the volume. Click it to mute or
unmute it, and scroll over it to change the volume.

## Events

- `volume`: the volume heard, 0..100 (0 while muted), when it changes.
- `level`, on a GPIO: the pin's average level in the window just ended,
  0..65535, when it differs from the previous window's. A steady duty cycle,
  or a pin left alone, sends nothing.

## Actions

- `mute` and `unmute`.
- `volume` with a value from 0 to 100.

```sh
tartelette fw.elf --board my-board.toml --headless --wav out.wav --input spk:volume=50@0
```

## Not modelled

The sound is made at the output's rate (48 kHz for `--wav`, the sound
card's rate live) from the latest DAC code or pin level in each sample
period: anything faster is dropped, without an anti-aliasing filter. On a
GPIO, the window average is the only filter: a carrier near `rate` or below
it is heard. The DAC's settling time, the amplifier's limits and the
speaker's own response aren't modelled.
