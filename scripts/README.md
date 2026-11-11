# Mangosteen 编译脚本归档

## 历有脚本归档

### build-release.sh.bak
原通用跨平台构建脚本已归档。
- 用于构建当前平台
- 支持指定目标三元组（--target）
- 不支持 macOS 双向编译
- 不支持 Windows 交叉编译（需在 Windows 上构建）

## 现有脚本（可用）

### macOS 构建
- `scripts/build-macos.sh` - macOS 双架构构建
  - `--intel` - 仅 x86_64
  - `--apple-silicon` - 仅 aarch64
  - 默认双架构

### Windows 构建
- `scripts/build-windows.sh` - 原脚本（已被弃用）
- `scripts/build-windows-docker.sh` - Docker 交叉编译（推荐）

### 统一构建
- `scripts/build-all.sh` - macOS + Windows 全平台构建脚本
  - `--macos-only` - 仅 macOS
  - `--windows-only` - 使用 Docker 构建Windows

## 编译脚本依赖

### macOS
- Rust 工具链（rustup + cargo）
- 无额外依赖

### Windows Docker
- Docker Desktop（需运行）
- rust:latest 镜像（首次需下载约 600MB）
- MinGW（容器内 apt-get）

## 快速开始

```bash
# macOS 双架构
./scripts/build-macos.sh

# macOS + Windows（需 Docker）
./scripts/build-all.sh

# 仅 macOS
./scripts/build-all.sh --macos-only

# 仅 Windows（需 Docker）
./scripts/build-all.sh --windows-only
```

## 输出产物

- macOS: `dist/mangosteen-macos-x86_64.tar.gz` / `dist/mangosteen-macos-aarch64.tar.gz`
- Windows: `dist/mangosteen-windows-x86_64.zip`
- 所有平台：校验和文件

详细说明见 `docs/BUILD.md`
