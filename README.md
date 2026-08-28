# Union Builder

`union-builder` 2.0 是 Union 官方发行组合、契约验证、打包和文件生命周期 CLI。Builder 在发行
构建阶段决定发行包**包含**哪些模块；Union Core 在运行阶段决定这些已包含模块是否**运行**。
两者是不同状态，profile 不再映射 Cargo feature，也不会替用户启用模块。

## 架构边界

- Core 和 Web Shell 各构建一次。Builder 不向 Core 传递任何业务 Cargo feature；Core 的 Cargo
  graph 本身不得包含业务模块代码。Web Shell 只提供认证、导航、权限门控和动态模块加载能力。
- 每个选中的模块 Backend 独立编译为私有 worker，并与该模块的 Manifest、权限、配置 Schema、
  版本元数据、Frontend 和 Migration 组装为 `modules/<id>` 包。
- 模块只允许 `process` execution、loopback bind，Backend service 必须具有 platform visibility，
  因此所有业务流量都只能通过 Union Gateway。管理面路由必须使用平台认证与 RBAC；只有 profile
  的 `module_auth_routes` 精确列出的非浏览器设备令牌或短期媒体能力路由可以使用模块认证。Builder
  不生成模块独立公共产品、容器镜像或模块 Release。
- Manifest v1 使用 `sarmg-platform-core` 的同一 Rust validator；整个选择集还会校验 Core/API
  兼容范围、依赖版本、缺失依赖、服务名冲突和依赖环。
- profile 是不可变源码 revision 与发行包含集合；模块的 enabled/disabled 状态由 Core 持久化，
  不进入 profile、release manifest 或 Builder install 事务。
- GitHub Actions 只安装工具链并调用 CLI，没有第二套隐藏的组合、校验或打包逻辑。

这是一种“发行时模块组合 + 运行时启停 + 独立进程隔离”模型。它既不是把业务 feature 编译进
Core，也不是从公网任意下载代码的插件市场。

## 模块源模板

每个独立模块仓库在项目根或 profile 指定的 bundle 子目录提供元数据。Sunshine 的 bundle 位于
仓库根，Host Monitoring 的 bundle 位于 `host-monitoring-worker/`。Builder 只复制下列 Manifest
声明的白名单，不会复制 `src/`、`.git/`、
`target/`、mobile、docs 或其他源码内容：

```text
<bundle-root>/
├── manifest.json
├── permissions.json
├── version.json
├── config/schema.json
├── frontend/entry.js
├── frontend/...                 # manifest 引用的样式和资源
├── migrations/...               # embedded migration 可省略
└── version/...                  # 可选 release notes
```

`permissions.json` 必须与 Manifest `permissions` 数组完全一致。源 `version.json` 声明
`manifest_version`、`id`、`version`，以及可选且必须与 Manifest 一致的 `channel`、`distribution`、
`license`、`compatibility`。源文件不能声明 `source_revision`，因为提交不能预知自身 Git ID；
Builder 从 profile 锁定的 revision 生成最终 `version.json`，并同时把 revision 写入最终 Manifest。

如果模块声明 `[module.frontend]`，Builder 对该模块单独执行固定的 `npm ci`、`npm run build`，
并将 output 作为模块 frontend；否则使用 bundle 中已版本化的独立 ES module 资源。两种方式都
保持模块页面与 Web Shell 分离，不能把另一份 React/ReactDOM 打进模块入口。

## 使用

```bash
cargo install --path . --locked

union-builder check --config profiles/full.toml
union-builder plan --config profiles/full.toml
union-builder plan --config profiles/full.toml --format json
union-builder materialize \
  --config profiles/full.toml \
  --caller-repository https://github.com/isarmg/union-rust.git \
  --caller-source /absolute/path/to/union-rust \
  --caller-revision <full-40-character-git-id> \
  --output profiles/full.materialized.toml
union-builder build --config profiles/full.toml --cargo-profile release
union-builder verify --release dist/full
```

`check` 在编译前验证完整源码身份、许可证、bundle 文件、Manifest 语义、依赖拓扑和平台兼容性。
本地 source 不存在时，只能从无凭据 GitHub HTTPS URL 获取完整 40 位 revision。正式 profile
设置 `require_clean_sources = true`，拒绝脏工作树。

`materialize` 专用于可复用 Actions 中的 Union 调用方构建：它验证调用方路径确为 Git worktree
根、`HEAD` 等于显式 40 位 revision，并要求 repository 精确等于
`https://github.com/isarmg/union-rust.git`。当前官方 profile 中只有 distribution 指向该仓库，
因此只把 Core/Web Shell 改指向调用方 checkout；Sunshine、Host Monitoring 和其他模块继续使用
各自独立仓库的不可变 revision。模块包含集合、版本、包名和 binary 均保持不变。输出会重新
通过 strict schema v2 解析与校验，采用原子发布且拒绝覆盖。该命令不会修改正式 profile，因此
打破的是发布编排的最终 SHA 循环，而不是放宽源码身份校验。

外部 reusable-workflow 调用还必须把同一个不可变 Builder SHA 同时写入 `uses@<sha>` 与
`builder-revision`；workflow 会在 checkout Builder、物化或构建前拒绝缺失、短 SHA 和 tag：

```yaml
uses: isarmg/union-builder/.github/workflows/build-union.yml@0123456789abcdef0123456789abcdef01234567
with:
  profile: full
  builder-revision: 0123456789abcdef0123456789abcdef01234567
  materialize-caller-source: true
  caller-revision: ${{ github.sha }}
```

Builder 仓库自身的 workflow call 和手工 dispatch 不接受另一个源码 pin，始终使用当前
`github.sha`。

`build` 拒绝覆盖已有目录，输出：

```text
dist/full/
├── bin/unionc
├── modules/
│   └── <id>/
│       ├── manifest.json
│       ├── permissions.json
│       ├── version.json
│       ├── config/schema.json
│       ├── backend/<executable>
│       ├── frontend/...
│       └── migrations/...
├── share/union/web/...
├── share/licenses/...
├── union-release.json
└── SHA256SUMS
```

`verify` 会再次运行 Manifest/依赖/兼容校验，检查 identity、version、source revision、必需文件、
可执行位、所有 bundle 引用、无符号链接/路径逃逸，以及 `SHA256SUMS` 与实际文件集合完全一致；
文件数、单文件/总字节、路径长度和目录深度均有上限，防止恶意包造成无界扫描。
`union-release.json` 同时固化每个模块的 `module_auth_routes`，所以离线复验仍会拒绝 Manifest 增加、
删除或改名任何模块认证例外。

## 安装和回滚

```bash
union-builder stage --release dist/full --root /opt/union
union-builder install --release dist/full --root /opt/union
union-builder rollback --root /opt/union
```

Unix 上，`stage` 写入不可变 `releases/<release-id>` slot；`install` 原子切换相对 `current` 符号
链接；`rollback` 切回 previous slot。Windows 支持构建、验证和 staging，但激活需由平台安装器
处理。

重要边界：Builder 回滚的只是发行文件与指针，**不回滚数据库 Migration、模块数据库、媒体或
其他业务数据**。详见 [Release lifecycle](docs/RELEASE-LIFECYCLE.md)。

官方组合矩阵见 [Profiles](docs/PROFILES.md)，自定义字段见
[`union-build.example.toml`](union-build.example.toml) 和 [config schema v2](docs/CONFIG-V2.md)。
