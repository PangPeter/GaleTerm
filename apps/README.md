# Apps

## `gale-desktop`（Phase 3）

gpui + gpui-component 原生桌面壳（经 meta crate `gpui-kit = 0.10.0`，锁定 `gpui-pre =0.3.12`）。

```bash
cargo run -p gale-desktop
```

当前已具备：

- **Forge 壳**：Menu / Toolbar / Session Rail / Stage / Status（见仓库根 `docs/UI.md`）
- 主题 `themes/gale-forge.json`（墨钢深色 + 海藻青绿）
- Tab + **本地 Shell**（PTY → `gale-term-view`）
- 内置 Meslo 字体；多语言 `en` / `zh-CN`

下一步：菜单命令绑定、SSH Tab、cell 级终端、双栏文件管理器。
