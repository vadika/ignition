// Plain-C bridge over vmnet.framework. Creates the dispatch queue + blocks
// internally so Rust only deals with function pointers.
#include <vmnet/vmnet.h>
#include <dispatch/dispatch.h>
#include <xpc/xpc.h>
#include <stdio.h>
#include <string.h>
#include <stdint.h>
#include <stdlib.h>

typedef void (*ig_frame_cb)(void *ctx, const uint8_t *data, uintptr_t len);

struct ig_vmnet {
    interface_ref iface;
    dispatch_queue_t queue;
    ig_frame_cb cb;
    void *ctx;
    uint32_t max_packet;
};

// Start vmnet shared (NAT) mode. On success returns a handle and fills mac_out
// (6 bytes). Blocks until the async start handler fires. Returns NULL on failure.
struct ig_vmnet *ig_vmnet_start(uint8_t mac_out[6], ig_frame_cb cb, void *ctx) {
    struct ig_vmnet *h = calloc(1, sizeof(*h));
    h->cb = cb;
    h->ctx = ctx;
    h->queue = dispatch_queue_create("ignition.vmnet", DISPATCH_QUEUE_SERIAL);

    xpc_object_t desc = xpc_dictionary_create(NULL, NULL, 0);
    xpc_dictionary_set_uint64(desc, vmnet_operation_mode_key, VMNET_SHARED_MODE);

    dispatch_semaphore_t sem = dispatch_semaphore_create(0);
    __block vmnet_return_t start_status = VMNET_FAILURE;
    h->iface = vmnet_start_interface(desc, h->queue,
        ^(vmnet_return_t status, xpc_object_t params) {
            start_status = status;
            if (status == VMNET_SUCCESS) {
                const char *mac = xpc_dictionary_get_string(params, vmnet_mac_address_key);
                // mac is "xx:xx:xx:xx:xx:xx"
                unsigned m[6];
                if (mac && sscanf(mac, "%x:%x:%x:%x:%x:%x",
                        &m[0],&m[1],&m[2],&m[3],&m[4],&m[5]) == 6) {
                    for (int i = 0; i < 6; i++) mac_out[i] = (uint8_t)m[i];
                }
                h->max_packet = (uint32_t)xpc_dictionary_get_uint64(params,
                    vmnet_max_packet_size_key);
            }
            dispatch_semaphore_signal(sem);
        });
    dispatch_semaphore_wait(sem, DISPATCH_TIME_FOREVER);
    xpc_release(desc);
    if (h->iface == NULL || start_status != VMNET_SUCCESS) {
        free(h);
        return NULL;
    }

    // Deliver received frames via the callback.
    vmnet_interface_set_event_callback(h->iface, VMNET_INTERFACE_PACKETS_AVAILABLE,
        h->queue, ^(interface_event_t ev __attribute__((unused)), xpc_object_t einfo) {
            int max = (int)xpc_dictionary_get_uint64(einfo,
                vmnet_estimated_packets_available_key);
            for (int i = 0; i < max; i++) {
                uint8_t buf[65536];
                struct iovec iov = { .iov_base = buf, .iov_len = sizeof(buf) };
                struct vmpktdesc pd = { .vm_pkt_size = sizeof(buf),
                    .vm_pkt_iov = &iov, .vm_pkt_iovcnt = 1, .vm_flags = 0 };
                int count = 1;
                if (vmnet_read(h->iface, &pd, &count) != VMNET_SUCCESS || count < 1) break;
                h->cb(h->ctx, buf, pd.vm_pkt_size);
            }
        });
    return h;
}

// Send one frame. Returns 0 on success.
int ig_vmnet_write(struct ig_vmnet *h, const uint8_t *data, uintptr_t len) {
    struct iovec iov = { .iov_base = (void *)data, .iov_len = len };
    struct vmpktdesc pd = { .vm_pkt_size = len, .vm_pkt_iov = &iov,
        .vm_pkt_iovcnt = 1, .vm_flags = 0 };
    int count = 1;
    return vmnet_write(h->iface, &pd, &count) == VMNET_SUCCESS ? 0 : -1;
}
