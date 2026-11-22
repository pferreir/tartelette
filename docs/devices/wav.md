# `wav`: a WAV file player

Plays a WAV file as an analog signal on a pin, for the ADC to sample: a
line-level signal biased to mid-rail, like the output of a microphone preamp
or the coupling network on an audio input. Only GPIO26 to 29 reach the ADC
(AIN0 to AIN3). The player sets the pin's analog level only: the pin's
digital level stays whatever pulls or other parts make it.

## Keys

| Key | Type | Default | |
|-----|------|---------|---|
| `gpio` | GPIO | required | the pin the signal is on |
| `file` | path | | the WAV file to load, relative to the board file |
| `bias` | 0..3300 | `1650` | the level of silence, in millivolts |
| `gain` | 0..10000 | `1650` | how far a full-scale sample swings from `bias`, in millivolts |
| `channel` | `left` \| `right` \| `mix` | `mix` | the channel of a stereo file to play |
| `loop` | `off` \| `on` | `off` | start over at the end of the file |
| `autoplay` | `off` \| `on` | `off` | play from power-up, and each file as it is loaded |
| `mhz` | 1..1000 | `125` | core clock, to turn time into cycles |

```toml
[parts.mic]
kind = "wav"
gpio = 26
file = "drums.wav"

[panel]
mic = { at = [0, 0], span = [2, 1], label = "Input" }
```

A sample `s` between -1 and 1 puts `bias + gain × s` on the pin, so the
defaults swing a full-scale file across the ADC's whole 0 to 3.3 V range.
The ADC clips anything outside that range. While the player is stopped, the
pin sits at `bias`.

The file can be 8-, 16-, 24- or 32-bit PCM, or 32-bit float, at any sample
rate and with any number of channels. `channel` picks the first or second
channel, or their average. The player keeps time with the emulated clock, and
the level between two samples is interpolated. That means the ADC can sample
at any rate it likes, and a run gives the same samples however it is stepped.

`file` is read when the run starts. Natively it is resolved from the board or
shield file that names it, or from the working directory for a built-in
board. In the browser, a page whose `board=` is a URL fetches it relative to
that URL (see the README's "In the browser"); a board opened from disk there
can't reach its files, so load the WAV from the panel. A file can also be
loaded from the panel's **Load** button, or by dropping a `.wav` file on the
window (it goes to the first player). A file loaded that way replaces the
board's, and is kept across restarts.

## Panel

The file's name, a bar showing the position, and the buttons rewind,
play/stop and **Load**, followed by the position and length.

## Events

`playing`: 1 when it starts, 0 when it stops or reaches the end of a file
that doesn't loop.

## Actions

- `play` plays from where it stopped. At the end of a file that doesn't
  loop, it plays from the start.
- `stop` pauses where it is.
- `rewind` goes back to the start, and keeps playing if it was.

Without `autoplay`, the player starts out stopped. Headless, start it with
`--input`:

```sh
tartelette fw.elf --board my-board.toml --headless --input mic:play@0
```

There is no seeking, and no resampling filter: the interpolation is linear.
