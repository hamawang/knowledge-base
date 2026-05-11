# 移动端迁移（Tauri Mobile · iOS + Android）— 任务跟踪

> 目标：在现有 v1.7.1 桌面版（Tauri 2.x + React 19 + rusqlite）基础上，**直接在 master 上**增加 iOS / Android target，复用三层架构与业务 Commands。
> 创建日期：2026-05-04
>
> ## 核心决策（已定）
> | 决策项 | 选择 | 理由 |
> |--------|------|------|
> | 仓库策略 | **同一仓库 + cfg gate**（不另起仓库） | commands 改一次两端生效，schema 不漂移 |
> | 分支策略 | **直接在 master 开发**（不开 mobile 分支） | 减少合并成本；用 cfg gate 在编译时隔离桌面/移动代码，桌面端编译时移动代码完全剥离不进二进制；每次 commit 前必须 `cargo build` 桌面端验证不破 |
> | 兼容策略 | **桌面端继续按 master 出 v1.7.x 版本**，移动端阶段性发版 | 桌面端用户不受影响 |
> | 默认行为 | 闪卡（cards）默认关闭，与 PC 端 `DEFAULT_ENABLED_VIEWS` 保持一致（除 cards 外全部启用） | 复用 PC 端 `FeatureModulesSection` 设计 |
>
> ## 评估结论
> - 后端复用率 **70-75%**（核心业务全保留，砍掉 tray/global-shortcut/autostart/updater/multi-instance/sidecar/PDFium）
> - 前端复用率 **30-40%**（业务组件可用，布局/导航需重写）
> - 总体可行性：⭐⭐⭐⭐ 可行
> - 估算工作量：**一人 4-7 周**

---

## 🔴 执行规则（继续任务前必读）

**每次要开始某条任务前，必须先走三步：**

1. **重新评估必要性**：此刻这条任务是否仍值得做？优先级是否被新情况改变？
2. **给出实现方案**：
   - 涉及哪些文件（lib.rs / commands / services / 前端 layout / capabilities / tauri.conf.json）
   - 具体改什么、新增什么
   - 是否需要 cfg gate / mobile-only crate feature
   - 桌面端是否会受影响 → 如何验证
   - 预计工作量 + 潜在风险点
3. **等待用户确认**后再开始写代码。

⛔ 禁止直接跳过以上三步动手实现。

---

## 🔒 master 直接开发的安全约定

由于不开 mobile 分支，每次 commit 前必须做：

| 检查项 | 命令 | 说明 |
|-------|------|------|
| 桌面端编译通过 | `cd src-tauri && cargo check` | 任何改动后必跑，不破坏现有桌面构建 |
| 桌面端可启动 | `pnpm tauri dev` 启动一次，开主窗+笔记列表 | 验证 cfg gate 隔离没误删桌面代码 |
| 现有功能不退化 | 手测：创建笔记 / 搜索 / AI 对话 / 闪卡 / 同步 | 共享业务代码改动后必测 |
| Schema 兼容性 | 检查是否有 schema 变更，桌面端启动后能跑通迁移 | 移动端加表/字段同样作用于桌面端数据库 |
| `/check` 全栈检查 | `/check` slash command | TypeScript + Rust clippy + 类型对齐 |

每次 commit message 加 scope 标识：`feat(mobile): ...` / `chore(mobile): ...` / `refactor(shared): ...`，便于 git log 区分。

---

## 移动端不兼容功能清单（已盘点）

| 红区（必须 cfg gate 隔离） | 影响代码 |
|---------------------------|---------|
| 系统托盘 | `mod tray`, `tauri = features=["tray-icon"]` |
| 全局快捷键 | `tauri-plugin-global-shortcut`, `services::shortcut`, `commands::shortcut`, `commands::asr` 录音热键 |
| 开机自启 + `--start-minimized` | `tauri-plugin-autostart` |
| 应用自动更新 | `tauri-plugin-updater`（移动端走 App Store / Play） |
| 多开实例 + `--instance` 参数 + 文件锁 | `acquire_instance_lock`, `try_exclusive_lock`, `flock` |
| 多窗口 | `commands::notes::open_note_in_new_window`, `migration-splash` |
| 双击 `.md` 启动 | `extract_md_paths_from_args`, `deliver_md_to_default`, `start_md_deliver_watcher` |
| MCP sidecar（spawn 子进程） | kb-mcp 子 crate, `rmcp transport-child-process` |
| PDFium 动态库加载 | `pdfium-render` + `resources/pdfium/*.dll/dylib/so` |

