# Union 发行包生命周期

Builder 2.0 的文件事务单位是 Core、Web Shell 和所选模块包组成的完整 Union 发行目录。运行时
启停是 Core 的状态事务，不是 Builder 的文件事务。

## 发布前门禁

1. 锁定每个源码的完整 Git revision；相同仓库条目使用同一 revision。Union 调用方构建应保存
   `materialize` 输出作为证据；distribution、Sunshine、Host 必须同时等于 workflow 的
   `github.sha`，其他仓库 pin 与包含集合必须保持原 profile 值。
2. 对 profile 执行 `check` 和 `plan --format json`，保存 plan 作为审计证据。
3. `check` 必须通过 Manifest v1、权限/config/version 一致性、平台兼容、完整依赖图，以及
   `module_auth_routes` 与 Manifest 模块认证路由集合的精确一致性校验。
4. 执行 `build`；不得向输出目录手工追加或替换文件。
5. 在交付端和目标主机分别执行 `verify`。

`SHA256SUMS` 是精确文件清单：缺失、篡改、额外文件、符号链接、非普通文件或丢失可执行位都
会失败。SHA-256 不是发布者签名，发行包仍须通过受信任 TLS/Release 渠道或外层制品签名获取。

## Staging 与激活

`stage` 先验证输入，再复制到 install root 同一文件系统中的临时目录，复验后 rename 为不可变
`releases/<release-id>`。已有同 ID slot 只会验证并复用，绝不覆盖。

`install` 在 stage 后原子替换 `current` 相对符号链接，并保留原 current 为 `previous`。运行方
看到的始终是一个完整 release slot。Builder 不启停 systemd 服务、不修改 Core 的模块 enabled
状态，也不自动执行数据库迁移。

建议上线编排：

1. stage 新发行并 verify。
2. Core 在候选 slot 中发现 bundled manifests，完成 compatibility/dependency 预检。
3. 对每个模块的数据所有权范围创建备份/恢复点，执行向后兼容 Migration 并复验。
4. drain/停止当前 Union，由 Builder install 切换文件 slot。
5. 启动新 Core；Runtime 只恢复新发行中仍包含且此前 enabled 的模块，逐个检查健康与 Gateway。
6. 失败时停止新 Core，评估数据兼容性后再决定是否执行文件 rollback。

## 文件回滚不等于数据回滚

`rollback` 只交换 `current`/`previous` 指针。它不会：

- 逆向执行 PostgreSQL、SQLite 或 embedded Migration；
- 恢复模块数据库、媒体、监控数据或对象存储；
- 恢复已被新 worker 改写的配置或外部系统状态；
- 判断旧 worker 是否仍能读取升级后的 Schema。

模块必须优先采用 expand/migrate/contract、向后兼容读取或明确的备份恢复流程。若 Migration
不可逆，运维必须将“恢复旧文件”和“恢复模块数据”作为两个独立、有顺序约束的操作；不能因
Builder 指针已回退就宣称系统完成回滚。

## 中断与并发

- staging 中断：临时目录不会成为 slot，current 不变。
- 激活前中断：旧 current 继续生效。
- current rename 后中断：新完整 slot 生效，可复验后继续编排。
- rollback 中断：current 始终指向完整 slot，但应复验 current/previous 再恢复服务。

`current` 的单次 rename 是原子的；`current` 与 `previous` 两个名字不是跨文件系统事务。部署
编排必须串行化 Builder 操作，并禁止第三方同时修改 install root。
