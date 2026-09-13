duckdb_extension_load(json)
duckdb_extension_load(autocomplete)
duckdb_extension_load(icu)

duckdb_extension_load(httpfs
  GIT_URL https://github.com/duckdb/duckdb-httpfs
  GIT_TAG 13e18b3c9f3810334f5972b76a3acc247b28e537
)

duckdb_extension_load(quack
  GIT_URL https://github.com/duckdb/duckdb-quack
  GIT_TAG 40de7badae4193c29d9c0834473fb76acc6c51e6
)

duckdb_extension_load(sidequery_ui SOURCE_DIR ${CMAKE_CURRENT_LIST_DIR})