| 黄区（需适配） | 处理方案 |
|---------------|---------|
| `tauri-plugin-dialog` 多选/目录选 | 改单文件选择 |
| 自定义数据目录 / KB_DATA_DIR | 移动端沙盒强制，整功能砍 |
| WebDAV / S3 同步后台保活 | iOS 后台限制，改"手动按钮 + 通知重启" |
| `commands::import::scan_markdown_folder`（OB 整库导入） | 改 ZIP 上传 / 单文件导入 |

---

## 任务列表

### Phase 0 · 移动端原型设计（已完成 ✅）

#### T-M000 · 移动端 17 页 HTML 原型

- **状态**：`completed` · 完成日期：2026-05-04
- **价值**：⭐⭐⭐⭐⭐  成本：低
- **产物位置**：`output/UI原型/2026-05-04_知识库移动端App/`
- **包含**：
  - [x] `index.html` — 入口页（设计系统 + 流程图 + iframe 实时预览）
  - [x] 5 个 Tab 主页：00-home / 01-notes / 06-ai / 08-tasks / 10-me（统一 5 格 Tab）
  - [x] 9 个二级页：02-note-edit / 03-search / 04-daily / 05-tags / 07-ai-chat / 09-cards / 11-sync / 12-task-edit / 13-trash
  - [x] 2 个特殊页：14-quick-create（新建）/ 15-quick-capture（闪念）
  - [x] 1 个功能开关页：16-feature-toggle（含底部 Tab 配置 + Dashboard 显示项 + 8 个可选模块开关）
- **设计要点**：
  - 主色 `#1677FF`（沿用 PC 端 Ant Design 蓝）+ AI 强调橙 `#FA8C16`
  - 5 Tab 统一：主页 / 笔记 / AI / 待办 / 我的
  - 浮动 FAB 全局指向 `14-quick-create`
  - 闪卡（cards）默认关闭，与 PC 端 `DEFAULT_ENABLED_VIEWS` 一致
  - 所有页面顶栏统一为「← 返回 + 居中标题 + 右按钮」（参考 04-daily 模板）

---

### Phase 1 · 探针（预计 1 周）

#### T-M001 · master 上 Tauri Android 初始化

