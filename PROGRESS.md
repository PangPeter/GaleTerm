# GaleTerm — 进度与计划

> 给新会话的交接文档。打开本仓库后先读本文件，再读 `docs/ROADMAP.md` / `docs/ARCHITECTURE.md`。

**项目路径**：`/Users/makerv/myDisk/xgs_project/gale-term`  
**目标**：Rust 跨平台通用终端 / SSH 客户端，功能面囊括 WindTerm（非源码 1:1）  
**当前阶段**：Phase 3.5 进行中 — VNC / RDP MVP 已可连；下一步 SSH 隧道与剪贴板/缩放  
**本地 VNC 测试**：`docker start gale-vnc-test`（`127.0.0.1:5901`，密码 `secret`）；停用 `docker rm -f gale-vnc-test`  
**本地 RDP 测试**：`docker start gale-rdp-test`（`127.0.0.1:3389`，账号 `ubuntu`/`ubuntu`，镜像 `danielguerra/ubuntu-xrdp`；探针 `cargo run -p gale-rdp --example probe -- 127.0.0.1 3389 ubuntu ubuntu`）  
**测试机**：`makerv@192.168.55.3`（Ubuntu，OpenSSH 9.6，仅密码登录）可用于远程联调  
**更新日期**：2026-11-19

---

## 1. 当前进度（已完成）

### 工程与工具链

- [x] 本机安装 Rust stable（`rustc` / `cargo` 1.100）
- [x] Cargo workspace 骨架与 `.gitignore`
- [x] `cargo test --workspace` 通过
- [x] `gale` release 二进制可跑（本地 target）

### Phase 1 / 2 功能

| 模块 | 路径 | 状态 | 说明 |
|------|------|------|------|
| 会话模型 + JSON 存储 | `crates/gale-core` | 完成 | SSH / Telnet / TCP / Serial / Local；密码默认不落盘 |
| 本地 PTY | `crates/gale-term` | 完成 | `portable-pty` |
| 终端仿真 | `crates/gale-term` | 完成 | `alacritty_terminal`：`TerminalEmulator` 封装 |
| SSH 交互 shell | `crates/gale-ssh` | 完成 | `russh`；密码 / 公钥 / agent；known_hosts + TOFU |
| ProxyJump / 转发 / 出站代理 | `gale-ssh` + CLI | 完成 | 跳板、Local/Remote/Dynamic、HTTP/SOCKS5 |
| SFTP | `gale-ssh` + CLI | 完成 | `gale sftp ls\|get\|put\|rm\|mkdir\|mv` |
| SCP | `gale-ssh` + CLI | 完成 | `gale scp get\|put`（`-r` 递归）；OpenSSH 线协议 |
| FTP / FTPS | `crates/gale-ftp` | 完成 | 自研：被动模式（EPSV→PASV，用控制连接对端 IP 防 NAT）；MLSD 优先、`LIST -a`/`LIST` 回退（Unix + DOS 格式）；显式 `AUTH TLS` / 隐式 TLS / 自动降级；rustls + 系统根证书，不受信时弹窗确认（指纹可存 `ftp_known_certs`）；数据通道复用 TLS 会话；传输用 `fork()` 独立连接，支持进度与取消。已用 vsftpd 3.0.2（明文 + 强制 TLS + `require_ssl_reuse`）实测 |
| Telnet / Raw TCP | `crates/gale-net` | 完成 | IAC 协商（SGA/Echo/TTYPE/NAWS）；`gale connect` |
| Serial | `crates/gale-serial` | 完成 | `tokio-serial`；`gale ports`；baud/data/stop/parity |
| CLI | `crates/gale-cli` | 完成 | `list/add/remove/connect/forward/sftp/scp/ports/info` |
| 桌面壳 | `apps/gale-desktop` | 进行中 | Termius X 壳：顶栏导航 / Vault 主机卡 / 全宽终端 Tab；本地 PTY + 远程会话（`remote_session.rs`，独立 tokio 运行时） |
| 会话通道 I/O | `gale-core::SessionInput` + 各协议 `run_channels` | 完成 | SSH / Telnet / TCP / Serial 统一「输入/尺寸/关闭 → 输出字节」通道；`SecretPrompt` 让 GUI 接管密码与私钥口令 |
| UI 基调 | `docs/UI.md` + `themes/gale-termius.json` | 已定 | 柔和深色；紫主色；圆角抬升；Forge JSON 保留备选 |
| 终端视图 | `crates/gale-term-view` | 进行中 | canvas 按格自绘（16/256/truecolor、粗斜体/下划线/反显、CJK 两格）；光标（块/竖线/下划线，失焦空心）；鼠标选区复制；滚轮回看；待 damage 增量、鼠标上报（vim `mouse=a` / tmux） |
| 多语言 | `gale-desktop` | 进行中 | `en` / `zh-CN`；跟随系统或手动；`locales/ui.yml` |

