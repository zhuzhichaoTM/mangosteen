#!/usr/bin/env bash
# Build a CUDA-enabled llama.cpp llama-server for NVIDIA GPUs.
#
# This intentionally builds from source instead of downloading a vendor-specific
# binary. CUDA fat binaries are only portable across architectures supported by
# the installed CUDA toolkit. Override CUDA_ARCHITECTURES when building a fleet.
#
# Examples:
#   bash build-cuda-llama.sh
#   CUDA_ARCHITECTURES=61 bash build-cuda-llama.sh   # Tesla P40 / Pascal
#   CUDA_ARCHITECTURES='61;70;75;80;86;89;90' bash build-cuda-llama.sh
#   NVIDIA_ARCH=all bash build-cuda-llama.sh  # use known-compatible list
#   SRC_DIR=/root/src/llama.cpp OUT_DIR=/root/mangosteen/bin bash build-cuda-llama.sh

set -Eeuo pipefail

SRC_DIR="${SRC_DIR:-/root/mangosteen/src/llama.cpp}"
OUT_DIR="${OUT_DIR:-/root/mangosteen/bin}"
BUILD_DIR="${BUILD_DIR:-$SRC_DIR/build-cuda}"
REPO_URL="${REPO_URL:-https://github.com/ggml-org/llama.cpp.git}"
REPO_MIRROR="${REPO_MIRROR:-https://ghproxy.net/https://github.com/ggml-org/llama.cpp.git}"
CUDA_ARCHITECTURES="${CUDA_ARCHITECTURES:-}"
# CUDA 13 rejects CMake's literal `all-major` and cannot compile deprecated
# Pascal sm_61. This explicit list covers the maintained NVIDIA generations;
# override it for a CUDA 11 toolkit if Pascal/P40 support is required.
CUDA_ALL_ARCHES="${CUDA_ALL_ARCHES:-70;75;80;86;89;90}"
JOBS="${JOBS:-$(nproc 2>/dev/null || printf 4)}"

info() { printf '[INFO] %s\n' "$*"; }
ok() { printf '[ OK ] %s\n' "$*"; }
fail() { printf '[FAIL] %s\n' "$*" >&2; exit 1; }

command -v cmake >/dev/null 2>&1 || fail '缺少 cmake'
command -v nvcc >/dev/null 2>&1 || fail '缺少 nvcc，请安装 CUDA Toolkit'
command -v git >/dev/null 2>&1 || fail '缺少 git'

CUDA_VERSION="$(nvcc --version | sed -n 's/.*release \([0-9][0-9]*\.[0-9][0-9]*\).*/\1/p' | tail -1)"
[ -n "$CUDA_VERSION" ] || fail '无法读取 CUDA Toolkit 版本'
info "CUDA Toolkit: $CUDA_VERSION"

if [ "${CUDA_ARCHITECTURES:-}" = "all-major" ]; then
  CUDA_ARCHITECTURES="$CUDA_ALL_ARCHES"
fi

if [ -z "$CUDA_ARCHITECTURES" ]; then
  if command -v nvidia-smi >/dev/null 2>&1; then
    GPU_INFO="$(nvidia-smi --query-gpu=name,compute_cap --format=csv,noheader 2>/dev/null | head -1 || true)"
    info "GPU: ${GPU_INFO:-unknown}"
    COMPUTE="$(printf '%s' "$GPU_INFO" | sed -n 's/.*\([0-9][0-9][0-9]\)$/\1/p')"
    if [ -n "$COMPUTE" ]; then
      CUDA_ARCHITECTURES="$COMPUTE"
    fi
  fi
fi

# A single-GPU build is the safest default: it works with the current card and
# avoids asking an old toolkit to compile unsupported newer architectures.
# Set CUDA_ARCHITECTURES explicitly for a portable fat binary.
if [ -z "$CUDA_ARCHITECTURES" ]; then
  CUDA_ARCHITECTURES="$CUDA_ALL_ARCHES"
  info "未探测到 compute capability，使用兼容架构列表：$CUDA_ARCHITECTURES"
else
  info "CUDA architectures: $CUDA_ARCHITECTURES"
fi

if [ ! -d "$SRC_DIR/.git" ]; then
  mkdir -p "$(dirname "$SRC_DIR")"
  info "克隆 llama.cpp"
  if ! git clone --depth 1 "$REPO_URL" "$SRC_DIR"; then
    rm -rf "$SRC_DIR"
    git clone --depth 1 "$REPO_MIRROR" "$SRC_DIR" || fail 'llama.cpp 源码下载失败'
  fi
else
  info "复用源码：$SRC_DIR"
fi

rm -rf "$BUILD_DIR"
info "配置 CUDA 构建（并行 $JOBS）"
cmake -S "$SRC_DIR" -B "$BUILD_DIR" \
  -DGGML_CUDA=ON \
  -DGGML_NATIVE=OFF \
  -DLLAMA_CURL=OFF \
  -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_CUDA_ARCHITECTURES="$CUDA_ARCHITECTURES" \
  -DCMAKE_CUDA_COMPILER="$(command -v nvcc)"

info '编译 llama-server'
cmake --build "$BUILD_DIR" --config Release --target llama-server -j "$JOBS"

BIN="$(find "$BUILD_DIR" -type f -name llama-server -perm -111 | head -1)"
[ -n "$BIN" ] || fail '构建结束但找不到 llama-server'
mkdir -p "$OUT_DIR"
cp -f "$BIN" "$OUT_DIR/llama-server"
chmod 755 "$OUT_DIR/llama-server"

# Copy runtime CUDA libraries when the build tree provides them. The binary
# still relies on the node's CUDA driver; these are user-space toolkit libs.
find "$BUILD_DIR" -type f \( -name 'libggml-cuda.so*' -o -name 'libggml.so*' -o -name 'libggml-base.so*' \) \
  -exec cp -f {} "$OUT_DIR"/ \; 2>/dev/null || true

ok "llama-server: $OUT_DIR/llama-server"
"$OUT_DIR/llama-server" --version || true
cat <<EOF

完成。启动时建议：
  export LD_LIBRARY_PATH="$OUT_DIR:\${LD_LIBRARY_PATH:-}"
  "$OUT_DIR/llama-server" -m /root/mangosteen/models/<model>.gguf \\
    --n-gpu-layers 99 --host 0.0.0.0 --port 8080

注意：
- P40 是 compute capability 6.1，通常需要 CUDA 11.x；CUDA 12+ 可能不再支持 sm_61。
- “所有 NVIDIA 卡”不能由一个固定二进制无条件保证；本脚本默认覆盖
  sm_70/75/80/86/89/90。P40(sm_61) 需要 CUDA 11.x，使用：
  CUDA_ALL_ARCHES=61 CUDA_ARCHITECTURES=61 bash build-cuda-llama.sh
EOF
