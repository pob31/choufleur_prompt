/* One delayable work item runs the whole show: each tick either lights
 * something for WINK_MS or puts it out and books the next wink. Winks are
 * 20 ms — enough to catch in peripheral vision, cheap on the cell. */

#include <zephyr/kernel.h>
#include <zephyr/drivers/gpio.h>

#include "led.h"

#define WINK_MS 20

/* The XIAO's RGB is three active-low pins; the board aliases them led0
 * (red), led1 (green), led2 (blue). */
static const struct gpio_dt_spec red = GPIO_DT_SPEC_GET(DT_ALIAS(led0), gpios);
static const struct gpio_dt_spec green = GPIO_DT_SPEC_GET(DT_ALIAS(led1), gpios);
static const struct gpio_dt_spec blue = GPIO_DT_SPEC_GET(DT_ALIAS(led2), gpios);

static enum led_mode mode = LED_OFF;
static bool lit;
static int64_t dropped_at;
static uint8_t ident_left; /* pending identify half-cycles */

static void all_off(void)
{
	gpio_pin_set_dt(&red, 0);
	gpio_pin_set_dt(&green, 0);
	gpio_pin_set_dt(&blue, 0);
	lit = false;
}

static void tick_fn(struct k_work *work);
static K_WORK_DELAYABLE_DEFINE(tick_work, tick_fn);

static void tick_fn(struct k_work *work)
{
	ARG_UNUSED(work);

	if (lit) {
		/* Put the wink out, then book the gap to the next one. */
		all_off();

		if (ident_left > 0) {
			ident_left--;
			k_work_reschedule(&tick_work, K_MSEC(120));
			return;
		}

		switch (mode) {
		case LED_ADVERTISING:
		case LED_STALE:
			k_work_reschedule(&tick_work, K_SECONDS(2));
			break;
		case LED_DROPPED:
			/* Insistent for the first minute — the operator is
			 * probably still nearby — then patient. */
			k_work_reschedule(&tick_work,
					  (k_uptime_get() - dropped_at) < 60000
						  ? K_SECONDS(1)
						  : K_SECONDS(3));
			break;
		default:
			break;
		}
		return;
	}

	if (ident_left > 0) {
		gpio_pin_set_dt(&red, 1);
		gpio_pin_set_dt(&green, 1);
		gpio_pin_set_dt(&blue, 1);
		lit = true;
		k_work_reschedule(&tick_work, K_MSEC(60));
		return;
	}

	switch (mode) {
	case LED_ADVERTISING:
		gpio_pin_set_dt(&blue, 1);
		break;
	case LED_DROPPED:
		gpio_pin_set_dt(&red, 1);
		break;
	case LED_STALE:
		gpio_pin_set_dt(&red, 1);
		gpio_pin_set_dt(&green, 1);
		break;
	default:
		return; /* LED_OFF and LED_CONNECTED are dark: no next tick */
	}
	lit = true;
	k_work_reschedule(&tick_work, K_MSEC(WINK_MS));
}

void led_set(enum led_mode m)
{
	if (m == mode && m != LED_DROPPED) {
		return;
	}
	mode = m;
	if (m == LED_DROPPED) {
		dropped_at = k_uptime_get();
	}
	all_off();
	k_work_reschedule(&tick_work, K_NO_WAIT);
}

void led_identify(void)
{
	ident_left = 3;
	all_off();
	k_work_reschedule(&tick_work, K_NO_WAIT);
}

int led_init(void)
{
	if (!gpio_is_ready_dt(&red) || !gpio_is_ready_dt(&green) ||
	    !gpio_is_ready_dt(&blue)) {
		return -ENODEV;
	}
	gpio_pin_configure_dt(&red, GPIO_OUTPUT_INACTIVE);
	gpio_pin_configure_dt(&green, GPIO_OUTPUT_INACTIVE);
	gpio_pin_configure_dt(&blue, GPIO_OUTPUT_INACTIVE);
	return 0;
}
