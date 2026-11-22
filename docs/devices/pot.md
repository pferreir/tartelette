# `pot`: a potentiometer

A potentiometer wired as a voltage divider between GND and 3V3, with its
wiper on a pin, for the ADC to read. Only GPIO26 to 29 reach the ADC (AIN0 to
AIN3). The pot sets the pin's analog level only: the pin's digital level
stays whatever pulls or other parts make it.

## Keys

| Key | Type | Default | |
|-----|------|---------|---|
| `gpio` | GPIO | required | the pin the wiper is on |
| `value` | 0..1000 | `500` | the wiper's starting position, in per mille of the travel |

```toml
[parts.knob]
kind = "pot"
gpio = 26

[panel]
knob = { at = [0, 0], label = "Level" }
```

## Panel

A knob with a 270° travel. Drag it up or right to turn it up, or scroll
over it. The wiper's voltage is shown below the knob.

## Events

`value`, with the new position (0..1000).

## Actions

`set`, with the position in per mille:

```sh
tartelette fw.elf --board my-board.toml --headless \
    --input knob:set=750@500000      # three quarters (2.475 V) at 0.5 s
```

The track is ideal: linear, with no wiper resistance or noise.