- **状态**：`completed` · 完成日期：2026-05-04 · commit `44e0451`
- **价值**：⭐⭐⭐⭐⭐  成本：中
- **实际安装路径**：
  - Android SDK：`D:\software\dev\android-sdk\`
  - JDK：`D:\software\dev\jdk-21\`（用户已有）
  - NDK：`D:\software\dev\android-sdk\ndk\27.0.12077973\`
- **完成情况**：
  - [x] JDK 21 ✅（用户已有）+ `JAVA_HOME` 设到 `D:\software\dev\jdk-21`
  - [x] Android cmdline-tools 12.0 装在 `cmdline-tools/latest/`
  - [x] platform-tools / build-tools 34.0.0 / android-34 / NDK 27.0.12077973 全装
  - [x] `ANDROID_HOME` / `NDK_HOME` / `ANDROID_NDK_ROOT` 用户级永久写入
  - [x] PATH 加 jdk-21\bin / cmdline-tools\latest\bin / platform-tools
  - [x] SDK Licenses 全部接受
  - [x] `rustup target add` 4 个 Android target（aarch64 / armv7 / i686 / x86_64）
  - [x] `pnpm tauri android init` 成功 → 生成 `src-tauri/gen/android/`（41 文件）
  - [x] `.gitignore` 自动配好（Gradle 模板保留，忽略 build / .gradle / local.properties）
  - [x] 桌面端 0 影响：git diff 空 + `cargo check` 通过（仅 1 dead_code warning 历史遗留）
  - [x] 提交：`chore(mobile): tauri android init` (44e0451)
- **遗留**：
  - [ ] `pnpm tauri android dev` 启动验证 — 推迟到 T-M002 完成后（当前桌面专属依赖编译不过 Android target，预期失败）

#### T-M002 · `lib.rs` 桌面专属代码 cfg gate 隔离

- **状态**：`completed` ✅ · 完成日期：2026-05-04 · 5 个 commit (`d6ebac8` → `fcf769d`)
- **价值**：⭐⭐⭐⭐⭐  成本：中
- **目标**：让 `cargo check --target aarch64-linux-android` 能过编译，且**桌面端 `cargo check` 仍能通过**
- **完成情况**：
  - ✅ 桌面端 cargo check 通过（仅 1 历史 dead_code warning）
  - ✅ Android target cargo check 通过（0 errors，43 unused warnings，8.45s 增量编译）
  - ✅ Cargo.toml 桌面专属 crate 全部 target gate（updater / autostart / global-shortcut /
    pdfium-render / rmcp transport-child-process / rust-s3 / calamine / docx-rs）
  - ✅ reqwest 切到 rustls-tls（双端共享，绕开 openssl-sys Android 编译失败）
  - ✅ Capabilities 拆分：default 桌面 + mobile 简化版（顺手完成 T-M003）
  - ✅ tauri.{android,ios}.conf.json platform-specific override（移除 sidecar / pdfium 资源）
  - ✅ 5 个完全桌面专属 service 模块整体 cfg gate：emergency_window / popout_window /
    excel_parser / export_word / mcp_client
  - ✅ 桌面专属 helper 函数全部 cfg gate（10 个 lib.rs 顶层函数）
  - ✅ AppState.mcp_external 字段 cfg gate
  - ✅ services/ai.rs / services/skills.rs / services/task_reminder.rs 双写实现
    （桌面 / 移动 stub）
  - ✅ commands 层桌面专属 commands 全部加 cfg gate 在函数和 generate_handler! 中
  - ✅ commands/mcp.rs locate_*_config 加 Android fallback
  - ✅ services/sync_v1/backend.rs S3 分支双写（桌面真实 / 移动返错）
  - ✅ services/pdf.rs PDFium 全部 cfg gate（桌面端 fallback / 移动端纯 pdf-extract）
- **子任务**：
  - [ ] `mod tray;` 加 `#[cfg(desktop)]`
  - [ ] `tauri = features=["tray-icon"]` → 改为条件 feature 或 cfg gate 注册逻辑
  - [ ] `tauri-plugin-global-shortcut` 注册 + `services::shortcut::register_all` 加 `#[cfg(desktop)]`
  - [ ] `tauri-plugin-autostart` 注册 加 `#[cfg(desktop)]`
  - [ ] `tauri-plugin-updater` 注册 加 `#[cfg(desktop)]`
  - [ ] 多开实例锁逻辑（`acquire_instance_lock` / `try_exclusive_lock` / `parse_instance_arg` / `early_app_data_dir`）加 `#[cfg(desktop)]`
  - [ ] `.md` 投递（`deliver_md_to_default` / `start_md_deliver_watcher`）加 `#[cfg(desktop)]`
  - [ ] MCP sidecar：保留 in-memory `setup_internal_mcp`，砍 `rmcp transport-child-process`（本就 desktop only）
  - [ ] PDFium 动态加载 加 `#[cfg(desktop)]`
  - [ ] `Cargo.toml` 桌面专属依赖加 `[target.'cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))'.dependencies]`
  - [ ] 桌面端回归：`cd src-tauri && cargo build` 通过 + `pnpm tauri dev` 启动验证笔记 CRUD/搜索/AI 不退化
- **风险**：误删桌面专属代码导致桌面端 panic / 启动崩溃

#### T-M003 · Capabilities 移动端版本

- **状态**：`completed` ✅ · 完成日期：2026-05-04 · 与 T-M002 同步完成（commit `91d7aa0`）
- **价值**：⭐⭐⭐⭐  成本：低
- **目标**：`src-tauri/capabilities/mobile.json` 只包含移动端可用权限
- **子任务**：
  - [ ] 新建 `capabilities/mobile.json`：删除 tray / autostart / global-shortcut / updater / opener:allow-reveal-item-in-dir
  - [ ] `default.json` 加 `"platforms": ["windows", "macOS", "linux"]`
  - [ ] `mobile.json` 加 `"platforms": ["android", "iOS"]`
  - [ ] `tauri.conf.json` 增加 `bundle.android.minSdkVersion`（建议 26+）

