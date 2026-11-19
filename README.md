# GaleTerm

Rust 跨平台通用终端 / SSH 客户端，功能面对齐 [WindTerm](https://github.com/kingToolbox/WindTerm)（非源码 1:1）。

桌面壳对标 **Termius X**：Vault 主机卡 + 全宽终端 Tab，柔和深色 + 紫主色。

> **Phase 3 进行中** — 协议面已齐；桌面可连本地 Shell / SSH / Telnet / TCP / Serial，双栏文件管理器（SFTP + FTP/FTPS）可用。下一步：凭据保险箱、RDP/VNC（Phase 3.5）。

## 快速开始

需要 **Rust 1.75+**（推荐 stable）。

```bash
# 桌面客户端（推荐）
cargo run -p gale-desktop

# CLI（二进制名 gale）
cargo build -p gale-cli --release
cargo run -p gale-cli -- info
cargo run -p gale-cli -- connect local
```

会话默认保存在（macOS）：

`~/Library/Application Support/gale-term/sessions.json`

### CLI 常用

```bash
# 保存并连接 SSH
gale add prod --host 10.0.0.1 --user ubuntu --identity ~/.ssh/id_ed25519
gale list
gale connect prod

# 跳板 / 出站代理
gale add bastion --host bastion.example --user jump --identity ~/.ssh/id_ed25519
gale add prod --host 10.0.0.5 --user ubuntu --identity ~/.ssh/id_ed25519 --jump bastion
gale add proxied --host 10.0.0.1 --user ubuntu --proxy socks5://127.0.0.1:1080

# 端口转发
gale forward local prod --bind 8080 --dest 10.0.0.5:80
gale forward dynamic prod --bind 1080

# SFTP / SCP
gale sftp ls prod /tmp -l
gale sftp get prod /etc/hosts ./hosts
gale scp put prod ./hosts /tmp/hosts

# Telnet / Serial / FTP
gale add bbs --protocol telnet --host towel.blinkenlights.nl
gale ports
gale add uart --protocol serial --device /dev/cu.usbserial --baud 115200
gale add files --protocol ftps --host ftp.example --user demo
```

更多子命令：`gale --help` / `gale <cmd> --help`。

## 能力一览

| 层 | 内容 |
|----|------|
| 终端 | 本地 PTY、`alacritty_terminal` 仿真；桌面 canvas 真彩 / CJK / 选区复制 / 滚轮回看 |
| SSH | 密码 / 公钥 / agent、known_hosts + TOFU、ProxyJump、Local/Remote/Dynamic 转发、HTTP/SOCKS5 出站代理 |
| 文件 | SFTP、SCP；FTP / FTPS；桌面双栏（拖拽、传输队列、右键菜单、Material 图标） |
| 其它协议 | Telnet、Raw TCP、Serial |
| 桌面 | Termius Night 壳、Vault、命令面板（⌘K）、多语言 `en` / `zh-CN` |

## 工作区

```
gale-term/
  apps/
    gale-desktop/     # gpui 桌面（gpui-kit 0.10）
  crates/
    gale-core/        # 会话模型与持久化
    gale-term/        # PTY + 终端仿真
    gale-term-view/   # gpui 终端视图
    gale-ssh/         # SSH + SFTP + SCP
    gale-ftp/         # FTP / FTPS
    gale-net/         # Telnet + Raw TCP
    gale-serial/      # 串口
    gale-cli/         # CLI 入口（bin: gale）
  docs/
    ROADMAP.md
    ARCHITECTURE.md
    UI.md
```

## 文档

- [进度与计划（新会话先读）](PROGRESS.md)
- [功能路线图](docs/ROADMAP.md)
- [架构说明](docs/ARCHITECTURE.md)
- [桌面 UI 基调](docs/UI.md)

## 许可

[MIT](LICENSE)
