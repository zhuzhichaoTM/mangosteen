#!/bin/bash

# mangosteen CLI 指令测试脚本
# 模型: qwen2.5-0.5b default配置模板
# 节点约束: 单价小于1.5元的GPU节点

MANGOSTEEN="/Users/zhuzhichao/Documents/mangosteen/target/release/mangosteen"
LOG_DIR="/Users/zhuzhichao/Documents/mangosteen/test_logs"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
LOG_FILE="$LOG_DIR/test_$TIMESTAMP.log"
SUMMARY_FILE="$LOG_DIR/summary_$TIMESTAMP.txt"

# 创建日志目录
mkdir -p "$LOG_DIR"

echo "========================================" | tee -a "$LOG_FILE"
echo "Mangosteen CLI 指令测试" | tee -a "$LOG_FILE"
echo "时间: $(date)" | tee -a "$LOG_FILE"
echo "========================================" | tee -a "$LOG_FILE"
echo "" | tee -a "$LOG_FILE"

# 指令列表（排除 cloud key 和 deploy/stop）
COMMANDS=(
    "help"
    "version"
    "status"
    "config show"
    "config validate"
    "device list"
    "doctor"
    "cloud instances list"
    "cloud pick --max-price 1.5"
    "cloud cheapest --min-vram 1"
)

# 统计变量
TOTAL_COMMANDS=0
SUCCESS_COUNT=0
FAIL_COUNT=0

# 循环10次
for ROUND in {1..10}; do
    echo "========================================" | tee -a "$LOG_FILE"
    echo "第 $ROUND 轮测试" | tee -a "$LOG_FILE"
    echo "时间: $(date)" | tee -a "$LOG_FILE"
    echo "========================================" | tee -a "$LOG_FILE"
    echo "" | tee -a "$LOG_FILE"

    for CMD in "${COMMANDS[@]}"; do
        TOTAL_COMMANDS=$((TOTAL_COMMANDS + 1))
        echo "执行: mangosteen $CMD" | tee -a "$LOG_FILE"
        echo "时间: $(date)" | tee -a "$LOG_FILE"

        # 执行命令并记录结果
        if $MANGOSTEEN $CMD >> "$LOG_FILE" 2>&1; then
            echo "状态: 成功" | tee -a "$LOG_FILE"
            SUCCESS_COUNT=$((SUCCESS_COUNT + 1))
        else
            echo "状态: 失败" | tee -a "$LOG_FILE"
            FAIL_COUNT=$((FAIL_COUNT + 1))
        fi

        echo "" | tee -a "$LOG_FILE"
        sleep 3
    done

    echo "" | tee -a "$LOG_FILE"
done

# 生成汇总报告
echo "========================================" | tee -a "$SUMMARY_FILE"
echo "CLI 指令测试汇总报告" | tee -a "$SUMMARY_FILE"
echo "========================================" | tee -a "$SUMMARY_FILE"
echo "总执行次数: $TOTAL_COMMANDS" | tee -a "$SUMMARY_FILE"
echo "成功次数: $SUCCESS_COUNT" | tee -a "$SUMMARY_FILE"
echo "失败次数: $FAIL_COUNT" | tee -a "$SUMMARY_FILE"
echo "成功率: $(echo "scale=2; $SUCCESS_COUNT * 100 / $TOTAL_COMMANDS" | bc)%" | tee -a "$SUMMARY_FILE"
echo "" | tee -a "$SUMMARY_FILE"
echo "详细日志: $LOG_FILE" | tee -a "$SUMMARY_FILE"
echo "========================================" | tee -a "$SUMMARY_FILE"

echo "测试完成！"
echo "汇总报告: $SUMMARY_FILE"
echo "详细日志: $LOG_FILE"
