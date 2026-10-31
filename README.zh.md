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

## 为什么 DSH 需要本地计算引擎

[DSH](https://github.com/deepseek-ai/deepseek-harness) 上的 agent 靠
**工具调用**推进推理，而不是 notebook。真实分析会话往往从工作区里的一个
文件开始（导出表、日志、CSV dump），体积远超上下文窗口——但模型仍需要
schema、分布、过滤、连接和聚合才能回答用户。

没有专用计算引擎时，宿主通常会掉进三种坑里：

| 坑 | 后果 |
|---|---|
| **整表贴进上下文** | 几 MB 的表冲爆窗口、答案被截断，还把模型根本不需要全文的数据烧成 token。 |
| **无边界的 shell + Python** | agent 里跑 `pandas` 能用一阵就炸：没有行数/字节上限、没有路径沙箱、没有稳定 JSON 契约，也难审计加固。 |
| **完整 SQL / 数仓** | 适合在线多租户数据；不适合“本会话工作区里就有一个本地文件、零网络”的场景。 |

dsh-analyze 正是 DSH 会话缺的中间层：

1. **磁盘上的 dataset 存储**——入库一次，表落在
   `<workspace>/.dsh-analyze/`，用 `datasetId` 引用。
2. **小 JSON 工具**——head / describe / values / sample / agg / join
   只回传很小的结构化结果（样例 ≤ 20 行；聚合受 limit 约束）。
3. **硬且显式的限额**——字节/行上限、路径围栏、白名单表达式（禁止任意
   代码）、插件探活失败 fail-soft 不注册。
4. **默认离线**——纯标准库 Rust 二进制；无云、无驱动。

模型像分析师一样推进（schema → 样例 → 统计 → 分组聚合 → 连接），而全表
不必进入上下文；每一步都是宿主可记录、可约束的有界工具调用。

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
| `transform_agg` | 过滤 / 派生列 / 投影 / 分组聚合（含时间桶） |
| `dataset_values` | 单列取值分布（`nunique`、`top`） |
| `dataset_sample` | 确定性随机或分层抽样（≤ 20 行） |
| `dataset_join` | 等值连接 → 新 dataset（只返回 id 与计数） |
| `dataset_profile` | 质量画像：空值率、nunique、类型稳定性、疑似主键 |
| `dataset_dedup` | 精确或键控重复检测（计数 + 样例组） |
| `dataset_outliers` | IQR / z-score / MAD 异常值；可选写出异常 dataset |
| `dataset_corr` | 数值列两两相关（Pearson/Spearman，按 \|r\| 取 top pairs） |
| `dataset_crosstab` | 两列列联表：top 类别、矩阵、边际、top cells |

CLI 子命令与工具同名（`values` / `sample` / `join` / `profile` 等）。
冻结 JSON 契约见 [docs/interface.md](docs/interface.md)。

### 质量工具（v0.1.3）

- **`dataset_profile`**：一次调用回答「这张表能不能算」。
- **`dataset_dedup`**：不传 `keyColumns` 为整行精确去重；传键做导出式碰撞检查。
- **`dataset_outliers`**：`method: iqr|zscore|mad`；`outLabel` 写出新 dataset，绝不整表回传异常行。
- **`stats_describe`**：数值列额外返回 `q01` / `q10` / `q90` / `q99`。

### 关系工具（v0.1.4）

- **`dataset_corr`**：2–16 个数值列；`method: pearson|spearman`；pairs 按 \|r\| 排序（上限 50）。
- **`dataset_crosstab`**：`rows` × `cols` 频次表，仅回 top 类别与 top cells。
- **`dataset_join`**：`how` 新增 `anti` \| `semi`（仅保留左表列；null 键永不匹配）。

### `transform_agg` 扩展（v0.1.2）

- **`mutate`**：白名单表达式派生列（标识符、数字、`+ - * /`、括号）。
  禁止函数调用；非法表达式 → `analysis/invalid-args`。
- **时间 `groupBy`**：`{"dateColumn":"created_at","period":"day|week|month"}`
  → 列名 `created_at_day` 等。无法解析的非空值进入 `null` 桶并计入
  `parseFailCount`（不会静默丢弃）。

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
| `DSH_ANALYZE_DEFAULT_SAMPLE_SEED` | 42 | 未指定 seed 时的抽样种子 |

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
