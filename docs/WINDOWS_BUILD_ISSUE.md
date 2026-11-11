# macOS 14 无法构建 Windows 版本的原因说明

## 问题原因

当前系统信息：
- macOS 版本：14.6.1 (Sonoma)
- Homebrew 状态：已停止为 macOS 14 提供预编译 bottles

### Homebrew 限制

Homebrew 官方已停止为 macOS 14（Sonoma）提供预编译二进制包（bottles），原因如下：

1. **版本过旧**：macOS 14 发布于 2023 年，Homebrew 团队已将支持重心转向更新版本（macOS 15+）
2. **资源限制**：维护旧版本的预编译包需要大量资源
3. **兼容性问题**：旧版本系统与新版依赖包可能存在兼容性问题

### MinGW 安装失败

尝试安装 MinGW（mingw-w64）时遇到的问题：

```bash
brew install mingw-w64
```

错误信息：
```
Warning: You are using macOS 14.
We (and Apple) do not provide support for this old version.
Homebrew no longer builds bottles for this configuration.
```

Homebrew 会尝试从源码编译，但由于：
- 缺少预编译 bottle，需要从源码编译
- 编译过程中需要下载大量依赖
- 网络问题或 sandbox 权限限制导致失败

## 解决方案

### 方案 1：使用 cross 交叉编译工具（需要 Docker）

`cross` 是 Rust 官方推荐的跨平台编译工具，使用 Docker 容器进行交叉编译，无需宿主机安装 MinGW。

**已安装 cross 工具**

**当前状态**：
- ✅ cross 已安装成功（v0.2.5）
- ❌ Docker 未运行，cross 无法使用

使用前需要启动 Docker Desktop：

```bash
# 启动 Docker Desktop（macOS）
open -a Docker

# 等待 Docker 启动后验证
docker ps

# 使用 cross 构建 Windows 版本
export PATH="$HOME/.cargo/bin:$PATH"
cross build --release --workspace --target x86_64-pc-windows-gnu
```

**优点**：
- 不依赖 MinGW
- 使用 Docker 容器隔离环境
- 支持更多目标平台

**缺点**：
- 需要安装并运行 Docker Desktop
- 首次构建需要下载 Docker 镜像（较大）
- 占用较多系统资源

### 方案 2：升级 macOS 版本

升级到 macOS 15（Sequoia）或更新版本，Homebrew 将恢复提供预编译 bottles。

升级后：

```bash
brew install mingw-w64
./scripts/build-all.sh
```

### 方案 3：使用 MacPorts 替代 Homebrew

MacPorts 对旧版本 macOS 的支持更好：

```bash
# 安装 MacPorts（需先下载 pkg 安装包）
# https://www.macports.org/install.php

sudo port selfupdate
sudo port install mingw-w64
```

### 方案 4：使用 GitHub Actions（生产环境推荐）

在 GitHub 上配置 CI/CD，使用 Windows runner 构建：

```yaml
# .github/workflows/release.yml
name: Release

on:
  push:
    tags:
      - 'v*'

jobs:
  build-windows:
    runs-on: windows-latest
    steps:
      - uses: actions/checkout@v4
      - uses: actions-rust-lang/setup-rust-toolchain@v1
      - run: cargo build --release --workspace
      - uses: actions/upload-artifact@v4
        with:
          name: mangosteen-windows-x86_64
          path: target/release/mangosteen.exe
```

### 方案 5：在 Windows 机器上直接构建

如果有 Windows 环境访问权限：

```powershell
# Windows 上执行
cargo build --release --workspace
# 输出：target/release/mangosteen.exe
```

## 当前状态

### 已完成
- ✅ macOS 双架构构建成功（x86_64 + aarch64）
- ✅ 统一构建脚本 `scripts/build-all.sh` 已创建
- ✅ cross 交叉编译工具已安装（v0.2.5）
- ✅ Windows 编译依赖已修复（windows-sys Win32_Storage 特性）

### 待完成
- ⏳ Docker Desktop 未运行，无法使用 cross
- ⏳ 启动 Docker 后使用 cross 构建 Windows 版本
- ⏳ 打包 Windows 发布件

## 推荐

对于生产环境发布，**强烈推荐使用 GitHub Actions**：
- 免费（公开仓库）
- 稳定可靠
- 无需本地环境配置
- 自动化流程

对于本地开发测试，**推荐方案优先级**：
1. **启动 Docker Desktop + cross**（最佳本地方案）
   - 需要先启动 Docker Desktop
   - 然后运行：`cross build --release --workspace --target x86_64-pc-windows-gnu`
2. **升级 macOS 到 15+**（长期方案）
   - Homebrew 将恢复提供预编译 bottles
   - 可以直接 `brew install mingw-w64`
3. **使用 MacPorts**（备选方案）
   - 对旧版本 macOS 支持更好
   - 安装：`sudo port install mingw-w64`
