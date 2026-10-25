# Match the dependencies tested by the official DuckDB release.
duckdb_extension_load(icu)
include(${DUCKDB_MODULE_BASE_DIR}/.github/config/extensions/quack.cmake)
duckdb_extension_load(sqlnb SOURCE_DIR ${CMAKE_CURRENT_LIST_DIR})
