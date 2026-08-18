#pragma once

#include <stdint.h>

/* Patterns are distinct by pulse count (1 / 2 / 3), countable without looking.
 * The exact library effects behind them live in haptic.c; audition candidates
 * on a wrist with opcode 0x07 before changing them. */
enum haptic_pattern {
	HAPTIC_STANDBY,   /* one soft bump — cue entered the glance window */
	HAPTIC_FINAL,     /* two sharp clicks — hand on the fader */
	HAPTIC_LOST_NEAR, /* three light ticks — tracker lost, cue close */
	HAPTIC_LINK_LOST, /* one long heavy buzz — the safety net is gone */
	HAPTIC_LINK_BACK, /* two feather bumps — and it is back */
};

/* Wakes the DRV2605L, runs auto-calibration against the overlay's LRA ratings
 * (a short twitch), caches the results, and powers the chip back down. */
int haptic_init(void);

void haptic_play(enum haptic_pattern p);

/* Raw DRV2605 library effect 1..123, played as-is (opcode 0x07). */
void haptic_effect(uint8_t effect);

/* which = 0: standby, final, lost_near in sequence, 700 ms apart.
 * which = 1..3: just that one. */
void haptic_tour(uint8_t which);

/* Stop any running pattern, clear pending, power the driver down. */
void haptic_cancel(void);

/* EN low immediately — the road to sys_poweroff(). */
void haptic_off(void);
