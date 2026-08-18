/* The wire half of the contract in buzzer/README.md — change that table and
 * this file and the live page's buzzer section together, or not at all.
 *
 * The page writes 1-2 byte opcodes to the vibe characteristic; unknown ones
 * are dropped so a newer page degrades to silence on new verbs, never to
 * garbage. Everything lands in one message queue drained by one work item,
 * so haptic.c only ever runs in one context — link events included, queued
 * here as internal pseudo-opcodes.
 *
 * The 90 s watchdog is the honesty rule pointed inward: the page heartbeats
 * every 10 s, so an amber wink means the link is up but nobody is home —
 * a throttled, discarded or wedged tab — which is exactly the state a wrist
 * would otherwise trust. */

#include <zephyr/kernel.h>
#include <zephyr/bluetooth/bluetooth.h>
#include <zephyr/bluetooth/conn.h>
#include <zephyr/bluetooth/gatt.h>
#include <zephyr/bluetooth/uuid.h>
#include <zephyr/logging/log.h>

#include <stdio.h>
#include <string.h>

#include "ble.h"
#include "haptic.h"
#include "led.h"

LOG_MODULE_REGISTER(ble, LOG_LEVEL_INF);

#define CONTRACT 1
#define FW_MAJOR 0
#define FW_MINOR 1

#define OP_HB        0x00
#define OP_STANDBY   0x01
#define OP_FINAL     0x02
#define OP_LOST_NEAR 0x03
#define OP_CANCEL    0x04
#define OP_TEST      0x05
#define OP_IDENTIFY  0x06
#define OP_EFFECT    0x07
/* Internal, never on the wire: link events from the conn callbacks. */
#define OP_LINK_LOST 0xf0
#define OP_LINK_BACK 0xf1

#define UUID_BASE(n) \
	BT_UUID_128_ENCODE(0xc0f1e000 + (n), 0xa7d3, 0x4b8f, 0x96f1, 0x2b73d3c5a001)

static const struct bt_uuid_128 svc_uuid = BT_UUID_INIT_128(UUID_BASE(0));
static const struct bt_uuid_128 vibe_uuid = BT_UUID_INIT_128(UUID_BASE(1));
static const struct bt_uuid_128 info_uuid = BT_UUID_INIT_128(UUID_BASE(2));

static ble_link_cb_t link_cb;
static char name[CONFIG_BT_DEVICE_NAME_MAX];

struct op_frame {
	uint8_t op;
	uint8_t param;
};

K_MSGQ_DEFINE(opq, sizeof(struct op_frame), 8, 1);

static void stale_fn(struct k_work *work);
static K_WORK_DELAYABLE_DEFINE(stale_work, stale_fn);

static void stale_fn(struct k_work *work)
{
	ARG_UNUSED(work);
	led_set(LED_STALE);
}

static void ops_fn(struct k_work *work);
static K_WORK_DEFINE(ops_work, ops_fn);

static void ops_fn(struct k_work *work)
{
	ARG_UNUSED(work);

	struct op_frame f;

	while (k_msgq_get(&opq, &f, K_NO_WAIT) == 0) {
		switch (f.op) {
		case OP_HB:
			break; /* the reschedule below is the whole point */
		case OP_STANDBY:
			haptic_play(HAPTIC_STANDBY);
			break;
		case OP_FINAL:
			haptic_play(HAPTIC_FINAL);
			break;
		case OP_LOST_NEAR:
			haptic_play(HAPTIC_LOST_NEAR);
			break;
		case OP_CANCEL:
			haptic_cancel();
			break;
		case OP_TEST:
			haptic_tour(f.param);
			break;
		case OP_IDENTIFY:
			led_identify();
			break;
		case OP_EFFECT:
			haptic_effect(f.param);
			break;
		case OP_LINK_LOST:
			haptic_play(HAPTIC_LINK_LOST);
			break;
		case OP_LINK_BACK:
			haptic_play(HAPTIC_LINK_BACK);
			break;
		default:
			break; /* newer page, older wearable: silence */
		}

		if (f.op <= OP_EFFECT) {
			/* Any write from the page proves somebody is home. */
			k_work_reschedule(&stale_work, K_SECONDS(90));
			led_set(LED_CONNECTED);
		}
	}
}

static void queue_op(uint8_t op, uint8_t param)
{
	struct op_frame f = {.op = op, .param = param};

	if (k_msgq_put(&opq, &f, K_NO_WAIT) == 0) {
		k_work_submit(&ops_work);
	}
}

