# GaleTerm

Rust 实现的跨平台通用终端 / SSH 客户端，目标功能面覆盖 [WindTerm](https://github.com/kingToolbox/WindTerm)。

> 当前仓库处于 **Phase 3 进行中**：协议面已齐；桌面 `gale-desktop`（gpui-kit）可开本地 Shell。SSH 桌面连接 / 双栏文件管理器推进中。

## 快速开始

```bash
# 需要 Rust 1.75+
cargo build -p gale-cli --release

# 桌面（Phase 3；UI 支持 en / 简体中文，默认跟随系统语言）
cargo run -p gale-desktop

# 本地 shell（CLI）
cargo run -p gale-cli -- connect local

# 保存并连接 SSH 会话
cargo run -p gale-cli -- add prod --host 10.0.0.1 --user ubuntu --identity ~/.ssh/id_ed25519
cargo run -p gale-cli -- list
cargo run -p gale-cli -- connect prod

# 使用 ssh-agent（需 SSH_AUTH_SOCK；Windows 可用 Pageant）
cargo run -p gale-cli -- add agent-prod --host 10.0.0.1 --user ubuntu --auth agent
cargo run -p gale-cli -- connect agent-prod

# ProxyJump：先保存跳板，再让目标会话经跳板连接
cargo run -p gale-cli -- add bastion --host bastion.example --user jump --identity ~/.ssh/id_ed25519
cargo run -p gale-cli -- add prod --host 10.0.0.5 --user ubuntu --identity ~/.ssh/id_ed25519 --jump bastion
cargo run -p gale-cli -- connect prod

# 端口转发
cargo run -p gale-cli -- forward local prod --bind 8080 --dest 10.0.0.5:80
cargo run -p gale-cli -- forward remote prod --bind 0.0.0.0:9000 --dest 127.0.0.1:3000
cargo run -p gale-cli -- forward dynamic prod --bind 1080

# 经出站代理连接 SSH
cargo run -p gale-cli -- add proxied --host 10.0.0.1 --user ubuntu --proxy socks5://127.0.0.1:1080
cargo run -p gale-cli -- connect proxied
# 或临时指定：gale connect prod --proxy http://user:pass@proxy:8080

# SFTP
cargo run -p gale-cli -- sftp ls prod /tmp -l
cargo run -p gale-cli -- sftp get prod /etc/hosts ./hosts
cargo run -p gale-cli -- sftp put prod ./local.txt /tmp/remote.txt
cargo run -p gale-cli -- sftp mkdir prod /tmp/gale-demo
cargo run -p gale-cli -- sftp mv prod /tmp/a.txt /tmp/b.txt
cargo run -p gale-cli -- sftp rm prod /tmp/b.txt
# 递归目录：gale sftp get prod /var/log ./logs -r

# SCP
cargo run -p gale-cli -- scp get prod /etc/hosts ./hosts
cargo run -p gale-cli -- scp put prod ./hosts /tmp/hosts

# Telnet / Raw TCP
cargo run -p gale-cli -- add bbs --protocol telnet --host towel.blinkenlights.nl
cargo run -p gale-cli -- connect bbs
cargo run -p gale-cli -- add raw --protocol tcp --host 127.0.0.1 --port 9000

# Serial
cargo run -p gale-cli -- ports
cargo run -p gale-cli -- add uart --protocol serial --device /dev/cu.usbserial --baud 115200
cargo run -p gale-cli -- connect uart
```

## 工作区结构

```
gale-term/
  crates/
    gale-core/    # 会话模型与存储
    gale-term/    # PTY + 终端仿真（alacritty_terminal）
    gale-ssh/     # SSH + SFTP + SCP
    gale-net/     # Telnet + Raw TCP
    gale-serial/     # 串口
    gale-term-view/  # gpui 终端视图
    gale-cli/        # 命令行入口 (bin: gale)
  apps/
    gale-desktop/    # gpui 桌面
  docs/
    ROADMAP.md
    ARCHITECTURE.md
```

## 文档

- [**进度与计划（新会话先读）**](PROGRESS.md)
- [功能路线图（对齐 WindTerm）](docs/ROADMAP.md)
- [架构说明](docs/ARCHITECTURE.md)
- [桌面 UI 基调与布局](docs/UI.md)

## 许可

MIT
