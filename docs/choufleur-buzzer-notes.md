# The buzzer meets a wrist — PIM452, 24 August 2026

What happened the evening the parts arrived: a XIAO nRF52840 met its compiler,
its bootloader, a DRV2605L, and then two people. Kept in the spirit of
[the phase-0 notes](choufleur-phase0-notes.md): what was tried, what it said,
including the parts that were wrong.

## On the bench

- A **base-model XIAO nRF52840** — the bootloader announces itself on USB as a
  "Sense" whichever board it is, which cost a pristine rebuild once the truth came
  out. USB-powered all evening; the battery reads 100 % because the pad sees the
  charger rail.
- A **Pimoroni PIM452**, DRV2605L with an ELV1411A LRA bonded to its PCB, on four
  STEMMA QT wires (red 3V3, black GND, blue D4/SDA, yellow D5/SCL). No EN pin, so
  idle power-down is the chip's standby bit.
- The Mac had **no toolchain**. headtracker_v1 was built on Windows. nrfutil and
  `sdk-manager install v3.3.0` put NCS at `/opt/nordic/ncs` (~11 GB, 15 min).

## Getting the firmware on

The first build — the first time the code had met a compiler — was clean: no
errors, no warnings, 150 KB. Flashing found its own path. The stock Arduino
firmware answered the 1200-baud "touch" and dropped into the bootloader, but the
bootloader never mounted a drive on this Mac; its serial port was there, and
`adafruit-nrfutil dfu serial` took the app over it. From then on the bootloader
is reached by a double-tap on reset — Zephyr's CDC does not do the touch.

The board comes back on USB as "Zephyr Project": the XIAO board definition
switches on a USB serial console by default, which turned out to be the
evening's most useful instrument.

## Proving BLE from the Mac

`bleak` from Python, once run outside the sandbox: the wearable advertises as
`CHF-4DE9` with the service UUID in the AD, reads back `[1, 0, 2, …]` from the
info characteristic, notifies battery, and takes opcodes. Every vibration of the
evening was written from this Mac, not from the page — the page's pairing flow in
Chrome is still untested.

## Three bugs the bench found

None of them a compiler could have caught.

1. **The wrong I2C bus.** The XIAO's edge pins D4/D5 are `i2c1`, labelled
   `xiao_i2c`. `i2c0` is the Sense's internal bus with the IMU on it, and on a
   base board nothing at all. The DRV node had been on `i2c0`.
2. **Giving up before the chip did.** The first logged calibration failed 1.47 s
   after the write — the firmware's own 1.5 s limit. The DRV2605L takes ~1.2 s
   plus startup. Status `0xEC` read mid-run said only "a DRV2605L is here and
   busy". The wait is 3 s now.
3. **A lowercase token.** `DT_ENUM_HAS_VALUE(node, actuator_mode, LRA)` compiles
   and quietly evaluates to 0, because the generated devicetree spells it
   `_ENUM_VAL_lra_EXISTS`. So `IS_LRA` was false and the chip spent the first
   hours in **ERM mode driving an LRA**, from the ERM effect library. The
   feedback register reading back `0x37` instead of `0xB7` in the console log is
   what gave it away. Lowercase now, with a `BUILD_ASSERT`.

Two smaller ones: the BLE name defaulted to GATT-writable (off now — nobody at a
venue renames a wrist), and a warm reboot can hand the chip to the new firmware
mid-waveform, so init now issues `DEV_RESET` first. That build is packaged and
not yet flashed.

## Tools that came out of it

- **Opcode `0x08` calibrate**, with an optional resonance seed (Hz ÷ 2). A swap
  on the cable gets a fresh calibration without a power cycle, and a sweep finds
  an actuator's real resonance from the page.
- **A fourth info byte**: did the last calibration pass. The page says "driver
  uncalibrated" in the same breath as the battery.
- The page's panel grew a **calibrate** button beside the **audition** field
  (any of the 123 library effects, on the wrist that is wearing it).
- Bench scripts: scan / read / write opcodes; a seed sweep; a console reader.

## What the chip said about the ELV1411A

A reseller page said 150 Hz. Swept in true LRA mode: **150 fails calibration
every time; 170, 190, 210 and 235 all lock**, converging to the same values. The
overlay seeds 200. The lesson generalises: a wrong seed fails loudly rather than
sounding merely dull, so the sweep is a measurement, not a guess.

## The mounting finding

For a stretch every seed failed with status `0xE8` — calibration cannot
converge, feedback fine — while the board **hung from its cable**. Laid flat with
a cable resting on it, a minute later: pass at every seed. An LRA rings against
the mass it is bonded to; a couple of grams swinging on four wires is not one.
Rule, now in the buzzer README: **calibrate as mounted**, and read a fail as "not
coupled" before "wrong numbers". Strapped or under a waistband, every calibration
of the evening passed first time.

## Two people, six sites

The light vocabulary — one soft bump (standby), two sharp clicks (final), three
light ticks (lost with a cue near) — followed by three heavier candidates: strong
click (effect 1), strong buzz (14), 1000 ms alert (16).

| Site | Coupling | Result |
|---|---|---|
| Fingertips | held | reads best of all — and therefore not a valid test |
| Forearm | strap | every pulse read, light set sufficient |
| Upper arm | strap | every pulse read |
| Shoulder / trapezius | strap | every pulse read; the least sensitive site |
| Neck | held | read; heavier set noticeably more present |
| **Hipbone**, under the trouser waistband | waistband | **short pulses read especially well** |

Both people, all sites: every pulse felt; the heavier set never *needed*.
Fingertips ≫ forearm > trapezius, as skin physiology predicts. The hipbone
result may be the most practical: bone underneath, beltpack territory, and
nothing to design — a clip, or a pocket.

