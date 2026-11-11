# Windows 交叉编译 Docker 方案

## 方案一：使用 Docker 容器直接编译（推荐）

在 Linux 容器中安装 MinGW 和 Rust 工具链，然后交叉编译 Windows 版本。

### 单次构建命令

```bash
cd /Users/zhuzhichao/Documents/mangosteen

docker run --rm \
  -v "$(pwd)":/project \
  -w /project \
  ubuntu:22.04 \
  bash -c "
    apt-get update && \
    apt-get install -y curl build-essential mingw-w64 && \
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y && \
    source \$HOME/.cargo/env && \
    rustup target add x86_64-pc-windows-gnu && \
    cargo build --release --workspace --target x86_64-pc-windows-gnu
  "
```

### 说明

- `-v "$(pwd)":/project` - 将当前项目目录挂载到容器的 /project
- `-w /project` - 设置工作目录
- `ubuntu:22.04` - 使用 Ubuntu 22.04 作为基础镜像
- `mingw-w64` - 安装 MinGW 交叉编译工具链
- `rustup target add x86_64-pc-windows-gnu` - 添加 Windows 目标

### 预计时间

- 首次构建：约 5-10 分钟（需要下载 Rust 工具链和依赖）
- 后续构建：约 2-3 分钟（依赖已缓存）

---

## 方案二：使用 Dockerfile 构建专用镜像

创建 Dockerfile 用于构建 Windows 版本。

### 创建 Dockerfile

```dockerfile
# Dockerfile.windows
FROM ubuntu:22.04

# 安装基础工具
RUN apt-get update && \
    apt-get install -y \
    curl \
    build-essential \
    mingw-w64 \
    && rm -rf /var/lib/apt/lists/*

# 安装 Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

# 添加 Windows 目标
RUN rustup target add x86_64-pc-windows-gnu

# 设置工作目录
WORKDIR /project

# 默认命令
CMD ["cargo", "build", "--release", "--workspace", "--target", "x86_64-pc-windows-gnu"]
```

### 构建镜像

```bash
docker build -f Dockerfile.windows -t mangosteen-windows-builder .
```

### 使用镜像构建

```bash
docker run --rm \
  -v "$(pwd)":/project \
  mangosteen-windows-builder
```

---

## 方案三：使用预装 Rust 的 Docker 镜像

使用官方 Rust Docker 镜像，减少构建时间。

```bash
docker run --rm \
  -v "$(pwd)":/project \
  -w /project \
  rust:1.75 \
  bash -c "
    apt-get update && \
    apt-get install -y mingw-w64 && \
    rustup target add x86_64-pc-windows-gnu && \
    cargo build --release --workspace --target x86_64-pc-windows-gnu
  "
```

---

## 构建后处理

构建完成后，Windows 可执行文件位于：

```
target/x86_64-pc-windows-gnu/release/mangosteen.exe
```

### 打包发布件

```bash
# 创建打包目录
mkdir -p dist/mangosteen-windows-x86_64

# 复制可执行文件
cp target/x86_64-pc-windows-gnu/release/mangosteen.exe dist/mangosteen-windows-x86_64/

# 复制文档和配置
cp README.md dist/mangosteen-windows-x86_64/
cp LICENSE dist/mangosteen-windows-x86_64/ 2>/dev/null || true
cp -r docs dist/mangosteen-windows-x86_64/ 2>/dev/null || true
mkdir -p dist/mangosteen-windows-x86_64/config/providers
cp config/config.example.toml dist/mangosteen-windows-x86_64/config/ 2>/dev/null || true
cp config/providers/*.toml dist/mangosteen-windows-x86_64/config/providers/ 2>/dev/null || true

# 打包为 zip
cd dist
zip -qr mangosteen-windows-x86_64.zip mangosteen-windows-x86_64

# 生成校验和
shasum -a 256 mangosteen-windows-x86_64.zip > mangosteen-windows-x86_64.zip.sha256
```

---

## 完整脚本

将以下脚本保存为 `scripts/build-windows-docker.sh`：

```bash
#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="${SCRIPT_DIR}/.."
cd "$PROJECT_DIR"

info()  { printf '\033[36m→\033[0m %s\n' "$*"; }
ok()    { printf '\033[32m✓\033[0m %s\n' "$*"; }
warn()  { printf '\033[33m⚠\033[0m %s\n' "$*"; }
fail()  { printf '\033[31m✗\033[0m %s\n' "$*" >&2; exit 1; }

info "在 Docker 容器中构建 Windows 版本..."

if ! docker ps &>/dev/null; then
  fail "Docker 未运行，请先启动 Docker Desktop"
fi

info "运行 Docker 容器编译..."
docker run --rm \
  -v "$(pwd)":/project \
  -w /project \
  rust:1.75 \
  bash -c "
    apt-get update && \
    apt-get install -y mingw-w64 && \
    rustup target add x86_64-pc-windows-gnu && \
    cargo build --release --workspace --target x86_64-pc-windows-gnu
  "

if [ ! -f "target/x86_64-pc-windows-gnu/release/mangosteen.exe" ]; then
  fail "构建失败：找不到 mangosteen.exe"
fi

ok "编译成功"

info "打包发布件..."
VERSION=$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')
DIST="dist"
STAGE="${DIST}/mangosteen-windows-x86_64"

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

if command -v shasum >/dev/null 2>&1; then
  shasum -a 256 "mangosteen-windows-x86_64.zip" > "mangosteen-windows-x86_64.zip.sha256"
elif command -v sha256sum >/dev/null 2>&1; then
  sha256sum "mangosteen-windows-x86_64.zip" > "mangosteen-windows-x86_64.zip.sha256"
fi

ok "已生成 dist/mangosteen-windows-x86_64.zip"
du -h "dist/mangosteen-windows-x86_64.zip" | awk '{print "   大小：" $1}'
```

### 使用脚本

```bash
chmod +x scripts/build-windows-docker.sh
./scripts/build-windows-docker.sh
```

---

## 推荐方案

**推荐使用方案一（单次构建命令）**，原因：
- 简单直接，无需额外文件
- 每次使用最新镜像
- 适合偶尔构建

**如需频繁构建，使用方案二（Dockerfile）**，原因：
- 预先构建镜像，节省时间
- 可定制镜像内容
- 适合 CI/CD 环境
