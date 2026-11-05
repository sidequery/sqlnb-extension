PROJ_DIR := $(dir $(abspath $(lastword $(MAKEFILE_LIST))))

EXT_NAME := sqlnb
EXT_CONFIG := $(PROJ_DIR)extension_config.cmake

include extension-ci-tools/makefiles/duckdb_extension.Makefile
include extension-ci-tools/makefiles/vcpkg.Makefile

.PHONY: sqlnb_ci_setup
configure_ci: sqlnb_ci_setup
sqlnb_ci_setup:
	bash scripts/configure-ci.sh
