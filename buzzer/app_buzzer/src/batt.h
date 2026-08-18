#pragma once

#include <stdint.h>

/* Reads VBAT through the XIAO's divider every minute and feeds the standard
 * Battery Service, so the page can say "is it charged?" before a show. */
int batt_init(void);
