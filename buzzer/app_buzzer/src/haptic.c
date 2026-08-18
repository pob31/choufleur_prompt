/* DRV2605L over raw I2C registers, deliberately not Zephyr's ti,drv2605 driver:
 * boot-time auto-calibration, the cal-result cache and EN power-down discipline
 * are not reachable through the haptics subsystem API, and all three matter
 * here. The contract with the page is buzzer/README.md.
 *
 * Power model: the wearable spends almost all of its life not vibrating, so the
 * chip spends almost all of its life with EN low — shutdown, not standby.
 * Shutdown is not trusted to preserve registers, so every wake rewrites the
 * handful that matter from RAM (~1 ms, invisible against the 100 ms budget).
 *
 * Threading: every entry point runs on the system workqueue (ble.c funnels all
 * opcodes and link events through one work item) or in main() before BLE
 * starts. One context, no locks.
 */

#include <zephyr/kernel.h>
#include <zephyr/drivers/gpio.h>
#include <zephyr/drivers/i2c.h>
#include <zephyr/logging/log.h>

#include "haptic.h"

LOG_MODULE_REGISTER(haptic, LOG_LEVEL_INF);

#define REG_STATUS     0x00
#define REG_MODE       0x01
#define REG_LIBRARY    0x03
#define REG_SEQ        0x04 /* eight slots, 0x04..0x0b */
#define REG_GO         0x0c
#define REG_RATED      0x16
#define REG_OD_CLAMP   0x17
#define REG_A_CAL_COMP 0x18
#define REG_A_CAL_BEMF 0x19
#define REG_FEEDBACK   0x1a
#define REG_CONTROL1   0x1b

#define MODE_ACTIVE  0x00
#define MODE_AUTOCAL 0x07
#define MODE_STANDBY 0x40

/* Feedback register base for an LRA: N_ERM_LRA set, brake factor 3x, loop gain
 * high — the datasheet's recommended starting point. Auto-cal rewrites the
 * BEMF gain bits; the calibrated value replaces this after boot. */
#define FEEDBACK_LRA 0xb6

/* Library 6 is the LRA-tuned effect set. */
#define LIBRARY_LRA 6

/* In a sequencer slot, MSB set means "wait (value & 0x7f) * 10 ms". */
#define SEQ_WAIT(ms) (0x80 | ((ms) / 10))

static const struct i2c_dt_spec bus = I2C_DT_SPEC_GET(DT_NODELABEL(drv2605));
static const struct gpio_dt_spec en = GPIO_DT_SPEC_GET(DT_NODELABEL(drv2605), en_gpios);

#define RATED_MV DT_PROP(DT_NODELABEL(drv2605), vib_rated_mv)
#define OD_MV    DT_PROP(DT_NODELABEL(drv2605), vib_overdrive_mv)
#define LRA_HZ   DT_PROP(DT_PATH(zephyr_user), lra_freq_hz)

/* Register LSBs from the datasheet: rated ~20.58 mV, overdrive clamp ~21.22 mV.
 * Close enough to seed auto-calibration, which then trims the back-EMF gain —
 * for exact closed-loop math see DRV2605L datasheet §8.5.2.1. */
#define RATED_REG MIN(255, (RATED_MV * 100) / 2058)
#define OD_REG    MIN(255, (OD_MV * 100) / 2122)

/* DRIVE_TIME (CONTROL1 bits 4:0) is half the LRA period, offset per datasheet:
 * (half-period-us - 500) / 100. Bit 7 keeps STARTUP_BOOST on. */
#define DRIVE_TIME MIN(31, ((500000 / LRA_HZ) - 500) / 100)
#define CONTROL1_VAL (0x80 | DRIVE_TIME)

/* Auto-cal results, read once at boot, rewritten on every wake. */
static uint8_t cal_feedback = FEEDBACK_LRA;
static uint8_t cal_comp;
static uint8_t cal_bemf;
static bool calibrated;
static bool awake;

static const uint8_t pat_standby[]   = {7, 0};                              /* soft bump 100% */
static const uint8_t pat_final[]     = {10, 0};                             /* double click 100% */
static const uint8_t pat_lost[]      = {26, SEQ_WAIT(120), 26, SEQ_WAIT(120), 26, 0}; /* sharp tick 60% x3 */
static const uint8_t pat_link_lost[] = {15, 0};                             /* 750 ms alert 100% */
static const uint8_t pat_link_back[] = {9, SEQ_WAIT(60), 9, 0};             /* soft bump 30% x2 */

static int wr(uint8_t reg, uint8_t val)
{
	int err = i2c_reg_write_byte_dt(&bus, reg, val);

	if (err) {
		LOG_WRN("write %02x=%02x failed (%d)", reg, val, err);
	}
	return err;
}

static int wake(void)
{
	if (awake) {
		return 0;
	}
	if (gpio_pin_set_dt(&en, 1)) {
		return -EIO;
	}
	k_msleep(1); /* 250 us minimum from EN to I2C-ready; a round millisecond */

	wr(REG_MODE, MODE_ACTIVE);
	wr(REG_FEEDBACK, cal_feedback);
	wr(REG_LIBRARY, LIBRARY_LRA);
	wr(REG_RATED, RATED_REG);
	wr(REG_OD_CLAMP, OD_REG);
	wr(REG_CONTROL1, CONTROL1_VAL);
	if (calibrated) {
		wr(REG_A_CAL_COMP, cal_comp);
		wr(REG_A_CAL_BEMF, cal_bemf);
	}
	awake = true;
	return 0;
}

