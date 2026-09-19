#pragma once

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

void *sidequery_http_start(const char *config_json, char **error_out);
typedef char *(*sidequery_query_callback)(void *, uint64_t, const char *, uint64_t, uint64_t);
typedef void (*sidequery_interrupt_callback)(void *, uint64_t);
typedef void (*sidequery_result_free_callback)(char *);
void *sidequery_http_start_with_query(const char *config_json, void *context,
                                      sidequery_query_callback query,
                                      sidequery_interrupt_callback interrupt,
                                      sidequery_result_free_callback free_result, char **error_out);
uint16_t sidequery_http_port(void *server);
void sidequery_http_stop(void *server);
void sidequery_http_string_free(char *value);
uint16_t sidequery_http_unused_port(void);

#ifdef __cplusplus
}
#endif
