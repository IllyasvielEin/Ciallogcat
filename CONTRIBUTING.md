# 参与开发

使用当前 Rust stable 工具链，构建依赖见 [README](README.md)。仓库保留 `Cargo.lock`，常规构建使用 `--locked`；更新依赖时一并提交锁文件。

新主机接手先读 [TODO](TODO.md) 和 [迁移指南](docs/migration.md)。

提交前运行：

```bash
cargo fmt --all --check
cargo test --locked --all-targets
cargo test --locked --no-default-features --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo clippy --locked --no-default-features --all-targets -- -D warnings
```

Linux 上还应运行 fake ADB 协议自测与 CLI 打包契约测试（Python 3 标准库；DEB 归档检查需要 `dpkg-deb`）：

```bash
python3 -B tests/fixtures/test_fake_adb.py
python3 -B -m unittest discover -s examples -p 'test_*.py'
python3 -B -m unittest discover -s tools -p test_transport_benchmark.py
cargo fmt --manifest-path tools/proto-codegen/Cargo.toml --check
cargo run --locked --manifest-path tools/proto-codegen/Cargo.toml -- --check
python3 -B -m unittest discover -s .github/scripts -p test_package_cli.py
```

[Checks 工作流](.github/workflows/checks.yml) 在 Pull request、推送 main 和手动触发时运行三个作业：Linux headless 测试与 Clippy（含 Python 自测）、Linux GUI 行为测试与默认功能 Clippy，以及 Windows 默认功能测试与 Clippy、headless CLI 构建及冒烟检查。格式检查只在 Linux headless 作业运行一次。作业有独立超时，同一分支或 PR 的新检查取消旧检查；失败时保留已有的受控测试诊断文件七天。

发布前另运行 `cargo build --locked --release`。标签发布工作流继续构建 Linux 和 Windows 发行包，并验证 Ubuntu 24.04 DEB 的安装与卸载；日常 Checks 不打包或发布。修改采集、设备发现或图形窗口行为时，应注明验证的平台、ADB 版本及是否使用真机。完整 Python fake ADB 场景在 Linux 运行，共用原生 Rust fixture 覆盖 Linux/Windows 关键生命周期与二进制通道；CI 仍不能代替原生桌面交互、真机连接和 GPU 性能验证。

性能测量命令和限制见 [UI 设计与性能](docs/ui-review.md)。不要把合成 CPU 测量报告成实际帧率。

问题报告请包含操作步骤、预期与实际结果、系统及应用版本。附加日志或截图前请隐去设备序列号、账号、令牌和业务数据。PR 请说明具体行为变化与验证结果。

`src/` 为应用源码，`assets/fonts/` 为内置字体及其许可，`prototypes/` 为不参与构建的布局原型，`docs/` 为设计、验证与发布指南。临时截图、模拟 ADB、诊断报告和构建输出放在被忽略的 `target/` 下。

ADB 子进程插桩测试无需真机，运行方法、场景与隔离边界见 [ADB 测试说明](docs/adb-testing.md)。
