"""Ratings sweep: for each rated voltage, set it (opcode 0x09), calibrate (0x08)
at the given seed, report pass/fail. Tells a supply-headroom failure from a
resonance one — the seed sweep says nothing if the drive never reaches target.

  ratings.py 235                    seed 235 Hz; 1.0 .. 2.0 V in 0.2 V steps
  ratings.py 235 800 1200 1600      your own rated mV
  ratings.py --name CHF-A011 235 ...
Ends by restoring the overlay's rating (0x09 00).
"""
import asyncio, sys
from bleak import BleakScanner, BleakClient
VIBE = "c0f1e001-a7d3-4b8f-96f1-2b73d3c5a001"; INFO = "c0f1e002-a7d3-4b8f-96f1-2b73d3c5a001"

argv = sys.argv[1:]; name = None
if argv[:1] == ["--name"]: name, argv = argv[1], argv[2:]
seed = int(argv[0]) if argv else 235
mvs = [int(x) for x in argv[1:]] or [1000, 1200, 1400, 1600, 1800, 2000]

async def main():
    dev = await BleakScanner.find_device_by_filter(
        lambda d, a: (a.local_name or "").startswith("CHF-") and (not name or a.local_name == name), timeout=10)
    if not dev: print("no CHF- wearable"); return 1
    async with BleakClient(dev, timeout=20) as c:
        print("connected", dev.name, "seed", seed, "Hz")
        for mv in mvs:
            await c.write_gatt_char(VIBE, bytes([0x09, min(255, mv // 20)]), response=False)
            await asyncio.sleep(0.3)
            await c.write_gatt_char(VIBE, bytes([0x08, seed // 2]), response=False)
            await asyncio.sleep(4.0)
            info = await c.read_gatt_char(INFO)
            print(f"rated {mv:4d} mV -> {'CAL OK' if info[3] else 'fail'}", flush=True)
        await c.write_gatt_char(VIBE, bytes([0x09, 0]), response=False)
        await asyncio.sleep(0.3)
    return 0
sys.exit(asyncio.run(main()))
