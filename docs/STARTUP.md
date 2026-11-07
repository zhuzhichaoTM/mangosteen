# Mangosteen 启动指南

## 启动方式

### 1. 使用 release 二进制（推荐）

```bash
# 启动服务（常驻后台）
/Users/zhuzhichao/Documents/mangosteen/target/release/mangosteen serve

# 启动指定端口（默认 11516）
/Users/zhuzhichao/Documents/mangosteen/target/release/mangosteen serve --port 11517

# 停止所有服务
/Users/zhuzhichao/Documents/mangosteen/target/release/mangosteen stop --all

# 清理端口
lsof -ti:11516 | xargs kill -9 2>/dev/null
```

### 2. 使用 debug 二进制

```bash
/Users/zhuzhichao/Documents/mangosteen/target/debug/mangosteen serve
```

## 常驻服务管理

### 启动
```bash
# 方式一：直接启动 release 二进制（推荐）
/Users/zhuzhichao/Documents/mangosteen/target/release/mangosteen serve

# 方式二：启动指定端口
/Users/zhuzhichao/Documents/mangosteen/target/release/mangosteen serve --port 11517

# 方式三：清理端口后启动
lsof -ti:11516 | xargs kill -9 2>/dev/null
/Users/zhuzhichao/Documents/mangosteen/target/release/mangosteen serve
```

### 重启
```bash
pkill -9 -f "mangosteen serve"
sleep 3
/Users/zhuzhichao/Documents/mangosteen/target/release/mangosteen serve
```

### 停止
```bash
pkill -9 -f "mangosteen serve"
```

### 查看状态
```bash
# 检查服务是否存活
curl http://127.0.0.1:11516/health

# 查看端口占用
lsof -i :11516
```

## 关于 launchd 守护进程

系统有一个 launchd 配置：
```
/Users/zhuzhichao/Library/LaunchAgents/com.mangosteen.serve.plist
```

配置指向 release 二进制，开启后后台运行服务。

## 端口说明

| 端口 | 用途 |
|------|------|
| 11516 | Mangosteen 网关服务（默认） |
| 8080 | 远程 llama-server 端口（云端实例） |

## 常见问题

### 端口被占用
```bash
lsof -ti:11516 | xargs kill -9 2>/dev/null
```

### 网络操作失败（Address already in use）
```bash
lsof -ti:11516 | xargs kill -9 2>/dev/null
```

### 端口占用
```bash
lsof -ti:11516
```