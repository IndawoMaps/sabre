// C ABI of sabre-mobile. See crates/mobile/src/ffi.rs.
#pragma once
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// Start the tile server on 127.0.0.1, serving file:// sources inside
// file_root. Returns JSON, {"port":…,"token":…,"base_url":…} or
// {"error":…}; free it with sabre_string_free. Never NULL.
char *sabre_start(const char *file_root, uint64_t cache_bytes);

// Stop the server, if one is running.
void sabre_stop(void);

void sabre_string_free(char *s);

#ifdef __cplusplus
}
#endif
