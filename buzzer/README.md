# The buzzer

A wrist wearable that taps the operator ahead of their cues. It vibrates warnings —
never triggers anything — and, like everything else here, it says so when it is dead
rather than sitting quiet: link loss is felt on the wrist and shown on the page.

One wearable per operator, paired to their own screen over Web Bluetooth. The live
page computes the warnings (the server sends none — see the M2.3 seam note at the
bottom) and writes tiny opcodes to the device. Latency budget is 100 ms; a 30–50 ms
connection interval spends well under half of it.

Hardware: a Seeed XIAO nRF52840 (base or Sense), a DRV2605L haptic driver and an
actuator still on trial — LRAs for crisp millisecond-attack pulses, an ERM for
contrast — countable without looking, silent to a neighbour.
Firmware: nRF Connect SDK v3.3.0 / Zephyr, same toolchain and layout as
[headtracker_v1](https://github.com/pob31/headtracker_v1), BLE instead of ESB.

## The contract

This table is the normative copy. `app_buzzer/src/ble.c` and the buzzer section of
`server/crates/choufleur-replay/assets/live.html` both point here; change all three
together or not at all.

| Item | Value |
|---|---|
| Advertised name | `CHF-<XXXX>` — last two bytes of the BT address, so two wearables at one desk are tellable apart |
| Primary service (in the AD, filterable) | `c0f1e000-a7d3-4b8f-96f1-2b73d3c5a001` |
| Vibe characteristic (write, write-without-response) | `c0f1e001-a7d3-4b8f-96f1-2b73d3c5a001` |
| Info characteristic (read) | `c0f1e002-a7d3-4b8f-96f1-2b73d3c5a001` → `[contract, fw_major, fw_minor, calibrated, driver_present]`, contract = **1**; bytes 4 and 5 are additive — 1 = the driver's last auto-cal passed, 1 = the DRV2605L answered its last I2C write (a broken lead is not an uncalibrated actuator) — and a page reading three bytes is none the wiser |
| Battery Service | standard `0x180f` / level `0x2a19`, read + notify |
| Connection parameters (requested by the wearable) | interval 30–50 ms, latency 2, supervision timeout 5 s |

Opcode frames written to the vibe characteristic: one or two bytes, `[op]` or
`[op, param]`. Unknown opcodes are ignored — a newer page against an older wearable
degrades to silence on the new verbs, never to garbage.

| Op | Name | Param | Wearable behaviour |
|---|---|---|---|
| `0x00` | `hb` | 0 | Keepalive. Resets the 90 s staleness watchdog; no vibration |
| `0x01` | `standby` | — | One soft bump — a cue has entered the glance window |
| `0x02` | `final` | — | Two sharp clicks — hand on the fader |
| `0x03` | `lost_near` | — | Three light ticks — tracker lost and your cue is close; eyes up |
| `0x04` | `cancel` | — | Stop any running pattern, clear pending |
| `0x05` | `test` | 0 = tour of the three patterns, 700 ms apart; 1–3 = one of them | How an operator learns the vocabulary, from the panel |
| `0x06` | `identify` | — | LED-only triple wink — which wearable is this one |
| `0x07` | `effect` | DRV2605 library effect 1–123 | Plays the raw effect. For auditioning the vocabulary on a wrist before freezing the constants in `haptic.c` |
| `0x08` | `calibrate` | 0, or resonance seed in Hz ÷ 2 (75 = 150 Hz, 118 = 236 Hz) | Re-runs the driver's auto-calibration now (up to ~3 s, a twitch) — after swapping the actuator on its cable, no power cycle needed. A non-zero param first re-seeds the LRA drive time, so a sweep finds an actuator's real resonance from the page. The info byte reports the result |
| `0x09` | `rated` | rated voltage ÷ 20 mV (100 = 2.0 V); 0 = overlay value | Tryout only: overrides the actuator's rated voltage for the session (overdrive follows at 1.25×), applied at the next calibrate. A ratings sweep tells a supply-headroom failure from a resonance one |

Wearable-initiated, no opcode: **link-lost** — one long heavy buzz on disconnect or
supervision timeout, the wrist learns the safety net is gone; **link-back** — two
feather bumps on (re)connect. Patterns are distinct by pulse count (1 / 2 / 3),
countable without looking.

## Hardware — an actuator tryout

The board is a XIAO nRF52840, base or Sense — the firmware builds for either, and
the base model is the better fit (nothing on it to idle-drain). UF2 bootloader, no
probe needed. Battery: a 3.7 V LiPo with protection, 250–500 mAh (502530 fits), on
the BAT pads; the XIAO charges it at 50 mA (~5 h for 250 mAh), or 100 mA with the
HICHG pin (P0.13) pulled low — only for cells of 250 mAh or more. Strap:
hook-and-loop watch strap, board + cell + breakout heat-shrunk to it, actuator
against the skin, USB-C and the reset hole left reachable.

The driver is a DRV2605L breakout at I2C `0x5a`, and the actuator is deliberately
undecided — this round is a tryout, narrowed after wrists have voted. Swapping is
numbers in `app_buzzer/app.overlay`, plus a lap of the effect audition (the panel's
audition field, or opcode `0x07`) to re-pick the vocabulary:

| Unit under test | actuator-mode | vib-rated-mv | vib-overdrive-mv | lra-freq-hz |
|---|---|---|---|---|
| Adafruit 2305 + PUI HD-LA0803-LW10-R (8×8×3.2 mm LRA, 2 Vrms / 235 Hz / 25 Ω per its datasheet) | `"LRA"` | **1400** — the 3.3 V rail's ceiling: locks up to 1400, refuses from 1600 | 1750 | 235 (any seed 100–300 locks once the rating is within the rail) |
| Pimoroni PIM452, ELV1411A on the PCB (14×11×2.5 mm LRA, 2 Vrms) | `"LRA"` | **1400** — same rail, same ceiling; it passed at "2000" only while the register maths under-asked | 1750 | 200 — swept: 150 fails auto-cal, 170–235 all lock |
| Adafruit 2305 + 3 V coin ERM (`erm.overlay`) | `"ERM"` | **2600** — the rail again: locks up to 2600, refuses from 2800 | 3000 | unused |

The frequency only seeds auto-resonance; calibration trims from there, so a
roughly-right number starts crisp and gets crisper — and a wrong one fails
calibration outright rather than sounding merely dull, which is how the PIM452's
"150 Hz" was caught. Opcode `0x08` with a seed sweeps for the truth from the page.

**The rail sets the rating.** Both LRAs are 2 Vrms parts, and on a 3.3 V supply the
DRV2605L cannot reach 2 Vrms plus overdrive: calibration locks at 1.4 V rated and
refuses at 1.6, on either unit. A ratings sweep (opcode `0x09`, `tools/ratings.py`)
finds that ceiling in forty seconds. 1.4 V is about 70 % of the parts' rating — and
it is what two people felt on five sites, since the first night's firmware was
quietly asking for 1.6. Feeding the breakout more than 3.3 V is not an option while
its I2C pull-ups hang off the same pin — the nRF52840 is not 5 V tolerant.

**Calibrate as mounted — the datasheet means it.** An LRA rings against the mass it
is bonded to; a breakout dangling from its four wires has none, swings instead of
ringing, and fails calibration at every seed (status `0xe8`). The same board laid
flat with a cable resting on it passed at every seed a minute later. On the wrist
the strap is the mass: calibrate there, and treat a fail as "not coupled" before
"wrong numbers". Auto-cal runs at every boot (a
short twitch — calibrate strapped, the datasheet wants the actuator mounted as
worn). For an ERM the firmware switches feedback topology and effect library by
itself, from `actuator-mode`.

Four wires, no discrete parts — and they are the four of a STEMMA QT / Qwiic cable,
whose colours are fixed, so the harness cannot be wired wrong:

```
red    3.3 V   XIAO 3V3 → breakout VIN
black  GND     XIAO GND → breakout GND
blue   SDA     XIAO D4 (P0.04) → SDA
yellow SCL     XIAO D5 (P0.05) → SCL
```

No board on the bench carries a JST-SH socket (bare XIAO, PIM452, plain Adafruit
2305), so the solder joint lives on the XIAO once — a socket on 3V3/GND/D4/D5 — and
each breakout gets a plug-ended pigtail. Swapping units under test is then a
connector, not an iron.

The actuator goes on the Adafruit's OUT+ / OUT− (polarity-insensitive for an LRA);
the PIM452's is already on the PCB. Both breakouts' IN/TRIG pin stays unconnected,
and neither exposes EN — the power story lives in the notes below.

An LRA at resonance draws tens of mA with no ERM-style inrush, so no bulk capacitor.
A show's worth of vibes is a couple of mAh; connected idle is tens of µA — a 250 mAh
cell runs weeks of shows.

## Tryout log

**2026-08-24 — PIM452 (ELV1411A), first night.** Two people, strapped (no fingers):
forearm, upper arm, shoulder, neck. Every pulse of the light vocabulary — one soft
bump, two sharp clicks, three light ticks — was felt at every site, and the three
heavier candidates (strong click 1, strong buzz 14, 1000 ms alert 16) all got
through too. Fingertips read best, the trapezius least, the forearm in between;
none needed the heavier set. Tucked at the hipbone under a trouser waistband —
beltpack territory, and bone underneath — the short pulses read especially well,
which makes the waistband a wearing position with no strap to design. The full account is
[docs/choufleur-buzzer-notes.md](../docs/choufleur-buzzer-notes.md).

**2026-08-25 — Adafruit 2305 + PUI HD-LA0803-LW10-R, second XIAO.** Vibrated at once,
calibrated never — at any seed from 100 to 300 Hz, dangling or held, before and after
the register maths gained the LRA sampling factor it had been missing. A ratings sweep
from the page (opcode `0x09`) settled it in forty seconds: locks at 0.8, 1.0, 1.2 and
1.4 V rated, fails at 1.6, 1.8 and 2.0. The boxed datasheet says 2 Vrms, 235 Hz,
25 Ω, 90 mA — so it is not the part but the 3.3 V rail, which the corrected register
maths had just started asking for the full 2 V of. The PIM452 on the same XIAO
calibrated first time only because it was tested before that correction; it shares
the ceiling. Both LRAs now run at 1.4 V rated; with that in the overlay the PUI
calibrates at boot, unaided, and plays the tour. Between the fingers the two feel
quite different for the same pattern: the 8 mm coin's mass moves **axially**, into
the skin; the ELV1411A's moves **laterally**, shearing along it. A mounting fact as
much as a preference — which way the actuator faces the wrist is part of the design.

**2026-08-25 — Adafruit 2305 + 3 V coin ERM, third XIAO.** `erm.overlay` on the same
firmware; ERM mode confirmed by the feedback register (`0x36`). Calibrated at boot,
unaided, at 2.6 V; the ratings sweep locks up to 2.6 and refuses from 2.8 — the
3.3 V rail once more, so a 3 V motor runs at 2.6. Feels different again, as it
should: a spinning mass with 50 ms spin-up and 80 ms spin-down rumbles where the
LRAs click, and the short countable ticks blur towards a buzz. Mounted to the back
of the PCB and recalibrated, it is **stronger than both LRAs** — as the arithmetic
says it should be: 2.6 of 3 V is 87 % of its rating where the LRAs get 70 % of theirs,
and a coin motor's mass is the larger. Strength against crispness is the trade the
three rows now put on the table.

**Side by side, at the hips.** Both LRAs under the trouser waistband, one on each
hipbone, calibrated as worn, fired together with `tools/all.py`: the **PIM452 reads
stronger** than the PUI. The geometry says why. The coin's stroke is axial — into the
skin, the very axis the waistband clamps, so the pressure that couples it also
shortens its travel. The ELV1411A's stroke is lateral, shearing along the skin;
clamping barely touches that axis and only improves the coupling — and it carries
the larger moving mass. Rule: **under a strap or waistband, a lateral LRA; an axial
coin belongs where it faces the skin without being pressed.** Behind the clavicle,
one at a time under a hand-held strap, the coin made the nuance exact: with gentle
pressure it gives "a very nice pulsating feel"; with the strap pulled firm it is
choked. The coin is not the lesser actuator — it is the one whose mount must control
its preload: an elastic, a foam pad spreading the load, or a shallow well in a rigid
housing so the coin's face meets skin without carrying strap tension. The ERM at
the same spot: **"brutal — would be felt even through heavy fabric."** That is its
place in the picture, stated exactly: not the vocabulary's actuator, but the one for
a wearer who cannot have it against skin — a jacket pocket, a costume, a beltpack
pouch — where a click would never arrive and a rumble still does.

**ERM against PIM, same hips, same instant:** the ERM is stronger and **not good at
short bursts** — the tour's clicks and ticks smear into rumble. The vocabulary is
countable bursts; strength was never the shortage. **Where the tryout points:** a
lateral LRA — the ELV1411A as fitted on the PIM452 — under the strap or waistband,
driven at 1.4 V from the 3.3 V rail. The PUI coin for a mount that faces skin
unclamped; the ERM for nothing that has to be counted. Calibration passed at every site once strapped, and
failed only while the board hung from its cable. Vocabulary left as is pending the
other two actuators. The full account is
[docs/choufleur-buzzer-notes.md](../docs/choufleur-buzzer-notes.md).

## Build and flash

The toolchain is nRF Connect SDK v3.3.0, installed the way headtracker_v1 does it —
`nrfutil install sdk-manager`, then `nrfutil sdk-manager install v3.3.0`; on macOS
that lands in `/opt/nordic/ncs`. The build runs inside the launched environment:

```bash
# base board; /sense for a XIAO nRF52840 Sense — same firmware either way
nrfutil sdk-manager toolchain launch --ncs-version v3.3.0 --chdir /opt/nordic/ncs/v3.3.0 -- \
    west build -b xiao_ble/nrf52840/sense /path/to/choufleur/buzzer/app_buzzer \
    -d /path/to/choufleur/buzzer/build_buzzer
```

Flashing, two ways. Double-tap the reset button and the board mounts as `XIAO-SENSE`:

```bash
cp buzzer/build_buzzer/app_buzzer/zephyr/zephyr.uf2 /Volumes/XIAO-SENSE/
```

If the drive does not appear — it did not, the first night, though the bootloader's
serial port did — the same bootloader takes serial DFU:

```bash
pip install adafruit-nrfutil
adafruit-nrfutil dfu genpkg --dev-type 0x0052 \
    --application buzzer/build_buzzer/app_buzzer/zephyr/zephyr.hex buzzer.zip
adafruit-nrfutil dfu serial --package buzzer.zip -p /dev/cu.usbmodem* -b 115200 --singlebank
```

The stock bootloader announces itself on USB as "XIAO nRF52840 Sense" on the base
board too, so that string says nothing about which variant is on the bench — build
for the board you bought. Getting into the bootloader: double-tap reset, always. Only the stock Arduino
firmware answers the 1200-baud "touch" on its serial port; once this firmware is on,
the tap is the way.

The XIAO board definition switches on a USB serial console by default, so a running
wearable shows up as a CDC port ("Zephyr Project") whenever it is on USB — the
quickest sign that the app booted, and where logs go in a `debug_usb.conf` build.
On battery the USB peripheral is unpowered and costs nothing.

For the ERM contender, layer `erm.overlay` on top (it flips `actuator-mode` and the
ratings; nothing in the code changes):

```bash
west build ... -d buzzer/build_erm -- -DEXTRA_DTC_OVERLAY_FILE=erm.overlay
```

For log output on that console, add:

```bash
west build ... -- -DEXTRA_CONF_FILE=debug_usb.conf
```

## Bench test — no Choufleur needed

1. Power the board. LED winks blue every 2 s: advertising.
2. Phone, nRF Connect app: scan for `CHF-`, connect. Two feather bumps (link-back),
   LED goes dark — dark means healthy; a booth is a dark place.
3. On the vibe characteristic write `01`, `02`, `03`, then `05 00`: soft bump,
   double click, triple tick, then the tour. Crisp, no rattle — a rattle means the
   overlay's rated/overdrive voltages disagree with the LRA datasheet.
4. Write `07 <n>` to audition raw library effects (1–123) when choosing new
   patterns — or, once paired to the page, use the audition field in its panel.
   With several wearables powered, `tools/all.py 07 <n>` plays it on all of them
   at once — the side-by-side the actuator tryout is made of.
5. Read the info characteristic: `[01, xx, yy]` — contract 1.
6. Battery Service shows a plausible percentage and notifies.
7. Kill the app without disconnecting: within 5 s, one long heavy buzz (link-lost)
   and the LED starts winking red. Reconnect: two feather bumps, dark again.

## LED, in one line each

Blue wink every 2 s: advertising. Dark: connected and healthy. Red wink: had a link
and lost it (1 s cadence for a minute, then 3 s). Amber wink: connected but the page
has gone 90 s without writing — the tab is throttled, discarded, or wedged. Triple
white: identify.

After 30 min unconnected the advertising slows; after 4 h the board switches itself
off entirely — a tap on reset wakes it.

## Power path notes

The wearable spends almost all of its life not vibrating, so the driver spends
almost all of its life powered down: three seconds after the last pattern ends —
and immediately on disconnect — `haptic.c` puts the DRV2605L to sleep. On the
tryout breakouts, which expose no EN pin, that is the chip's standby bit (~5 µA);
a bare-chip build that wires EN (add `en-gpios` to the node in `app.overlay`) gets
hard shutdown instead, and the firmware adapts to whichever is there. Neither
state is trusted to preserve registers, so auto-calibration runs once at boot, its
three results are cached in RAM, and every wake rewrites the handful of registers
before the pattern plays — a millisecond against the 100 ms budget.

That is also why `haptic.c` drives the registers directly rather than through
Zephyr's `ti,drv2605` driver: calibration, the cal-result cache and the sleep
discipline are not reachable through the haptics subsystem API. The devicetree
node still uses the `ti,drv2605` binding so the properties are checked; no driver
binds to it (`CONFIG_HAPTICS` stays off).

## The M2.3 seam

Today the live page computes warnings from tracker position and the cue list,
because the wire protocol carries no warning events. When `cue_warning` lands
server-side (devplan M2.3), only the page's trigger source changes — server stages
map onto the same opcodes. The contract above, the firmware and the hardware are
meant to outlive that port unchanged.
