#!/usr/bin/env bash
# Mangosteen Linux x86_64 构建脚本（Docker，可在 macOS / Linux 宿主机上运行）
#
# 用法：
#   ./scripts/build-linux-docker.sh
#
# 输出：
#   dist/mangosteen-linux-x86_64.tar.gz
#   dist/mangosteen-linux-x86_64.tar.gz.sha256
#
# 说明：
#   - 依赖树为纯 Rust（Cargo.lock 无 openssl-sys/ring），rust:slim 即可构建；
#   - 容器内使用独立 CARGO_TARGET_DIR（dist/.linux-target），避免与宿主
#     macOS 的 target/release 产物混淆；
#   - cargo registry 用命名卷缓存，重复构建只重编改动 crate。
set -euo pipefail

DIST="dist"
BIN_NAME="mangosteen"
NAME="mangosteen-linux-x86_64"

if ! command -v docker >/dev/null 2>&1; then
  printf '\033[31m✗\033[0m 找不到 docker。请先安装 Docker Desktop 或 docker CLI\n' >&2
  exit 1
fi
if ! docker info >/dev/null 2>&1; then
  printf '\033[31m✗\033[0m Docker daemon 未运行，请先启动 Docker\n' >&2
  exit 1
fi

VERSION=$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')
if [ -z "${VERSION:-}" ]; then
  printf '\033[31m✗\033[0m 无法从 Cargo.toml 读取版本号\n' >&2
  exit 1
fi

info()  { printf '\033[36m→\033[0m %s\n' "$*"; }
ok()    { printf '\033[32m✓\033[0m %s\n' "$*"; }
fail()  { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

mkdir -p "$DIST"

info "容器内构建 x86_64-unknown-linux-gnu (v${VERSION}) ..."
# CARGO_TARGET_DIR 指到 dist/.linux-target：宿主 macOS 的 target/ 是
# Mach-O 产物目录，容器构建绝不能写进去（同路径不同格式会互相覆盖）。
docker run --rm \
  -v "$PWD":/src -w /src \
  -v mangosteen-cargo-registry:/usr/local/cargo/registry \
  -v "$PWD/dist/.linux-target":/linux-target \
  -e CARGO_TARGET_DIR=/linux-target \
  rust:1-slim-bookworm \
  bash -c "cargo build --release --workspace --locked"

BIN="dist/.linux-target/release/${BIN_NAME}"
[ -f "$BIN" ] || fail "找不到产物：$BIN"
file "$BIN" | grep -q "ELF" || fail "产物不是 Linux ELF：$BIN"

STAGE="${DIST}/${NAME}"
rm -rf "$STAGE"
mkdir -p "$STAGE"
cp "$BIN" "$STAGE/"
cp README.md "$STAGE/"
[ -f LICENSE ] && cp LICENSE "$STAGE/" || true
cp -r docs "$STAGE/" 2>/dev/null || true
mkdir -p "$STAGE/config/providers"
cp config/config.example.toml "$STAGE/config/" 2>/dev/null || true
cp config/providers/*.toml "$STAGE/config/providers/" 2>/dev/null || true

# Linux 版安装说明（配置目录与引擎安装与 macOS 不同）
cat > "$STAGE/INSTALL.txt" <<EOF
Mangosteen ${VERSION} — ${NAME}
==================================================

1. 把 ${BIN_NAME} 放到 PATH 中的任意目录：
     sudo cp ${BIN_NAME} /usr/local/bin/

2. 安装推理引擎（Mangosteen 唯一支持的推理引擎，llama.cpp）：
     Ubuntu/Debian: 参见 https://github.com/ggml-org/llama.cpp  releases
     或从源码编译    cmake -B build && cmake --build build --target llama-server
     并确保 llama-server 在 PATH 中

3. 初始化并自检：
     ${BIN_NAME} config init
     ${BIN_NAME} doctor

4. 配置云服务商凭证（示例：优云智算）：
     编辑 ~/.config/mangosteen/config.toml
     在 [providers.compshare] 段填写 driver、public_key、secret 等

5. 登记模型并部署：
     ${BIN_NAME} models add /path/to/model.gguf
     ${BIN_NAME} deploy <模型名> --context 8192

6. 启动 Web UI（可选）：
     ${BIN_NAME} ui

完整文档见 README.md 与 docs/DESIGN.md
EOF

info "打包 ${NAME}.tar.gz ..."
( cd "$DIST" && tar -czf "${NAME}.tar.gz" "$NAME" )

if command -v sha256sum >/dev/null 2>&1; then
  ( cd "$DIST" && sha256sum "${NAME}.tar.gz" > "${NAME}.tar.gz.sha256" )
else
  ( cd "$DIST" && shasum -a 256 "${NAME}.tar.gz" > "${NAME}.tar.gz.sha256" )
fi

ok "已生成 dist/${NAME}.tar.gz"
du -h "dist/${NAME}.tar.gz" | awk '{print "   大小：" $1}'