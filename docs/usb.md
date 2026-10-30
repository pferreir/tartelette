# USB

The RP2040's USB controller is emulated in device mode, and tartelette plugs a
small USB host into the other end of the cable. The host enumerates the
device the way a PC does and then talks to the functions it recognises:

- CDC-ACM serial ports;
- HID interfaces (keyboards, mice, anything with a report descriptor);
- USB-MIDI.

Unmodified TinyUSB (pico-sdk, MicroPython), embassy-usb and usb-device
(rp2040-hal) firmware works.

## How it's modelled

The controller (`rp2040-core/src/periph/usb.rs`) follows section 4.1 of the
datasheet: the registers, the 4 KiB dual-port RAM, the buffer control words
(AVAILABLE, FULL, PID, LENGTH, STALL, double buffering and its buffer
selector), BUFF_STATUS and the SIE_STATUS interrupts.

The host (`crates/usbhost`) speaks to it one transaction at a time: SETUP, IN
and OUT tokens, answered with data, ACK, NAK or STALL from the endpoint's
buffer control word. Each transaction takes as long as it would on a
full-speed bus. Like everything else, it runs on emulated time and is
deterministic.

Electrical detail (line states, the PHY, suspend and resume) and host mode
are not modelled.

## Without a bridge

With nothing else set, the serial port shows up in the console's **USB
serial** tab, and the **USB** tab logs the traffic: enumeration, every control
transfer decoded, every packet on the other endpoints. Both work in the
browser too.

Headless, the serial port is on stdin and stdout, and `--usb-log` prints the
traffic log to stderr.

```sh
tartelette micropython.uf2 --headless     # MicroPython's REPL in the terminal
```

## Bridges

`--usb-bridge` hands the device's functions to the computer tartelette runs
on. It takes a comma-separated list:

| Bridge | The function becomes | Platforms |
|--------|---------------------|-----------|
| `serial` | a pseudo-terminal, like `/dev/ttyACM0`. DTR follows whether a program has it open. `--usb-tty-link PATH` adds a symlink with a stable name | Linux |
| `hid` | a kernel input device through `/dev/uhid`: an emulated keyboard types into your desktop | Linux, needs access to `/dev/uhid` (root or a udev rule) |
| `midi` | virtual MIDI ports, `tartelette: USB MIDI out` and `tartelette: USB MIDI in` | Linux (ALSA), macOS (CoreMIDI) |

On Windows and in the browser (WebMIDI), the USB tab can connect the MIDI
interface to existing MIDI ports instead.

```sh
# MicroPython over a real serial port
tartelette micropython.uf2 --usb-bridge serial --usb-tty-link /tmp/ttyACM
mpremote connect /tmp/ttyACM

# A MIDI device, in real time, with its traffic logged
tartelette synth.elf --usb-bridge midi --usb-log --headless --speed 1
```

When bridging to programs that expect a real device's timing, run with
`--speed 1` so the firmware doesn't run ahead of the wall clock.

## Example firmware

`firmware/examples/pico-usb` (embassy-usb) and `firmware/examples/pico-usb-hal`
(rp2040-hal with usb-device) are the same composite device: a serial port
that echoes in upper case, a boot keyboard, and a MIDI interface that echoes
notes an octave up. The end-to-end tests use both.

`scripts/micropython-tests.sh` runs MicroPython's own test suite over the
emulated USB serial port; see [development.md](development.md).
