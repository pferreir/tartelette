# `encoder`: a rotary encoder

A mechanical rotary encoder: two contacts, A (CLK) and B (DT), each closing to
GND, producing a quadrature signal as the knob turns. Open contacts float, so
the firmware needs pull-ups, as with the real part. The push switch that
most encoders have is a separate [`button`](button.md).

One detent is a full quadrature cycle, four transitions `step_us` apart:

```text
clockwise:          CLK leads    (CLK,DT) 11 -> 01 -> 00 -> 10 -> 11
counter-clockwise:  DT leads     (CLK,DT) 11 -> 10 -> 00 -> 01 -> 11
```

Parts disagree on which contact leads clockwise; `cw = "dt"` swaps them.

## Keys

| Key | Type | Default | |
|-----|------|---------|---|
| `clk` | GPIO | required | contact A |
| `dt` | GPIO | required | contact B |
| `cw` | `clk` \| `dt` | `clk` | the contact that leads when turning clockwise |
| `step_us` | 1..1000000 | 300 | time between transitions, in µs |
| `mhz` | 1..1000 | 125 | core clock |

```toml
[parts.knob]
kind = "encoder"
clk = 22
dt = 21
cw = "dt"

[panel]
knob = { at = [4, 0], span = [2, 2], keys = ["Left", "Right"] }
```

## Panel

A knob. Drag it round, scroll over it, or use the two `keys`
(counter-clockwise, then clockwise).

## Events

`detent` with +1 or -1 each time the knob completes a cycle and lands on a
new detent.

## Actions

`turn` with a number of detents, positive for clockwise:

```sh
--input knob:turn=3@2000000      # three clicks clockwise at 2 s
--input knob:turn=-1@2500000     # one back
```

Steps are queued and played out at `step_us`, so a fast turn takes as long as
it would by hand. Turning the other way cancels steps still queued from the
last turn.

Contact bounce isn't modelled.
