# `led`: an LED

An LED on a GPIO, through a resistor to GND (lit when the pin is high) or to
3V3 with `active = "low"` (lit when the pin is low).

The LED only watches its pin, so it can share it with another part. That
makes it handy for things that aren't LEDs at all: an activity line, a clock
or trigger output you want to see blink on the panel, or a gate for the [test synth](../running.md#test-synth).

## Keys

| Key | Type | Default | |
|-----|------|---------|---|
| `gpio` | GPIO, shareable | required | the pin |
| `active` | `high` \| `low` | `high` | the level that lights it |

```toml
[parts.status]
kind = "led"
gpio = 25

[panel]
status = { at = [0, 0], label = "LED", color = "green" }
```

## Panel

A lamp. `color` is one of `red` (the default), `green`, `blue`, `yellow`,
`orange`, `white`, or six hex digits like `"ff8800"` (no `#`, which would
start a TOML comment).

## Events

`on` / `off` as the LED changes state.

## Actions

None.