static ssize_t vibe_write(struct bt_conn *conn, const struct bt_gatt_attr *attr,
			  const void *buf, uint16_t len, uint16_t offset,
			  uint8_t flags)
{
	ARG_UNUSED(conn);
	ARG_UNUSED(attr);
	ARG_UNUSED(flags);

	const uint8_t *b = buf;

	if (offset != 0 || len < 1 || len > 2) {
		return BT_GATT_ERR(BT_ATT_ERR_INVALID_ATTRIBUTE_LEN);
	}
	queue_op(b[0], len == 2 ? b[1] : 0);
	return len;
}

static ssize_t info_read(struct bt_conn *conn, const struct bt_gatt_attr *attr,
			 void *buf, uint16_t len, uint16_t offset)
{
	static const uint8_t info[3] = {CONTRACT, FW_MAJOR, FW_MINOR};

	return bt_gatt_attr_read(conn, attr, buf, len, offset, info,
				 sizeof(info));
}

BT_GATT_SERVICE_DEFINE(buzz_svc,
	BT_GATT_PRIMARY_SERVICE((void *)&svc_uuid),
	BT_GATT_CHARACTERISTIC(&vibe_uuid.uuid,
			       BT_GATT_CHRC_WRITE | BT_GATT_CHRC_WRITE_WITHOUT_RESP,
			       BT_GATT_PERM_WRITE, NULL, vibe_write, NULL),
	BT_GATT_CHARACTERISTIC(&info_uuid.uuid, BT_GATT_CHRC_READ,
			       BT_GATT_PERM_READ, info_read, NULL, NULL),
);

/* The 128-bit service UUID must ride in the AD itself — Chrome's service
 * filter can't see a scan response until it has already decided to show the
 * device. The name goes in the scan response where there is room for it. */
static const struct bt_data ad[] = {
	BT_DATA_BYTES(BT_DATA_FLAGS, (BT_LE_AD_GENERAL | BT_LE_AD_NO_BREDR)),
	BT_DATA_BYTES(BT_DATA_UUID128_ALL, UUID_BASE(0)),
};

static struct bt_data sd[] = {
	BT_DATA(BT_DATA_NAME_COMPLETE, NULL, 0),
};

/* 100-150 ms while somebody is likely looking for us; 2.5 s once the room
 * has clearly gone home. Units of 0.625 ms. */
static const struct bt_le_adv_param adv_fast =
	BT_LE_ADV_PARAM_INIT(BT_LE_ADV_OPT_CONN, 0x00a0, 0x00f0, NULL);
static const struct bt_le_adv_param adv_slow =
	BT_LE_ADV_PARAM_INIT(BT_LE_ADV_OPT_CONN, 0x0fa0, 0x0fa0, NULL);

static int adv_start(const struct bt_le_adv_param *param)
{
	sd[0].data = (const uint8_t *)name;
	sd[0].data_len = strlen(name);
	return bt_le_adv_start(param, ad, ARRAY_SIZE(ad), sd, ARRAY_SIZE(sd));
}

static void readv_fn(struct k_work *work);
static K_WORK_DEFINE(readv_work, readv_fn);

static void readv_fn(struct k_work *work)
{
	ARG_UNUSED(work);

	int err = adv_start(&adv_fast);

	if (err && err != -EALREADY) {
		LOG_WRN("re-advertise failed (%d)", err);
	}
}

static void connected(struct bt_conn *conn, uint8_t err)
{
	ARG_UNUSED(conn);

	if (err) {
		k_work_submit(&readv_work);
		return;
	}
	led_set(LED_CONNECTED);
	queue_op(OP_LINK_BACK, 0);
	k_work_reschedule(&stale_work, K_SECONDS(90));
	if (link_cb) {
		link_cb(true);
	}
}

static void disconnected(struct bt_conn *conn, uint8_t reason)
{
	ARG_UNUSED(conn);
	ARG_UNUSED(reason);

	led_set(LED_DROPPED);
	k_work_cancel_delayable(&stale_work);
	queue_op(OP_LINK_LOST, 0);
	k_work_submit(&readv_work);
	if (link_cb) {
		link_cb(false);
	}
}

BT_CONN_CB_DEFINE(conn_cbs) = {
	.connected = connected,
	.disconnected = disconnected,
};

void ble_adv_slow(void)
{
	bt_le_adv_stop();
	adv_start(&adv_slow);
}

int ble_start(ble_link_cb_t cb)
{
	int err;

	link_cb = cb;

	err = bt_enable(NULL);
	if (err) {
		return err;
	}

	/* CHF-XXXX from the identity address: stable across boots, unique
	 * across a drawer of wearables. */
	bt_addr_le_t addr;
	size_t count = 1;

	bt_id_get(&addr, &count);
	snprintf(name, sizeof(name), "CHF-%02X%02X", addr.a.val[1],
		 addr.a.val[0]);
	bt_set_name(name);

	err = adv_start(&adv_fast);
	if (err) {
		return err;
	}
	led_set(LED_ADVERTISING);
	LOG_INF("%s advertising", name);
	return 0;
}
