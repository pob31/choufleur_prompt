/* DRV2605L over raw I2C registers, deliberately not Zephyr's ti,drv2605 driver:
 * boot-time auto-calibration, the cal-result cache and EN power-down discipline
 * are not reachable through the haptics subsystem API, and all three matter
 * here. The contract with the page is buzzer/README.md.
 *
 * Power model: the wearable spends almost all of its life not vibrating, so the
 * chip spends almost all of its life powered down. With EN wired (bare-chip
 * builds) that is hard shutdown; the tryout breakouts expose no EN, so there it
 * is the standby bit (~5 µA) — same discipline, one pin less. Neither state is
 * trusted to preserve registers, so every wake rewrites the handful that matter
 * from RAM (~1 ms, invisible against the 100 ms budget).
 *
 * The actuator is a devicetree matter: actuator-mode "LRA" or "ERM" in
 * app.overlay picks the feedback topology and effect library; the ratings and
 * resonant seed live beside it. Swapping actuators for the tryout is numbers,
 * not code.
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

/* In a sequencer slot, MSB set means "wait (value & 0x7f) * 10 ms". */
#define SEQ_WAIT(ms) (0x80 | ((ms) / 10))

static const struct i2c_dt_spec bus = I2C_DT_SPEC_GET(DT_NODELABEL(drv2605));
/* EN is optional: absent on the tryout breakouts, wired on bare-chip builds. */
static const struct gpio_dt_spec en =
	GPIO_DT_SPEC_GET_OR(DT_NODELABEL(drv2605), en_gpios, {0});

/* The enum token is lowercased in the generated devicetree ("LRA" -> lra); asked
 * for LRA in capitals this macro quietly answers 0, and the chip spends the
 * night in ERM mode driving an LRA. It did. */
#define IS_LRA DT_ENUM_HAS_VALUE(DT_NODELABEL(drv2605), actuator_mode, lra)
BUILD_ASSERT(IS_LRA || DT_ENUM_HAS_VALUE(DT_NODELABEL(drv2605), actuator_mode, erm),
	     "actuator-mode must be LRA or ERM");

/* Feedback register base: N_ERM_LRA per the fitted actuator, brake factor 3x,
 * loop gain high — the datasheet's recommended starting points. Auto-cal
 * rewrites the BEMF gain bits; the calibrated value replaces this after boot. */
#define FEEDBACK_DEFAULT (IS_LRA ? 0xb6 : 0x36)

/* Library 6 is the LRA-tuned effect set; 2 (TS2200 B) suits a 3 V ERM — the
 * ERM libraries 1-5 differ by voltage class, worth a lap of the tryout. */
#define LIBRARY_SEL (IS_LRA ? 6 : 2)

#define RATED_MV DT_PROP(DT_NODELABEL(drv2605), vib_rated_mv)
#define OD_MV    DT_PROP(DT_NODELABEL(drv2605), vib_overdrive_mv)
#define LRA_HZ   DT_PROP(DT_PATH(zephyr_user), lra_freq_hz)

/* Register LSBs from the datasheet: rated 20.58 mV, overdrive clamp 21.22 mV.
 * For a closed-loop LRA both are scaled by sqrt(1 - (4*t_sample + 300 us) * f)
 * — the back-EMF sampling dead time, 1.5 ms per cycle at the default t_sample —
 * which is 0.80 at 235 Hz. Left out, the chip is asked for four fifths of the
 * actuator's rating, never reaches its own target, and reports 0xE8: cannot
 * converge. The PUI found that; the ELV1411A at 200 Hz had just enough margin.
 * DRV2605L datasheet §8.5.2.1. Recomputed whenever the resonance seed moves. */
static uint8_t rated_reg;
static uint8_t od_reg;

static uint32_t isqrt(uint32_t n)
{
	uint32_t r = 0, bit = 1u << 30;

	while (bit > n) {
		bit >>= 2;
	}
	while (bit) {
		if (n >= r + bit) {
			n -= r + bit;
			r = (r >> 1) + bit;
		} else {
			r >>= 1;
		}
		bit >>= 2;
	}
	return r;
}

/* Opcode 0x09 overrides the overlay's rated voltage for the session (tryout
 * only): rated_mv = param * 20, overdrive kept at 1.25x. A ratings sweep from
 * the page says whether a unit is being asked for more than its supply gives. */
static uint32_t rated_mv = RATED_MV;
static uint32_t od_mv = OD_MV;
static uint32_t seed_hz = LRA_HZ;

static void ratings_for(uint32_t hz)
{
	/* s = 1000 * sqrt(1 - 0.0015 * hz); an ERM has no dead time (s = 1000). */
	uint32_t s = 1000;

	seed_hz = hz;
	if (IS_LRA) {
		uint32_t f2 = 1000 - MIN(999, (1500 * hz) / 1000); /* x1000 */

		s = isqrt(f2 * 1000);
	}
	rated_reg = MIN(255, (rated_mv * 100000) / (2058 * s));
	od_reg = MIN(255, (od_mv * 100000) / (2122 * s));
}

void haptic_set_rated(uint8_t mv_over_20)
{
	if (mv_over_20 == 0) {
		rated_mv = RATED_MV;
		od_mv = OD_MV;
	} else {
		rated_mv = (uint32_t)mv_over_20 * 20;
		od_mv = rated_mv * 5 / 4;
	}
	ratings_for(seed_hz);
	LOG_INF("rated %u mV od %u mV -> rated %02x od %02x", rated_mv, od_mv,
		rated_reg, od_reg);
}

