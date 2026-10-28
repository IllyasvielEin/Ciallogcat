# 迁移到新主机与 Agent

当前工作树包含大量未提交和未跟踪的功能文件。只克隆远端或只保存 `git diff` 会遗漏它们。后续任务以根目录 [TODO](../TODO.md) 为准；实施仍暂停，迁移本身不代表恢复产品修改或授权发布。

## 迁移包

本次整理生成的包包含：

- `source.tar.gz`：`git ls-files --cached --others --exclude-standard` 中实际存在的完整文件，含未跟踪源码、工具、锁文件、字体和许可证；不含 `.git`、target、个人配置或凭据。
- `history.bundle`：本地 HEAD 可达的 Git 历史，基线是 `853976f69ae7f362a6bbe5c12781172f5cd86403`，包含 main 分支；不含远端凭据。
- `manifest.json`：每个源文件的相对路径、SHA-256 和权限，以及基线跟踪但已删除的文件列表。
- `SHA256SUMS`：上述三个文件的校验值。

包保存于旧主机 `/tmp/ciallogcat-migration-2026-10-24/`，需在清理临时目录或关闭主机前传走。包只包含生成时的快照；之后修改源码须重新生成。

在新主机将包放于工作目录外，在包目录执行：

```sh
sha256sum -c SHA256SUMS
git clone -b main history.bundle Ciallogcat
tar -xzpf source.tar.gz -C Ciallogcat
python3 - <<'PY'
import hashlib, json
from pathlib import Path
m = json.loads(Path('manifest.json').read_text())
root = Path('Ciallogcat')
for name in m['deleted_tracked_files']:
    (root / name).unlink(missing_ok=True)
for name, expected in m['files'].items():
    p = root / name
    if hashlib.sha256(p.read_bytes()).hexdigest() != expected['sha256']:
        raise SystemExit(f'hash mismatch: {name}')
    if (p.stat().st_mode & 0o777) != expected['mode']:
        raise SystemExit(f'mode mismatch: {name}')
print('source snapshot verified')
PY
cd Ciallogcat
git remote set-url origin https://github.com/IllyasvielEin/Ciallogcat.git
git status --short
```

在全新目录恢复，避免覆盖已有工作。校验失败时停止使用该副本。恢复后的工作树保留“基线提交 + 未提交改动”，不会自动提交或推送。新 Agent 开始时应核对 HEAD、manifest、TODO 和用户最新指令。

## 环境与第一轮检查

使用 Rust stable（rustfmt、Clippy）、Python 3.10+ 和对应平台编译工具。GUI Linux 系统依赖见 [README](../README.md#build-from-source) 与 [Checks](../.github/workflows/checks.yml)。新机首先允许正常获取锁定依赖；缓存未齐时不要照搬旧主机的 `--offline`。

```sh
rustc --version
cargo --version
python3 --version
cargo fetch --locked
cargo test --locked --no-default-features --all-targets
cargo build --locked --release --no-default-features --bin ciallogcat-cli
```

随后按 [贡献指南](../CONTRIBUTING.md) 完成默认 GUI、Clippy、Python 和生成代码检查。Linux 原生体验需要 Xvfb、X11/XTest/XRes、bubblewrap、用户命名空间及系统 Python GTK；按 [体验工具](../tools/experience-lab/README.md) 在新主机重新准备，不复制旧主机下载的运行库。命名空间不可用时如实记录受阻，仍可运行无设备核心测试。

没有设备时继续使用 [fake ADB](adb-testing.md) 与 [Agent 示例](../examples/README.md)。不要复用旧主机的配置绝对路径、签名密钥或活动 session cursor；迁移文件可在新环境重新查询取得引用。需要旧 session 状态/游标时必须另行明确其绑定路径及配置，不能假设可移植。

## 可选证据

历史截图、原始日志、性能输入和审查 PoC 位于旧主机被忽略的 `target/`，不包含在源码包。关键结论、复现与限制已写入 TODO 和验证摘要；新主机可重新生成。若要保留原始审计材料，只另行拷贝所需审查/体验目录，避免整个 target 的编译缓存、旧二进制与本地配置混入源包。不要迁移 ADB 私钥、令牌或用户环境文件。
