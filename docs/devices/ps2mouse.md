# `ps2mouse`: a PS/2 mouse

A PS/2 mouse on two open-collector lines, CLK and DATA. The mouse pulls a line
low or lets it float, so the firmware needs pull-ups (the internal ones do),
and drives the lines open-drain too: switch the pin between input and output
low, as PIO programs do with `pindirs`. A line the chip drives high can't be
pulled low by the mouse, and tartelette warns once when that happens.

The mouse makes the clock in both directions, at 12.5 kHz (40 µs low, 40 µs
high). Each byte is an 11-bit frame: a start bit (0), eight data bits LSB
first, odd parity and a stop bit (1).

```text
mouse to host: DATA changes while CLK is high; the host samples it as CLK falls
host to mouse: the host holds CLK low (100 µs), pulls DATA low and lets CLK go;
               the mouse reads each bit as CLK rises, then holds DATA low for
               one more pulse to acknowledge
```

Holding CLK low inhibits the mouse. A byte cut off before its eleventh clock
pulse is sent again once CLK is released, unless the host sends a command,
which empties the mouse's output. The mouse waits 100 µs after CLK is released,
and between the bytes it sends.

It answers the mouse command set: reset (`FF`: `FA`, then `AA 00` after a
300 ms self-test, which it also runs at power-up), resend (`FE`), set defaults
(`F6`), enable and disable reporting (`F4`, `F5`), sample rate (`F3`), device
ID (`F2`), remote and stream mode (`F0`, `EA`, read data `EB`), wrap mode
(`EE`, `EC`), status (`E9`), resolution (`E8`) and 2:1 scaling (`E7`, `E6`).
Anything else, a bad argument or a bad parity bit gets `FE`. It starts in
stream mode with reporting off, so the firmware has to send `F4` first.

The `wheel` model becomes an IntelliMouse (ID 3, a fourth byte for the wheel)
after the sample rates 200, 100, 80; the `explorer` model also becomes ID 4
(wheel and buttons 4 and 5 in the fourth byte) after 200, 200, 80.

## Keys

| Key | Type | Default | |
|-----|------|---------|---|
| `clk` | GPIO | required | the clock line |
| `data` | GPIO | required | the data line |
| `model` | `basic` \| `wheel` \| `explorer` | `wheel` | three buttons; a wheel (IntelliMouse); a wheel and five buttons (Explorer) |
| `mhz` | 1..1000 | clk_sys | core clock |

```toml
[parts.mouse]
kind = "ps2mouse"
clk = 14
data = 15

[panel]
mouse = { at = [4, 0], span = [2, 2] }
```

## Panel

A pad. Click it to capture the pointer: the cursor is hidden and held in the
window, and your mouse drives the emulated one, a count for each unit it moves
(about a pixel), however far it goes. Its buttons (left, middle and right,
and back and forward on an `explorer`) and its wheel work anywhere in the
window meanwhile. Escape lets go, and so does leaving the window. In a browser
this is the Pointer Lock API, which may ask you first; Escape is the
browser's. Under the pad it says whether the firmware has turned reporting
on, and the ID it has switched the mouse to.

## Events

`received` with each byte the host sends.

## Actions

All in screen terms:

| Action | Value |
|--------|-------|
| `move_x` | counts, right is positive |
| `move_y` | counts, down is positive |
| `scroll` | wheel notches, up (away from you) is positive |
| `press`, `release` | a button: 0 left, 1 right, 2 middle, 3 back, 4 forward |

```sh
--input mouse:move_x=40@2000000 --input mouse:move_y=-10@2000000
--input mouse:press=0@2100000 --input mouse:release=0@2200000
```

Counts are at the default resolution, 4 per mm: at 8 per mm the mouse reports
twice as many, at 2 or 1 per mm half or a quarter, keeping the remainder for
the next packet. Motion and buttons build up between packets, which go no
faster than the sample rate, and only when something changed. Movements past
±255 counts in a packet are clipped, with the overflow bit set. Without the
wheel turned on, `scroll` does nothing.

The 5 V supply and the timing tolerances of real mice aren't modelled; the
clock always runs at the same rate.
