/* VBAT reaches P0.31 through the board's 1 M / 510 k divider, switched in by
 * P0.14 (active low) so the megohms don't leak between reads. The divider
 * needs a settling moment after the switch — 1 M into the SAADC's sampling
 * cap — hence the small sleep, which is fine on the workqueue once a minute. */

#include <zephyr/kernel.h>
#include <zephyr/drivers/adc.h>
#include <zephyr/drivers/gpio.h>
#include <zephyr/bluetooth/services/bas.h>

#include "batt.h"

static const struct adc_dt_spec ch = ADC_DT_SPEC_GET_BY_IDX(DT_PATH(zephyr_user), 0);
static const struct gpio_dt_spec gate =
	GPIO_DT_SPEC_GET(DT_PATH(zephyr_user), vbatt_gpios);

/* Divider: VBAT * 510 / (1000 + 510). */
#define DIV_NUM 1510
#define DIV_DEN 510

/* A LiPo discharge curve, flat in the middle like they all are. Between the
 * points, linear is honest enough for "charge it tonight or not". */
static const struct {
	uint16_t mv;
	uint8_t pct;
} curve[] = {
	{4200, 100}, {4050, 90}, {3950, 80}, {3870, 70}, {3800, 60},
	{3750, 50},  {3700, 40}, {3650, 30}, {3600, 20}, {3500, 10},
	{3300, 0},
};

static uint8_t percent(int mv)
{
	if (mv >= curve[0].mv) {
		return 100;
	}
	for (size_t i = 1; i < ARRAY_SIZE(curve); i++) {
		if (mv >= curve[i].mv) {
			int span = curve[i - 1].mv - curve[i].mv;
			int up = mv - curve[i].mv;

			return curve[i].pct +
			       (curve[i - 1].pct - curve[i].pct) * up / span;
		}
	}
	return 0;
}

static int read_mv(void)
{
	int16_t sample;
	struct adc_sequence seq;
	int err;

	adc_sequence_init_dt(&ch, &seq);
	seq.buffer = &sample;
	seq.buffer_size = sizeof(sample);

	gpio_pin_set_dt(&gate, 1);
	k_msleep(5);
	err = adc_read_dt(&ch, &seq);
	gpio_pin_set_dt(&gate, 0);
	if (err) {
		return err;
	}

	int32_t mv = sample;

	err = adc_raw_to_millivolts_dt(&ch, &mv);
	if (err) {
		return err;
	}
	return mv * DIV_NUM / DIV_DEN;
}

static void sample_fn(struct k_work *work);
static K_WORK_DELAYABLE_DEFINE(sample_work, sample_fn);

static void sample_fn(struct k_work *work)
{
	ARG_UNUSED(work);

	int mv = read_mv();

	if (mv > 0) {
		bt_bas_set_battery_level(percent(mv));
	}
	k_work_reschedule(&sample_work, K_SECONDS(60));
}

int batt_init(void)
{
	if (!adc_is_ready_dt(&ch) || !gpio_is_ready_dt(&gate)) {
		return -ENODEV;
	}
	if (adc_channel_setup_dt(&ch)) {
		return -EIO;
	}
	if (gpio_pin_configure_dt(&gate, GPIO_OUTPUT_INACTIVE)) {
		return -EIO;
	}
	k_work_reschedule(&sample_work, K_NO_WAIT);
	return 0;
}
