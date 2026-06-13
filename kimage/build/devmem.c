/*
 * devmem — minimal busybox-compatible /dev/mem poke tool.
 *
 *   devmem ADDRESS [WIDTH [VALUE]]
 *
 * WIDTH is in bits: 8, 16, 32 (default), or 64. With VALUE it writes; without
 * it reads and prints the value as hex. mmap()s the containing page of /dev/mem
 * and does a single naturally-sized access at the target offset — the access
 * width matters for MMIO device registers, which is why `dd` is not a substitute.
 *
 * Built static against musl in the rootfs build container; installed at
 * /usr/bin/devmem. Used by /etc/local.d/boottime.start to signal the VMM's
 * boot_timer device (8-bit write of 123 to BOOT_TIMER_ADDR).
 */
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <fcntl.h>
#include <unistd.h>
#include <sys/mman.h>

int main(int argc, char **argv)
{
	if (argc < 2) {
		fprintf(stderr, "Usage: %s ADDRESS [WIDTH [VALUE]]\n", argv[0]);
		return 1;
	}

	off_t    target = (off_t)strtoull(argv[1], NULL, 0);
	int      width  = (argc > 2) ? atoi(argv[2]) : 32;
	int      writing = (argc > 3);
	uint64_t value  = writing ? strtoull(argv[3], NULL, 0) : 0;

	if (width != 8 && width != 16 && width != 32 && width != 64) {
		fprintf(stderr, "devmem: bad width %d (use 8/16/32/64)\n", width);
		return 1;
	}

	int fd = open("/dev/mem", (writing ? O_RDWR : O_RDONLY) | O_SYNC);
	if (fd < 0) { perror("devmem: open /dev/mem"); return 1; }

	long  ps   = sysconf(_SC_PAGE_SIZE);
	off_t base = target & ~(off_t)(ps - 1);
	off_t off  = target - base;

	void *map = mmap(NULL, ps + off, writing ? (PROT_READ | PROT_WRITE) : PROT_READ,
	                 MAP_SHARED, fd, base);
	if (map == MAP_FAILED) { perror("devmem: mmap"); return 1; }

	volatile void *va = (char *)map + off;
	uint64_t out = 0;

	if (writing) {
		switch (width) {
		case 8:  *(volatile uint8_t  *)va = (uint8_t)value;  break;
		case 16: *(volatile uint16_t *)va = (uint16_t)value; break;
		case 32: *(volatile uint32_t *)va = (uint32_t)value; break;
		case 64: *(volatile uint64_t *)va = value;           break;
		}
		/* Do not read back: the target may be write-only MMIO. */
		printf("Written 0x%llX\n", (unsigned long long)value);
	} else {
		switch (width) {
		case 8:  out = *(volatile uint8_t  *)va; break;
		case 16: out = *(volatile uint16_t *)va; break;
		case 32: out = *(volatile uint32_t *)va; break;
		case 64: out = *(volatile uint64_t *)va; break;
		}
		printf("0x%0*llX\n", width / 4, (unsigned long long)out);
	}

	munmap(map, ps + off);
	close(fd);
	return 0;
}
