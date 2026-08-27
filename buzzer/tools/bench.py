"""Bench from the Mac: find a CHF- wearable, read info + battery, write opcodes.

  bench.py                 scan, connect, read
  bench.py 05 00           ...then write the test tour
  bench.py 08 64           ...calibrate with a 200 Hz seed, report the result
  bench.py --name CHF-4DE9 05 00   pick one wearable when several are on the air

Needs the venv beside it: python3 -m venv .venv && .venv/bin/pip install bleak
On macOS, run from a shell that has Bluetooth permission (not a sandbox).
"""
import asyncio, sys
from bleak import BleakScanner, BleakClient

SVC  = "c0f1e000-a7d3-4b8f-96f1-2b73d3c5a001"
VIBE = "c0f1e001-a7d3-4b8f-96f1-2b73d3c5a001"
INFO = "c0f1e002-a7d3-4b8f-96f1-2b73d3c5a001"
BATT = "00002a19-0000-1000-8000-00805f9b34fb"

def parse(argv):
    name, ops = None, []
    i = 0
    while i < len(argv):
        if argv[i] == "--name": name = argv[i + 1]; i += 2
        else: ops.append(int(argv[i], 16)); i += 1
    return name, ops

async def main():
    name, ops = parse(sys.argv[1:])
    found = {}
    def cb(d, adv):
        n = adv.local_name or ""
        if n.startswith("CHF-") or SVC in [u.lower() for u in adv.service_uuids]:
            found[d.address] = (d, adv)
    scanner = BleakScanner(cb)
    await scanner.start(); await asyncio.sleep(6); await scanner.stop()
    if not found:
        print("no CHF- wearable seen in 6 s"); return 1
    for d, adv in found.values():
        print(f"seen {adv.local_name!r} rssi {adv.rssi} service-in-AD {SVC in [u.lower() for u in adv.service_uuids]}")
    pick = [v for v in found.values() if not name or v[1].local_name == name]
    if not pick:
        print(f"{name} not seen"); return 1
    d, adv = pick[0]
    async with BleakClient(d, timeout=20) as c:
        print("connected", adv.local_name)
        info = await c.read_gatt_char(INFO)
        cal = ("calibrated" if info[3] else "UNCALIBRATED") if len(info) > 3 else "(no cal byte)"
        print("info", list(info), "-> contract", info[0], f"fw {info[1]}.{info[2]}", cal)
        try:
            print("battery %", (await c.read_gatt_char(BATT))[0])
        except Exception as e:
            print("battery read failed:", e)
        if ops:
            await c.write_gatt_char(VIBE, bytes(ops), response=False)
            print("wrote", [hex(o) for o in ops])
        await asyncio.sleep(3.5 if 8 in ops else 1.5)
        if 8 in ops:
            info = await c.read_gatt_char(INFO)
            print("after calibrate:", "calibrated" if info[3] else "UNCALIBRATED")
    return 0

sys.exit(asyncio.run(main()))
