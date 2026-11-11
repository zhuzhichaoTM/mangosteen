#!/usr/bin/env bash
# Mangosteen Windows 交叉编译 Docker 构建脚本
#
# 用法：
#   ./scripts/build-windows-docker.sh
#
# 需求：
#   - Docker Desktop 已运行
#
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="${SCRIPT_DIR}/.."
cd "$PROJECT_DIR"

info()  { printf '\033[36m→\033[0m %s\n' "$*"; }
ok()    { printf '\033[32m✓\033[0m %s\n' "$*"; }
warn()  { printf '\033[33m⚠\033[0m %s\n' "$*"; }
fail()  { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

# 检查 Docker
if ! docker ps &>/dev/null; then
  fail "Docker 未运行，请先启动 Docker Desktop"
fi

VERSION=$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')
if [ -z "${VERSION:-}" ]; then
  fail "无法从 Cargo.toml 读取版本号"
fi

DIST="dist"
STAGE="${DIST}/mangosteen-windows-x86_64"

info "==================== Windows Docker 交叉编译 ===================="
info "版本 ${VERSION}"
info "项目目录 $(pwd)"
echo

info "步骤 1/3：在 Docker 容器中编译..."
docker run --rm \
  -v "$(pwd)":/project \
  -w /project \
  rust:latest \
  bash -c "
    # 配置 Rust 国内镜像源（USTC）
    mkdir -p ~/.cargo && \
    echo '[source.crates-io]
replace-with = 'ustc'
[source.ustc]
registry = \"sparse+https://mirrors.ustc.edu.cn/crates.io-index/\"
' > ~/.cargo/config.toml && \
    apt-get update -qq && \
    apt-get install -y -qq mingw-w64 && \
    rustup target add x86_64-pc-windows-gnu && \
    cargo build --release --workspace --target x86_64-pc-windows-gnu
  "

if [ ! -f "target/x86_64-pc-windows-gnu/release/mangosteen.exe" ]; then
  fail "构建失败：找不到 mangosteen.exe"
fi

ok "编译成功"

info "步骤 2/3：打包发布件..."
rm -rf "$STAGE"
mkdir -p "$STAGE/config/providers"

cp target/x86_64-pc-windows-gnu/release/mangosteen.exe "$STAGE/"
cp README.md "$STAGE/"
[ -f LICENSE ] && cp LICENSE "$STAGE/"
cp -r docs "$STAGE/" 2>/dev/null || true
cp config/config.example.toml "$STAGE/config/" 2>/dev/null || true
cp config/providers/*.toml "$STAGE/config/providers/" 2>/dev/null || true

cat > "$STAGE/INSTALL.txt" <<EOF
Mangosteen ${VERSION} — mangosteen-windows-x86_64
==================================================

1. 把 mangosteen.exe 放到 PATH 中的任意目录：
     Windows: 把所在目录加入「系统环境变量 PATH」

2. 安装推理引擎（Mangosteen 唯一支持的推理引擎）：
     从 https://github.com/ggml-org/llama.cpp/releases 下载
     并确保 llama-server.exe 在 PATH 中

3. 初始化并自检：
     mangosteen.exe config init
     mangosteen.exe doctor

4. 配置云服务商凭证（示例：优云智算）：
     编辑 %APPDATA%\mangosteen\config.toml
     在 [providers.compshare] 段填写 driver、public_key、secret 等

5. 登记模型并部署：
     mangosteen.exe models add /path/to/model.gguf
     mangosteen.exe deploy <模型名> --context 8192

完整文档见 README.md 与 docs/DESIGN.md
EOF

cd "$DIST"
zip -qr "mangosteen-windows-x86_64.zip" "mangosteen-windows-x86_64"
cd "$PROJECT_DIR"

ok "打包完成"

info "步骤 3/3：生成校验和..."
if command -v shasum >/dev/null 2>&1; then
  ( cd "$DIST" && shasum -a 256 "mangosteen-windows-x86_64.zip" > "mangosteen-windows-x86_64.zip.sha256" )
elif command -v sha256sum >/dev/null 2>&1; then
  ( cd "$DIST" && sha256sum "mangosteen-windows-x86_64.zip" > "mangosteen-windows-x86_64.zip.sha256" )
fi

ok "校验和已生成"

echo
ok "==================== 构建完成 ===================="
echo
info "产物位于 ${DIST}/："
ls -1 "$DIST" | sed 's/^/   /'
echo
info "Windows 发布件："
ls -lh "$DIST/mangosteen-windows-x86_64.zip" | awk '{print "   大小：" $5}'
echo
