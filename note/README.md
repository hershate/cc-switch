# cc-switch 项目要点总览（note 索引）

> 本文件是 `note/` 目录总入口：汇总两次工作流的全部要点（全库代码分析 + 本机构建环境配置），并索引详细文档。最后更新：2026-11-20，对应版本 v4.0.35（commit `9d66f783`）。

## 1. 文档索引

```mermaid
flowchart TD
    N["note 总入口 README.md"] --> R["report/codebase-analysis<br/>全库分析 7 份报告"]
    N --> D["dev/environment-setup.md<br/>构建环境配置与验证"]
    R --> R1["index.md 报告索引"]
    R --> R2["01 架构"]
    R --> R3["02 数据结构"]
    R --> R4["03 运行原理"]
    R --> R5["04 代理引擎"]
    R --> R6["05 业务工作流"]
    R --> R7["06 工程体系"]
```

| 文档 | 内容 | 产出背景 |
|---|---|---|
| [codebase-analysis/index.md](report/codebase-analysis/index.md) | 分析报告索引 + 核心发现 + 关键数据速查 | 2026-11-19 全库分析 |
| [01-architecture.md](report/codebase-analysis/01-architecture.md) | 分层架构、目录全景、模块依赖图、16 应用能力矩阵、预设体系 | 同上 |
| [02-data-structures.md](report/codebase-analysis/02-data-structures.md) | 类/变量级数据结构全表、17 表 ER 图、DAO 映射、前后端类型镜像 | 同上 |
| [03-operation-principles.md](report/codebase-analysis/03-operation-principles.md) | 启动/退出流程、事件系统、日志脱敏、崩溃恢复（写前意图/墓碑） | 同上 |
| [04-proxy-engine.md](report/codebase-analysis/04-proxy-engine.md) | 本地代理 103,943 行：请求生命周期、重试/熔断/故障转移、协议转换矩阵 | 同上 |
| [05-workflows.md](report/codebase-analysis/05-workflows.md) | 供应商切换、代理接管、同步、13 CLI 用量采集、深链接等 13 个业务流程 | 同上 |
| [06-engineering.md](report/codebase-analysis/06-engineering.md) | 测试体系、8 个 CI workflow、构建配置、发布与 R2 分发 | 同上 |
| [dev/environment-setup.md](dev/environment-setup.md) | 本机工具链（F 盘）、镜像、版本锁定变更、fork CI 降级、验证结果 | 2026-11-19 环境配置 |

## 2. 项目速览

| 项 | 值 |
|---|---|
| 定位 | 面向 16 个 CLI 编码工具的 API 供应商切换器 + 本地反向代理 + MCP/Skills/Prompts 统一管理器 |
| 技术栈 | Tauri 2（Rust）+ React 18 + TypeScript + TanStack Query + axum + SQLite |
| 规模 | Rust 436 文件 / 354,569 行；TypeScript 520 文件 / 166,304 行 |
| 关键数量 | 304 个 Tauri 命令；16 个 AppType；17 张数据库表（SCHEMA_VERSION 27）；默认代理 127.0.0.1:15721 |
| 仓库 | fork：`hershate/cc-switch` ← upstream `farion1231/cc-switch` |

## 3. 架构核心要点（五条）

1. **SSOT + 投影**：SQLite 为唯一事实源，各 CLI 原生配置文件仅是「关键字段」投影，非托管字节原样保留（`live/` 写入引擎）。
2. **崩溃安全贯穿**：live-state.json 写前意图五步提交、Claude 空值墓碑（10s TTL）、数据库迁移前自动快照、锁中毒恢复。
3. **本地代理是复杂度核心**：占 29% 代码；手动 hyper accept 保留 wire 级 header 大小写；四协议互转（Anthropic/ChatCompletions/Responses/Gemini Native）；熔断 HalfOpen 轮次化名额。
4. **双来源用量合流**：代理实时记账（`data_source='proxy'`）与 13 个 CLI 会话文件解析（`'session_log'`）经语义去重写入同一 `proxy_request_logs` 表。
5. **前后端强契约**：304 命令 + 16 类事件 + 5 份金标 JSON 双端比对测试，保证 35 万行 Rust 与 17 万行 TS 结构对齐。

## 4. 构建环境要点

| 项 | 值 / 结论 |
|---|---|
| Node.js 24.22.1 / pnpm 10.12.3 | `F:\Program Files\nodejs`；store 在 `F:\DevCache\pnpm-store`（全部 F 盘，符合全局规范） |
| Rust 1.99.1（项目锁定） | `RUSTUP_HOME/CARGO_HOME = F:\soft`；rustup 补装走 USTC 镜像（`RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static`；TUNA/rsproxy 缺 1.9x 频道不可用） |
| MSVC | VS 18 Community @ `F:\Program Files\Microsoft Visual Studio\18` |
| crates.io | 直连可用，未配镜像 |
| 版本唯一改点 | Rust 只改 `rust-toolchain.toml#channel`；Node 改 `.node-version` + workflow 的 node-version |

## 5. 验证过的命令（2026-11-19 实测）

```bash
pnpm install --frozen-lockfile        # 34.4s
pnpm typecheck                        # 0 错误
pnpm build:renderer                   # 21.6s
cargo build                           # debug 全量 2m15s（Rust 1.99.1）
# 完整打包（本地无 updater 签名密钥时必须关闭 updater 产物）：
pnpm tauri build --bundles msi --config '{"bundle":{"createUpdaterArtifacts":false},"plugins":{"updater":{"active":false}}}'
#   → release 5m44s；产出 src-tauri/target/release/bundle/msi/CC Switch_4.0.35_x64_en-US.msi（19.06 MiB）
#   → 便携版 src-tauri/target/release/cc-switch.exe
# 测试：
pnpm test:unit                        # 前端 vitest
cargo nextest run --profile ci        # 后端（或 cargo test）
```

## 6. CI / 发布要点（fork 适配）

- fork 缺发布 secrets 时自动降级（官方仓库路径不变）：无 `TAURI_SIGNING_PRIVATE_KEY` → 未签名安装包（`--config` 关 updater 产物、`.sig` 缺失仅告警）；无 `APPLE_CERTIFICATE` → macOS 链整体跳过；无任何 `.sig` → 跳过 latest.json，安装包照常发 Release。
- `sync-r2.yml` 天然兼容 fork（缺 R2 secrets 优雅跳过，仅官方仓库强制）。
- fork 推 tag 发布前必须存在 `src/whats-new/<版本>.json`（whats-new-check 强制）。
- 外部 PR 改 `SCHEMA_VERSION` 会被 schema-version.yml 自动关闭；改表先 `SCHEMA_VERSION += 1` 再在 `schema.rs` match 加迁移。
- 已知非阻断警告：Rust 1.99.1 下 3 处 `fetch_update`→`try_update` 弃用警告（`services/auto_sync.rs:73` 等）。

## 7. 提交记录

| commit | 内容 |
|---|---|
| `529f56e2` | 全库分析报告（note/report/codebase-analysis 7 份，2,587 行 28 图） |
| `68225faa` | 工具链对齐本地（rust-toolchain 1.99.1、.node-version 24.22.1、6 处 workflow node 24） |
| `283c7f64` | release.yml fork 降级改造 |
| `9d66f783` | 环境配置记录（note/dev/environment-setup.md） |
