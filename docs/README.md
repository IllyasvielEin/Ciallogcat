# 文档入口

新 Agent 先读 [TODO 与交接](../TODO.md)、[主机迁移](migration.md) 和 [项目架构与方向](development-plan.md)。TODO 是唯一后续任务队列；历史阶段报告已合并为[验证状态](validation.md)，不再维护多份进度。

## 使用与契约

- [桌面交互与性能](ui-review.md)、[文件工作流](file-workflow.md)
- [CLI](cli.md)、[Agent 查询](agent-query.md)、[外部测试证据流程](agent-test-workflow.md)、[可运行示例](../examples/README.md)
- [共享核心](headless-core.md)、[原生格式](native-format.md)、[设备诊断](device-diagnostics.md)

## 验证与交付

- [贡献与检查命令](../CONTRIBUTING.md)、[ADB/mock 测试](adb-testing.md)
- [场景体验协议](experience-testing.md)、[原生体验工具](../tools/experience-lab/README.md)
- [验证状态与限制](validation.md)、[发布与打包](release.md)

## 设计依据与性能

- [Logcat 选项与生态](logcat-options-and-ecosystem.md)、[protobuf 研究与设备矩阵](logcat-protobuf.md)
- [原生紧凑存档设计](protobuf-storage-plan.md)、[固定 schema 与许可](../proto/README.md)
- [三组合性能实测](transport-performance-2026-10-24.md)、[基准复现工具](../tools/transport-benchmark.md)

保留的性能报告对应明确的旧构建和合成负载，不代表当前源码已最终验收。原始 target 工件只在旧主机可用；后续任务及构建不依赖它们。
