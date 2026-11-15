#pragma once

#include <stdint.h>
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

void *sidequery_http_start(const char *config_json, char **error_out);
typedef char *(*sidequery_query_callback)(void *, uint64_t, const char *, uint64_t, uint64_t, bool);
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
// A null previous validates a new notebook and starts its content version at 1.
char *sidequery_duckdb_ui_next_version(const char *previous, const char *edited, char **error_out);

// Result cache (docs/results.md). Arrow structs follow the C Data Interface;
// a function that takes one releases it. Errors are written to *error_out, to
// be freed with sidequery_http_string_free.
struct ArrowSchema;
struct ArrowArray;
struct ArrowArrayStream;
typedef struct sidequery_result_writer sidequery_result_writer;
// Each database that loads the extension retains the cache; the last release
// removes every result and this process's session directory.
void sidequery_results_retain(void);
void sidequery_results_release(void);
// Validates a new result's id, meta and schema before its statement runs.
bool sidequery_result_check(const char *id, const char *meta, struct ArrowSchema *schema,
                            char **error_out);
// The limits of a caller that names none: bytes kept for one result, bytes
// kept for all results, and seconds a result is kept after its last use.
void sidequery_result_defaults(uint64_t *max_result_bytes, uint64_t *max_cache_bytes,
                               uint64_t *ttl_seconds);
// Registers an unreadable result; finish makes it readable, abort discards it.
// Results older than max_cache_bytes allows are evicted here. The result
// expires ttl_seconds after it was finished or last read; 0 keeps it until
// the cache is released.
sidequery_result_writer *sidequery_result_begin(const char *id, const char *meta,
                                                struct ArrowSchema *schema, uint64_t head_rows,
                                                uint64_t max_result_bytes,
                                                uint64_t max_cache_bytes, uint64_t ttl_seconds,
                                                char **error_out);
// Stores the rows of a struct array whose fields match the schema, from any
// thread. The result reads back in the order of the keys (major, then minor),
// whatever order the batches were stored in. Returns false when the batch was
// not stored; the result is then the batches before the first one missing.
bool sidequery_result_append(sidequery_result_writer *writer, struct ArrowArray *batch,
                             uint64_t major, uint64_t minor);
// Marks the batch with this key as one the caller could not store (limit
// reason "io" unless a limit already stopped storing).
void sidequery_result_missing(sidequery_result_writer *writer, uint64_t major, uint64_t minor);
// rows counts every row the statement produced; rows_exact is false when the
// statement stopped reading early and rows is only a lower bound.
void sidequery_result_finish(sidequery_result_writer *writer, uint64_t rows, bool rows_exact);
void sidequery_result_abort(sidequery_result_writer *writer);
// Exports one row per readable result, newest first. Listing, reading,
// dropping and beginning a result first remove the results that expired.
bool sidequery_results_list(struct ArrowArrayStream *out, char **error_out);
// Exports the columns of a stored result, then a BIGINT column named row_number
// unless row_number is null.
bool sidequery_result_schema(const char *id, const char *row_number, struct ArrowSchema *out,
                             char **error_out);
// Reads rows [first_row, first_row + row_count) of a stored result, projected to
// columns (indices into its schema above).
bool sidequery_result_scan(const char *id, uint64_t first_row, uint64_t row_count,
                           const char *row_number, const uint64_t *columns, uint64_t column_count,
                           struct ArrowArrayStream *out, char **error_out);
// Removes a readable result; false when it does not exist.
bool sidequery_result_drop(const char *id);

#ifdef __cplusplus
}
#endif
