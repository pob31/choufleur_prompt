/* Boot, calibrate, advertise, and otherwise stay out of the way — everything
 * after main() is work items. The idle policy is the only thing living here:
 * a wearable nobody has paired for half an hour advertises slowly; one nobody
 * has paired for four hours switches itself off entirely (System OFF, ~µA),
 * honest about being asleep — a tap on reset wakes it. */

#include <zephyr/kernel.h>
#include <zephyr/sys/poweroff.h>
#include <zephyr/logging/log.h>

#include "batt.h"
#include "ble.h"
#include "haptic.h"
#include "led.h"

LOG_MODULE_REGISTER(main, LOG_LEVEL_INF);

static void slow_fn(struct k_work *work)
{
	ARG_UNUSED(work);
	ble_adv_slow();
}
static K_WORK_DELAYABLE_DEFINE(slow_work, slow_fn);

static void off_fn(struct k_work *work)
{
	ARG_UNUSED(work);
	haptic_off();
	led_set(LED_OFF);
	sys_poweroff();
}
static K_WORK_DELAYABLE_DEFINE(off_work, off_fn);

static void arm_idle(void)
{
	k_work_reschedule(&slow_work, K_MINUTES(30));
	k_work_reschedule(&off_work, K_HOURS(4));
}

static void on_link(bool connected)
{
	if (connected) {
		k_work_cancel_delayable(&slow_work);
		k_work_cancel_delayable(&off_work);
	} else {
		arm_idle();
	}
}

int main(void)
{
	if (led_init()) {
		LOG_ERR("led init failed");
	}
	if (haptic_init()) {
		/* A wearable that cannot vibrate is a strap. Advertise anyway
		 * so the page can at least say what is wrong. */
		LOG_ERR("haptic init failed");
	}
	if (batt_init()) {
		LOG_ERR("batt init failed");
	}
	if (ble_start(on_link)) {
		LOG_ERR("ble start failed");
		return 0;
	}
	arm_idle();
	return 0;
}
