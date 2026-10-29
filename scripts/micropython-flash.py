#!/usr/bin/env python3
"""Put files on MicroPython's filesystem in a tartelette flash image.

    micropython-flash.py MICROPYTHON_DIR IMAGE 'TARTELETTE FIRMWARE ARGS...' LOCAL:REMOTE...

Boots the firmware with `--flash IMAGE --headless`, copies each LOCAL file to
REMOTE on the board with MicroPython's own pyboard.py (from MICROPYTHON_DIR),
then closes the emulator's stdin: the run ends, and the emulator saves the
flash, filesystem included, to IMAGE. The command is run by the shell.
"""

import os
import sys


def main():
    mp_dir, image, command, *copies = sys.argv[1:]
    sys.path.insert(0, os.path.join(mp_dir, "tools"))
    import pyboard

    pyb = pyboard.Pyboard(f"exec:{command} --flash {image}")
    pyb.enter_raw_repl()
    for copy in copies:
        local, remote = copy.split(":", 1)
        # Make each directory on the way (they may exist already).
        parts = remote.strip("/").split("/")[:-1]
        for i in range(len(parts)):
            pyb.exec(f"import os\ntry:\n os.mkdir('/{'/'.join(parts[: i + 1])}')\nexcept OSError:\n pass")
        pyb.fs_put(local, remote)
        print(f"{local} -> :{remote}")
    pyb.exit_raw_repl()
    # Not pyb.close(): that kills the emulator, which then saves nothing.
    proc = pyb.serial.subp
    proc.stdin.close()
    if proc.wait(timeout=60) != 0:
        sys.exit(f"the emulator exited with status {proc.returncode}")


if __name__ == "__main__":
    main()
