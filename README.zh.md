# dsh-analyze

[English](README.md) | [中文](README.zh.md)

**面向 [DSH](https://github.com/deepseek-ai/deepseek-harness) 的本地数据分析工具。**  
文件入库一次，大表留在磁盘，模型通过小 JSON 工具调用完成分析——而不是把整表塞进上下文。

```text
没有 dsh-analyze              有 dsh-analyze
─────────────────────        ─────────────────────────────
整表粘贴 → 烧 token           dataset_ingest  → { datasetId }
上下文爆炸                    dataset_head    → 5 行样例
无审计痕迹                    stats_describe  → 列统计
                             transform_agg   → 聚合结果
                             （全表留在磁盘）
```

## 定位（是 / 不是）

| 是 | 不是 |
|---|---|
| 本地**文件** dataset 句柄（CSV / JSONL） | SQL 数据库连接器 |
| 离线 Rust CLI + DSH 工具插件 | Python/R 科学计算栈 |
| 会话工作区沙箱 + 硬限额 | 多租户云服务 |

## 安装

```bash
dsh plugin --profile web add dsh-analyze
```

将 `dsh-analyze` 二进制放入 `PATH`，或设置：

```bash
export DSH_ANALYZE_CLI_PATH=/path/to/dsh-analyze
```

### 预编译二进制

从 GitHub Releases 下载，并用 `SHA256SUMS` 校验：

| 目标平台 | 产物 |
|---|---|
| `x86_64-pc-windows-msvc` | `dsh-analyze-x86_64-pc-windows-msvc.zip` |
| `aarch64-apple-darwin` | `dsh-analyze-aarch64-apple-darwin.tar.gz` |
| `x86_64-apple-darwin` | `dsh-analyze-x86_64-apple-darwin.tar.gz` |
| `x86_64-unknown-linux-gnu` | `dsh-analyze-x86_64-unknown-linux-gnu.tar.gz` |

CLI 查找顺序：`DSH_ANALYZE_CLI_PATH` → 旧名 `INV_ANALYSIS_CLI_PATH` →
`PATH` 上的 `dsh-analyze` → 找不到则记日志且**不注册**工具。

### 源码构建

```bash
cargo build --release -p dsh-analyze
```

crate 为纯标准库实现，可离线构建。

## 工具清单

| 工具 | 用途 |
|---|---|
| `dataset_ingest` | 工作区 CSV/JSONL 入库 → `datasetId` + schema 摘要 |
| `dataset_head` | 最多 20 行样例 |
| `dataset_schema` | 列名、dtype、行数 |
| `dataset_list` | 本工作区全部 dataset |
| `dataset_export` | 全表导出为 outputs/ 下 CSV |
| `dataset_rm` | 永久删除 dataset |
| `stats_describe` | 空值、min/max/mean/std/分位数 |
| `transform_agg` | 过滤 / 投影 / 分组聚合 |

## 限额与数据根

```text
<workspace>/.dsh-analyze/{datasets,outputs,tmp}/
```

| 环境变量 | 默认 | 含义 |
|---|---|---|
| `DSH_ANALYZE_CLI_PATH` | — | CLI 可执行文件 |
| `DSH_ANALYZE_WORKSPACE` | `process.cwd()` | 会话工作区根 |
| `DSH_ANALYZE_MAX_ROWS` | 5,000,000 | ingest 行上限 |
| `DSH_ANALYZE_MAX_INGEST_BYTES` | 256 MiB | ingest 字节上限 |
| `DSH_ANALYZE_DEFAULT_ROW_LIMIT` | 1000 | agg 默认行限 |

兼容期仍可读旧名 `INV_ANALYSIS_*`（新名优先）。

回传模型的结果刻意保持小体积（样例 ≤ 20 行）。全量数据走
`outLabel` / `dataset_export`，留在磁盘。

## 与社区方案对比

| | dsh-analyze | SQL 连接器 | Notebook 栈（Python/R） |
|---|---|---|---|
| 输入 | 本地 CSV/JSONL | 在线库 | 任意 |
| 网络 | 从不 | 必需 | 常见 |
| 模型 token 成本 | 每步小 JSON | 查询结果 | 内核输出 |
| 运行时 | 单一静态二进制 | 驱动 + 服务 | 完整解释器 |

## 安全

路径围栏、凭据路径拒绝、env 白名单与硬限额——见
[SECURITY.md](SECURITY.md) 与 [docs/security.md](docs/security.md)。

## 许可证

MIT — 见 [LICENSE](LICENSE)。
