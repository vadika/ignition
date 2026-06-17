/*
 * vmid-reseed -- force a kernel CRNG reseed from a host-supplied seed.
 *
 * Reads a 37-byte frame from stdin: magic "VMID" | version 0x01 | 32-byte seed.
 * Mixes the seed into the kernel entropy pool (RNDADDENTROPY, crediting 256
 * bits) and forces an immediate CRNG reseed (RNDRESEEDCRNG). Run per connection
 * by socat:  socat VSOCK-LISTEN:9000,fork EXEC:/usr/bin/vmid-reseed
 *
 * On restore the host pushes a fresh seed so sibling clones (which resume with
 * identical snapshotted CRNG state) diverge before any userspace getrandom().
 * Built static against musl in the rootfs build container; needs linux-headers.
 */
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <fcntl.h>
#include <sys/ioctl.h>
#include <linux/random.h>

int main(void)
{
	unsigned char frame[37];
	size_t got = 0;
	while (got < sizeof(frame)) {
		ssize_t n = read(0, frame + got, sizeof(frame) - got);
		if (n <= 0) break;
		got += (size_t)n;
	}
	if (got != sizeof(frame)) return 1;
	if (memcmp(frame, "VMID", 4) != 0 || frame[4] != 0x01) return 1;

	struct {
		int entropy_count;          /* bits credited */
		int buf_size;               /* bytes in buf  */
		unsigned char buf[32];
	} pool;
	pool.entropy_count = 256;
	pool.buf_size = 32;
	memcpy(pool.buf, frame + 5, 32);

	int fd = open("/dev/random", O_RDWR);
	if (fd < 0) { perror("vmid-reseed: open /dev/random"); return 1; }
	if (ioctl(fd, RNDADDENTROPY, &pool) < 0) perror("vmid-reseed: RNDADDENTROPY");
	if (ioctl(fd, RNDRESEEDCRNG) < 0) perror("vmid-reseed: RNDRESEEDCRNG");
	close(fd);
	return 0;
}
