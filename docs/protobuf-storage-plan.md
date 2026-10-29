# 原生紧凑存档设计

方案已实现于共享 codec、CLI 采集/查询及 GUI 文件工作流。后续缺陷和设备验收统一见 [TODO](../TODO.md)；格式字段以 [native-format](native-format.md) 为准。

## 决策

GUI 与 CLI 只使用一种原生格式，不提供版本选择或旧格式读写。文本和 protobuf 是独立的传输选择，均使用同一 codec；默认文本，proto 必须显式选择。proto 记录仅存一份 base64 原始 payload，通过固定 projection profile 派生展示和已知字段；非 proto 行存普通字段。不改 Android wire schema，不引入数据库、压缩或隐式降级。内部 schema 常量仅是校验契约，见格式文档。

所有来源、身份、scope、capture 与未知扩展保留；派生字段容器内的扩展通过 projection_extensions 往返。冲突、超预算或不支持的 profile 明确失败。`get --view stored|decoded` 区分原行和派生全文，decoded 独立限制 64 MiB，普通工件行及 header/footer 仍有 8 MiB 边界。

## 共享接口

`DocumentHeader::live`、`Document::from_snapshot` 与 `StreamEncoder` 共用唯一布局；`select` 创建派生 artifact，记录 derived_from_artifact 并保留 session/seq。格式选择和转换 API 已移除。读写逐行检查取消和预算，不能静默截断；ADB 与文件共用 `DecodedEntry::project()`。

深层扩展新增包装后可能跨过读取递归限制，当前 X29 尚未修复；不要将编码成功等同于任何输入都可重读。

## 历史测量与后续取舍

以下为旧双格式实现的历史测量，不是当前构建的性能验收。真实 encoder 对 50,000×128B / 20,000×2KiB 输入，v2→v3 大小分别由 60,128,660→27,878,773 B、164,838,660→62,338,773 B，减少 53.6348% / 62.1819%。70,000 条 raw payload 与原 wire 逐条一致，展示、身份、证据核对通过；此处是转换工件，header 与直接采集工件不同。

[三组合端到端基准](transport-performance-2026-10-24.md)进一步表明磁盘缩小不保证 wall 吞吐提高。内存仍可能同时持有 raw/decoded/display，对象去重和重复验证优化应先测量。设备/OEM 兼容与 byte channel 验收仍延期，方法见 [protobuf 研究](logcat-protobuf.md#11-可重复的设备验收矩阵)。
