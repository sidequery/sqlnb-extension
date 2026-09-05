#pragma once

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

void *sidequery_http_start(const char *config_json, char **error_out);
uint16_t sidequery_http_port(void *server);
void sidequery_http_stop(void *server);
void sidequery_http_string_free(char *value);
uint16_t sidequery_http_unused_port(void);

#ifdef __cplusplus
}
#endif
