# 参与开发

使用当前 Rust stable 工具链，构建依赖见 [README](README.md)。仓库保留 `Cargo.lock`，常规构建使用 `--locked`；更新依赖时一并提交锁文件。

提交前运行：

```bash
cargo fmt --all --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked --release
```

修改采集、设备发现或图形窗口行为时，应注明验证的平台、ADB 版本及是否使用真机。CI 仅在推送版本标签时检查 Linux 和 Windows 的编译与单元测试，不能代替桌面交互、真机连接和 GPU 性能验证。

性能测量命令和限制见 [UI 设计与性能](docs/ui-review.md)。不要把合成 CPU 测量报告成实际帧率。

问题报告请包含操作步骤、预期与实际结果、系统及应用版本。附加日志或截图前请隐去设备序列号、账号、令牌和业务数据。PR 请说明具体行为变化与验证结果。

`src/` 为应用源码，`assets/fonts/` 为内置字体及其许可，`prototypes/` 为不参与构建的布局原型，`docs/` 为设计、验证与发布指南。临时截图、模拟 ADB、诊断报告和构建输出放在被忽略的 `target/` 下。
