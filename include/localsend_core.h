#ifndef LOCALSEND_CORE_H
#define LOCALSEND_CORE_H

#include <stdbool.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef void (*LocalSendEventCallback)(char *payload);

void rust_flutter_free_string(char *value);
void rust_flutter_start_discovery(
    uint16_t port,
    const char *user_alias,
    LocalSendEventCallback on_event
);
void rust_flutter_discover_once(uint16_t port, const char *user_alias);
void rust_flutter_start_file_server(
    uint16_t port,
    const char *save_dir,
    LocalSendEventCallback on_event
);
char *rust_flutter_send_file(
    const char *target_ip,
    uint16_t port,
    const char *file_path,
    uint64_t parallel_count,
    LocalSendEventCallback on_event
);
bool rust_flutter_cancel_transfer(const char *transfer_id);
bool rust_flutter_retry_transfer(const char *transfer_id);
bool rust_flutter_configure_device_identity(
    const char *device_id,
    const char *device_name
);
bool rust_flutter_accept_transfer(const char *offer_id, const char *destination);
bool rust_flutter_reject_transfer(const char *offer_id);
void rust_flutter_stop_server(void);

#ifdef __cplusplus
}
#endif

#endif