### 快速验证

```bash
cd /Users/makerv/myDisk/xgs_project/gale-term
cargo run -p gale-desktop
cargo run -p gale-cli -- info
cargo run -p gale-cli -- connect local
```

会话文件默认位置（macOS）：

`~/Library/Application Support/gale-term/sessions.json`

---

## 2. 已知缺口 / 技术债

1. **Host key 证书**：`@cert-authority` / 主机证书链校验尚未做。
2. **Agent 转发**：远端 agent forwarding 未做。
3. **CLI 本地 / 串口 / telnet connect**：stdin 非 raw mode，交互体验粗糙；桌面端会换掉。
4. **终端仿真**：CLI 尚未接入 `TerminalEmulator`（仍直通 stdout）。
5. **SCP**：依赖远端 `scp` 二进制；无 OpenSSH 兼容实现。无 `scp -O` 强制旧协议场景由远端决定。
6. **仓库尚未 `git init`**。
7. ~~桌面终端尺寸固定 100×30~~ ✅ `TerminalPane` 按 em 宽 × 1.3 行高铺满并发 `TerminalResized`；本地 PTY / 远端 `window-change` / Telnet NAWS 跟随。CJK 已按格定位，不再错位。
8. **认证回落**：公钥 / agent 失败且服务器接受 password 时改问密码（同 OpenSSH）；keyboard-interactive 仍按密码方式发送。

---

## 3. 下一步计划

### Phase 2（已完成）

1. ~~known_hosts / agent / ProxyJump / 转发 / 出站代理~~ ✅  
2. ~~SFTP~~ ✅  
3. ~~SCP / Telnet / Raw TCP / Serial~~ ✅  

Phase 2 可选长尾（不阻塞 Phase 3）：ProxyCommand、ControlMaster、ZModem、X11、**FTP 协议 crate（可与 Phase 3 并行）**。

### Phase 3（进行中）

1. ~~锁定版本 + 脚手架 `apps/gale-desktop`（`gpui-kit 0.10.0`）~~ ✅  
2. ~~`gale-term-view` MVP + 本地 Shell Tab~~ ✅  
3. ~~会话侧栏读取 `SessionStore`~~ ✅  
4. ~~多语言 `en` / `zh-CN`（跟随系统 + 手动）~~ ✅  
5. ~~UI 基调 Forge 骨架~~ ✅ → ~~改版 Termius X 壳（Vault + 全宽 Tab）~~ ✅  
6. ~~`gale-term` `visible_styled_lines` + `gale-term-view` StyledText 段色~~ ✅（p10k / powerline 背景段可用）  
7. ~~桌面内从 Vault 连接 SSH / Telnet / TCP / Serial~~ ✅（主机卡 / 搜索框快速连接；GUI 指纹确认 + 密码/口令弹窗；ProxyJump 链）  
8. ~~完整菜单 / 命令面板（Cmd+K）~~ ✅（`menus.rs` 原生菜单 + 快捷键；`app/palette.rs` 搜主机/标签页/命令/`user@host`；终端粘贴含 bracketed paste、DECCKM 方向键）  
9. ~~`gale-term-view` 光标 + 选中复制 + 滚动回看~~ ✅（`ScreenSnapshot` 按列定位、宽字符占 2 格；canvas 自绘 + `force_width` 对齐网格；alacritty `Selection` 单/双/三击；⌘C/⌘V；滚轮回看，备用屏转方向键）。真正的 damage 增量重绘仍待做（目前每帧整屏快照，输出按帧合并）  
10. ~~双栏文件管理器（左本地 \| 右远程）~~ ✅  
    - ~~SFTP 双栏~~ ✅（`apps/gale-desktop/src/files.rs` + `files/backend.rs` 的 `Backend{Local,Sftp}`；选主机/`user@host` 连接、复用 GUI 弹窗与跳板链；双击进目录/传文件、⌘/Shift 多选、两栏拖拽、Finder 拖入上传；新建文件夹/重命名/删除确认/同名替换确认；传输队列带进度与取消；`SftpClient::*_with_progress` 已在测试机实测）  
    - ~~FTP / FTPS~~ ✅（`gale-ftp` 作为 `Backend::Ftp` 接入；`ftp://`、`ftps://`、`ftp user@host` 快速连接；明文 FTP 在远程栏标“未加密”；证书确认复用主机指纹弹窗）  
    - ~~交互增强~~ ✅（右键菜单：打开/上传下载/默认应用打开/访达显示/复制名称与路径/重命名/改权限/删除/新建文件与文件夹/全选/多选/隐藏文件/刷新；多选模式（复选框列 + 表头全选，Space 勾选、Esc 退出）；列头点击排序；栏内筛选；F2 重命名、⌘C 复制路径；文件图标用 VS Code Material Icon Theme（MIT，`scripts/vendor-file-icons.py` 抽取 860 个 SVG 到 `assets/file-icons`，`build.rs` 嵌入，`file_icons.rs` 按文件名→最长扩展名→默认匹配））  
    - **后续**：断点续传（REST / SFTP offset）；FTP 主动模式（PORT）；远程文件在线编辑（下载→外部编辑→回传）；栏内复制/剪切粘贴  
