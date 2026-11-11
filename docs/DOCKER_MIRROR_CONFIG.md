# Docker 镜像源配置指南

## macOS 配置 Docker Desktop 镜像源

### 方法一：通过 Docker Desktop 图形界面配置

1. 打开 Docker Desktop
2. 点击右上角齿轮图标（Settings）
3. 选择 "Docker Engine"
4. 在 "Registry mirrors" 部分添加：

```json
{
  "registry-mirrors": [
    "https://docker.mirrors.ustc.edu.cn",
    "https://hub-mirror.c.163.com",
    "https://mirror.ccs.tencentyun.com"
  ]
}
```

5. 点击 "Apply & Restart"

### 方法二：通过命令行配置（需要重启 Docker Desktop）

```bash
# 备份原配置
cp ~/Library/Group\ Containers/group.com.docker.json ~/Library/Group\ Containers/group.com.docker.json.bak

# 编辑配置
open ~/Library/Group\ Containers/group.com.json
```

在 `registry-mirrors` 部分添加国内镜像源。

### 方法三：临时修改脚本使用预下载的镜像

由于 rust:latest 镜像已下载完成，我们可以直接使用缓存的镜像，不需要重新下载。

## 验证镜像源配置

```bash
docker info
# 查看是否有镜像源配置
```

## 使用已缓存的镜像重新构建

由于 rust:latest 镜像已下载完成，即使不配置镜像源，下次构建也会使用缓存。不过 cargo 依赖下载仍需要配置 Rust 镜像源。

## 配置完成后重新构建

```bash
./scripts/build-windows-dash.sh
```

## 备注

- Docker 镜像源只影响 Docker 镜像下载速度
- Rust crates 依赖下载通过 `~/.cargo/config.toml` 配置（脚本已配置 USTC）
- 首次构建需下载 rust:latest 镜像（约 600MB）
- 后续构建使用缓存，速度会快很多
