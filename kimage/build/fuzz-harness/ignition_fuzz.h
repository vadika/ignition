/* Mirror of crates/devices/src/fuzz/protocol.rs. Keep in sync by hand. */
#ifndef IGNITION_FUZZ_H
#define IGNITION_FUZZ_H
#include <stdint.h>

/* Fixed GPAs (mirror docs plan "Layout constants"; 16 KiB-aligned). */
#define IGNITION_FUZZ_CTRL_GPA   0x09200000UL
#define IGNITION_FUZZ_CTRL_SIZE  0x4000UL     /* 16 KiB, one guest page */
#define IGNITION_FUZZ_WIN_GPA    0x09204000UL
#define IGNITION_FUZZ_WIN_SIZE   0x200000UL   /* default 2 MiB */
#define IGNITION_FUZZ_COV_GPA    0x09404000UL  /* WIN_GPA + WIN_SIZE (0x200000) */
#define IGNITION_FUZZ_COV_SIZE   0x10000UL     /* 64 KiB, 8-bit edge counters */

/* Control-register offsets. */
#define REG_DOORBELL    0x00
#define REG_INPUT_LEN   0x04
#define REG_CRASH_CODE  0x08
#define REG_STATUS      0x0c

/* Doorbell commands. */
#define CMD_SNAPSHOT_ME 0x1u
#define CMD_DONE        0x2u
#define CMD_CRASH       0x3u

#endif
