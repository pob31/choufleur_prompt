#pragma once

/* A booth is a dark place; the LED policy is built around that. Dark means
 * connected and healthy. Anything blinking is a state worth a glance. */
enum led_mode {
	LED_OFF,
	LED_ADVERTISING, /* blue wink every 2 s — waiting for a page */
	LED_CONNECTED,   /* dark */
	LED_DROPPED,     /* red wink — had a link and lost it */
	LED_STALE,       /* amber wink — connected but the page went quiet 90 s */
};

int led_init(void);
void led_set(enum led_mode m);

/* Triple white wink: "which wearable is this one" (opcode 0x06). */
void led_identify(void);
