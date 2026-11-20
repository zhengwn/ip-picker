# IP Picker 开发与维护

IP Picker 的桌面界面使用 Vue 3 和 TypeScript，通过 Tauri 2 调用 Rust 测速核心。本文涵盖本地开发、跨平台验证和安装包发布。 [安装与使用](../README.md)

## 开发环境

共同依赖包括 Node.js 22.12 或更新的 22.x、npm 和 Rust stable（锁定的依赖要求至少 1.90）。首次安装依赖和构建需要访问 npm 与 crates.io。 [Rust 安装](https://rustup.rs/)

macOS 仅开发桌面应用时，可以安装 Xcode Command Line Tools：

```sh
xcode-select --install
```

Windows 需要 Visual Studio 的 **Desktop development with C++** 工作负载、Microsoft Edge WebView2 Runtime，以及 Rust **MSVC** 工具链。安装后重新打开终端。完整安装步骤参见 [Tauri 官方环境要求](https://v2.tauri.app/start/prerequisites/)。

在项目目录运行：

```sh
npm ci
npm run tauri -- dev
```

仅调试界面时，可启动浏览器预览。实际测速和桌面数据读写需要 Tauri 窗口。

```sh
npm run dev
```

## 检查与构建

```sh
npm run build
npm run test:frontend
cargo fmt --manifest-path src-tauri/Cargo.toml --all -- --check
cargo test --manifest-path src-tauri/Cargo.toml --locked
```

`npm test` 是 Rust 测试命令的快捷入口。前端回归测试通过 `npm run test:frontend` 单独运行。

在有图形桌面的环境中运行 `npm run test:desktop-smoke`，会启动真实 Tauri 窗口，验证开始测速、停止、收藏、关闭后重开，以及字段错误和收藏快照持久化。测试使用独立临时数据目录和固定测量样本。

`npm run test:desktop-performance` 在真实 Tauri 窗口里加载 1,000 和 10,000 个模拟候选，输出完整结果和下载进度事件的 JSON 大小、IP 筛选响应时间中位数，以及从点击停止到窗口显示「已停止」的耗时。筛选耗时截至匹配行更新，停止耗时包含测试结果的本地保存。

编译当前操作系统的桌面程序，不制作安装包：

```sh
npm run tauri -- build --no-bundle
```

按 Tauri 配置制作本机安装包：

```sh
npm run tauri -- build
```

产物位于 `src-tauri/target/release/`，安装包位于其 `bundle/` 子目录。不同操作系统的安装包应在各自系统上构建。更多选项见 [Tauri 构建文档](https://v2.tauri.app/distribute/)。

常规 CI 在 macOS 和 Windows 上运行前端构建与回归、Rust 检查和桌面冒烟。随后在 macOS 编译调试版桌面程序，在 Windows 构建调试版 NSIS 安装包。真实网络结果仍需在实际运行环境验证。 [Desktop checks 工作流](../.github/workflows/ci.yml)

## Windows x64 安装包发布

1. 在发布提交中同步更新 `package.json`、`package-lock.json`、`src-tauri/Cargo.toml`、`src-tauri/Cargo.lock` 中的应用版本，以及 `src-tauri/tauri.conf.json` 的版本。将对应的 `v<版本号>` tag 推送到 GitHub。
2. 在仓库的 Actions 中打开 **Build Windows installer**，选择包含该工作流的分支（通常是 `main`），点击 **Run workflow**，在 `tag` 中填入发布 tag，例如 `v0.1.2`。工作流会检出该 tag 并检查它是否与该提交的 Tauri 应用版本一致。
3. 运行成功后，下载名为 `ip-picker-<tag>-windows-x64` 的 Actions artifact。内含 `IP-Picker-<版本号>-Windows-x64-setup.exe` 和对应的 `.sha256` 文件。准备发布时，将这两个文件上传到同一 tag 的 GitHub Release 的 Assets。

工作流会在 Windows runner 上安装应用，核对可执行文件的版本和 x64 架构，并验证窗口打开、关闭和卸载。Windows 实机上的窗口交互和实际网络测速需另行验证。仓库的打包配置尚未设置 macOS Developer ID 签名和公证，也未配置 Windows 代码签名。 [Build Windows installer 工作流](../.github/workflows/windows-installer.yml)

## 代码结构

| 入口 | 职责 |
| --- | --- |
| [src/App.vue](../src/App.vue)、[src/components/](../src/components/) | 参数设置、任务事件与结果／收藏界面 |
| [src/types.ts](../src/types.ts)、[src/ui-state.ts](../src/ui-state.ts) | 前端类型、默认设置、参数校验与测量状态解释 |
| [src/result-preferences.ts](../src/result-preferences.ts) | 结果筛选开关与排序偏好的本地存储 |
| [src-tauri/src/lib.rs](../src-tauri/src/lib.rs) | Tauri 命令、任务互斥、事件桥接和关闭恢复 |
| [src-tauri/src/model.rs](../src-tauri/src/model.rs)、[src-tauri/src/engine.rs](../src-tauri/src/engine.rs) | Rust 配置／测量模型、候选抽样与测速流程 |
| [src-tauri/src/stability.rs](../src-tauri/src/stability.rs)、[src-tauri/src/network.rs](../src-tauri/src/network.rs) | TCP 稳定性探测与网络路径快照 |
| [src-tauri/src/favorites.rs](../src-tauri/src/favorites.rs)、[src-tauri/src/history.rs](../src-tauri/src/history.rs) | 收藏快照、复测任务、测速历史和对比基线 |
| [src-tauri/src/storage.rs](../src-tauri/src/storage.rs)、[src-tauri/src/checkpoint.rs](../src-tauri/src/checkpoint.rs) | 状态迁移、原子写入和运行中检查点 |
| [tests/](../tests/)、[.github/workflows/ci.yml](../.github/workflows/ci.yml) | 前端回归测试和跨平台检查；Rust 单元测试在各模块内 |

修改配置字段、默认值或校验规则时，同步检查前端和 Rust 的 `ScanConfig`，并更新两端共用的[校验用例](../tests/fixtures/config-validation.json)。Tauri 的保存设置与开始测速命令用 `{ field, message }` 返回设置错误，界面据此定位字段。新测量的排除或未完成记录保留可读的 `exclusionReason`，并以 `exclusionCode` 供界面分类。没有原因码的旧记录继续按当时的文字兼容显示。

前端通过 Tauri 命令启动、取消和读取任务。运行中通过 `scan-update` 和 `favorite-test-update` 接收快照或增量，按任务 ID 和 `updateRevision` 应用更新。更新序号出现缺口时，前端向后端读取最新快照。修改事件载荷时，应同步检查 Rust 的发送逻辑和 `App.vue` 的接收逻辑。 [Rust 事件桥接](../src-tauri/src/lib.rs) · [前端事件入口](../src/App.vue)

修改本地数据格式时，检查旧版 `state.json`、单独保存的历史记录和收藏候选列表能否读取，并同步更新备份恢复说明。 [存储实现](../src-tauri/src/storage.rs) · [本地数据与备份](../README.md#本地数据与备份)

## 小规模网络诊断

命令行诊断入口调用与桌面相同的 Rust 测速核心，不写入桌面设置或测试历史。必须显式提供 JSON 配置文件；不带参数只显示用法并退出，不会启动默认网段扫描。配置字段使用 camelCase，与 [Rust `ScanConfig`](../src-tauri/src/model.rs) 一致，省略字段采用其默认值。Rust 保留的推荐默认值与桌面有差异，例如 `ipsPerRegion` 为 3、`minDownloadMbps` 为 20；下面显式设为 1 和 0，使结果不受最低推荐速率门槛限制。

将下面内容保存为 `scan-config.json`。它只对列出的 4 个候选 IP 做初筛；`limited` 模式配合 `candidatesPerRegion: 1`，使每个所选地区最多 1 个候选进入精测，不保证 4 个地址都进入下载。下载正文总预算为 8 MiB，每轮正文读取最多 1 秒，共 2 轮，HTTPS 验证 2 次。地区列表与桌面提供的香港、日本、新加坡、美国一致，实际可能只发现其中少数地区。这些 IP 只是测试输入，并非预先推荐的结果。

```json
{
  "regions": ["HK", "JP", "SG", "US"],
  "source": "custom",
  "customIps": "104.16.1.1\n104.17.1.1\n104.18.1.1\n104.19.1.1",
  "maxCandidates": 4,
  "maxTcpMs": 230,
  "tcpLatencyScreening": false,
  "stabilityFirst": true,
  "stabilityAttempts": 100,
  "stabilityIntervalMs": 200,
  "maxStabilityFailurePercent": 1,
  "stabilityLatencyMetric": "p95",
  "tcpConcurrency": 4,
  "ipsPerRegion": 1,
  "preference": "daily",
  "minDownloadMbps": 0,
  "candidatesPerRegion": 1,
  "regionalCandidateLimit": 20,
  "precisionMode": "limited",
  "verificationAttempts": 2,
  "downloadRounds": 2,
  "downloadSeconds": 1,
  "downloadBudgetEnabled": true,
  "downloadBudgetMiB": 8,
  "targetUrl": "https://speed.cloudflare.com/__down?bytes=1048576"
}
```

运行诊断：

```sh
cargo run --manifest-path src-tauri/Cargo.toml --example scan -- ./scan-config.json
```

阶段和简短进度写入标准错误，最终 `{ "progress": ..., "results": ... }` JSON 写入标准输出，可在命令末尾添加 `> scan-result.json` 保存。配置错误、文件错误或测速核心错误返回非零退出码；测试完成但没有可用 IP 时仍应查看 JSON 中的实际结果。诊断入口保留证书验证，不提供跳过 TLS 校验的选项。

另有单地址 TCP 对照入口，用于比较应用探测与标准库阻塞 socket 的结果，不执行 HTTPS 或下载，也不写应用历史：

```sh
cargo run --manifest-path src-tauri/Cargo.toml --example tcp_compare -- 104.16.1.1:443 20
```

必须提供目标地址和端口；次数默认为每组 20 次，可设 1–100。两组同时运行，每组内部串行探测，启动间隔 200 ms，连接超时 1,500 ms，输出含 `applicationProbe`、`blockingSocketBaseline` 及可采集到的运行前后网络路径。该工具测满指定次数，统计超限样本使用固定 200 ms 阈值，不执行桌面当前的累计 2 次失败淘汰规则，不能直接作为地区优选通过判定。
