# Community builds use DuckDB's core repository for runtime dependencies.
duckdb_extension_load(sqlnb
  SOURCE_DIR ${CMAKE_CURRENT_LIST_DIR}
  LOAD_TESTS
)
