# 官方 Profile：发行包含集合

所有 profile 都构建同一套 Union Core 和动态 Web Shell。差异只在最终发行目录预装哪些完整模块
包，不涉及 Core Cargo feature，也不代表模块运行时已启用。

| 模块 | minimal | storage | monitoring | full |
|---|---:|---:|---:|---:|
| Photo Backup | — | ✓ | — | ✓ |
| Dufs | — | ✓ | — | ✓ |
| Sentinel Monitor | — | — | ✓ | ✓ |
| Host Monitoring | — | — | ✓ | ✓ |
| Sunshine | — | — | — | ✓ |

每个 `[[module]]` 只描述不可变源码、Backend Cargo target、bundle 模板、可选独立 Frontend build，
以及严格的 `module_auth_routes` 路由 ID 例外清单。
模块监听地址、Gateway 路由、健康检查、权限、配置与 Migration 均来自经过 Platform validator
验证的 Manifest，避免 profile 与运行契约形成两份事实源。

官方认证例外只有 Photo Backup 的 `upload-part`、`mobile-api`，Sentinel Monitor 的 `media-hls`，
以及 Host Monitoring 的 Agent 上报、配对请求创建/读取/轮询和 Agent 本地一次性 code 激活路由；
Dufs 与 Sunshine 均为空。Host 的浏览器管理页使用另一条平台认证激活路由和
`host-monitoring.agents.write`，不在例外清单中。例外只用于
非浏览器设备令牌、一次性配对凭据或短期媒体能力，Manifest
中所有 `auth = module` 路由的 ID 集合必须与 profile 完全相等。管理面始终使用平台 RBAC，worker
仍只绑定 loopback，所有请求仍只能经 Union Gateway 到达。

发行后的状态边界：

- `included`：`modules/<id>` 存在且在 `union-release.json` 中；只能由 Builder 生成新发行改变。
- `enabled`：Core 允许 Runtime 启动该模块；可在运行时改变，不需要重建 Core/Web Shell。
- `available`：worker 已通过健康检查并被 Gateway 纳入路由；这是瞬时运行状态。

因此 minimal 不能在运行时启用 Photo Backup；storage 可以启停 Photo Backup/Dufs，但不能凭空
启用 Sentinel。增加或移除发行包含内容必须生成一个新的完整 Union 发行包。

官方 profile 必须为每个独立源码仓库锁定完整 40 位 Git revision，并在发版前通过 `check`。
Core/Web Shell 来自 `union-rust`，Sunshine 与 Host Monitoring 分别来自 `sunshine-worker` 和
`host-monitoring`；数据库 URL、凭据、存储目录和设备地址属于运行配置，不进入 profile。

Builder 2.0.0 的四个官方 profile 已分别锁定协调完成的 Union、Dufs、Photo Backup、Sentinel
Monitor、Sunshine 和 Host Monitoring 完整 revision。任何后续发布都必须先更新相应 revision，
并让 CI 的四个官方 profile `check` 全部通过。Release workflow
还会拒绝残留 `TODO(release)` 的 profile，避免把迁移期占位配置发布为正式发行。

Actions 可用 `profile: minimal|storage|monitoring|full` 选择官方文件，或通过 `config` 使用调用
仓库内的自定义文件；二者互斥。最终 workflow 只调用 Builder CLI 并上传一个保留 Unix mode 的
完整 distribution tar，不发布模块单独 artifact 或 public Release。

当 `isarmg/union-rust` 通过 reusable workflow 构建自身提交时，调用方必须显式传入
`materialize-caller-source: true`、`caller-revision: ${{ github.sha }}` 和完整 40 位
`builder-revision`。workflow 同时校验调用仓库与 caller SHA，再用 `materialize` 生成邻接临时
profile；Builder 自身 workflow 和 dispatch 固定使用自己的 `github.sha`。当前官方 profile 只会
替换 distribution 的 source/revision；通用实现仍会原子替换自定义 profile 中与 Union 仓库身份
完全相同的条目。它不改变 profile 定义的包含集合，也不允许其他调用仓库借此注入源码。
为保证正式 pin 和相对路径语义可审计，workflow 启用物化时只接受 Builder 官方 `profile`，拒绝
调用仓库自带的 `config`。

Union 调用方示例：

```yaml
jobs:
  distribution:
    uses: isarmg/union-builder/.github/workflows/build-union.yml@0123456789abcdef0123456789abcdef01234567
    with:
      profile: full
      builder-revision: 0123456789abcdef0123456789abcdef01234567
      materialize-caller-source: true
      caller-revision: ${{ github.sha }}
```

`uses@...` 与 `builder-revision` 必须是同一个受信任 Builder 完整 SHA；禁止 branch、tag、短 SHA
或两处指向不同提交。物化只解决 profile 无法预知调用方最终 SHA 的单向引用，不允许 Union 选择
另一个 Builder 源码或跳过 Builder 的 package 校验。

CLI 的 `--cargo-profile debug|release` 只控制 Rust artifact 优化级别，特意采用不同名称；它不
改变上述发行包含集合，也不能启用 Cargo 业务 feature。
