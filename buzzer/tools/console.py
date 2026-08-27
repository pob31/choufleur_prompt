"""Read the wearable's USB console for N seconds (a debug_usb.conf build logs there)."""
import sys, time, glob, serial
port = (glob.glob('/dev/cu.usbmodem*') or [None])[0]
secs = float(sys.argv[1]) if len(sys.argv) > 1 else 10
if not port: print("no CDC port"); sys.exit(1)
with serial.Serial(port, 115200, timeout=0.2) as s:
    s.dtr = True
    end = time.time() + secs
    while time.time() < end:
        line = s.readline()
        if line: print(line.decode(errors='replace').rstrip(), flush=True)