#### T-M004 · Android 探针验证：笔记 CRUD + 搜索能跑通

- **状态**：`testing` · 应用启动 OK 但桌面 Layout 不可操作（→ T-M006 重构）· commit `3118ca3`
- **价值**：⭐⭐⭐⭐⭐  成本：低
- **目标**：在 Android 模拟器上能创建笔记、列出、搜索（验证 rusqlite + 三层架构在移动端工作）
- **子任务**：
  - [ ] 启动 Android 模拟器，`pnpm tauri android dev` 安装应用
  - [ ] 验证 `commands::notes::list_notes` 返回空数组
  - [ ] 调 `commands::notes::create_note` 创建 1 条
  - [ ] 调 `commands::search::search_notes` 验证 FTS 搜索
  - [ ] 验证数据持久化（杀进程重启后数据还在）

#### T-M005 · iOS 初始化（独立任务，需 macOS）

- **状态**：`pending` · 阻塞中（需 Mac 设备）
- **价值**：⭐⭐⭐⭐  成本：高
- **前置**：macOS + Xcode 16+ + Apple Developer Program 账号（$99/年，TestFlight/App Store 必须）
- **子任务**：
  - [ ] 在 Mac 上 `pnpm tauri ios init` → 生成 `src-tauri/gen/apple/`
  - [ ] commit `gen/apple/` 进 master（CI 后续 build 依赖）
  - [ ] 配置 iOS Bundle ID（建议 `com.agilefr.kb.mobile`）
  - [ ] `tauri.conf.json` 增加 `bundle.iOS.minimumSystemVersion`（建议 16+）

---

### Phase 2 · 移植绿区业务（预计 2-3 周）

#### T-M006 · 前端响应式布局 / 双布局决策 + 移动端布局实现

- **状态**：`completed` ✅ · 完成日期：2026-05-04 · commit `cb8abad`
- **决策**：方案 A 改良版 — 桌面 / 移动端各自独立组件，路由层用 `LayoutSwitch` + 各页 wrapper 模式（`useIsMobile()` 检测）
- **价值**：⭐⭐⭐⭐⭐  成本：高
- **完成情况**：
  - [x] `useIsMobile()` hook：基于 UA + 窗口宽度（< 768px）+ matchMedia 监听
  - [x] `MobileLayout.tsx`：底部 5 Tab（主页/笔记/AI/待办/我的）+ 浮动 FAB + safe-area-inset 处理
  - [x] `LayoutSwitch.tsx` 根分发器：`isMobile ? MobileLayout : AppLayout`
  - [x] 桌面端零影响：所有页面用 wrapper 模式 — 桌面组件原样保留，末尾追加 `useIsMobile` 分发

#### T-M007 · 砍 / 隐藏桌面专属设置项

- **状态**：`pending`
- **价值**：⭐⭐⭐  成本：低
- **依赖**：T-M015 完成（功能开关页要先有 isMobile 判断）
- **子任务**：
  - [ ] 设置页（PC）的"全局快捷键 / 开机启动 / 自定义数据目录 / 多开实例 / 应用更新"分页加 `isMobile` 判断隐藏
  - [ ] `migration-splash` / `emergency-reminder` / `popout-note` 三类窗口在移动端改 Modal / 路由跳转

#### T-M008 · Dashboard 主页 + 全套移动端 Tab/二级页

