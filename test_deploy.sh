#!/bin/bash

# mangosteen 部署和卸载循环测试脚本
# 模型: qwen2.5-0.5b default配置模板
# GPU: v100s

MANGOSTEEN="/Users/zhuzhichao/Documents/mangosteen/target/release/mangosteen"
LOG_DIR="/Users/zhuzhichao/Documents/mangosteen/test_logs"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
LOG_FILE="$LOG_DIR/deploy_test_$TIMESTAMP.log"
SUMMARY_FILE="$LOG_DIR/deploy_summary_$TIMESTAMP.txt"

# 创建日志目录
mkdir -p "$LOG_DIR"

printf "========================================\n" >> "$LOG_FILE"
printf "Mangosteen 部署和卸载循环测试\n" >> "$LOG_FILE"
printf "模型: qwen2.5-0.5b (default配置模板)\n" >> "$LOG_FILE"
printf "GPU型号: v100s\n" >> "$LOG_FILE"
printf "循环次数: 10\n" >> "$LOG_FILE"
printf "时间: $(date)\n" >> "$LOG_FILE"
printf "========================================\n\n" >> "$LOG_FILE"

# 统计变量
DEPLOY_SUCCESS=0
DEPLOY_FAIL=0
STOP_SUCCESS=0
STOP_FAIL=0
SSH_PASSWORD_CHANGED=false

# 循环10次
for ROUND in 1 2 3 4 5 6 7 8 9 10; do
    printf "========================================\n" >> "$LOG_FILE"
    printf "第 %d 轮测试\n" "$ROUND" >> "$LOG_FILE"
    printf "时间: $(date)\n" >> "$LOG_FILE"
    printf "========================================\n\n" >> "$LOG_FILE"

    # 检查并修复SSH密码
    CURRENT_PASS=$(cat "/Users/zhuzhichao/Library/Application Support/mangosteen/secrets/ssh_password")
    if [ "$CURRENT_PASS" != "1qaz@WSX" ]; then
        printf "检测到SSH密码不匹配，正在修复...\n" >> "$LOG_FILE"
        echo "1qaz@WSX" > "/Users/zhuzhichao/Library/Application Support/mangosteen/secrets/ssh_password"
        SSH_PASSWORD_CHANGED=true
        printf "SSH密码已修复为: 1qaz@WSX\n" >> "$LOG_FILE"
    else
        printf "SSH密码正确: 1qaz@WSX\n" >> "$LOG_FILE"
    fi
    printf "\n" >> "$LOG_FILE"
    sleep 3

    # 测试1: 部署
    printf "执行: mangosteen deploy qwen2.5-0.5b --gpu-model v100s --async\n" >> "$LOG_FILE"
    printf "时间: $(date)\n" >> "$LOG_FILE"
    if $MANGOSTEEN deploy qwen2.5-0.5b --gpu-model v100s --async >> "$LOG_FILE" 2>&1; then
        printf "部署: 成功\n" >> "$LOG_FILE"
        DEPLOY_SUCCESS=$((DEPLOY_SUCCESS + 1))
        
        # 等待10秒让部署初始化
        printf "等待10秒让部署初始化...\n" >> "$LOG_FILE"
        sleep 10
        
        # 获取部署ID
        DEPLOY_ID=$($MANGOSTEEN status --json 2>/dev/null | grep -o '"deployment_id":"[^"]*"' | head -1 | cut -d'"' -f4)
        printf "部署ID: $DEPLOY_ID\n" >> "$LOG_FILE"
    else
        printf "部署: 失败\n" >> "$LOG_FILE"
        DEPLOY_FAIL=$((DEPLOY_FAIL + 1))
    fi
    printf "\n" >> "$LOG_FILE"
    sleep 5

    # 测试2: 如果部署成功，测试停止
    if [ -n "$DEPLOY_ID" ]; then
        printf "执行: mangosteen stop $DEPLOY_ID --yes\n" >> "$LOG_FILE"
        printf "时间: $(date)\n" >> "$LOG_FILE"
        if $MANGOSTEEN stop "$DEPLOY_ID" --yes >> "$LOG_FILE" 2>&1; then
            printf "停止: 成功\n" >> "$LOG_FILE"
            STOP_SUCCESS=$((STOP_SUCCESS + 1))
        else
            printf "停止: 失败\n" >> "$LOG_FILE"
            STOP_FAIL=$((STOP_FAIL + 1))
        fi
        printf "\n" >> "$LOG_FILE"
        sleep 5
    fi

    # 测试3: 查看状态
    printf "执行: mangosteen status\n" >> "$LOG_FILE"
    printf "时间: $(date)\n" >> "$LOG_FILE"
    $MANGOSTEEN status >> "$LOG_FILE" 2>&1
    printf "\n" >> "$LOG_FILE"
    sleep 3
done

# 生成汇总报告
printf "========================================\n" >> "$SUMMARY_FILE"
printf "部署和卸载测试汇总报告\n" >> "$SUMMARY_FILE"
printf "========================================\n" >> "$SUMMARY_FILE"
printf "模型: qwen2.5-0.5b (default配置模板)\n" >> "$SUMMARY_FILE"
printf "GPU型号: v100s\n" >> "$SUMMARY_FILE"
printf "循环次数: 10\n" >> "$SUMMARY_FILE"
printf "\n" >> "$SUMMARY_FILE"
printf "部署测试:\n" >> "$SUMMARY_FILE"
printf "  成功: %d\n" "$DEPLOY_SUCCESS" >> "$SUMMARY_FILE"
printf "  失败: %d\n" "$DEPLOY_FAIL" >> "$SUMMARY_FILE"
printf "\n" >> "$SUMMARY_FILE"
printf "卸载测试:\n" >> "$SUMMARY_FILE"
printf "  成功: %d\n" "$STOP_SUCCESS" >> "$SUMMARY_FILE"
printf "  夓败: %d\n" "$STOP_FAIL" >> "$SUMMARY_FILE"
printf "\n" >> "$SUMMARY_FILE"
if [ "$SSH_PASSWORD_CHANGED" = true ]; then
    printf "SSH密码: 已修复为 1qaz@WSX\n" >> "$SUMMARY_FILE"
else
    printf "SSH密码: 正确 (1qaz@WSX)\n" >> "$SUMMARY_FILE"
fi
printf "\n" >> "$SUMMARY_FILE"
printf "详细日志: %s\n" "$LOG_FILE" >> "$SUMMARY_FILE"
printf "========================================\n" >> "$SUMMARY_FILE"

printf "测试完成！\n"
printf "汇总报告: %s\n" "$SUMMARY_FILE"
printf "详细日志: %s\n" "$LOG_FILE"