## Open

- The `DEV_RESET` build goes on at the next double-tap.
- The other two contenders: the PUI HD-LA0803-LW10-R on the Adafruit board
  (resonance unknown — sweep it), and the ERM (`actuator-mode = "ERM"`, rebuild).
- Pairing from the live page in Chrome — the operator's actual path — untested.
- Nothing has run on the cell yet; the battery curve and the idle draw are
  paper numbers until it does.
- The vocabulary is unchanged and unfrozen: two wrists said it reads; nobody has
  yet said which stage they would *want* louder or softer.

---

# The second LRA — Adafruit 2305 + PUI HD-LA0803-LW10-R, 25 August 2026

A second XIAO on stock firmware, the Adafruit breakout, the 8 mm PUI coin LRA taped
to the back of the PCB, the same four QT wires. The bench tools had died with the
scratch directory overnight and were rebuilt into `buzzer/tools/` first.

## What happened, in order

1. The 1200-baud touch took the fresh board into its bootloader; serial DFU put the
   firmware on. Advertised as `CHF-A011`, info, battery, opcodes — all as the first
   board. It vibrated at once. It calibrated never: `0xE8` at every seed, boot and
   on demand, dangling or held.
2. Swapping the **PIM452 onto the new XIAO**: calibrated first time. So the board,
   firmware and cable were clean; the unit was the difference.
3. A real error surfaced in the register maths: the datasheet scales a closed-loop
   LRA's rated and overdrive registers by √(1 − 1.5 ms·f) — the back-EMF sampling
   dead time — which is 0.80 at 235 Hz. Left out, the chip was asked for four
   fifths of the rating. Fixed, recomputed with every seed. **It did not fix the
   PUI.**
4. A resonance sweep from 100 to 300 Hz: every seed failed. Not the frequency.
5. A new opcode, `0x09 rated`, and `tools/ratings.py`: step the rated voltage from
   0.8 to 2.0 V, calibrating at each. **Locks at 0.8, 1.0, 1.2, 1.4; refuses at
   1.6, 1.8, 2.0.** Forty seconds, and the whole picture.
6. The boxed datasheet arrived: **2 Vrms, 235 Hz, 25 Ω, 90 mA at rating.** The
   vendor page had been right. So the ceiling is not the part but the **3.3 V
   rail**: 2 Vrms is 2.83 V peak before overdrive, and an H-bridge on 3.3 V with
   90 mA through it does not get there.

## What it means

- **Both LRAs are 2 V parts on a 3.3 V supply.** The PIM452 passed at "2000" on the
  first night only because the old maths was asking for 1.6. With the maths right,
  it shares the ceiling. Both are rated **1400** in the overlay now — about 70 % of
  their rating, and exactly what two people felt on five sites last night.
- Feeding the breakout more than 3.3 V is not available: its I2C pull-ups hang off
  the same VIN, and the nRF52840 is not 5 V tolerant. On the cell the 3V3 LDO holds
  3.3 V, so the number is the same off USB.
- The order of diagnosis mattered. Mount, then resonance, then rating — each one a
  sweep from the page, none a rebuild. The tools that do it are in the repository.
- **Three bugs the first night, one the second**, and one lesson the datasheet had to
  deliver in person: a calibration that "cannot converge" can be the rail, and the
  fastest way to know is to ask for less and watch it lock.

## Open

- ~~The 1400 build goes onto the second XIAO; the PIM452 gets re-checked under the
  corrected maths at 1400.~~ Done the same evening: the PUI calibrates at boot,
  unaided, and the PIM452 on the same XIAO locks at 170, 200 and 235 Hz. Both LRAs,
  one rating, one explanation.
- How the PUI reads on skin against the PIM — crisper, smaller, 235 Hz coin against
  a 200 Hz rectangle — is the felt comparison still to be written down.
- ~~The ERM remains untried.~~ Tried the same evening, on a third XIAO with the same
  Adafruit breakout, via `erm.overlay`: calibrates at boot at 2.6 V, refuses from 2.8
  (the rail again), and mounted it is **stronger than both LRAs** — 87 % of its rating
  against the LRAs' 70 %, and the larger mass — at the cost of the click: 50 ms of
  spin-up turns ticks into rumble. Three contenders in, one trade on the table:
  strength against crispness, and which way the mass moves against the skin.
- All three boards on USB, driven together with `tools/all.py`: one opcode, three
  actuators, the same instant. Both LRAs at the hipbones under the waistband, each
  calibrated as worn: **the PIM452 reads stronger than the PUI coin** — the coin's
  axial stroke is the axis the waistband clamps, the ELV1411A's lateral stroke is
  not, and the ELV carries the larger mass. Under a strap, lateral wins.
- Then the ERM in the PUI's place, same hips, same instant: stronger, and **not good
  at short bursts** — the countable vocabulary smears into rumble. Where the tryout
  points: the lateral LRA (ELV1411A / PIM452) under the strap at 1.4 V. Strength was
  never the shortage; countability is the whole design.
- Behind the clavicle (above the scapula, under a hand-held strap), one at a time:
  the PUI coin under **gentle** pressure — "a very nice pulsating feel"; under a firm
  strap — choked. The axial rule gains its nuance: the coin is fine where the mount
  controls preload (elastic, foam, a shallow well), and wrong where a strap clamps.
  The ERM's turn was lost to a broken I2C lead, which the wearable reported as
  "uncalibrated" — a fifth info byte now says "driver not answering" instead.
- Pairing from the live page in Chrome remains untested; so does a night on the cell.