- **状态**：`completed` ✅ · 完成日期：2026-05-04 · commits 8d2ae07 → 77c24a7（共 12 个 commit）
- **价值**：⭐⭐⭐⭐⭐  成本：中
- **依赖**：T-M006 完成
- **完成情况**：5 Tab 主页 + 9 二级页 + 2 沉浸式全屏页（除 09-cards 外的所有原型）
  - [x] **MobileHome（00-home）**：问候 + 4 数据卡 + 快速操作 + 今日待办速览 + 最近编辑 (`cb8abad`)
  - [x] **MobileNotes（01-notes）**：顶栏 + 文件夹 chips + 置顶/最近分组 + 笔记卡片 (`8d2ae07`)
  - [x] **MobileNoteEditor（02-note-edit）**：fixed 全屏 + 标题/正文 textarea + Markdown 工具栏 + 智能保存状态机 (`5ba96f5`)
  - [x] **MobileSearch（03-search）**：autofocus + 300ms debounce + 关键词 mark 高亮 (`ddb5fe0`)
  - [x] **MobileDaily（04-daily）**：月份切换 + 日历网格 + 当日笔记预览 + 编辑跳转 (`3cda6ef`)
  - [x] **MobileTags（05-tags）**：标签云字号动态 + 9 色调色板 + 重命名/删除 (`6211e04`)
  - [x] **MobileAi（06-ai）**：模型 chips + 4 快捷入口 + 对话历史 + 橙色 FAB (`8d2ae07`)
  - [x] **MobileAiChat（07-ai-chat）** *沉浸式*：流式响应 + 模型 Drawer + 内联新增模型 (`29cd826`)
  - [x] **MobileTasks（08-tasks）**：4 chips + 今日/本周/无截止/已完成分组 (`8d2ae07`)
  - [x] **MobileMe（10-me）**：渐变 banner + 3 数据卡 + 4 板块设置入口 (`02aa6ef`)
  - [x] **MobileTaskDetail（12-task-edit）** *沉浸式*：标题 + 子任务 + 元属性 + 推迟/完成 (`77c24a7`，T-M010)
  - [x] **MobileTrash（13-trash）**：信息横幅 + 即将清理（红框）+ 最近删除 + 清空 (`5b55f48`)
  - [x] **MobileQuickCreate（14-quick-create）**：2 渐变卡 + 6 来源 + 2 其他 (`4ca4728`，T-M009 一期)
  - [x] **MobileQuickCapture（15-quick-capture）** *沉浸式*：橙色渐变 + 自动草稿 + 落到今日 (`2f0196d`，T-M009 二期)
  - [x] **MobileFeatureToggle（16-feature-toggle）**：核心模块锁 + 8 可选模块 Switch (`b708c42`，T-M015 一期)
- **顺手交付**：
  - [x] `MobileAiModelModal` + `aiProviderPresets` 共享 lib（PROVIDERS / DEFAULT_URLS / MODEL_PRESETS）
  - [x] FAB 路由黑名单 `PAGES_WITH_OWN_FAB = ["/ai"]`
  - [x] 保存竞态修复：MobileNoteEditor flushAndExit() + bumpNotesRefresh，MobileNotes 监听 notesRefreshTick
- **未做（下迭代）**：
  - [ ] 30 天写作热力图（需 `get_writing_trend` 接入 + 自绘 grid）
  - [ ] 09-cards 闪卡复习页（默认关，优先级低）
  - [ ] MobileTaskDetail 截止时间 / 提醒分钟 / 重复规则 完整编辑器（移动端日期选择器）

#### T-M009 · 新建抽屉 + 闪念捕获页

- **状态**：`completed` ✅ · 一期 commit `4ca4728` / 二期 commit `2f0196d`
- **完成情况**：见 T-M008
- **未做**：剪贴板自动检测 URL（要 `tauri-plugin-clipboard-manager`，移动端原生集成下迭代）

#### T-M010 · 任务详情页（12-task-edit 对应代码）

- **状态**：`completed` ✅ · commit `77c24a7`
- **完成情况**：见 T-M008
- **未做**：日期选择器 / 重复规则 / 子任务拖拽排序
  - [ ] 子任务拖拽排序（react-dnd 移动端兼容）

#### T-M011 · 图谱页移动端简化版

- **状态**：`pending` · 优先级 P2
- **价值**：⭐⭐⭐  成本：中
- **风险**：当前桌面图谱触屏交互（pan/zoom/pinch）需重写
- **子任务**：
  - [ ] 评估：移动端图谱是否做？（用户决定）
  - [ ] 若做：换轻量图谱库（如 react-flow），桌面继续用现有方案

---

### Phase 3 · 黄区适配（预计 1-2 周）

#### T-M012 · PDFium 替换 / 移动端砍 PDF 解析

- **状态**：`pending`
- **价值**：⭐⭐⭐  成本：中
- **子任务**：
  - [ ] 评估：移动端是否保留 PDF 导入？
  - [ ] 若保留：`pdf-extract` + 字体兜底，砍 `pdfium-render` cfg gate
  - [ ] 测试中文 CMap PDF 提取效果

