"""Every CHF- wearable on the air, the same opcode, at once — for side-by-side
comparison of actuators on the bench.

  all.py 05 00          tour on all of them
  all.py 07 0e          effect 14 on all
  all.py 08 00          calibrate all (each as mounted)
  all.py                just list them, with info and battery

Connections are held in parallel; writes go out together, so the wearables fire
within tens of milliseconds of each other.
"""
import asyncio, sys
from bleak import BleakScanner, BleakClient
SVC="c0f1e000-a7d3-4b8f-96f1-2b73d3c5a001"; VIBE="c0f1e001-a7d3-4b8f-96f1-2b73d3c5a001"
INFO="c0f1e002-a7d3-4b8f-96f1-2b73d3c5a001"; BATT="00002a19-0000-1000-8000-00805f9b34fb"
ops = [int(x, 16) for x in sys.argv[1:]]

async def one(d, name):
    try:
        async with BleakClient(d, timeout=20) as c:
            info = await c.read_gatt_char(INFO)
            try: batt = (await c.read_gatt_char(BATT))[0]
            except Exception: batt = "?"
            cal = ("calibrated" if info[3] else "UNCALIBRATED") if len(info) > 3 else "?"
            if len(info) > 4 and not info[4]: cal += " DRIVER-NOT-ANSWERING"
            if ops:
                await c.write_gatt_char(VIBE, bytes(ops), response=False)
            print(f"{name}: fw {info[1]}.{info[2]} {cal} battery {batt}%" + (f" wrote {[hex(o) for o in ops]}" if ops else ""), flush=True)
            await asyncio.sleep(4.0 if ops and ops[0] == 0x08 else 2.0)
    except Exception as e:
        print(f"{name}: {e}")

async def main():
    found = {}
    def cb(d, adv):
        if (adv.local_name or "").startswith("CHF-"): found[d.address] = (d, adv.local_name)
    s = BleakScanner(cb); await s.start(); await asyncio.sleep(6); await s.stop()
    if not found: print("no CHF- wearables seen"); return 1
    print("seen:", ", ".join(n for _, n in found.values()))
    await asyncio.gather(*(one(d, n) for d, n in found.values()))
    return 0
sys.exit(asyncio.run(main()))
