#!/usr/bin/env bash
# Mangosteen 统一跨平台发布构建脚本
#
# 用法：
#   ./scripts/build-all.sh                  # 构建所有支持的平台（macOS + Windows）
#   ./scripts/build-all.sh --macos-only     # 仅构建 macOS 双架构
#   ./scripts/build-all.sh --windows-only   # 仅构建 Windows x86_64
#
# 输出：
#   dist/mangosteen-macos-x86_64.tar.gz
#   dist/mangosteen-macos-aarch64.tar.gz
#   dist/mangosteen-windows-x86_64.zip
#   dist/*.sha256                           # 校验和文件
#
# 注意：
#   - 此脚本仅支持在 macOS 宿主机上运行
#   - Windows 通过交叉编译生成（x86_64-pc-windows-msvc）
#   - Linux 需在 Linux 环境中单独构建
#
set -euo pipefail

# ==================== 配置区域 ====================
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="${SCRIPT_DIR}/.."
DIST="${PROJECT_DIR}/dist"
BIN_NAME="mangosteen"
VERSION=$(grep -m1 '^version = ' "${PROJECT_DIR}/Cargo.toml" | sed -E 's/.*"(.*)".*/\1/')
# =================================================

# 颜色输出
info()  { printf '\033[36m→\033[0m %s\n' "$*"; }
ok()    { printf '\033[32m✓\033[0m %s\n' "$*"; }
warn()  { printf '\033[33m⚠\033[0m %s\n' "$*"; }
fail()  { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

# 检测操作系统
detect_os() {
  case "$(uname -s)" in
    Darwin) echo "macos" ;;
    Linux)  echo "linux" ;;
    MINGW*|MSYS*|CYGWIN*) echo "windows" ;;
    *)      echo "unknown" ;;
  esac
}

# 确保 cargo 可用
ensure_cargo() {
  if ! command -v cargo >/dev/null 2>&1; then
    for d in "${CARGO_HOME:-$HOME/.cargo}/bin" "/usr/local/cargo/bin"; do
      if [ -x "$d/cargo" ]; then
        export PATH="$d:$PATH"
        break
      fi
    done
  fi

  if ! command -v cargo >/dev/null 2>&1; then
    fail "找不到 cargo。请先安装 Rust：https://rustup.rs"
  fi
}

# 检查目标是否已安装
target_installed() {
  rustup target list --installed 2>/dev/null | grep -qx "$1"
}

# 确保目标已安装
ensure_target() {
  local triple="$1"
  if ! target_installed "$triple"; then
    info "安装目标 $triple ..."
    if ! rustup target add "$triple"; then
      fail "无法安装目标 $triple，请检查网络或手动执行：rustup target add $triple"
    fi
  fi
}

# 目标 → 归档名
archive_name() {
  local triple="$1"
  case "$triple" in
    x86_64-apple-darwin)   echo "mangosteen-macos-x86_64" ;;
    aarch64-apple-darwin)  echo "mangosteen-macos-aarch64" ;;
    x86_64-pc-windows-msvc|x86_64-pc-windows-gnu) echo "mangosteen-windows-x86_64" ;;
    *)                     fail "不支持的构建目标：$triple" ;;
  esac
}

