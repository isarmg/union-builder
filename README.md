# Union Builder

`union-builder` 是 Union 唯一的官方组合构建、打包和发布包生命周期工具。它把原来散落在
GitHub Actions YAML 中的源码固定、Cargo feature 选择、Rust/前端构建、发行目录组装、完整性
验证、安装和回滚收敛为同一套命令。GitHub Actions 只准备工具链并调用 CLI，不再承载另一套
隐式构建逻辑。

## 架构保证

- 清单在编译期选择模块，并把对应 `module-*` feature 编译进 Union 控制面。
- 每个选中模块编译为 Union 私有 worker，安装到 `libexec/union/modules` 并在运行时保持进程隔离。
- worker 必须监听 loopback 固定地址；唯一公共入口是 Union 网关。
- 输出是一个完整 Union 发行目录；本工具不生成或发布模块独立程序、容器或 Release。
- 所有源码使用完整 40 位 Git revision，不能使用会漂移的 branch 或 tag。
- 前端只允许固定流程 `npm ci`、`npm run build`，清单不能注入 shell 命令。
- 每个源码输入必须提供普通文件形式的 `LICENSE` 或 `LICENSE-APACHE`；发行目录保存各自许可
  文本及可选 `NOTICE`，并纳入完整性清单。
- `SHA256SUMS` 覆盖发行目录中的每个程序、前端资源和清单；额外文件同样会导致验证失败。

这套模型是“编译期裁剪 + 运行时进程隔离”，类似 Kconfig 决定交付图，而不是动态插件目录。
向 `libexec` 复制额外程序不会让 Union 识别它。

## 安装 Builder

Builder 1.0.0 Release 提供 Linux、macOS 和 Windows 命令行程序，也可从源码构建：

```bash
cargo install --path . --locked
```

构建与验证命令在所有平台可用。原子激活和回滚依赖 Unix 相对符号链接，目前只支持 Unix；
Windows 可以构建、验证和 staging，但必须由平台安装器完成激活。

## 构建一个发行版

```bash
union-builder check --config profiles/full.toml
union-builder plan --config profiles/full.toml
union-builder plan --config profiles/full.toml --format json
union-builder build --config profiles/full.toml --profile release
union-builder verify --release dist/full
```

`check` 验证清单、源码 revision、工作树状态、模块 ID、feature、URL 路径、端口冲突、loopback
约束和前端安装路径。若本地 `source` 不存在且配置了无凭据 GitHub HTTPS `repository`，工具只
获取指定 revision，不检出默认分支。`require_clean_sources = true` 会拒绝脏源码树。

`build` 始终对 Union 使用 `--no-default-features`，清单因此是交付图的唯一事实来源。它拒绝
覆盖已有输出目录。发行目录形状为：

```text
dist/full/
├── bin/unionc
├── libexec/union/modules/<module-id>
├── share/union/web/...
├── share/union/modules/<module-id>/...  # 仅有独立前端的模块
├── share/licenses/unionc/...
├── share/licenses/modules/<module-id>/...
├── union-release.json
└── SHA256SUMS
```

清单示例见 [`union-build.example.toml`](union-build.example.toml)。

## 官方 profiles

| Profile | 编译内容 | 适用范围 |
|---|---|---|
| `minimal` | Union 控制面 | 最小部署、基础管理 |
| `storage` | Union、Photo Backup、Dufs | 照片备份和通用文件管理 |
| `monitoring` | Union、Sentinel、主机监控 | 摄像头与主机可观测性 |
| `full` | Union 和五个私有 worker | 完整功能部署 |

仓库中的四个官方 profile 已锁定 Union 0.4.0 发行线所使用的完整 revision；它们既是构建输入，
也是源码溯源记录。只有 `union-build.example.toml` 为便于复制而保留全零示例值，使用者必须先
替换；`check` 会验证 revision 确实存在，因此示例配置不能被误发布。

精确能力矩阵与配置边界见 [`docs/PROFILES.md`](docs/PROFILES.md)。

## 前端配置

发行本体或模块可声明：

```toml
[distribution.frontend]
directory = "web"
output = "dist"
install = "share/union/web"
```

`directory` 相对源码根；`output` 相对该前端目录；`install` 相对发行根且必须位于
`share/union` 的子目录。路径不能包含 `..`、绝对路径或重叠目标。输出树中的符号链接和特殊
文件会被拒绝，所有普通文件都会进入校验和清单。

