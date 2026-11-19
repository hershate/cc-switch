# 开发环境配置记录（Windows / F 盘）

> 配置日期：2026-11-19。按全局规范（通用开发环境装 F 盘、项目专有环境进项目文件夹、优先复用已有工具）完成；本文记录最终状态与可复现命令。

## 1. 工具链清单（全部位于 F 盘）

| 工具 | 版本 | 位置 | 备注 |
|---|---|---|---|
| Node.js | 24.22.1 | `F:\Program Files\nodejs` | 原有安装，直接复用；`.node-version` 已同步为 24.22.1 |
| pnpm | 10.12.3 | npm 全局（`C:\Users\Z\AppData\Roaming\npm`，仅 shim） | 与 `package.json#packageManager` 一致；store 已在 `F:\DevCache\pnpm-store\v10` |
| Rust | **1.99.1**（项目锁定，`rust-toolchain.toml`） | `RUSTUP_HOME=F:\soft\rustup`、`CARGO_HOME=F:\soft\cargo` | 本地已有工具链；为 1.99.1 补装了缺失的 clippy 组件 |
| MSVC（VC++ 工具集 / link.exe） | VS 18 Community | `F:\Program Files\Microsoft Visual Studio\18\Community` | 原有安装，直接复用 |
| WebView2 Runtime | 157.0.4341.60 | 系统组件 | Tauri 运行时依赖，已存在 |
| Git | 2.56.0 | — | 原有 |

Rust 工具链本地库存：`1.95`、`1.99.1`、`stable` 三套（`F:\soft\rustup\toolchains`）。

## 2. 网络与镜像

- rustup 官方源 `static.rust-lang.org` 在本机极慢（<30KB/s）。已验证可用镜像：
  - **USTC（可用，推荐）**：`RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static`
  - TUNA / rsproxy 缺 1.95/1.99 频道清单（404），勿用
  - 示例（一次性，不持久化）：`RUSTUP_DIST_SERVER=https://mirrors.ustc.edu.cn/rust-static rustup component add clippy --toolchain 1.99.1-x86_64-pc-windows-msvc`
- crates.io sparse 索引与静态下载直连可用，无需镜像（`cargo fetch` 全量 600+ crate 正常）。

## 3. 版本锁定变更（commit `68225faa` / `283c7f64`）

| 文件 | 变更 |
|---|---|
| `rust-toolchain.toml` | channel `1.95` → `1.99.1`（本地已有；CI 经 `sed` 读此文件自动跟随） |
| `.node-version` | `22.12.0` → `24.22.1` |
| `.github/workflows/ci.yml` | node-version `20` → `24`（2 处） |
| `.github/workflows/release.yml` | node-version `20` → `24`（4 处）+ fork 降级改造（见 §5） |

## 4. 编译与打包验证结果

| 验证项 | 命令 | 结果 |
|---|---|---|
| 前端依赖 | `pnpm install --frozen-lockfile` | ✅ 34.4s |
| 类型检查 | `pnpm typecheck` | ✅ 0 错误 |
| 前端构建 | `pnpm build:renderer` | ✅ 21.6s |
| Rust debug 全量 | `cargo build`（1.99.1） | ✅ 2m15s |
| Rust 依赖拉取 | `cargo fetch` | ✅ |
| 完整打包 | `pnpm tauri build --bundles msi --config <unsigned>` | ✅ release 构建 5m44s；产出 `CC Switch_4.0.35_x64_en-US.msi`（19.06 MiB）与便携版 `cc-switch.exe`（52 MB） |

未签名打包命令（本地无 updater minisign 密钥时必须关闭 updater 产物，与 fork CI 改造同构）：

```bash
pnpm tauri build --bundles msi --config '{"bundle":{"createUpdaterArtifacts":false},"plugins":{"updater":{"active":false}}}'
```

产物位置：`src-tauri/target/release/bundle/msi/*.msi`；便携版 exe：`src-tauri/target/release/cc-switch.exe`。

注意：Tauri 打包器首次打 MSI 会把 WiX 工具集下载到 `%LOCALAPPDATA%\tauri`（约几 MB，无官方重定向途径）。

已知非阻断警告：Rust 1.99.1 下 `std::sync::atomic::Atomic::fetch_update` 更名为 `try_update`，release 构建产生 3 处 deprecation warning（`services/auto_sync.rs:73` 等），不影响编译与产物。

## 5. CI fork 适配要点（commit `283c7f64`）

fork（`hershate/cc-switch` ← upstream `farion1231/cc-switch`）缺少发布 secrets，release.yml 已做三类降级，**官方仓库（secrets 齐全）路径行为不变**：

```mermaid
flowchart TD
    A["whats-new-check 根 job"] --> P["Probe release secrets<br/>tauri_key / apple_certs 两输出"]
    P --> R["release matrix win linux"]
    P --> MB["macos-binary"]
    MB --> MR["macos-release"]
    R --> PUB["publish-release"]
    MR --> PUB
    P --|apple_certs 未配置| SKIP["macOS 链整体跳过<br/>publish-release 放行 skipped"]
    R --|tauri_key 未配置| UNSIGN["跳过签名准备<br/>build 追加 --config 关 updater 产物<br/>资产整理 .sig 缺失仅告警"]
    PUB --|无任何 .sig| NOJSON["跳过 latest.json<br/>安装包照常上传 Release"]
    P --|secrets 齐全 官方仓库| ORIG["原签名/公证/校验流程不变"]
```

其余 workflow 现状：`sync-r2.yml` 本身已对 fork 优雅跳过（仅官方仓库强制）；`ci.yml`/`wsl2-nightly.yml` 不依赖 secrets；fork 上推 tag 发布前需存在 `src/whats-new/<版本>.json`（whats-new-check 在 tag push 时强制）。

## 6. 后续维护提示

- 升级 Rust 版本：改 `rust-toolchain.toml#channel` 一处即可（本地 + CI 同源）；建议先 `rustup toolchain install <ver>` 确认本地已有。
- pnpm store 位置由 pnpm 全局配置管理（`F:\DevCache\pnpm-store`），勿删除；`node_modules` 与 `src-tauri/target` 均在项目内，符合项目专有环境规范。
- 构建临时文件位于系统 TEMP 由工具自动管理；项目自身未使用 TEMP 目录做测试。