# 构建并打包单个目标
package_target() {
  local triple="$1"
  local name
  name=$(archive_name "$triple")

  info "构建 $triple (v${VERSION}) ..."
  cd "$PROJECT_DIR"
  cargo build --release --workspace --target "$triple"

  local bin="target/${triple}/release/${BIN_NAME}"
  local ext=""
  case "$triple" in
    *windows*) bin="${bin}.exe"; ext=".exe" ;;
  esac
  [ -f "$bin" ] || fail "找不到产物：$bin"

  local stage="${DIST}/${name}"
  rm -rf "$stage"
  mkdir -p "$stage"

  # 复制二进制
  cp "$bin" "$stage/"

  # 复制文档与配置示例
  cp README.md "$stage/"
  [ -f LICENSE ] && cp LICENSE "$stage/"
  cp -r docs "$stage/" 2>/dev/null || true
  mkdir -p "$stage/config/providers"
  cp config/config.example.toml "$stage/config/" 2>/dev/null || true
  cp config/providers/*.toml "$stage/config/providers/" 2>/dev/null || true

  # 安装说明
  cat > "$stage/INSTALL.txt" <<EOF
Mangosteen ${VERSION} — ${name}
==================================================

1. 把 ${BIN_NAME}${ext} 放到 PATH 中的任意目录：
     macOS:  sudo cp ${BIN_NAME} /usr/local/bin/
     或:     export PATH="\$PATH:/path/to/mangosteen"
     Windows: 把所在目录加入「系统环境变量 PATH」

2. 安装推理引擎（Mangosteen 唯一支持的推理引擎，llama.cpp）：
     macOS:  brew install llama.cpp
     并确保 llama-server 在 PATH 中
     Windows: 从 https://github.com/ggml-org/llama.cpp/releases 下载
              并确保 llama-server.exe 在 PATH 中

3. 初始化并自检：
     ${BIN_NAME} config init
     ${BIN_NAME} doctor

4. 配置云服务商凭证（示例：优云智算）：
     编辑配置文件：
       macOS:   ~/Library/Application Support/mangosteen/config.toml
       Windows: %APPDATA%\mangosteen\config.toml
     在 [providers.compshare] 段填写 driver、public_key、secret 等

5. 登记模型并部署：
     ${BIN_NAME} models add /path/to/model.gguf
     ${BIN_NAME} deploy <模型名> --context 8192

6. 启动 Web UI（可选）：
     ${BIN_NAME} ui

完整文档见 README.md 与 docs/DESIGN.md
EOF

  # 打包
  info "打包 ${name} ..."
  if [ "$ext" = ".exe" ]; then
    ( cd "$DIST" && zip -qr "${name}.zip" "$name" )
    local out="${DIST}/${name}.zip"
  else
    ( cd "$DIST" && tar -czf "${name}.tar.gz" "$name" )
    local out="${DIST}/${name}.tar.gz"
  fi

  # 校验和
  if command -v shasum >/dev/null 2>&1; then
    ( cd "$DIST" && shasum -a 256 "$(basename "$out")" > "$(basename "$out").sha256" )
  elif command -v sha256sum >/dev/null 2>&1; then
    ( cd "$DIST" && sha256sum "$(basename "$out")" > "$(basename "$out").sha256" )
  fi

  ok "已生成 dist/$(basename "$out")"
  du -h "$out" | awk '{print "   大小：" $1}'
}

# 构建 macOS 双架构
build_macos() {
  info "==================== 构建 macOS 双架构 ===================="
  ensure_target "aarch64-apple-darwin"
  ensure_target "x86_64-apple-darwin"
  package_target "aarch64-apple-darwin"
  package_target "x86_64-apple-darwin"
}

# 构建 Windows x86_64（Docker 交叉编译）
build_windows() {
  info "==================== 构建 Windows x86_64 ===================="
  info "使用 Docker 容器进行交叉编译..."

  if ! docker ps &>/dev/null; then
    fail "Docker 未运行，请先启动 Docker Desktop"
  fi

  # 调用 Docker 构建脚本
  if [ -f "${SCRIPT_DIR}/build-windows-docker.sh" ]; then
    bash "${SCRIPT_DIR}/build-windows-docker.sh"
  else
    fail "找不到 Docker 构建脚本：${SCRIPT_DIR}/build-windows-docker.sh"
  fi
}

# 主函数
main() {
  mkdir -p "$DIST"

  local os
  os=$(detect_os)

  if [ "$os" != "macos" ]; then
    fail "此脚本仅支持在 macOS 宿主机上运行（当前系统：$os）"
  fi

  ensure_cargo

  local host
  host=$(rustc -vV | sed -n 's/^host: //p')
  info "宿主机 ${host} / 版本 ${VERSION}"
  info "输出目录 ${DIST}"
  echo

  # 根据参数决定构建哪些平台
  case "${1:-}" in
    --macos-only)
      build_macos
      ;;
    --windows-only)
      build_windows
      ;;
    ""|--all)
      build_macos
      echo
      build_windows
      ;;
    -h|--help)
      sed -n '2,22p' "$0"
      exit 0
      ;;
    *)
      fail "未知参数：$1（可用：--macos-only, --windows-only, --all）"
      ;;
  esac

  echo
  ok "==================== 构建完成 ===================="
  echo
  info "产物位于 ${DIST}/："
  ls -1 "$DIST" | sed 's/^/   /'
  echo
  info "校验和："
  ls -1 "$DIST"/*.sha256 2>/dev/null | sed 's/^/   /' || warn "未找到校验和文件"
  echo
}

main "$@"
