"""Resonance sweep: calibrate at each seed and report which the DRV2605L accepts.

  sweep.py                      170 190 210 235 260 Hz
  sweep.py 150 175 200          your own seeds
  sweep.py --name CHF-4DE9 ...  pick one wearable

The actuator must be coupled to a mass (strapped, taped down, lying flat) —
hanging from its wires, every seed fails.
"""
import asyncio, sys
from bleak import BleakScanner, BleakClient
VIBE = "c0f1e001-a7d3-4b8f-96f1-2b73d3c5a001"; INFO = "c0f1e002-a7d3-4b8f-96f1-2b73d3c5a001"

argv = sys.argv[1:]; name = None
if argv[:1] == ["--name"]: name, argv = argv[1], argv[2:]
seeds = [int(x) for x in argv] or [170, 190, 210, 235, 260]

async def main():
    dev = await BleakScanner.find_device_by_filter(
        lambda d, a: (a.local_name or "").startswith("CHF-") and (not name or a.local_name == name), timeout=10)
    if not dev: print("no CHF- wearable"); return 1
    async with BleakClient(dev, timeout=20) as c:
        print("connected", dev.name)
        for hz in seeds:
            await c.write_gatt_char(VIBE, bytes([0x08, hz // 2]), response=False)
            await asyncio.sleep(4.0)
            info = await c.read_gatt_char(INFO)
            print(f"seed {hz:3d} Hz -> {'CAL OK' if info[3] else 'fail'}", flush=True)
    return 0
sys.exit(asyncio.run(main()))
