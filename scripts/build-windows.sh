#!/bin/bash
# ==============================================================================
# Mangosteen Windows 可执行文件交叉编译脚本（macOS 宿主）
# 用途：在 macOS 上编译可在 Windows 运行的 mangosteen.exe
# 依赖：rustup, cargo
# ==============================================================================
set -euo pipefail

# ==================== 配置区域 ====================
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_DIR="${SCRIPT_DIR}"  # 脚本位于项目根目录
TARGET="x86_64-pc-windows-msvc"
PROFILE="release"
BINARY_NAME="mangosteen.exe"
OUTPUT_NAME="mangosteen-windows-x86_64.exe"
CARGO_CMD="cargo"
RUSTUP_CMD="rustup"
# =================================================

# 颜色输出
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

log_info()  { echo -e "${BLUE}[INFO]${NC}  $*"; }
log_ok()    { echo -e "${GREEN}[OK]${NC}   $*"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
log_err()   { echo -e "${RED}[ERR]${NC}   $*" >&2; }

echo "================================================"
echo "  Mangosteen Windows 可执行文件交叉编译"
echo "  宿主：macOS ($(uname -m))"
echo "  目标：Windows x86_64 ($TARGET)"
echo "================================================"
echo ""

# ------------------------------------------------------------------
# 步骤 1：检查 rustup 是否安装
# ------------------------------------------------------------------
log_info "步骤 1/6：检查 rustup 环境..."
if ! command -v "$RUSTUP_CMD" &> /dev/null; then
    log_err "未找到 rustup，请先安装 Rust 工具链："
    echo "  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh"
    exit 1
fi
log_ok "rustup 已安装：$($RUSTUP_CMD --version)"

if ! command -v "$CARGO_CMD" &> /dev/null; then
    log_err "未找到 cargo，请确保 Rust 工具链完整安装。"
    exit 1
fi
log_ok "cargo 已安装：$($CARGO_CMD --version)"

# ------------------------------------------------------------------
# 步骤 2：添加 Windows 交叉编译目标
# ------------------------------------------------------------------
log_info "步骤 2/6：添加交叉编译目标 $TARGET..."
if "$RUSTUP_CMD" target list --installed 2>/dev/null | grep -qx "$TARGET"; then
    log_ok "目标 $TARGET 已安装，跳过添加。"
else
    log_info "正在安装目标 $TARGET..."
    "$RUSTUP_CMD" target add "$TARGET"
    log_ok "目标 $TARGET 安装成功。"
fi

# ------------------------------------------------------------------
# 步骤 3：检查项目文件
# ------------------------------------------------------------------
log_info "步骤 3/6：检查项目文件..."
if [ ! -f "$PROJECT_DIR/Cargo.toml" ]; then
    log_err "未找到 Cargo.toml，请确保脚本位于 Mangosteen 项目根目录。"
    exit 1
fi
log_ok "找到 Cargo.toml：$PROJECT_DIR/Cargo.toml"

# ------------------------------------------------------------------
# 步骤 4：执行交叉编译
# ------------------------------------------------------------------
log_info "步骤 4/6：开始交叉编译（release 模式）..."
log_info "这可能需要几分钟，请耐心等待..."

START_TIME=$(date +%s)

"$CARGO_CMD" build \
    --release \
    --target "$TARGET" \
    --manifest-path "$PROJECT_DIR/Cargo.toml"

END_TIME=$(date +%s)
ELAPSED=$((END_TIME - START_TIME))
log_ok "编译完成，耗时约 ${ELAPSED} 秒。"

# ------------------------------------------------------------------
# 步骤 5：验证输出文件
# ------------------------------------------------------------------
log_info "步骤 5/6：验证输出文件..."
BUILT_BINARY="$PROJECT_DIR/target/$TARGET/$PROFILE/$BINARY_NAME"

if [ ! -f "$BUILT_BINARY" ]; then
    log_err "编译后未找到预期的可执行文件：$BUILT_BINARY"
    log_info "请检查编译输出是否有错误。"
    exit 1
fi

FILE_SIZE=$(du -h "$BUILT_BINARY" | cut -f1)
log_ok "可执行文件生成成功：$BUILT_BINARY"
log_ok "文件大小：$FILE_SIZE"

# ------------------------------------------------------------------
# 步骤 6：复制到输出目录并重命名
# ------------------------------------------------------------------
log_info "步骤 6/6：复制到输出目录..."
OUTPUT_DIR="$PROJECT_DIR/dist"
mkdir -p "$OUTPUT_DIR"

OUTPUT_PATH="$OUTPUT_DIR/$OUTPUT_NAME"
cp "$BUILT_BINARY" "$OUTPUT_PATH"
log_ok "已复制到：$OUTPUT_PATH"
log_ok "输出大小：$(du -h "$OUTPUT_PATH" | cut -f1)"

# ------------------------------------------------------------------
# 完成提示
# ------------------------------------------------------------------
echo ""
echo "================================================"
echo "  交叉编译完成！"
echo "================================================"
echo ""
echo "  输出文件：$OUTPUT_PATH"
echo "  文件大小：$(du -h "$OUTPUT_PATH" | cut -f1)"
echo "  目标平台：Windows x86_64"
echo ""
echo "  下一步："
echo "    1. 将 $OUTPUT_NAME 传输到 Windows 机器运行。"
echo "    2. （可选）使用代码签名证书对 .exe 进行签名，"
echo "       避免 Windows SmartScreen 警告。"
echo ""
echo "  代码签名示例（macOS 上使用 osslsigncode）："
echo "    osslsigncode sign \\"
echo "      -certs codesign.pfx \\"
echo "      -key \"你的密码\" \\"
echo "      -n \"Mangosteen\" \\"
echo "      -i \"https://github.com/zhuzhichaoTM/mangosteen\" \\"
echo "      -t http://timestamp.digicert.com \\"
echo "      $OUTPUT_NAME \\"
echo "      $OUTPUT_NAME.signed.exe"
echo ""
echo "  注意："
echo "    - 首次编译可能需要下载目标标准库，耗时较长。"
echo "    - 后续增量编译会更快。"
echo "    - 未签名的 .exe 在 Windows 上可能触发 SmartScreen 警告。"
echo "================================================"
