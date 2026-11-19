#!/usr/bin/env bash
# rcd-mcp-server build environment — source this before ANY cargo command.
# Keeping these EXACT values stable is what keeps llama.cpp's cmake build
# cached; changing any of them triggers a ~30 min CUDA recompile.
#
# 2026-11-19: CUDA_PATH now FORCED to v13.5 (no `${CUDA_PATH:-}` fallback).
# The machine's global CUDA_PATH points to v12.9, whose nvcc rejects the
# installed MSVC 14.44 with "unsupported Microsoft Visual Studio version"
# (fatal error C1189 in host_config.h). Also pins CUDACXX/CMAKE_CUDA_COMPILER
# so CMake's nvcc probe can't pick the stale 12.9 from the inherited PATH,
# and sets CUDA_PATH_OVERRIDE so find_cuda_helper ignores the system value.

CUDA_ROOT="C:/Program Files/NVIDIA GPU Computing Toolkit/CUDA/v13.5"
export CUDA_PATH="$CUDA_ROOT"
export CUDA_PATH_OVERRIDE=1
export CUDACXX="$CUDA_ROOT/bin/nvcc.exe"
export CMAKE_CUDA_COMPILER="$CUDA_ROOT/bin/nvcc.exe"
export LIBCLANG_PATH="${LIBCLANG_PATH:-C:/Program Files/LLVM/bin}"
export CMAKE_GENERATOR=Ninja

# Windows SDK + MSVC lib/include paths: a fresh llama.cpp CUDA configure
# needs kernel32.lib (LNK1104 without LIB) and rc.exe/mt.exe on PATH
# (CMAKE_MT-NOTFOUND without the SDK bin dir).
WIN_SDK_VER=10.0.26100.0
MSVC_VER=14.44.35207
export LIB="$CUDA_ROOT/lib/x64;C:/Program Files (x86)/Windows Kits/10/Lib/$WIN_SDK_VER/um/x64;C:/Program Files (x86)/Windows Kits/10/Lib/$WIN_SDK_VER/ucrt/x64;D:/ohmyenv/vsbuild/VC/Tools/MSVC/$MSVC_VER/lib/x64"
export INCLUDE="C:/Program Files (x86)/Windows Kits/10/Include/$WIN_SDK_VER/ucrt;C:/Program Files (x86)/Windows Kits/10/Include/$WIN_SDK_VER/um;C:/Program Files (x86)/Windows Kits/10/Include/$WIN_SDK_VER/shared;D:/ohmyenv/vsbuild/VC/Tools/MSVC/$MSVC_VER/include"

# PATH: cmake/ninja from VS, CUDA 13.5 nvcc, SDK rc/mt. Note this is a fresh
# prepend (not $PATH-dependent) so the stale CUDA 12.9 entries inherited from
# the global PATH can't win the nvcc probe.
export PATH="/d/ohmyenv/vsbuild/Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin:$CUDA_ROOT/bin:$CUDA_ROOT/bin/x64:/c/Program Files (x86)/Windows Kits/10/bin/$WIN_SDK_VER/x64:/d/ohmyenv/vsbuild/VC/Tools/MSVC/$MSVC_VER/bin/Hostx64/x64:$PATH"

# CUDA runtime DLLs staged next to the exe by build.rs (from vendor/dlls).
# For running outside the build dir, point this at the DLL folder.
export RCD_LIB_DIR="${RCD_LIB_DIR:-$(cd "$(dirname "$0")/.." && pwd)/vendor/dlls}"
