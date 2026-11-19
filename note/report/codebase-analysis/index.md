# cc-switch 代码分析报告索引

| 项 | 值 |
|---|---|
| 分析时间 | 2026-11-19 |
| 分析范围 | `F:\Github\repo\cc-switch` 全仓库（v4.0.35，commit `6a6ad60e`） |
| 分析模式 | 大规模项目采样精读（入口/数据层/服务层/代理层/模式层全读；超大文件按调用点归纳） |
| 代码规模 | Rust 436 文件 / 354,569 行；TypeScript 520 文件 / 166,304 行 |
| 技术栈 | Tauri 2（Rust 1.95）+ React 18 + TypeScript + TanStack Query + axum + SQLite |
| 本次范围说明 | 按要求仅输出架构/数据结构/运行原理/代理引擎/工作流/工程体系六份报告；不含 AI 替代方案评估与 Skill Blueprint |

## 报告目录

| # | 报告 | 内容概要 |
|---|---|---|
| 1 | [项目架构](01-architecture.md) | 项目快照、技术栈、分层规则、目录全景、后端模块依赖图、前端组件树与状态分层、16 应用能力矩阵、预设体系、设计模式清单 |
| 2 | [数据结构](02-data-structures.md) | 类与变量级全量数据结构：AppState/Database/Provider（29 字段 meta）/AppSettings（60+ 字段全表）/mode 状态结构/代理配置族/三大 OAuth 管理器；SQLite ER 图（17 表）与 DAO 映射；前端 TS 类型镜像表 |
| 3 | [运行原理](03-operation-principles.md) | 启动完整时序（setup 12 步明细）、退出三分类状态图、16 类事件桥、日志与脱敏体系、写前意图崩溃恢复、墓碑机制、定时任务表 |
| 4 | [代理引擎](04-proxy-engine.md) | 服务器架构与路由表、请求完整生命周期时序、转发重试状态机、forward 改写管线 26 步流程图、熔断器状态机、故障转移、协议转换矩阵、用量记账数据流 |
| 5 | [业务工作流](05-workflows.md) | 供应商添加/切换时序、代理接管 enter/exit/启动恢复、配置同步（WebDAV/S3）、13 CLI 会话用量采集、会话管理、MCP/Skills、深链接、托盘、自动更新、异常恢复路径汇总表 |
| 6 | [工程体系](06-engineering.md) | 测试体系（5000+ 测试、前后端契约金标）、CI/CD 全景图（8 workflow）、构建配置（Tauri/Vite/Cargo profile）、发布与 R2 分发、质量门禁 |

## 核心发现

1. **SSOT + 投影架构**：SQLite 是唯一事实源（SCHEMA_VERSION 27），16 个 CLI 的原生配置文件只是「关键字段投影」；`live/` 写入引擎保证非托管字节原样保留（`live/mod.rs:1-13`）。
2. **崩溃安全贯穿全链路**：live-state.json 写前意图（5 步提交、丢弃/前滚恢复）、Claude 空值墓碑（10s TTL 三时机删除）、数据库迁移前自动快照、锁中毒恢复（`operation.rs`、`claude_direct.rs:141-421`、`database/mod.rs:175-187`）。
3. **内置本地代理是复杂度核心**：103,943 行（29% 代码）实现手动 hyper accept（保 header 大小写 wire 级伪装）、四协议互转（Anthropic/ChatCompletions/Responses/Gemini Native）、熔断（HalfOpen 轮次化名额）、故障转移、密文整流、用量记账。
4. **双来源用量合流**：代理实时记账（data_source='proxy'）与 13 个 CLI 会话文件离线解析（'session_log'）经 `session_usage_dedup` 语义去重后写入同一 `proxy_request_logs` 表。
5. **前后端强契约**：304 个 Tauri 命令 + 16 类事件 + 5 份金标 JSON 双端比对测试，使 35 万行 Rust 与 17 万行 TypeScript 保持结构对齐。

## 关键数据速查

| 项 | 值 | 出处 |
|---|---|---|
| Tauri 命令数 | 304 | grep `#[tauri::command]` |
| 支持应用数 | 16（AppType） | `app_config.rs:311-329` |
| 数据库表数 | 17 | `database/schema.rs` |
| SCHEMA_VERSION | 27 | `database/mod.rs:54` |
| 默认代理端口 | 127.0.0.1:15721 | `proxy/types.rs:16-20` |
| 熔断默认参数 | 4 次失败 / 2 次成功 / 60s / 0.6 错误率 / 10 样本 | `circuit_breaker.rs:63-73` |
| 供应商预设总数 | 1,000+（claude 141 / codex 139 / 其余 8 目录 + 5 派生） | `src/config/*ProviderPresets.ts` |
| 同步协议 | PROTOCOL_VERSION 2，db.sql + skills.zip + manifest，无加密 | `sync_protocol.rs:29-39` |
| 国际化 | 8 语言单命名空间 | `src/i18n/` |
| Rust 测试 | 4,449 #[test] + 610 #[tokio::test] | grep 统计 |
| 前端测试 | 322 文件（tests/）+ 20 内联 | find 统计 |
