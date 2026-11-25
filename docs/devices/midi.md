# `midi`: a MIDI in jack

Where MIDI from the host enters the board: a DIN or TRS jack behind its
optocoupler. The jack feeds either a UART's receiver, or a GPIO, for
firmware that reads MIDI with a PIO program or a software UART on any pin.
The toolbar's MIDI inputs feed it (see
[running.md](../running.md#midi-in)).

## Keys

| Key | Type | Default | |
|-----|------|---------|---|
| `uart` | 0..1 | | the UART whose RX the jack feeds |
| `gpio` | GPIO | | the pin the jack feeds, as serial |
| `mhz` | 1..1000 | clk_sys | on a GPIO: core clock, to time the bits |

A jack takes either `uart` or `gpio`.

```toml
[parts.din]
kind = "midi"
gpio = 5
```

## What's modelled

On a UART, the bytes go to its receiver as they would from the jack, at the
UART's baud rate. As on the chip, bytes that arrive while the receiver is
off are lost.

On a GPIO, the jack drives the pin as the optocoupler's output would:
31250 baud, 8N1, idle high, a bit every 32 µs. Bytes that
arrive while one is on its way wait their turn (up to 1024, then the oldest
go), so each byte follows the last one's stop bit.

## Panel

Where the jack goes (`MIDI in → GPIO5`), with a light that flashes as bytes
go down its pin. With the panel option `keyboard = "C3"` (a note name or
number), piano keys from that note fill the rest of the item: hold one to
play it into the jack (note on, then note off when you let go), on MIDI
channel 1, or `channel = 2..16`.

## Events

`sent`, with each byte as it starts on the pin.

## Actions

`byte` with a value from 0 to 255 sends a byte into the jack: down its pin,
or to its UART's receiver. It lets headless runs send MIDI:

```sh
tartelette fw.elf --board my-board.toml --headless \
    --input din:byte=0x90@1000 --input din:byte=60@1000 --input din:byte=100@1000
```

## Not modelled

The optocoupler's rise and fall times, and MIDI out.