#### T-M013 · 文件导入改单文件模式

- **状态**：`completed` ✅ 一期 · commit `e043210`
- **价值**：⭐⭐⭐⭐  成本：中
- **完成情况**：
  - [x] MobileQuickCreate「导入文件」实装（HTML5 input + file.text() 读 .md/.markdown/.txt）
  - [x] 文件名去后缀作标题 → noteApi.create → 跳编辑器
- **未做**：
  - [ ] PDF 解析（需后端 pdf-extract，移动端复杂）
  - [ ] Word .docx / Excel .xlsx（calamine 是桌面专属）
  - [ ] OB 整库 ZIP 上传

#### T-M014 · 同步功能移动端 UX

- **状态**：`completed` ✅ 一期 · commit `8add81d`
- **价值**：⭐⭐⭐⭐⭐  成本：中（核心：PC ↔ 手机数据互通）
- **完成情况**：
  - [x] 后台调度器（`sync_scheduler` / `sync_v1_scheduler`）已在 T-M002 cfg gate 出去
  - [x] /sync 路由 + MobileSync 页 — 列出 backend / 推送 / 拉取 / 测试 / 增删改
  - [x] 仅 WebDAV（S3 在移动端 cfg gate 不能用）
  - [x] 推/拉返回结果 toast 给用户：上传 N · 删除 N · 跳过 N · 错误 N
  - [x] 拉取后 bumpNotesRefresh 让笔记列表立即同步
  - [x] MobileMe 「云端同步」入口 wire 到 /sync
- **未做（下一期）**：
  - [ ] 监听 `sync:v1_progress` 事件显示进度条
  - [ ] "一键同步"按钮（push + pull 串行）
  - [ ] 网络变化提示（断网时 disable 按钮）
  - [ ] 冲突解决专门 UI

#### T-M015 · 功能模块开关页（16-feature-toggle 对应代码）

- **状态**：`completed` ✅ · commits `b708c42` + `48c50d8` + `cdaa251`
- **价值**：⭐⭐⭐⭐⭐  成本：中（PC 与移动端共用，需双向兼容）
- **依赖**：T-M006
- **完成情况**：
  - [x] 复用 PC 端 useAppStore.enabledViews + toggleEnabledView，写 app_config.enabled_views
  - [x] **移动端独有**：底部 Tab 配置（4 格 + 我的固定，12 候选 tabs，自动换位）
  - [x] **移动端独有**：主页 Dashboard 8 项显示开关（today_words / due_cards / today_tasks_card / total_notes / quick_actions / today_tasks_list / heatmap / recent_notes）
  - [x] 持久化到 `app_config`：enabled_views / mobile_dashboard_items / mobile_tab_keys
  - [x] **桌面端零影响**：mobile_* 配置桌面不读，桌面继续用 enabled_views 控制 ActivityBar

---

### Phase 4 · 平台特化 + 发布（预计 1-2 周）

#### T-M016 · iOS Share Extension（接受外部 .md / 链接剪藏）

- **状态**：`pending` · 阻塞中（依赖 T-M005 iOS init）
- **价值**：⭐⭐⭐⭐  成本：高
- **目标**：从 Safari / 其他 App 分享文本/链接/图片到知识库
- **子任务**：
  - [ ] Xcode 工程添加 Share Extension target
  - [ ] Swift 桥接：把分享内容写入应用沙盒 + 主程序检测启动

#### T-M017 · Android Intent Filter（接受 .md 文件 / 文本分享）

- **状态**：`pending`
- **价值**：⭐⭐⭐⭐  成本：中
- **子任务**：
  - [ ] `AndroidManifest.xml` 加 `<intent-filter>` for `text/markdown` `text/plain`
  - [ ] Rust 侧通过 deeplink 接收数据（替代 PC 端的 `extract_md_paths_from_args`）

#### T-M018 · iOS 签名 + TestFlight 发布

- **状态**：`pending` · 阻塞中（依赖 Apple Developer Program）
- **价值**：⭐⭐⭐⭐⭐  成本：中
- **子任务**：
  - [ ] Apple Developer Program 注册（$99/年）
  - [ ] 创建 App ID + Provisioning Profile
  - [ ] 导入证书 `.p12` 到 GitHub Secrets
  - [ ] CI 跑 `tauri ios build` + 上传 TestFlight

