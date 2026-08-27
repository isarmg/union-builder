# 官方 Profile 边界

四个 profile 都构建 Union 控制面和其前端，区别仅在编译进入发行图的私有 worker：

| 模块 | minimal | storage | monitoring | full |
|---|---:|---:|---:|---:|
| Photo Backup | — | ✓ | — | ✓ |
| Dufs | — | ✓ | — | ✓ |
| Sentinel | — | — | ✓ | ✓ |
| 主机监控 | — | — | ✓ | ✓ |
| Sunshine | — | — | — | ✓ |

每个 `[[module]]` 同时选择 Union 的 `module-*` feature 和对应 worker 二进制。缺少任何一侧都
不是受支持的发行图。运行时配置只能配置已编译模块，不能增加 profile 未选择的模块。

Sunshine 与主机监控的 Union-console 前端开关由 Builder 从表格中的同一模块集合推导，不在
profile 中重复配置；这保证 Rust feature、worker 和浏览器 chunk 三者同时出现或同时省略。

所有 worker 的 bind、网关前缀和健康检查路径都由 profile 固定，并由 Builder 验证 loopback
和冲突。数据库 URL、凭据、存储目录、设备地址等环境相关信息不进入 profile，也不进入发行
清单。

可复用 Actions workflow 通过 `profile: minimal|storage|monitoring|full` 直接选择本目录的官方
清单，或通过 `config` 使用调用仓库的自定义清单；两者严格互斥。手动触发默认选择 `full`，使用
自定义清单时必须明确选择 `custom`。无论入口为何，最终都只调用同一个 `union-builder build`
命令并产生一份 distribution artifact。artifact 的唯一载荷是保留 Unix mode 的
`union-distribution.tar`；下载方须先安全解包，再验证 manifest、精确文件清单、可执行位和
`SHA256SUMS`。

官方 profile 必须提交完整且不可变的 revision，并在发版前由 `check` 复验；当前文件锁定
Union 0.4.0 发行线。不能为模块建立独立 Release；更改任一 worker 都要生成新的完整 Union
发行包并提升发行版本。
