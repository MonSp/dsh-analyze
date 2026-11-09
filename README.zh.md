# dsh-analyze

[English](README.md) | [中文](README.zh.md)

**面向 [DSH](https://github.com/deepseek-ai/deepseek-harness) 的本地数据分析工具。**  
CSV/JSON/JSONL/Parquet/XLSX 入库一次，大表留在磁盘，模型通过小 JSON 工具调用完成分析——而不是把整表塞进上下文。

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
4. **默认离线**——单一 Rust 二进制；无云、无驱动。

模型像分析师一样推进（schema → 样例 → 统计 → 分组聚合 → 连接），而全表
不必进入上下文；每一步都是宿主可记录、可约束的有界工具调用。

## 定位（是 / 不是）

| 是 | 不是 |
|---|---|
| 本地**文件** dataset 句柄（CSV / JSON / JSONL / Parquet / XLSX） | SQL 数据库连接器 |
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

运行期完全离线——无网络调用、无数据库驱动。

## 工具清单

| 工具 | 用途 |
|---|---|
| `dataset_ingest` | 工作区 CSV/JSON/JSONL/Parquet/XLSX 入库 → `datasetId` + schema 摘要 |
| `dataset_head` | 最多 20 行样例 |
| `dataset_schema` | 列名、dtype、行数 |
| `dataset_list` | 本工作区全部 dataset |
| `dataset_export` | 全表导出为 outputs/ 下 CSV/XLSX/Parquet |
| `dataset_rm` | 永久删除 dataset |
| `dataset_verify` | 校验已存 dataset：数据文件、rowCount、父链、源文件校验和 |
| `stats_describe` | 空值、min/max/mean/std/分位数 |
| `transform_agg` | 过滤 / 窗口 / 派生列 / 投影 / 分组聚合（含时间桶） |
| `dataset_values` | 单列取值分布（`nunique`、`top`） |
| `dataset_sample` | 确定性随机或分层抽样（≤ 20 行） |
| `dataset_join` | 等值连接 → 新 dataset（只返回 id 与计数） |
| `dataset_profile` | 质量画像：空值率、nunique、类型稳定性、疑似主键 |
| `dataset_dedup` | 精确或键控重复检测（计数 + 样例组） |
| `dataset_outliers` | IQR / z-score / MAD 异常值；可选写出异常 dataset |
| `dataset_corr` | 数值列两两相关（Pearson/Spearman，按 \|r\| 取 top pairs，带 pValue；可选 `by` 分组） |
| `dataset_crosstab` | 列联表（可选第三维）：top 类别、矩阵、边际、top cells |
| `dataset_series` | 时序变换（lag/lead/diff/pct_change/rolling）→ 新 dataset |
| `dataset_union` | 两个 dataset 追加合并（strict / intersect 列策略）→ 新 dataset |
| `dataset_diff` | 两个 dataset 键控对比：新增 / 删除 / 变更（+ 样例） |
| `dataset_pivot` | 长表 → 宽表透视（index × columns → values，带 agg）→ 新 dataset |

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

### 时序工具（v0.1.5）

- **`dataset_series`**：单列 `orderBy`（null 排最后）；一次调用可做 `lag` / `lead` / `diff` / `pct_change` / `rolling`（mean|sum|min|max）。
- **必写新 dataset**；响应只有 id 与 schema——用 `dataset_head` / `stats_describe` 继续查看。

### 集合与透视工具（v0.1.7）

- **`dataset_union`**：`columnsPolicy: strict|intersect`——按匹配列追加右表行；`strict` 要求表头完全一致。
- **`dataset_diff`**：可选 `on` 键；一次调用报告新增 / 删除 / 变更计数与键集样例（上限 20）。
- **`dataset_pivot`**：`index` / `columns` / `values` / `agg`——长表转宽表；写出新 dataset。

### 窗口运算与 Parquet 入库（v0.1.8）

- **`transform_agg` `window`**：`rank` / `row_number` / `cumsum` / `ewm`——每项新增一列、行数不变（0..=16 个）。
- **`dataset_ingest` `--format parquet`**：列类型映射为 dtype；映射外类型（Decimal、Time 等）以稳定文案拒绝。

### 导出格式（v0.1.9）

- **`dataset_export` `--format`**：`csv`（默认）| `xlsx`（表头 + 真实单元格类型）| `parquet`（可经 parquet ingest 回读）。

### corr pValue 与 crosstab 第三维（v0.1.10）

- **`dataset_corr`**：每对变量除 `r` / `df` / `n` 外还给出 `pValue`（双侧 Student-t）。
- **`dataset_crosstab`**：可选 `third` 列，按第三维类别返回二维切片（`tables[]`）。

### 时区分组与 CSV 编码（v0.1.11）

- **时间 `groupBy` 的 `tz`**：`{"dateColumn":…,"period":"day|week|month","tz":"+08:00"|"Asia/Shanghai"}`
  ——`day` / `week`（周一）/ `month` 的日界按目标时区日历计算。缺省仍为 UTC；
  date-only 值不受影响。非法 `tz` → `analysis/invalid-args`。
- **`dataset_ingest` `--encoding`**：CSV 编码 `utf-8`（默认）| `gbk` | `gb18030` |
  `auto`——Excel 导出的 GBK 文件直接入库；`auto` 回落 GB18030 时挂
  `encoding-fallback` 警告。解码永不有损。

### XLSX 入库、Parquet 存储与 verify（v0.1.12）

- **`dataset_ingest --format xlsx`**：默认读第一张 sheet，`sheet` 参数精确
  名字优先于纯数字索引（`2024` 这类表名可直接选中）；多 sheet 工作簿读一张
  + `sheet-skipped` 警告；`.xlsm` → exit 4（另存为 `.xlsx`）。
- **存储改为 Parquet**：新写入 `{id}.parquet`；旧 `{id}.csv` 仍可读；
  `dataset_rm` 两者一起删。
- **数据溯源**：ingest 记录 `sourceChecksum`（sha256），派生数据集记录
  `parentDatasetIds`；`dataset_verify` 检查数据文件、rowCount、父链，
  加 `checkSource` 时重算源文件校验和。
- **`dataset_corr` `by`**：按组两两相关，每对带 `by` 组键（上限 50 组 /
  100 对，超限挂截断警告）。
- **`transform_agg` 预览**：无论有无 `outLabel`，始终返回最多 **20** 行
  内联结果（`min(20, rowCount)`）。

### `transform_agg` 扩展（v0.1.2）

- **`mutate`**：白名单表达式派生列——标识符、数字、`+ - * /`、括号，以及
  类型转换 `to_int(e)` / `to_str(e)` / `to_date(e, fmt)`。禁止任意函数调用；
  非法表达式 → `analysis/invalid-args`。
- **时间 `groupBy`**：`{"dateColumn":"created_at","period":"day|week|month"}`
  → 列名 `created_at_day` 等，可选 `tz`（v0.1.11，见上）。无法解析的非空值
  进入 `null` 桶并计入 `parseFailCount`（不会静默丢弃）。
- **`having` / `topKPerGroup` / `select` 改名**（v0.1.7）：聚合后对结果列过滤、
  每组保留 ≤ k 行、输出列改名。

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
| 输入 | 本地 CSV/JSON/JSONL/Parquet/XLSX | 在线库 | 任意 |
| 网络 | 从不 | 必需 | 常见 |
| 模型 token 成本 | 每步小 JSON | 查询结果 | 内核输出 |
| 运行时 | 单一静态二进制 | 驱动 + 服务 | 完整解释器 |

## 安全

路径围栏、凭据路径拒绝、env 白名单与硬限额——见
[SECURITY.md](SECURITY.md) 与 [docs/security.md](docs/security.md)。

## 许可证

MIT — 见 [LICENSE](LICENSE)。
