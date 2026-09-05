#!/bin/bash

# vcpkg builds host tools (for example pkgconf:arm64-osx) in the same process
# that cross-compiles target ports for iOS. Xcode and SwiftPM export target
# SDK/compiler variables into child processes; if those reach vcpkg's host
# triplet, Clang tries to link a macOS host executable against the iOS SDK.
run_vcpkg_with_clean_apple_toolchain_environment() {
  env \
    -u AR \
    -u ARCHS \
    -u AS \
    -u CC \
    -u CFLAGS \
    -u CARGO_BUILD_TARGET \
    -u CMAKE_OSX_ARCHITECTURES \
    -u CMAKE_OSX_DEPLOYMENT_TARGET \
    -u CMAKE_OSX_SYSROOT \
    -u CMAKE_SYSTEM_NAME \
    -u CMAKE_SYSTEM_PROCESSOR \
    -u CMAKE_TOOLCHAIN_FILE \
    -u CPP \
    -u CPPFLAGS \
    -u CURRENT_ARCH \
    -u CXX \
    -u CXXFLAGS \
    -u EFFECTIVE_PLATFORM_NAME \
    -u IPHONEOS_DEPLOYMENT_TARGET \
    -u LD \
    -u LDFLAGS \
    -u LIBTOOL \
    -u LLVM_TARGET_TRIPLE_OS_VERSION \
    -u LLVM_TARGET_TRIPLE_SUFFIX \
    -u LLVM_TARGET_TRIPLE_VENDOR \
    -u MACOSX_DEPLOYMENT_TARGET \
    -u NATIVE_ARCH \
    -u NATIVE_ARCH_ACTUAL \
    -u NM \
    -u PKG_CONFIG \
    -u PKG_CONFIG_LIBDIR \
    -u PKG_CONFIG_PATH \
    -u PKG_CONFIG_SYSROOT_DIR \
    -u PLATFORM_NAME \
    -u RANLIB \
    -u RC_ARCHS \
    -u RUSTDOCFLAGS \
    -u RUSTFLAGS \
    -u SDKROOT \
    -u STRIP \
    -u TVOS_DEPLOYMENT_TARGET \
    -u VALID_ARCHS \
    -u WATCHOS_DEPLOYMENT_TARGET \
    -u XROS_DEPLOYMENT_TARGET \
    "$@"
}
