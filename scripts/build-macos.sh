#!/usr/bin/env bash
# Mangosteen macOS 双平台构建脚本
#
# 用法：
#   ./scripts/build-macos.sh              # 构建当前宿主机平台 + 交叉编译另一 macOS 架构
#   ./scripts/build-macos.sh --intel      # 仅构建 Intel (x86_64-apple-darwin)
#   ./scripts/build-macos.sh --apple-silicon  # 仅构建 Apple Silicon (aarch64-apple-darwin)
#
# 输出：
#   dist/mangosteen-macos-x86_64.tar.gz
#   dist/mangosteen-macos-aarch64.tar.gz
#   dist/*.sha256                         # 校验和文件
#
set -euo pipefail

# 将 rustup/cargo bin 加入 PATH（脚本可能不加载 ~/.profile 的环境里运行）
if ! command -v cargo >/dev/null 2>&1; then
  for d in "${CARGO_HOME:-$HOME/.cargo}/bin" "/usr/local/cargo/bin"; do
    if [ -x "$d/cargo" ]; then
      export PATH="$d:$PATH"
      break
    fi
  done
fi

if ! command -v cargo >/dev/null 2>&1; then
  printf '\033[31m✗\033[0m 找不到 cargo。请先安装 Rust：https://rustup.rs\n' >&2
  exit 1
fi

VERSION=$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')
if [ -z "${VERSION:-}" ]; then
  printf '\033[31m✗\033[0m 无法从 Cargo.toml 读取版本号\n' >&2
  exit 1
fi

DIST="dist"
BIN_NAME="mangosteen"

info()  { printf '\033[36m→\033[0m %s\n' "$*"; }
ok()    { printf '\033[32m✓\033[0m %s\n' "$*"; }
warn()  { printf '\033[33m⚠\033[0m %s\n' "$*"; }
fail()  { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

# 当前宿主机三元组
host_triple() {
  rustc -vV | sed -n 's/^host: //p'
}

# 目标 → 归档名（macOS 无 .exe 后缀）
archive_name() {
  local triple="$1"
  case "$triple" in
    x86_64-apple-darwin)   echo "mangosteen-macos-x86_64" ;;
    aarch64-apple-darwin)  echo "mangosteen-macos-aarch64" ;;
    *)                     fail "不支持的 macOS 目标：$triple" ;;
  esac
}

# 检查目标是否已安装
target_installed() {
  rustup target list --installed 2>/dev/null | grep -qx "$1"
}

# 确保目标已安装（失败时给出明确提示）
ensure_target() {
  local triple="$1"
  if ! target_installed "$triple"; then
    info "安装目标 $triple ..."
    if ! rustup target add "$triple"; then
      fail "无法安装目标 $triple，请检查网络或手动执行：rustup target add $triple"
    fi
  fi
}

# 构建并打包单个目标
package_target() {
  local triple="$1"
  local name
  name=$(archive_name "$triple")

  info "构建 $triple (v${VERSION}) ..."
  cargo build --release --workspace --target "$triple"

  local bin="target/${triple}/release/${BIN_NAME}"
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

1. 把 ${BIN_NAME} 放到 PATH 中的任意目录：
     macOS:  sudo cp ${BIN_NAME} /usr/local/bin/
     或:     export PATH="\$PATH:/path/to/mangosteen"

2. 安装推理引擎（Mangosteen 唯一支持的推理引擎，llama.cpp）：
     macOS:  brew install llama.cpp
     并确保 llama-server 在 PATH 中

3. 初始化并自检：
     ${BIN_NAME} config init
     ${BIN_NAME} doctor

4. 配置云服务商凭证（示例：优云智算）：
     编辑 ~/Library/Application Support/mangosteen/config.toml
     在 [providers.compshare] 段填写 driver、public_key、secret 等

5. 登记模型并部署：
     ${BIN_NAME} models add /path/to/model.gguf
     ${BIN_NAME} deploy <模型名> --context 8192

6. 启动 Web UI（可选）：
     ${BIN_NAME} ui

完整文档见 README.md 与 docs/DESIGN.md
EOF

  # 打包
  info "打包 ${name}.tar.gz ..."
  ( cd "$DIST" && tar -czf "${name}.tar.gz" "$name" )

  # 校验和
  if command -v shasum >/dev/null 2>&1; then
    ( cd "$DIST" && shasum -a 256 "$(basename "${name}.tar.gz")" > "${name}.tar.gz.sha256" )
  elif command -v sha256sum >/dev/null 2>&1; then
    ( cd "$DIST" && sha256sum "$(basename "${name}.tar.gz")" > "${name}.tar.gz.sha256" )
  fi

  ok "已生成 dist/${name}.tar.gz"
  du -h "dist/${name}.tar.gz" | awk '{print "   大小：" $1}'
}

main() {
  mkdir -p "$DIST"
  local host
  host=$(host_triple)
  info "宿主机 ${host} / 版本 ${VERSION}"

  case "${1:-}" in
    --intel)
      ensure_target "x86_64-apple-darwin"
      package_target "x86_64-apple-darwin"
      ;;
    --apple-silicon)
      ensure_target "aarch64-apple-darwin"
      package_target "aarch64-apple-darwin"
      ;;
    ""|--all|--both)
      # macOS 宿主机可双向编译两种架构
      ensure_target "aarch64-apple-darwin"
      ensure_target "x86_64-apple-darwin"
      package_target "aarch64-apple-darwin"
      package_target "x86_64-apple-darwin"
      ;;
    -h|--help)
      sed -n '2,14p' "$0"
      ;;
    *)
      fail "未知参数：$1（可用：--intel, --apple-silicon, --all）"
      ;;
  esac

  echo
  ok "完成。产物位于 $DIST/"
  ls -1 "$DIST" | sed 's/^/   /'
}

main "$@"
