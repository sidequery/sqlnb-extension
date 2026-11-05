cmake_minimum_required(VERSION 3.12)

set(TARGET_NAME sqlnb)
set(EXTENSION_NAME ${TARGET_NAME}_extension)
set(LOADABLE_EXTENSION_NAME ${TARGET_NAME}_loadable_extension)

project(${TARGET_NAME})

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

find_package(OpenSSL REQUIRED)
find_package(Threads REQUIRED)
find_package(ZLIB REQUIRED)
include_directories(src/include)

set(EXTENSION_SOURCES src/sidequery_ui_extension.cpp)

set(SIDEQUERY_HTTP_MANIFEST ${CMAKE_CURRENT_SOURCE_DIR}/http/Cargo.toml)
set(SIDEQUERY_HTTP_LOCK ${CMAKE_CURRENT_SOURCE_DIR}/http/Cargo.lock)
file(GLOB_RECURSE SIDEQUERY_HTTP_SOURCES CONFIGURE_DEPENDS
     ${CMAKE_CURRENT_SOURCE_DIR}/http/src/*.rs)
set(SIDEQUERY_HTTP_TARGET_DIR ${CMAKE_CURRENT_BINARY_DIR}/http-cargo)
set(SIDEQUERY_HTTP_PROFILE_DIR ${SIDEQUERY_HTTP_TARGET_DIR}/release)
set(SIDEQUERY_CARGO_TARGET_ARGS)
if(Rust_CARGO_TARGET)
  # Community CI cross-compiles Intel macOS on Apple Silicon runners.
  list(APPEND SIDEQUERY_CARGO_TARGET_ARGS --target ${Rust_CARGO_TARGET})
  set(SIDEQUERY_HTTP_PROFILE_DIR ${SIDEQUERY_HTTP_TARGET_DIR}/${Rust_CARGO_TARGET}/release)
endif()
if(MSVC)
  set(SIDEQUERY_HTTP_LIBRARY ${SIDEQUERY_HTTP_PROFILE_DIR}/sidequery_http.lib)
  # DuckDB uses the static MSVC runtime; Rust and its C dependencies must agree.
  set(SIDEQUERY_RUST_ENV "RUSTFLAGS=$ENV{RUSTFLAGS} -C target-feature=+crt-static")
else()
  set(SIDEQUERY_HTTP_LIBRARY ${SIDEQUERY_HTTP_PROFILE_DIR}/libsidequery_http.a)
endif()

set(SIDEQUERY_REPO_MBX ${CMAKE_CURRENT_SOURCE_DIR}/scripts/mbx)
if(EXISTS ${SIDEQUERY_REPO_MBX})
  set(SIDEQUERY_CARGO_BUILD ${SIDEQUERY_REPO_MBX} build)
else()
  find_program(SIDEQUERY_MBX_EXECUTABLE mbx HINTS ${CMAKE_CURRENT_SOURCE_DIR}/.ci/tools/bin)
  if(SIDEQUERY_MBX_EXECUTABLE AND "$ENV{RUSTC_WRAPPER}" STREQUAL ""
     AND "$ENV{RUSTC_WORKSPACE_WRAPPER}" STREQUAL "")
    set(SIDEQUERY_CARGO_BUILD ${SIDEQUERY_MBX_EXECUTABLE} build)
  else()
    find_program(SIDEQUERY_CARGO_EXECUTABLE cargo)
    if(NOT SIDEQUERY_CARGO_EXECUTABLE)
      message(FATAL_ERROR "building SQLNB requires mbx or cargo")
    endif()
    message(STATUS "repo mbx wrapper unavailable; building SQLNB HTTP library with Cargo")
    set(SIDEQUERY_CARGO_BUILD ${SIDEQUERY_CARGO_EXECUTABLE} build)
  endif()
endif()

add_custom_command(
  OUTPUT ${SIDEQUERY_HTTP_LIBRARY}
  COMMAND ${CMAKE_COMMAND} -E env CARGO_TARGET_DIR=${SIDEQUERY_HTTP_TARGET_DIR} ${SIDEQUERY_RUST_ENV}
          ${SIDEQUERY_CARGO_BUILD} --manifest-path ${SIDEQUERY_HTTP_MANIFEST} --locked --release ${SIDEQUERY_CARGO_TARGET_ARGS}
  DEPENDS ${SIDEQUERY_HTTP_MANIFEST} ${SIDEQUERY_HTTP_LOCK} ${SIDEQUERY_HTTP_SOURCES}
  WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}/http
  COMMENT "Building Sidequery's in-process Axum HTTP server"
  VERBATIM)
add_custom_target(sidequery_http_build DEPENDS ${SIDEQUERY_HTTP_LIBRARY})
add_library(sidequery_http STATIC IMPORTED GLOBAL)
set_target_properties(sidequery_http PROPERTIES IMPORTED_LOCATION ${SIDEQUERY_HTTP_LIBRARY})
add_dependencies(sidequery_http sidequery_http_build)

build_static_extension(${TARGET_NAME} ${EXTENSION_SOURCES})
build_loadable_extension(${TARGET_NAME} "" ${EXTENSION_SOURCES})

foreach(target ${EXTENSION_NAME} ${LOADABLE_EXTENSION_NAME})
  add_dependencies(${target} sidequery_http_build)
  target_link_libraries(${target} sidequery_http OpenSSL::Crypto ZLIB::ZLIB Threads::Threads ${CMAKE_DL_LIBS})
  if(APPLE)
    target_link_libraries(${target} iconv)
  elseif(UNIX)
    target_link_libraries(${target} rt util m)
  elseif(WIN32)
    target_link_libraries(${target} ws2_32 userenv bcrypt ntdll crypt32 secur32 shell32 advapi32 winhttp rpcrt4 ole32)
  endif()
endforeach()

install(
  TARGETS ${EXTENSION_NAME}
  EXPORT "${DUCKDB_EXPORT_SET}"
  LIBRARY DESTINATION "${INSTALL_LIB_DIR}"
  ARCHIVE DESTINATION "${INSTALL_LIB_DIR}")