/* DRIVE_TIME (CONTROL1 bits 4:0) is half the LRA period, offset per datasheet:
 * (half-period-us - 500) / 100. Bit 7 keeps STARTUP_BOOST on. LRA only; an
 * ERM keeps the register's default. */
#define DRIVE_TIME_FOR(hz) MIN(31, ((500000 / (hz)) - 500) / 100)

/* Seeded from the overlay; opcode 0x08 with a parameter re-seeds it, so the
 * tryout can sweep for an actuator's real resonance from the page. */
static uint8_t drive_time = DRIVE_TIME_FOR(LRA_HZ);

/* Auto-cal results, read once at boot, rewritten on every wake. */
static uint8_t cal_feedback = FEEDBACK_DEFAULT;
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
	if (en.port != NULL) {
		if (gpio_pin_set_dt(&en, 1)) {
			return -EIO;
		}
		k_msleep(1); /* 250 us minimum from EN to I2C-ready */
	}

	wr(REG_MODE, MODE_ACTIVE);
	wr(REG_FEEDBACK, cal_feedback);
	wr(REG_LIBRARY, LIBRARY_SEL);
	wr(REG_RATED, rated_reg);
	wr(REG_OD_CLAMP, od_reg);
	if (IS_LRA) {
		wr(REG_CONTROL1, 0x80 | drive_time); /* STARTUP_BOOST on */
	}
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
	if (en.port != NULL) {
		gpio_pin_set_dt(&en, 0);
	}
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

/* Auto-calibration against the overlay's ratings: a short twitch. The datasheet
 * wants the actuator mounted as worn, which is its normal state. On failure the
 * recommended defaults still drive the actuator — worse crispness, not silence —
 * and the info characteristic says so, so the page can too. Assumes awake. */
static bool autocal(void)
{
	/* Start from the datasheet base each time; BEMF gain is an output. */
	cal_feedback = FEEDBACK_DEFAULT;
	wr(REG_FEEDBACK, cal_feedback);
	wr(REG_MODE, MODE_AUTOCAL);
	wr(REG_GO, 1);

	/* The chip takes ~1.2 s by default, more with a weak start; the first
	 * night's firmware waited 1.5 s and gave up while it was still going. */
	uint8_t go = 1;

	for (int i = 0; i < 150 && go; i++) {
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
		calibrated = false;
		LOG_WRN("auto-cal failed (status %02x), using defaults", status);
	}
	wr(REG_MODE, MODE_ACTIVE);
	return calibrated;
}

bool haptic_calibrated(void)
{
	return calibrated;
}

/* Opcode 0x08: the tryout swaps actuators on a cable, and a swap deserves a
 * fresh calibration without a power cycle. Blocks the workqueue up to ~3 s.
 * hz_half: 0 keeps the current resonance seed; otherwise the seed becomes
 * hz_half * 2 Hz (75 -> 150 Hz, 118 -> 236 Hz) — a sweep finds an actuator's
 * real resonance without a rebuild. */
int haptic_calibrate(uint8_t hz_half)
{
	k_work_cancel_delayable(&tour_work);
	if (hz_half >= 50 && IS_LRA) { /* 100 Hz floor: below it the maths wraps */
		drive_time = DRIVE_TIME_FOR((uint32_t)hz_half * 2);
		ratings_for((uint32_t)hz_half * 2);
		LOG_INF("resonance seed %u Hz (drive_time %u rated %02x od %02x)",
			hz_half * 2, drive_time, rated_reg, od_reg);
	}
	if (wake()) {
		return -EIO;
	}
	if (IS_LRA) {
		wr(REG_CONTROL1, 0x80 | drive_time);
	}
	wr(REG_RATED, rated_reg);
	wr(REG_OD_CLAMP, od_reg);
	bool ok = autocal();

	k_work_reschedule(&sleep_work, K_SECONDS(3));
	return ok ? 0 : -EIO;
}

int haptic_init(void)
{
	ratings_for(LRA_HZ);
	LOG_INF("ratings: rated %02x od %02x drive_time %u (%u Hz)",
		rated_reg, od_reg, drive_time, LRA_HZ);
	if (!i2c_is_ready_dt(&bus)) {
		return -ENODEV;
	}
	if (en.port != NULL) {
		if (!gpio_is_ready_dt(&en) ||
		    gpio_pin_configure_dt(&en, GPIO_OUTPUT_INACTIVE)) {
			return -EIO;
		}
	}
	/* A warm reboot — DFU, a reset tap — leaves the chip wherever the last
	 * firmware left it, mid-waveform included, and calibration started from
	 * there has failed on the bench. DEV_RESET is a power-on reset in a bit;
	 * it self-clears when done. */
	if (en.port != NULL) {
		gpio_pin_set_dt(&en, 1);
		k_msleep(1);
	}
	wr(REG_MODE, 0x80);
	for (int i = 0; i < 20; i++) {
		uint8_t mode = 0x80;

		k_msleep(1);
		if (!i2c_reg_read_byte_dt(&bus, REG_MODE, &mode) && !(mode & 0x80)) {
			break;
		}
	}
	awake = false;
	if (wake()) {
		return -EIO;
	}
	/* The first run after power-up has failed fast on the bench — the chip
	 * answering before it was really ready. One more try, unhurried. */
	if (!autocal()) {
		k_msleep(500);
		autocal();
	}
	sleep_now();
	return 0;
}