#### T-M019 · Android 签名 + APK / AAB 打包

- **状态**：`in_progress`（本地签名链路已通；CI 接入 + AAB 上架待做）
- **价值**：⭐⭐⭐⭐⭐  成本：中
- **子任务**：
  - [x] 生成 Android keystore（`keytool -genkeypair`）→ `src-tauri/gen/android/kb-release.jks`（alias `kb`，RSA 2048，有效期到 2053-09-26）。**已 .gitignore，密码在本地 `key.properties` + 维护者密码管理器**
  - [x] `key.properties`（`src-tauri/gen/android/key.properties`，.gitignore）+ `app/build.gradle.kts` 接入 `signingConfigs.release`（key.properties 存在则 release APK/AAB 自动用此 keystore 签名，确保更新签名一致；不存在则 release 不签名、debug 不受影响）
  - [ ] keystore + 密码存 GitHub Secrets（base64 编码 .jks 内容 + storePassword/keyPassword/keyAlias），CI 解码后还原 key.properties
  - [ ] CI 跑 `tauri android build --aab` 出 Google Play 上架包
  - [ ] 自签 release APK 同时输出 + 上传 release 仓库（配合移动端"检查更新"的 APK 直链）
  - **⚠️ 关键约束**：第一个分发给用户的 release APK 用了 `kb-release.jks` → 此后所有版本都必须用同一个 keystore，否则用户的"检查更新→下载新 APK"会因签名不匹配装不上（`INSTALL_FAILED_UPDATE_INCOMPATIBLE`）。keystore 丢了 = 再也无法给已安装用户推更新。务必备份 `.jks` + 两个密码。

#### T-M020 · CI 扩展：`release.yml` 加 Android / iOS job

- **状态**：`pending`
- **价值**：⭐⭐⭐⭐  成本：中
- **子任务**：
  - [ ] `.github/workflows/release.yml` matrix 增加 `platform: ubuntu-22.04` + Android target 节点
  - [ ] 新增 `macos-latest` + iOS target 节点
  - [ ] 桌面端 4 个 job 不动，确保不破坏现有发布流程
  - [ ] tag `v*-mobile.*.*` 触发移动端 CI（与桌面 tag 区分）

#### T-M021 · 移动端"检查更新"（自更新引导）

- **状态**：`in_progress`（前后端已实现；待 update.json 加 android 字段 + CI 上传 release APK）
- **价值**：⭐⭐⭐⭐  成本：低
- **背景**：`tauri-plugin-updater` 不支持移动端（且本项目已 `#[cfg(desktop)]` 隔离），移动端没法原地热替换，只能"检查→引导用户去下载新 APK"
- **子任务**：
  - [x] Rust：`commands/mobile_update.rs`（`#[cfg(mobile)]`）`check_mobile_update` —— 拉 `update.json`（与桌面 updater 同 3 个 endpoint）比对版本，返回 `{has_update, current/latest_version, notes, download_url}`；`download_url` 优先 `platforms.android-arm64.url`，没有则回落 release 发布页
  - [x] 前端：`MobileMe` 加「检查更新」入口 → 有新版弹 Modal（版本 + 更新说明 + 去下载）→ `openUrl(download_url)` 浏览器接管下载，下完点一下进系统安装器（首次系统问"允许安装未知应用"，那是浏览器的权限，本 App 不需要 `REQUEST_INSTALL_PACKAGES`）
  - [ ] release 仓库的 `update.json` 加 `platforms.android-arm64.url`（指向 release APK 直链）—— 依赖 T-M019 CI + T-M020
  - [ ] CI（T-M020）出 release APK 后上传到 release 仓库，文件名约定 `knowledge-base_<version>_android-arm64.apk`

---

## 阶段进度看板

