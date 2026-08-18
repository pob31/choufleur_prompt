cmake_minimum_required(VERSION 3.20.0)

find_package(Zephyr REQUIRED HINTS $ENV{ZEPHYR_BASE})
project(app_buzzer)

target_sources(app PRIVATE
  src/main.c
  src/ble.c
  src/haptic.c
  src/batt.c
  src/led.c
)
