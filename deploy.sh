#!/bin/bash

# --- 基础配置 ---
APP_NAME="icon-hub"
WORKING_DIR="$(pwd)"
# 创建必要的目录
mkdir -p "$WORKING_DIR/icons"

echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "🎨 EPG 图标管理系统 - 远程一键部署"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"

# 1. 交互式获取端口
read -p "🔹 请输入服务运行端口 [默认: 3000]: " INPUT_PORT
TARGET_PORT=${INPUT_PORT:-3000}

# 2. 交互式获取下载地址
echo "💡 请从 GitHub Release 复制 .tar.gz 文件的下载链接"
read -p "🔹 请输入二进制包下载 URL: " DOWNLOAD_URL

if [ -z "$DOWNLOAD_URL" ]; then
    echo "❌ 错误: 必须提供有效的下载地址！"
    exit 1
fi

# 3. 下载并解压
echo "📥 正在下载二进制文件..."
curl -L "$DOWNLOAD_URL" -o "${APP_NAME}.tar.gz"

if [ $? -ne 0 ]; then
    echo "❌ 下载失败，请检查 URL 是否正确。"
    exit 1
fi

echo "📦 正在解压..."
# 解压并提取二进制文件
tar -xzf "${APP_NAME}.tar.gz"
chmod +x $APP_NAME
# 移动到当前目录（如果压缩包内带目录）
find . -name "$APP_NAME" -type f -exec mv {} . \;
rm "${APP_NAME}.tar.gz"

echo "✅ 二进制文件准备就绪。"

# 4. 配置 Systemd 服务
echo "⚙️  正在配置 Systemd 服务..."
sudo cat <<EOF > /etc/systemd/system/$APP_NAME.service
[Unit]
Description=EPG Icon Hub
After=network.target

[Service]
Type=simple
User=$USER
WorkingDirectory=$WORKING_DIR
# 直接指向当前目录下的二进制文件
ExecStart=$WORKING_DIR/$APP_NAME
Restart=always
RestartSec=5
# 传递交互端口
Environment=PORT=$TARGET_PORT
# 1GB 内存优化：内存分配策略
Environment=MALLOC_CONF=dirty_decay_ms:1000,muzzy_decay_ms:1000
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
EOF

# 5. 启动服务
echo "🔄 正在重载配置并启动..."
sudo systemctl daemon-reload
sudo systemctl enable $APP_NAME
sudo systemctl restart $APP_NAME

# 6. 获取 IP 并展示结果
LOCAL_IP=$(ip route get 8.8.8.8 | awk '{print $7; exit}' || echo "127.0.0.1")
echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "🎉 部署成功！"
echo "🌐 局域网地址: http://$LOCAL_IP:$TARGET_PORT/admin"
echo "📂 图标目录: $WORKING_DIR/icons"
echo "📜 实时日志: sudo journalctl -u $APP_NAME -f"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"