| Phase | 任务数 | 完成数 | 状态 |
|-------|-------|--------|------|
| Phase 0 原型设计 | 1 | 1 | ✅ `completed` |
| Phase 1 探针 | 5 | 4 | `in_progress` (T-M001~T-M004 ✅，T-M005 iOS 阻塞) |
| Phase 2 移植绿区 | 6 | 4 | `in_progress` (T-M006/008/009/010 ✅，T-M007/011 待) |
| Phase 3 黄区适配 | 4 | 3 | `in_progress` (T-M013/014 一期 + T-M015 全部 ✅，T-M012 PDFium 待) |
| Phase 4 平台特化 | 6 | 0 | `in_progress` (T-M019 本地签名链路 ✅ / T-M020 android.yml debug APK ✅ / T-M021 前后端 ✅；CI release 接入、iOS、Intent 待) |
| **合计** | **22** | **12** | — |

---

## 任务依赖图

```
Phase 0 (T-M000) ✅
   └── Phase 1
        ├── T-M001 Android init ─┐
        ├── T-M002 cfg gate ─────┼─→ T-M004 探针验证
        ├── T-M003 Capabilities ─┘
        └── T-M005 iOS init (独立，需 Mac)

Phase 2 (依赖 T-M002 + T-M004)
   ├── T-M006 布局决策 ──→ T-M007 砍设置项
   │                  ├── T-M008 Dashboard
   │                  ├── T-M009 新建抽屉 + 闪念
   │                  └── T-M010 任务详情
   └── T-M011 图谱（P2 可选）

Phase 3 (依赖 Phase 2)
   ├── T-M012 PDFium
   ├── T-M013 文件导入
   ├── T-M014 同步 UX
   └── T-M015 功能模块开关 (依赖 T-M006)

Phase 4 (依赖 Phase 3)
   ├── T-M016 iOS Share Ext (依赖 T-M005)
   ├── T-M017 Android Intent
   ├── T-M018 iOS 签名 + TestFlight (依赖 T-M005 + Apple Dev)
   ├── T-M019 Android 签名
   └── T-M020 CI 扩展
```

---

## 历史记录

- **2026-05-04** 创建任务跟踪文件，定义 4 Phase + 14 任务
- **2026-05-04** 完成 T-M000 原型设计（17 页 HTML，已对齐 PC 端功能模块清单）
- **2026-05-04** 决策：放弃 mobile 分支策略，改为 master 直接开发 + cfg gate 隔离 + commit 前桌面端回归验证
- **2026-05-04** 扩展为 21 个任务（增加 T-M005 iOS init 独立任务、T-M015 功能模块开关、T-M016/T-M020 平台特化任务）
- **2026-05-04** ✅ T-M001 完成（commit `44e0451`）— Android SDK 装在 `D:\software\dev\android-sdk\`，工具链全就绪，桌面端零影响
- **2026-05-04** ✅ T-M002 完成（5 个 commit `d6ebac8` → `fcf769d`）— 桌面专属代码 cfg gate
  完成；Android target `cargo check` 通过（0 errors）；同时顺手完成 T-M003（capabilities
  移动端版本）；reqwest 切到 rustls-tls；rust-s3 / pdfium-render / calamine / docx-rs / 
  tauri-plugin-{updater,autostart,global-shortcut} 全部移到 desktop target dependencies
- **2026-05-04** ✅ T-M006 / T-M008 / T-M009 / T-M010 / T-M015 一期完成
  （commits `cb8abad` → `77c24a7`，共 12 个 commit）
  覆盖 17 页设计中的 16 页（除 09-cards 外）；
  含智能保存状态机、流式 AI 响应、模型 Drawer、自动草稿、子任务等关键交互；
  桌面端零退化（wrapper 模式不影响桌面组件）
- **2026-05-04** ✅ 收尾批次（commits `8a173a4` → `8add81d`，6 个 commit）
  - `8a173a4` 30 天写作热力图（MobileHome 余项）
  - `48c50d8` T-M015 二期 Dashboard 显示项可定制
  - `9eed66f` MobileCards 闪卡复习页（17 页设计稿全部完成）
  - `cdaa251` T-M015 三期 底部 Tab 4 格可定制（mobileTabRegistry + 12 候选）
  - `e043210` T-M013 一期 单文件导入（HTML5 input file.text）
  - `8add81d` T-M014 一期 WebDAV 同步推/拉/测试 UX
  
  **进度从 1/21 → 12/21**。剩余主要工作：T-M005 iOS（阻塞需 Mac）、T-M007 桌面项隐藏、
  T-M011 图谱、T-M012 PDFium、T-M016~T-M020 平台特化（Android 签名 / iOS / Intent / CI）