构建 Union 主前端时，Builder 会从同一 profile 自动注入
`UNIONC_WEB_MODULE_SUNSHINE` 与 `UNIONC_WEB_MODULE_HOST_MONITORING`。清单没有相应模块时
值为 `false`，Vite/Rollup 会删除对应视图、JS 和 CSS；用户环境中的同名变量不能改变发行图。

## 安装、升级和回滚

推荐先验证，再安装：

```bash
union-builder verify --release dist/full
sudo union-builder install --release dist/full --root /opt/union
```

安装布局是不可变 release slots 加两个原子指针：

```text
/opt/union/
├── releases/<release-id>/...
├── current  -> releases/<release-id>
└── previous -> releases/<previous-release-id>
```

`install` 先完整验证输入，再复制到临时目录、二次验证并原子发布到 `releases`；已存在的相同
slot 只会复用，不会覆盖。最后原子切换 `current`，保留 `previous`。也可只执行 staging：

```bash
sudo union-builder stage --release dist/full --root /opt/union
```

服务管理器应执行 `/opt/union/current/bin/unionc`。数据库迁移必须保持向后兼容，且应在切换
前完成模块自己的 `migrate/verify` 流程；Builder 不读取数据库秘密，也不替模块执行迁移。
切换后若健康检查失败，停止新进程并执行：

```bash
sudo union-builder rollback --root /opt/union
```

回滚只交换 `current`/`previous`，不需要网络、源码仓库或重新编译，也不删除任何 release。
完成指针切换后由服务管理器重启 Union。Builder 不主动停止进程或写 systemd 配置，这一边界
避免构建工具持有运行时秘密和服务管理权限。

生产切换顺序、数据库迁移前提和中断语义见
[`docs/RELEASE-LIFECYCLE.md`](docs/RELEASE-LIFECYCLE.md)。

## GitHub Actions

可复用 workflow 支持两种且只能选择一种输入：Builder 仓库内的官方 `profile`，或调用仓库内的
自定义 `config`。使用官方 profile 时，调用仓库不需要复制 TOML：

```yaml
jobs:
  union:
    uses: isarmg/union-builder/.github/workflows/build-union.yml@v1.0.0
    with:
      profile: full
```

自定义组合则只传调用仓库中的相对路径，不能同时传 `profile`：

```yaml
jobs:
  union:
    uses: isarmg/union-builder/.github/workflows/build-union.yml@v1.0.0
    with:
      config: release/union-build.toml
      artifact-name: union-custom-distribution
```

`workflow_call` 会拒绝同时设置两项、两项都为空、未知 profile、绝对 config 路径和逃离调用仓库
的路径。官方值仅允许 `minimal`、`storage`、`monitoring`、`full`。

在 Builder 仓库手动运行 `Build Union Distribution` 时，`profile` 是下拉选项且默认 `full`。
若要使用本仓库中的自定义清单，选择 `custom` 并填写 `config`；选择官方 profile 时必须让
`config` 保持为空。手动运行会构建触发它的 Builder 提交，用于 tag 前候选验证；外部可复用调用
固定使用已发布的 `v1.0.0` 工具实现。

workflow 固定 Rust 1.98.0 和 Node.js 26.7.0，后者用于 Union/Sentinel 前端的 `npm ci` 与
`npm run build`。YAML 只解析输入、准备固定工具链、构建 Builder CLI，然后调用一次
`union-builder build`，先用同一 CLI 验证结果，再上传单一 `union-distribution` artifact。
artifact 内只有 `union-distribution.tar`；内层 tar 专门保留 worker 的 Unix 可执行位，避免
GitHub artifact 的 ZIP 传输把程序变成普通文件。模块选择、源码获取、前端构建、组装和校验
仍全部由 CLI 决定。模块仓库不得复制独立编译、打包或发布 workflow；Builder 自身 Release
仅发布 Builder CLI。Builder 发版门禁会解析四个官方 profile 的真实远端 revision，并额外组装、
验证 full distribution；该验证 artifact 不会混入 Builder CLI Release。

## 明确边界

Builder 负责可重复源码选择、编译、前端资源、组装、校验、不可变 staging 和指针回滚。它不
创建数据库、不生成或存储生产秘密、不开放 worker 网络端口、不配置反向代理、不管理 systemd、
不决定数据迁移的业务兼容性，也不是通用 CI shell 执行器。

## 开发验证

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

本仓库第一方代码和文档使用 [Apache License 2.0](LICENSE)。
