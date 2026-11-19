# GaleTerm 路线图（对齐 WindTerm 功能面）

目标：用 Rust 打造跨平台通用终端 / SSH 客户端，**功能上囊括 WindTerm**，但不是源码级 1:1 翻译。

默认技术选型：

- 核心与协议：Rust workspace（本仓库）
- 桌面壳（Phase 3+）：`gpui` + `gpui-component` 原生 GPU UI（会话树、文件管理器、设置页）
- 终端仿真：`alacritty_terminal`；桌面端由 gpui 直接渲染其网格（利用 damage 增量重绘）
- 平台：Windows / macOS / Linux

---

## Phase 1 — 核心打穿（当前）

| 能力 | 状态 | 对应 WindTerm |
|------|------|----------------|
| 本地 Shell / PTY | 已实现 (`gale-term`) | Shell |
| 终端仿真（`alacritty_terminal`） | 已实现 | Term |
| SSH 交互式 shell | 已实现 (`gale-ssh`) | SSH |
| 密码 / 公钥认证 | 已实现 | SSH auth |
| 会话增删查与持久化 | 已实现 (`gale-core`) | Session |
| CLI 入口 `gale` | 已实现 | — |

验收：`gale add` → `gale connect` 可登录远程并交互；`gale connect local` 可开本地 shell。

---

## Phase 2 — 协议与转发补齐

- SSH：agent（已实现：`SSH_AUTH_SOCK` / Pageant）、ProxyJump（已实现：`direct-tcpip` 多跳）、ProxyCommand、ControlMaster
- 端口转发：Local / Remote / Dynamic（已实现：`gale forward`；Dynamic 为 SOCKS5）
- known_hosts 校验与 TOFU 提示（已实现：ask / accept-new / strict；变更密钥拒绝）
- SFTP 基础（已实现：`gale sftp ls|get|put|rm|mkdir|mv`；`russh-sftp`）
- SCP（已实现：`gale scp get|put`，OpenSSH `scp -f/-t` 线协议）
- HTTP / SOCKS5 出站代理（已实现：连 SSH 时 HTTP CONNECT / SOCKS5）
- Telnet / Raw TCP（已实现：`gale-net`；`gale add --protocol telnet|tcp`）
- Serial（已实现：`gale-serial` + `tokio-serial`；`gale ports` / `gale connect`）
- FTP / FTPS（规划：协议 crate `gale-ftp`；CLI `gale ftp` 可后置；**双栏 UI 见 Phase 3**）
- X/Y/ZModem（可选优先级）

---

## Phase 3 — 桌面客户端（gpui）

- 主窗口脚手架（进行中：`apps/gale-desktop`，`gpui-kit 0.10.0`）
- **UI 基调 Termius Night**（见 [`docs/UI.md`](UI.md)）：顶栏 / Vault / 全宽连接 Tab；主题 `gale-termius.json`（Forge 备选）
- 主窗口：水平导航、主机卡网格、终端 Tab（远程连接与命令面板推进中）
- 嵌入式终端视图（进行中：`visible_styled_lines` + `StyledText` 段色已通；待补选中复制 / 光标 / damage）
- 主题 / 配色（UI：Termius 紫；终端默认 ANSI/256/truecolor 已解析；scheme 导入后续）
- 多语言 UI（进行中：桌面 `en` / `zh-CN`；跟随系统或手动切换；见 ARCHITECTURE「国际化」）
- 会话布局恢复
- **双栏文件管理器**（本地 | 远程，对标 WinSCP / FileZilla）— 见下方专节
- 凭据保险箱（系统 keychain，禁止明文落盘）
- 系统托盘：`tray-icon`
- 全局快捷键（如 Quake 式下拉终端）：`global-hotkey`
- 原生菜单：优先 gpui 自带平台菜单，不足再用 `muda`；命令面板（Cmd+K）后续
- ~~**会话模型预留**~~：`Protocol::Ftp` / `Rdp` / `Vnc` 与配置字段已落地；桌面可建档（`gale add` / 快速连接）

### 双栏文件管理器（FTP / SFTP 共用）

> 交互形态：**左栏本地目录，右栏远程目录**；拖拽 / 按钮传输；同一套 UI 后端可切换 FTP、FTPS、SFTP（以及本地↔本地）。

| 能力 | MVP | 后续 |
|------|-----|------|
| 浏览 | 双栏列表（名称 / 大小 / 时间 / 权限）、目录进入、上级、刷新、路径栏 | 书签、历史、隐藏文件、列排序持久化 |
| 传输 | 选中上传 / 下载、进度条、覆盖确认 | 队列、断点续传、校验、带宽限制 |
| 文件操作 | 新建目录、删除、重命名 | chmod、软链、多选批量 |
| 协议 | **SFTP**（已有 `gale-ssh` SFTP）+ **FTP**（`gale-ftp`） | **FTPS**（显式 TLS，`AUTH TLS`）；被动模式优先 |
| 布局 | 左右分栏可拖拽分隔；可与终端 Tab 并存（会话内面板或独立「文件」Tab） | 左右对调、三栏（含传输队列） |

架构落点：

