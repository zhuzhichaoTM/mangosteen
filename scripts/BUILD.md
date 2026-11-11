# Mangosteen 发布构建说明

## 统一构建脚本

Mangosteen 提供了统一的跨平台构建脚本 `scripts/build-all.sh`，支持在 macOS 宿主机上构建 macOS 和 Windows 的发布件。

### 用法

```bash
# 构建所有支持的平台（macOS + Windows）
./scripts/build-all.sh

# 仅构建 macOS 双架构
./scripts/build-all.sh --macos-only

# 仅构建 Windows x86_64
./scripts/build-all.sh --windows-only

# 查看帮助
./scripts/build-all.sh --help
```

### 输出产物

构建完成后，`dist/` 目录将包含以下文件：

| 文件 | 说明 |
|------|------|
| `mangosteen-macos-x86_64.tar.gz` | macOS Intel 版本 |
| `mangosteen-macos-aarch64.tar.gz` | macOS Apple Silicon 版本 |
| `mangosteen-windows-x86_64.zip` | Windows x86_64 版本 |
| `*.sha256` | 各文件对应的 SHA-256 校验和 |

### 环境要求

#### macOS 宿主机
- Rust 工具链（rustup + cargo）
- Xcode Command Line Tools（用于原生编译）

#### Windows 交叉编译（可选）
如需在 macOS 上交叉编译 Windows 版本，需要安装 MinGW：

```bash
brew install mingw-w64
```

**注意**：macOS 14 较老版本可能无法通过 Homebrew 安装 MinGW。这种情况下，建议使用 GitHub Actions 在 Windows runner 上构建。

### 在 Windows 环境中构建

如果无法在 macOS 上进行 Windows 交叉编译，可以：

1. **使用 GitHub Actions**（推荐）
   - 配置 `.github/workflows/release.yml` 使用 Windows runner
   - 参见项目中的 CI/CD 配置

2. **在 Windows 机器上直接构建**
   ```powershell
   # Windows 上执行
   .\scripts\build-windows.ps1
   # 或使用 cargo 直接构建
   cargo build --release --workspace
   ```

### 验证构建产物

使用校验和验证文件完整性：

```bash
# macOS/Linux
shasum -c dist/mangosteen-macos-x86_64.tar.gz.sha256

# Windows
certutil -hashfile mangosteen-windows-x86_64.zip SHA256
```

### 测试构建产物

解压后验证可执行文件：

```bash
# macOS
tar -xzf dist/mangosteen-macos-x86_64.tar.gz
cd mangosteen-macos-x86_64
./mangosteen --version
./mangosteen doctor

# Windows
unzip dist/mangosteen-windows-x86_64.zip
cd mangosteen-windows-x86_64
.\mangosteen.exe --version
.\mangosteen.exe doctor
```

## 版本发布流程

1. 更新 `Cargo.toml` 中的版本号
2. 运行构建脚本生成所有平台的发布件
3. 在 GitHub 上创建 Release
4. 上传对应的 tar.gz/zip 文件和校验和文件
5. 更新 CHANGELOG.md

## 故障排除

### Windows 交叉编译失败

**错误**：`linker not found`

**解决方案**：
- 安装 MinGW：`brew install mingw-w64`
- 或使用 GitHub Actions Windows runner
- 或在 Windows 机器上直接构建

### macOS 交叉编译失败

**错误**：找不到目标三元组

**解决方案**：
```bash
rustup target add aarch64-apple-darwin
rustup target add x86_64-apple-darwin
```

### 编译警告

当前代码存在一些未使用的变量和导入警告，不影响功能。可以后续通过 `cargo fix` 清理。

## 相关脚本

- `scripts/build-macos.sh` - 专门用于 macOS 双架构构建
- `scripts/build-windows.sh` - Windows 交叉编译脚本（macOS 宿主）
- `scripts/build-release.sh` - 通用发布构建脚本
