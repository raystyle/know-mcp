#!/usr/bin/env bash
# rcd-mcp-server build environment — source this before ANY cargo command.
# Keeping these EXACT values stable is what keeps llama.cpp's cmake build
# cached; changing any of them triggers a ~30 min CUDA recompile.

export CUDA_PATH="${CUDA_PATH:-C:/Program Files/NVIDIA GPU Computing Toolkit/CUDA/v13.5}"
export LIBCLANG_PATH="${LIBCLANG_PATH:-C:/Program Files/LLVM/bin}"
export LIB="$CUDA_PATH/lib/x64;${LIB:-}"
export PATH="/d/ohmyenv/vsbuild/Common7/IDE/CommonExtensions/Microsoft/CMake/CMake/bin:$CUDA_PATH/bin:$CUDA_PATH/bin/x64:$PATH"
export CMAKE_GENERATOR=Ninja

# CUDA runtime DLLs staged next to the exe by build.rs (from vendor/dlls).
# For running outside the build dir, point this at the DLL folder.
export RCD_LIB_DIR="${RCD_LIB_DIR:-$(cd "$(dirname "$0")/.." && pwd)/vendor/dlls}"
