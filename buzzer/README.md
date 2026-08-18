# The buzzer

A wrist wearable that taps the operator ahead of their cues. It vibrates warnings —
never triggers anything — and, like everything else here, it says so when it is dead
rather than sitting quiet: link loss is felt on the wrist and shown on the page.

One wearable per operator, paired to their own screen over Web Bluetooth. The live
page computes the warnings (the server sends none — see the M2.3 seam note at the
bottom) and writes tiny opcodes to the device. Latency budget is 100 ms; a 30–50 ms
connection interval spends well under half of it.

Hardware: Seeed XIAO nRF52840 Sense, a DRV2605L haptic driver and a coin LRA —
crisp millisecond-attack pulses, countable without looking, silent to a neighbour.
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
| Info characteristic (read) | `c0f1e002-a7d3-4b8f-96f1-2b73d3c5a001` → `[contract, fw_major, fw_minor]`, contract = **1** |
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

Wearable-initiated, no opcode: **link-lost** — one long heavy buzz on disconnect or
supervision timeout, the wrist learns the safety net is gone; **link-back** — two
feather bumps on (re)connect. Patterns are distinct by pulse count (1 / 2 / 3),
countable without looking.

## Hardware

| Item | Spec | Note |
|---|---|---|
| Board | XIAO nRF52840 Sense | UF2 bootloader, no probe needed |
| Haptic driver | DRV2605L breakout (Adafruit 2305, Grove, or bare) | I2C address `0x5a`; decoupling lives on the breakout |
| Actuator | 8–10 mm coin LRA, ~170–235 Hz (e.g. Vybronics VG0832013D) | millisecond attack, quiet, polarity-insensitive |
| Battery | 3.7 V LiPo with protection, 250–500 mAh (502530 fits) | on the BAT pads; the XIAO charges it at 50 mA (~5 h for 250 mAh) |
| Strap | hook-and-loop watch strap | board + cell + breakout heat-shrunk to it, LRA against the skin, USB-C and the reset hole left reachable |

Six wires, no discrete parts:

```
XIAO 3V3 → DRV2605L VIN        XIAO D4 (P0.04) → SDA
XIAO GND → DRV2605L GND        XIAO D5 (P0.05) → SCL
XIAO D10 (P1.15) → EN          LRA on OUT+ / OUT−
```

An LRA at resonance draws tens of mA with no ERM-style inrush, so no bulk capacitor.
A show's worth of vibes is a couple of mAh; connected idle is tens of µA — a 250 mAh
cell runs weeks of shows. Charge current can be raised to 100 mA by pulling the
XIAO's HICHG pin (P0.13) low — only with cells of 250 mAh or more.

The LRA's ratings live in the devicetree overlay
(`app_buzzer/boards/xiao_ble_nrf52840_sense.overlay`): `vib-rated-mv`,
`vib-overdrive-mv` and `lra-freq-hz`. For a different LRA, set them from its
datasheet; the firmware runs the DRV2605L's auto-calibration against them at every
boot (a short twitch at power-on — calibrate strapped, the datasheet wants the
actuator mounted as worn).

## Build and flash

From the nRF Connect SDK v3.3.0 environment (the same one headtracker_v1 uses):

```bash
west build -b xiao_ble/nrf52840/sense /path/to/choufleur/buzzer/app_buzzer \
    -d /path/to/choufleur/buzzer/build_buzzer
```

Double-tap the reset button — the board mounts as `XIAO-SENSE` — then:

```bash
cp buzzer/build_buzzer/app_buzzer/zephyr/zephyr.uf2 /Volumes/XIAO-SENSE/
# (non-sysbuild layouts put it at buzzer/build_buzzer/zephyr/zephyr.uf2)
```

For a USB serial console (log output, driverless CDC as in headtracker_v1), add:

```bash
west build ... -- -DEXTRA_CONF_FILE=debug_usb.conf -DEXTRA_DTC_OVERLAY_FILE=debug_usb.overlay
```

## Bench test — no Choufleur needed

1. Power the board. LED winks blue every 2 s: advertising.
2. Phone, nRF Connect app: scan for `CHF-`, connect. Two feather bumps (link-back),
   LED goes dark — dark means healthy; a booth is a dark place.
3. On the vibe characteristic write `01`, `02`, `03`, then `05 00`: soft bump,
   double click, triple tick, then the tour. Crisp, no rattle — a rattle means the
   overlay's rated/overdrive voltages disagree with the LRA datasheet.
4. Write `07 <n>` to audition raw library effects (1–123) when choosing new patterns.
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
almost all of its life off: three seconds after the last pattern ends — and
immediately on disconnect — `haptic.c` drops the DRV2605L's EN pin and the chip is
in shutdown, not standby. Shutdown may not preserve registers, so auto-calibration
runs once at boot, its three results are cached in RAM, and every wake (EN high,
~1 ms) rewrites the handful of registers before the pattern plays — invisible
against the 100 ms budget.

That is also why `haptic.c` drives the registers directly rather than through
Zephyr's `ti,drv2605` driver: calibration, the cal-result cache and EN discipline
are not reachable through the haptics subsystem API. The devicetree node still uses
the `ti,drv2605` binding so the properties are checked; no driver binds to it
(`CONFIG_HAPTICS` stays off).

## The M2.3 seam

Today the live page computes warnings from tracker position and the cue list,
because the wire protocol carries no warning events. When `cue_warning` lands
server-side (devplan M2.3), only the page's trigger source changes — server stages
map onto the same opcodes. The contract above, the firmware and the hardware are
meant to outlive that port unchanged.
