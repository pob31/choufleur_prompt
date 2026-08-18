#pragma once

#include <stdbool.h>

/* Called from the system workqueue on every link change; main() hangs the
 * idle policy (slow advertising, eventual poweroff) off it. */
typedef void (*ble_link_cb_t)(bool connected);

int ble_start(ble_link_cb_t cb);

/* Drop to slow advertising (~2.5 s interval) — nobody has come for a while. */
void ble_adv_slow(void);
