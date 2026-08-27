# Union Builder

`union-builder` 是 Union 的声明式组合构建工具。它把原来散落在 GitHub Actions YAML 中的
源码固定、Cargo feature 选择、模块编译、发行目录组装和校验和生成收敛为同一套命令；
GitHub Actions 只负责准备工具链和调用它。

它实现的模块模型是：

- 构建时通过清单选择模块，并把 `module-*` feature 编译进 Union 控制面；
- 每个选中模块仍编译为独立进程，安装在 Union 私有 `libexec` 目录；
- 模块只能监听 loopback 地址，公共入口只能由 Union 网关提供；
- 输出只有一个 Union 发行目录和一份 `union-release.json`，不生成模块独立发行版；
- 每个源码必须固定到完整的 40 位 Git revision，构建清单不能使用漂移的 branch/tag。

这不是通用 CI 脚本执行器。清单不能注入 shell 命令，也不能下载带凭据的 Git URL；当前只
接受无凭据的 GitHub HTTPS 仓库。工具不会安装服务、写生产配置、创建数据库或管理秘密。

## 安装

Release 提供 Linux、macOS 和 Windows 的命令行程序。也可从源码构建：

```bash
cargo install --path . --locked
```

## 使用

```bash
union-builder check --config union-build.toml
union-builder plan --config union-build.toml
union-builder plan --config union-build.toml --format json
union-builder build --config union-build.toml --profile release
```

`check` 会验证清单、源码 revision、模块 ID、feature、URL 路径、端口冲突和 loopback 约束。
若本地 `source` 不存在且配置了 `repository`，工具会把指定 revision 获取到该路径；不会
检出默认分支。`require_clean_sources = true` 可要求本地组合构建没有未提交修改。

`build` 拒绝覆盖已有输出目录。成功后目录形状固定为：

```text
dist/
├── bin/unionc
├── libexec/union/modules/<module-id>
├── union-release.json
└── SHA256SUMS
```

参见 [`union-build.example.toml`](union-build.example.toml)。
[`profiles/full-transition.toml`](profiles/full-transition.toml) 固定当前可实际组装的
Sentinel、Photo Backup 与 Dufs worker；Sunshine 和主机监控仍在 Union 进程内，因此该清单明确
标为过渡配置。`modules = []` 的核心构建也受支持，构建器仍会强制使用
`--no-default-features`，不会意外带入 Union 默认模块。

## 在 GitHub Actions 中复用

调用方仓库只需保留组合清单，编译与组装逻辑由本仓库的可复用 workflow 提供：

```yaml
jobs:
  union:
    uses: isarmg/union-builder/.github/workflows/build-union.yml@v0.2.0
    with:
      config: profiles/full-transition.toml
```

workflow 会 checkout 调用方清单与固定版本的构建器，执行同一个 CLI，并上传单一
`union-distribution` artifact。模块仓库不再复制编译、打包或发布 YAML。

## 清单边界

清单描述的是一个完整发行图，不是运行时插件目录。增删模块后必须重新构建 Union；把新程序
复制到 `libexec` 不会使 Union 识别它。运行配置可以提供数据库 URL、目录和秘密，但不能在
启动时增加未编译模块。

进程隔离与编译期选择并不矛盾：编译期决定发行物中包含哪些控制面适配器和工作进程，运行时
仍由操作系统提供地址空间、权限、资源限制和故障隔离。这个模型更接近内核的 Kconfig +
用户态服务集合，而不是 Rust 动态链接插件。

## 验证

```bash
cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
cargo test --locked
```

本仓库第一方代码和文档使用 [Apache License 2.0](LICENSE)。