static void sleep_now(void)
{
	if (!awake) {
		return;
	}
	wr(REG_GO, 0);
	wr(REG_MODE, MODE_STANDBY);
	gpio_pin_set_dt(&en, 0);
	awake = false;
}

/* Longest pattern is the 750 ms alert; three seconds covers any of them plus
 * the tour's gaps before the driver is powered back down. */
static void sleep_fn(struct k_work *work);
static K_WORK_DELAYABLE_DEFINE(sleep_work, sleep_fn);

static void sleep_fn(struct k_work *work)
{
	ARG_UNUSED(work);
	sleep_now();
}

static void play_seq(const uint8_t *seq, size_t len)
{
	if (wake()) {
		return;
	}
	i2c_burst_write_dt(&bus, REG_SEQ, seq, len);
	wr(REG_GO, 1);
	k_work_reschedule(&sleep_work, K_SECONDS(3));
}

void haptic_play(enum haptic_pattern p)
{
	switch (p) {
	case HAPTIC_STANDBY:
		play_seq(pat_standby, sizeof(pat_standby));
		break;
	case HAPTIC_FINAL:
		play_seq(pat_final, sizeof(pat_final));
		break;
	case HAPTIC_LOST_NEAR:
		play_seq(pat_lost, sizeof(pat_lost));
		break;
	case HAPTIC_LINK_LOST:
		play_seq(pat_link_lost, sizeof(pat_link_lost));
		break;
	case HAPTIC_LINK_BACK:
		play_seq(pat_link_back, sizeof(pat_link_back));
		break;
	}
}

void haptic_effect(uint8_t effect)
{
	uint8_t seq[2] = {effect & 0x7f, 0};

	if (seq[0] == 0) {
		return;
	}
	play_seq(seq, sizeof(seq));
}

/* The tour walks an operator through the vocabulary: standby, final,
 * lost_near, 700 ms apart. Learnt once from the panel, then trusted. */
static uint8_t tour_step;
static void tour_fn(struct k_work *work);
static K_WORK_DELAYABLE_DEFINE(tour_work, tour_fn);

static void tour_fn(struct k_work *work)
{
	ARG_UNUSED(work);

	static const enum haptic_pattern steps[] = {
		HAPTIC_STANDBY, HAPTIC_FINAL, HAPTIC_LOST_NEAR,
	};

	if (tour_step >= ARRAY_SIZE(steps)) {
		return;
	}
	haptic_play(steps[tour_step]);
	tour_step++;
	if (tour_step < ARRAY_SIZE(steps)) {
		k_work_reschedule(&tour_work, K_MSEC(700));
	}
}

void haptic_tour(uint8_t which)
{
	switch (which) {
	case 0:
		tour_step = 0;
		k_work_reschedule(&tour_work, K_NO_WAIT);
		break;
	case 1:
		haptic_play(HAPTIC_STANDBY);
		break;
	case 2:
		haptic_play(HAPTIC_FINAL);
		break;
	case 3:
		haptic_play(HAPTIC_LOST_NEAR);
		break;
	default:
		break;
	}
}

void haptic_cancel(void)
{
	k_work_cancel_delayable(&tour_work);
	k_work_cancel_delayable(&sleep_work);
	sleep_now();
}

void haptic_off(void)
{
	haptic_cancel();
}

int haptic_init(void)
{
	if (!i2c_is_ready_dt(&bus) || !gpio_is_ready_dt(&en)) {
		return -ENODEV;
	}
	if (gpio_pin_configure_dt(&en, GPIO_OUTPUT_INACTIVE)) {
		return -EIO;
	}
	if (wake()) {
		return -EIO;
	}

	/* Auto-calibration against the overlay's ratings: a short twitch at
	 * every boot. The datasheet wants the actuator mounted as worn, which
	 * is its normal state. On failure the recommended defaults still
	 * drive the LRA — worse crispness, not silence — and the bench test
	 * in the README is where a rattle gets noticed. */
	wr(REG_MODE, MODE_AUTOCAL);
	wr(REG_GO, 1);

	uint8_t go = 1;

	for (int i = 0; i < 75 && go; i++) {
		k_msleep(20);
		if (i2c_reg_read_byte_dt(&bus, REG_GO, &go)) {
			go = 1;
		}
	}

	uint8_t status = 0x08;

	i2c_reg_read_byte_dt(&bus, REG_STATUS, &status);
	if (!go && !(status & 0x08)) {
		i2c_reg_read_byte_dt(&bus, REG_FEEDBACK, &cal_feedback);
		i2c_reg_read_byte_dt(&bus, REG_A_CAL_COMP, &cal_comp);
		i2c_reg_read_byte_dt(&bus, REG_A_CAL_BEMF, &cal_bemf);
		calibrated = true;
		LOG_INF("auto-cal ok (fb %02x comp %02x bemf %02x)",
			cal_feedback, cal_comp, cal_bemf);
	} else {
		LOG_WRN("auto-cal failed (status %02x), using defaults", status);
	}

	sleep_now();
	return 0;
}