11. ~~凭据保险箱（系统 keychain）~~ ✅（`gale-core::vault`：`keyring` 4 → macOS 钥匙串 / Windows 凭据管理器 / Linux Secret Service，服务名 `GaleTerm`，账户名 `ssh://user@host:port`、`ftp://…`、`key:/私钥路径`；`vault_index.json` 只记“存了哪些”，不含密钥。`SecretPrompt` / `FtpPrompt` 新增 `saved_credential` / `ask_credential` / `credential_result`：先试已保存的，被拒则标记失效并重新询问；密码框“记住”勾选后在认证成功时才写入；SSH 与 SFTP 共用同一条。Vault → 钥匙串页可查看与删除）  
    - **后续**：CLI 读取已保存凭据；代理（HTTP/SOCKS5）密码进钥匙串；`sessions.json` 里历史明文 `password` 字段迁移  
12. ~~会话模型完善~~ ✅  
    - ~~`Protocol::Ftp`~~ ✅（`FtpConfig`/`FtpSecurity`；CLI `gale add --protocol ftp|ftps`）  
    - ~~`Protocol::Rdp` / `Protocol::Vnc`~~ ✅（`RdpConfig`/`VncConfig`：host/port/凭据字段 + 可选 `jump_session_id` 走 SSH 隧道；`gale add --protocol rdp|vnc`；桌面 `rdp://`/`vnc://` 快速连接与主机卡片）

### 紧随：Phase 3.5（RDP / VNC）

1. ~~`gale-vnc`：RFB MVP → gpui 帧显示 + 键鼠~~ ✅（`crates/gale-vnc`：RFB 3.8、VNC DES 密码认证、Raw/CopyRect、BGRA 帧缓冲；`crates/gale-desktop-view` 绘制 + 键鼠；桌面 `vnc://` / 主机卡开 Tab；钥匙串复用；Docker `dorowu/ubuntu-desktop-lxde-vnc` 于 `127.0.0.1:5901` 实测握手与出帧）  
   - **后续（VNC）**：ZRLE/Tight、本地光标、剪贴板双向、缩放/全屏、SSH `jump_session_id` 隧道  
