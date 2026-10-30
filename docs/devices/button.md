# `button`: a push button

A momentary switch between a GPIO and GND (the default) or 3V3. When it's
released the pin is left floating, so the firmware needs the matching pull-up
or pull-down, as it would on the real board.

## Keys

| Key | Type | Default | |
|-----|------|---------|---|
| `gpio` | GPIO | required | the pin it pulls |
| `active` | `high` \| `low` | `low` | the level it pulls the pin to when pressed |

```toml
[parts.a]
kind = "button"
gpio = 12

[panel]
a = { at = [0, 4], label = "A", key = "A" }
```

## Panel

A round button, held for as long as the mouse button is down. `key` binds a
keyboard key (egui names: `A`, `1`, `Space`, `Enter`, ...).

## Events

`pressed` / `released`.

## Actions

`press` and `release`. Headless, the shortcut is `--press NAME@US[+HOLD]`:

```sh
tartelette fw.elf --board pico-w --shield pico-display-2 --headless \
    --press a@500000+100000      # press A at 0.5 s, release at 0.6 s
```

Contact bounce isn't modelled.
