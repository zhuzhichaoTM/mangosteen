#!/usr/bin/env bash
# Mangosteen 跨平台发布构建脚本。
#
# 用法：
#   ./scripts/build-release.sh                 # 构建当前平台
#   ./scripts/build-release.sh --target <triple>  # 构建指定目标
#   ./scripts/build-release.sh --all-native    # 构建当前平台所有可原生编译的目标
#
# 设计要点：
#   1. 交叉编译只用于「真的能交叉」的目标（macOS 双向、Linux 多架构）；
#      Windows 必须在 Windows 上构建（MSVC 工具链无法在 Unix 上完整工作）。
#   2. 产物包含：二进制 + README + LICENSE + 示例配置 + 校验和。
#   3. 每个包附带 SHA-256，便于用户校验完整性。

set -euo pipefail

# 自动把 rustup 的 bin 加入 PATH（脚本可能在不加载 ~/.profile 的环境里运行）
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

# 当前平台三元组
host_triple() {
  rustc -vV | sed -n 's/^host: //p'
}

# 目标 → 归档名
archive_name() {
  local triple="$1"
  local os arch
  case "$triple" in
    *-apple-darwin*)   os="macos"   ;;
    *-linux-gnu*)      os="linux"   ;;
    *-windows-msvc*)   os="windows" ;;
    *)                 os="unknown" ;;
  esac
  case "$triple" in
    x86_64-*)   arch="x86_64"  ;;
    aarch64-*)  arch="aarch64" ;;
    *)          arch="unknown" ;;
  esac
  if [ "$os" = "windows" ]; then
    echo "${BIN_NAME}-${os}-${arch}"
  else
    echo "${BIN_NAME}-${os}-${arch}"
  fi
}

# 打包单个目标
package_target() {
  local triple="$1"
  local name
  name=$(archive_name "$triple")

  info "构建 $triple ..."
  if ! cargo build --release --workspace --target "$triple" 2>&1 | tail -5; then
    fail "构建失败：$triple"
  fi

  local bin="target/${triple}/release/${BIN_NAME}"
  local ext=""
  case "$triple" in
    *windows*) bin="${bin}.exe"; ext=".exe" ;;
  esac
  [ -f "$bin" ] || fail "找不到产物：$bin"

  local stage="${DIST}/${name}"
  rm -rf "$stage"
  mkdir -p "$stage"

  cp "$bin" "$stage/"
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
     macOS/Linux:  sudo cp ${BIN_NAME}${ext} /usr/local/bin/
     Windows:      把所在目录加入「系统环境变量 PATH」

2. 安装推理引擎（Mangosteen 唯一支持的推理引擎）：
     macOS/Linux:  brew install llama.cpp
     Windows:      从 https://github.com/ggml-org/llama.cpp/releases 下载
                   并确保 llama-server.exe 在 PATH 中

3. 初始化并自检：
     ${BIN_NAME} config init
     ${BIN_NAME} doctor

4. 登记模型并部署：
     ${BIN_NAME} models add /path/to/model.gguf
     ${BIN_NAME} deploy <模型名> --context 8192

完整文档见 README.md 与 docs/DESIGN.md
EOF

  # 打包
  info "打包 $name ..."
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

  ok "已生成 $out"
  du -h "$out" | awk '{print "   大小：" $1}'
}

# 检查目标是否已安装
target_installed() {
  rustup target list --installed 2>/dev/null | grep -qx "$1"
}

ensure_target() {
  local triple="$1"
  if ! target_installed "$triple"; then
    info "安装目标 $triple ..."
    rustup target add "$triple" || fail "无法安装目标 $triple"
  fi
}

main() {
  mkdir -p "$DIST"
  local host
  host=$(host_triple)
  info "宿主机 ${host} / 版本 ${VERSION}"

  case "${1:-}" in
    --target)
      local t="${2:-}"
      [ -n "$t" ] || fail "请指定目标三元组"
      ensure_target "$t"
      package_target "$t"
      ;;
    --all-native)
      case "$host" in
        *apple-darwin*)
          # macOS 可双向交叉
          ensure_target "aarch64-apple-darwin"
          ensure_target "x86_64-apple-darwin"
          package_target "aarch64-apple-darwin"
          package_target "x86_64-apple-darwin"
          ;;
        *linux*)
          package_target "$host"
          # 交叉到 musl 与 aarch64 需对应 linker，失败不阻断
          if ensure_target "x86_64-unknown-linux-musl" 2>/dev/null; then
            package_target "x86_64-unknown-linux-musl" || warn "musl 交叉编译失败（跳过）"
          fi
          ;;
        *windows*)
          package_target "$host"
          ;;
      esac
      ;;
    ""|--current)
      package_target "$host"
      ;;
    -h|--help)
      sed -n '2,20p' "$0"
      ;;
    *)
      fail "未知参数：$1（用 --help 查看用法）"
      ;;
  esac

  echo
  ok "完成。产物位于 $DIST/"
  ls -1 "$DIST" | sed 's/^/   /'
}

main "$@"