2. ~~`gale-rdp`：IronRDP → 同一桌面视图管线~~ ✅（`crates/gale-rdp`：IronRDP 0.17 + blocking I/O；TLS+CredSSP/NLA；BGRA 帧；键鼠经 `ironrdp-input`；桌面 `rdp://user@host` / 主机卡开 Tab；钥匙串 `rdp://…`；Docker xrdp `127.0.0.1:3389` 探针已出帧；workspace `[patch]` 对齐 `ironrdp-connector` 的 picky/sspi 以与 `russh` 共存，见 `vendor/ironrdp-connector` / [IronRDP#1363](https://github.com/Devolutions/IronRDP/issues/1363)）  
   - **后续（RDP）**：证书校验弹窗（现 MVP 接受任意 TLS 证书）、网关、驱动器/剪贴板重定向、音频  
3. 经 SSH 隧道 / ProxyJump 一键连远程桌面  
4. 剪贴板、全屏、缩放、重连  

详细见 [`docs/ROADMAP.md`](docs/ROADMAP.md)「Phase 3.5」。

### 再往后

- **Phase 4**：补全、命令面板、同步输入、tmux  
- **Phase 5**：性能、完整 VT、发布  

详细对照表见 [`docs/ROADMAP.md`](docs/ROADMAP.md)。

---

## 4. 新会话建议开场白

```text
打开项目 /Users/makerv/myDisk/xgs_project/gale-term。
先读 PROGRESS.md 和 docs/ROADMAP.md / docs/ARCHITECTURE.md。
Phase 3 已脚手架：cargo run -p gale-desktop 可开本地 Shell。
请继续：
1) 桌面 Tab 接入 SSH（gale-ssh）/ Telnet
2) gale-term-view：光标 / 选中复制 / damage
3) 双栏文件管理器 + gale-ftp
版本锁定：gpui-kit 0.10.0（gpui-pre =0.3.12）。
```

---

## 5. 目录速查

```
gale-term/
  PROGRESS.md
  README.md
  Cargo.toml
  crates/
    gale-core/       ← 会话 / 存储
    gale-term/       ← PTY + 终端仿真
    gale-ssh/        ← SSH + SFTP + SCP
    gale-net/        ← Telnet + Raw TCP
    gale-serial/     ← 串口
    gale-ftp/           ← FTP / FTPS
    gale-vnc/           ← VNC / RFB（Phase 3.5）
    gale-rdp/           ← RDP（Phase 3.5，IronRDP）
    gale-term-view/     ← gpui 终端视图
    gale-desktop-view/  ← gpui 远程桌面视图（VNC/RDP）
    gale-cli/           ← 命令行 gale
  apps/
    gale-desktop/       ← gpui 桌面
  vendor/
    ironrdp-connector/  ← picky/sspi 对齐补丁（与 russh 共存）
  docs/
```

## 6. 选型锁定（勿轻易改）

| 项 | 选择 |
|----|------|
| 语言 | Rust |
| SSH / SFTP / SCP | `russh` / `russh-sftp` / 自研 SCP 线协议 |
| Telnet / TCP | 自研 `gale-net` |
| Serial | `tokio-serial` 5.5 |
| 本地 PTY | `portable-pty` |
| 终端仿真 | `alacritty_terminal` |
| 桌面壳 | `gpui-kit = 0.10.0`；UI 基调 **Termius Night**（`docs/UI.md`）；终端视图自研；Zed `terminal_view` 为 GPL 不可复制 |
| UI 语言 | `rust-i18n`；`en` / `zh-CN`；默认跟随系统，可手动；见 ARCHITECTURE「国际化」 |
| FTP / FTPS（Phase 3） | 自研 `gale-ftp`（TLS 用 `tokio-rustls` + ring + `rustls-native-certs`）；双栏 UI 与 SFTP 共用 `files/backend.rs` 的 `Backend` |
| RDP / VNC（Phase 3.5） | `gale-rdp`（IronRDP 0.17 Apache-2.0）/ `gale-vnc`（RFB）；帧进 `gale-desktop-view`；可经 SSH 隧道（待做） |
| 平台目标 | Windows / macOS / Linux |
| 安全约束 | workspace `unsafe_code = forbid` |

## 7. 远程桌面备忘（RDP / VNC）

- **已写入方案**：是（ROADMAP Phase 3.5 + ARCHITECTURE crate 规划）
- **实现状态**：VNC MVP + RDP MVP 均已接入桌面 Tab；SSH 隧道 / 剪贴板 / 缩放仍待做
- **为何不放 Phase 2**：需要图形窗口；CLI 无法提供完整体验
- **为何不阻塞 Phase 3 主线**：先把终端 Tab / 会话树跑通，再接帧缓冲视图
- **许可**：只用 MIT/Apache；RDP 用 IronRDP；禁止 GPL 客户端核心
- **依赖注意**：IronRDP 与 `russh` 的 `curve25519-dalek` 线冲突，workspace 用 `vendor/ironrdp-connector` patch

## 8. FTP / 双栏文件管理器备忘

- **已写入方案**：是（ROADMAP Phase 3「双栏文件管理器」+ `gale-ftp`）
- **实现状态**：SFTP、FTP、FTPS 双栏均已完成
- **UI 形态**：左栏本地目录，右栏远程目录；拖拽 / 按钮传输；进度与队列
- **协议共用**：SFTP 与 FTP/FTPS 都走 `files/backend.rs` 的 `Backend` 枚举，换后端不换双栏
- **FTPS**：显式 / 隐式均支持；默认 `Auto`（服务器支持就上 TLS，否则明文并标“未加密”）；仅被动模式
- **本地测试服务器**：`docker run fauria/vsftpd`（见本次对话，明文 2121 / TLS 2990，账号 `tester`）；注意 macOS LibreSSL 的 `openssl req -x509` 默认生成 v1 证书，rustls 不接受，要加 `-addext` 生成 v3
- **明文 FTP**：允许连接，但 UI 应提示不安全（生产优先 FTPS/SFTP）
