# `sdcard`: SD card

An SDHC card in SPI mode, backed by a disk image held in memory. The BTL-01
OS drives it with `embedded-sdmmc`; other SPI-mode drivers that stick to the
commands below should work too.

## Keys

| Key | Type | Default | |
|-----|------|---------|---|
| `bus` | `spi0` \| `spi1` | | the bus it sits on |
| `cs` | GPIO | | chip select, active low; left out, always selected |
| `size_mb` | 1..2048 | 16 | size of the blank card, in MiB |

```toml
[buses]
spi1 = { sck = 10, mosi = 11, miso = 12 }

[parts.sd]
kind = "sdcard"
bus = "spi1"
cs = 8
```

Without an image, the card is blank: all zeros, no partition table, no file
system. A firmware that formats its own card will be happy with that; most
won't be. Make an image with `tartelette mkdisk` or any other tool:

```sh
tartelette mkdisk card.img --size-mb 64 --dir data
tartelette fw.elf --board btl-01 --sd sd=card.img
```

`--sd NAME=FILE` loads the image into the card called NAME. An image that's
smaller than 512 KiB is padded. See [running.md](../running.md#sd-cards) for
saving it back, browsing it while it runs and `--sd-live`.

## What's modelled

The card starts in native SD mode and ignores everything until it sees `CMD0`
with chip select low, which puts it in SPI mode. Then:

| Command | |
|---------|---|
| `CMD0` | go idle |
| `CMD8` | interface condition (v2 card) |
| `CMD9`, `CMD10` | CSD (v2) and CID |
| `CMD12` | stop a multi-block read |
| `CMD13` | status |
| `CMD16` | block length |
| `CMD17`, `CMD18` | single and multiple block read |
| `CMD24`, `CMD25` | single and multiple block write |
| `CMD55` + `ACMD41` | initialisation (takes a few polls, as on a real card) |
| `ACMD23` | pre-erase count |
| `CMD58` | OCR, with CCS set: block addressing |
| `CMD59` | CRC on / off |

The CRC7 of `CMD0` and `CMD8` is always checked, and every command's once CRC
is switched on, along with the CRC16 on data blocks in both directions.
Anything else gets "illegal command".

Timing is counted in bytes, not clocks: a response comes one byte after the
command, a data block one byte after its response, and the card holds the
data line low (busy) for a few bytes after each block written.

## Panel

The card's size and whether the firmware has written to it, with **Load…**,
**Save…** and **Browse…** buttons. Browse shows the files on a snapshot of
the card (FAT12/16/32, whole disk or partitioned), with text and hex preview.

## Not modelled

Standard-capacity (SDSC) cards with byte addressing and CSD v1, erase
commands, write protect, card detect, and native SD (4-bit) mode.