- 协议：`gale-ftp`（控制连接 + 数据连接；主动/被动；可选 FTPS）
- 抽象：`RemoteFs` trait（`list` / `stat` / `get` / `put` / `mkdir` / `rm` / `rename`）— SFTP 与 FTP 共用，双栏只依赖 trait
- UI：`FileBrowser`（`apps/gale-desktop`）双栏 + 传输队列；不绑死某一协议
- CLI（可选）：`gale ftp ls|get|put|…` 便于测协议，不替代双栏体验

建议实现顺序：

1. 抽出 `RemoteFs`，SFTP 适配现有 `SftpClient`  
2. `gale-ftp` MVP（USER/PASS、PASV、LIST/RETR/STOR/MKD/RMD/RNFR-RNTO）  
3. 桌面双栏接 SFTP → 再接 FTP  
4. FTPS + 凭据保险箱 + 传输队列  

验收：打开 FTP/SFTP 会话出现左右双栏；本地↔远程上传下载可见进度；目录操作可用。

---

## Phase 3.5 — 远程桌面（RDP / VNC）

> 图形协议，**依赖 Phase 3 桌面窗口**；CLI 仅做连通性探测 / 经 SSH 隧道拨号，不追求完整交互。

### 目标能力

| 协议 | MVP | 后续 |
|------|-----|------|
| **VNC (RFB)** | 密码认证、桌面帧渲染、键鼠、剪贴板文本 | 编码协商（Raw/ZRLE/Tight）、多显示器、文件传输（可选） |
| **RDP** | 用户名密码、基本桌面、键鼠、剪贴板 | NLA/CredSSP、网关、驱动器重定向、音频（按优先级） |

### 架构落点

- 新 crate：`gale-vnc`（RFB 客户端）、`gale-rdp`（RDP 客户端封装）
- 新视图：`gale-desktop-view`（或 `apps/gale-desktop` 内）— 将远程帧缓冲绘制到 gpui surface；与 `gale-term-view` 并列，同属「会话内容区」
- 复用：会话存储、凭据保险箱、经 SSH `LocalForward` / ProxyJump 打隧道再连 RDP/VNC（堡垒场景）
- 许可约束：只选 **MIT / Apache-2.0** 实现或自研；禁止引入 GPL 客户端核心

### 选型倾向（实施前再锁版本）

- **VNC**：优先自研精简 RFB（控制面清晰）或评估成熟 MIT/Apache crate；先 Raw/ZRLE，再补 Tight
- **RDP**：优先评估 [IronRDP](https://github.com/Devolutions/IronRDP)（Apache-2.0）作为协议栈；UI 帧仍由我们画进 gpui，不嵌对方窗口
- **不做**：嵌入系统 `mstsc` / 外部 `vncviewer` 进程当长期方案（可作开发期 fallback，不上正式架构）

### 建议实现顺序

1. ~~`gale-core` 增加 Rdp/Vnc 会话配置~~ ✅（CLI / 快速连接可建档；桌面内「新建远程桌面」表单仍待做）  
2. ~~VNC MVP（协议更简单）→ gpui 帧显示 + 输入~~ ✅（`gale-vnc` + `gale-desktop-view`；Raw/CopyRect）  
3. RDP MVP（IronRDP 或等价）→ 同视图管线  
4. SSH 隧道一键：会话已有 `jump_session_id` 字段 → 本地转发自动拉起  
5. 剪贴板、全屏、缩放、重连策略  

验收：桌面内打开 VNC/RDP Tab，可登录并键鼠操作；可经已有 SSH 会话隧道连接。

---

## Phase 4 — 生产力特性（WindTerm 差异化）

- Auto Completion
- Command Palette / Command Sender / Quick Bar
- Sync Input（多会话同步输入）
- Focus Mode / Free Type Mode
- Explorer Pane / Shell Pane
- 本地/远端 Vim 键位模式
- 时间戳、折叠、括号高亮
- tmux 集成
- 会话日志（手动 / 自动）
- 锁屏、透明度、在线搜索选中文本

---

## Phase 5 — 性能与质量

- 动态内存压缩 / 大回滚缓冲
- 低延迟输入路径、渲染合帧
- vttest 兼容（对标 WindTerm：除 Tektronix 外）
- Unicode / emoji / powerline 宽度完整支持
- 模糊测试、会话导入导出（WindTerm 配置迁移工具）
- 打包安装包（dmg / msi / deb / AppImage）：`cargo-packager`
- 自动更新：`cargo-packager-updater`
- 代码签名与 macOS 公证：CI 自行配置（不再有 Tauri 现成流程）

---

## 功能对照总表（摘要）

| WindTerm 能力域 | 目标 Phase |
|-----------------|------------|
| SSH / Shell / Term 基础 | 1 |
| SFTP、代理、跳板、转发、串口、Telnet；FTP 协议层 | 2（FTP 可与 Phase 3 并行） |
| 跨平台 GUI、会话树、**双栏文件管理器（本地\|远程，FTP/SFTP）** | 3 |
| RDP / VNC 远程桌面 | 3.5 |
| 补全、命令面板、同步输入、tmux 等 | 4 |
| 性能压缩、完整 VT、发布质量 | 5 |

说明：X11 转发、GSSAPI、ZModem、完整 tmux 集成等长尾能力按用户反馈插入 Phase 2–4，不阻塞主线。RDP/VNC 不阻塞 Phase 3 终端主路径，但作为正式产品能力写入 3.5。
