# Union 单一发行包生命周期

本文定义 Builder 1.0.0 的交付事务边界。一次发布的原子单位是整个 Union 目录，不是单个
worker。禁止只替换 `libexec/union/modules` 中的某个文件，因为 Union 控制面 feature、worker
二进制、前端资源和网关契约必须来自同一个构建图。

## 发布前

1. 确认官方 profile 中每个 revision 都是本次已推送源码的完整 40 位对象 ID；正式 profile
   不得保留全零占位。
2. 确认相同源码仓库的条目使用相同 revision，例如 Union 本体、Sunshine 与主机监控 worker。
3. 对 profile 执行 `check` 和 `plan --format json`，保存 plan 作为审计证据。
4. 执行 `build`；不要在输出目录中追加文件。
5. 在交付和目标主机上分别执行 `verify`。

`SHA256SUMS` 是完整文件清单：缺失、篡改或额外文件均失败；每个输入源码的 Apache
许可证及可选 `NOTICE` 也位于 `share/licenses` 并受同一清单保护。它不提供发布者身份认证，Release
仍需通过受信任的 GitHub Release/TLS 渠道获取，或由外层制品签名系统签名。

## 安装事务

`stage` 将发行包复制到同一文件系统内的临时目录，复验后 rename 为不可变 release slot。
`install` 进一步用 rename 原子替换 `current` 相对符号链接。任何时刻运行方看到的 `current`
要么是旧的完整发行版，要么是新的完整发行版，不会看到半复制目录。

Builder 不会删除旧 slot。空间回收是独立的、显式的运维操作，必须避开 `current`、`previous`
以及正在运行进程实际使用的 release。

## 数据库与服务切换

推荐顺序：

1. `stage` 新发行版。
2. 使用新 slot 中的 worker 执行向后兼容的数据库迁移和 verify。
3. 停止或 drain 当前 Union。
4. `install` 激活已验证发行版（重复 staging 会安全复用相同 slot）。
5. 启动 `/opt/union/current/bin/unionc` 并检查 Union 与每个已编译 worker 的 readiness。
6. 失败时停止新进程，执行 `rollback`，再启动旧 Union。

Builder 只管理文件与指针，不重启服务，也不接触数据库 URL、密钥和模块数据。不可逆数据库
迁移会破坏二进制回滚能力，属于模块迁移设计错误，不能由 Builder 的文件回滚补救。

## 中断语义

- staging 中断：临时目录不会成为 release slot，`current` 不变。
- 激活前中断：旧 `current` 继续生效。
- `current` rename 完成后中断：新发行版完整生效；release slots 均保留。
- rollback 中断：`current` 始终指向一个完整 slot，可重新检查指针并再次执行运维切换。

`current` 的单次替换是原子的；`current` 与 `previous` 两个名字不是跨文件系统事务。两者必须
位于同一真实 install root，且不得被第三方进程并发修改。部署编排应串行执行 Builder 命令。
