#!/usr/bin/env node
/**
 * Mock dsh-analyze CLI for unit tests (no Rust toolchain required).
 * Implements T0 JSON protocol subset for CSV files.
 * Usage mirrors: node mock-analyze-cli.mjs --workspace <dir> <cmd> ...
 */
import { createReadStream, createWriteStream, existsSync, mkdirSync, readFileSync, writeFileSync, statSync, readdirSync, unlinkSync } from 'node:fs'
import { createInterface } from 'node:readline'
import { dirname, join, resolve, sep } from 'node:path'
import { randomBytes } from 'node:crypto'

function fail(code, msg) {
  process.stderr.write(msg + '\n')
  process.exit(code)
}

function json(s) {
  return JSON.stringify(s)
}

function within(ws, p) {
  const abs = p.startsWith('/') || /^[A-Za-z]:[\\/]/.test(p) ? resolve(p) : resolve(ws, p)
  const root = resolve(ws)
  if (abs !== root && !abs.startsWith(root + sep)) fail(2, 'analysis/invalid-path: path escapes workspace')
  return abs
}

function newId() {
  return 'ds_' + randomBytes(6).toString('hex')
}

function analysisDirs(ws) {
  const root = join(ws, '.dsh-analyze')
  const datasets = join(root, 'datasets')
  const outputs = join(root, 'outputs')
  mkdirSync(datasets, { recursive: true })
  mkdirSync(outputs, { recursive: true })
  return { root, datasets, outputs }
}

function parseCsv(text) {
  // Strip UTF-8 BOM so the first column name stays clean (parity with Rust CLI).
  const body = text.startsWith('\uFEFF') ? text.slice(1) : text
  const lines = body.split(/\r?\n/).filter(l => l.length > 0)
  if (lines.length === 0) fail(1, 'analysis/cli-failed: empty csv')
  const headers = lines[0].split(',').map(s => s.trim())
  const rows = []
  for (let i = 1; i < lines.length; i++) {
    const cells = lines[i].split(',')
    const row = headers.map((_, j) => {
      const raw = (cells[j] ?? '').trim()
      if (raw === '' || raw === 'null' || raw === 'NA') return null
      if (raw === 'true') return true
      if (raw === 'false') return false
      if (/^-?\d+$/.test(raw)) return Number(raw)
      if (/^-?\d+\.\d+$/.test(raw)) return Number(raw)
      return raw
    })
    rows.push(row)
  }
  return { headers, rows }
}

function inferDtype(values) {
  let f = false, i = false, s = false
  for (const v of values) {
    if (v === null || typeof v === 'boolean') continue
    if (typeof v === 'number') {
      if (!Number.isInteger(v)) f = true
      else i = true
    } else s = true
  }
  if (s) return 'utf8'
  if (f) return 'f64'
  if (i) return 'i64'
  return 'utf8'
}

function loadDataset(ws, id) {
  if (!/^ds_[a-f0-9]{8,32}$/.test(id)) fail(2, 'analysis/invalid-args: bad datasetId')
  const { datasets } = analysisDirs(ws)
  const metaPath = join(datasets, `${id}.meta.json`)
  const csvPath = join(datasets, `${id}.csv`)
  if (!existsSync(metaPath) || !existsSync(csvPath)) fail(5, 'analysis/unknown-dataset')
  const meta = JSON.parse(readFileSync(metaPath, 'utf8'))
  const text = readFileSync(csvPath, 'utf8')
  const table = parseCsv(text)
  return { meta, table }
}

function writeDataset(ws, id, table, metaExtra) {
  const { datasets } = analysisDirs(ws)
  const csvPath = join(datasets, `${id}.csv`)
  const header = table.headers.join(',')
  const lines = [header]
  for (const row of table.rows) {
    lines.push(row.map(c => (c === null || c === undefined ? '' : String(c))).join(','))
  }
  writeFileSync(csvPath, lines.join('\n') + '\n', 'utf8')
  const columns = table.headers.map((name, idx) => ({
    name,
    dtype: inferDtype(table.rows.map(r => r[idx])),
  }))
  const meta = {
    version: 1,
    datasetId: id,
    rowCount: table.rows.length,
    columnCount: columns.length,
    columns,
    sourceKind: metaExtra.sourceKind,
    sourcePath: metaExtra.sourcePath,
    label: metaExtra.label ?? null,
    producedBy: metaExtra.producedBy,
    parentDatasetId: metaExtra.parentDatasetId ?? null,
  }
  writeFileSync(join(datasets, `${id}.meta.json`), JSON.stringify(meta) + '\n', 'utf8')
  return { meta, bytes: statSync(csvPath).size }
}

function getFlag(args, name) {
  const i = args.indexOf(name)
  return i >= 0 ? args[i + 1] : undefined
}

function parseSampleLimit(raw, defaultN) {
  if (raw === undefined) return defaultN
  const n = Number(raw)
  if (!Number.isInteger(n) || n < 1) {
    fail(2, 'analysis/invalid-args: --sample-limit must be integer 1..=20')
  }
  return Math.min(n, 20)
}

function envU64(key, legacyKey, fallback) {
  const v = process.env[key] ?? process.env[legacyKey]
  if (v === undefined || v === '') return fallback
  const n = Number(v)
  return Number.isFinite(n) && n >= 0 ? Math.floor(n) : fallback
}

async function main() {
  const argv = process.argv.slice(2)
  let i = 0
  let ws
  while (i < argv.length) {
    if (argv[i] === '--workspace') {
      ws = argv[++i]
      i++
      continue
    }
    break
  }
  const cmd = argv[i]
  if (cmd === 'version' || cmd === '--version') {
    console.log(json({
      ok: true,
      version: '0.1.4-mock',
      capabilities: ['ingest', 'head', 'schema', 'describe', 'agg', 'list', 'export', 'rm', 'values', 'sample', 'join', 'profile', 'dedup', 'outliers', 'corr', 'crosstab'],
    }))
    return
  }
  if (!ws) fail(2, 'analysis/invalid-args: --workspace required')
  ws = resolve(ws)
  const rest = argv.slice(i + 1)

  if (cmd === 'ingest') {
    const input = getFlag(rest, '--input')
    if (!input) fail(2, 'analysis/invalid-args: --input required')
    const path = within(ws, input)
    if (!existsSync(path)) fail(2, 'analysis/invalid-path: not a file')
    const format = getFlag(rest, '--format') || (path.endsWith('.jsonl') ? 'jsonl' : 'csv')
    if (format === 'parquet') fail(4, 'analysis/limit: parquet not enabled in T0 (use csv/jsonl)')
    if (format === 'jsonl') fail(4, 'analysis/limit: mock CLI supports csv only in T0 tests')
    const id = getFlag(rest, '--dataset-id') || newId()
    const text = readFileSync(path, 'utf8')
    const table = parseCsv(text)
    const label = getFlag(rest, '--label')
    const { meta, bytes } = writeDataset(ws, id, table, {
      sourceKind: 'csv',
      sourcePath: input,
      label,
      producedBy: 'ingest',
    })
    console.log(json({
      ok: true,
      datasetId: meta.datasetId,
      rowCount: meta.rowCount,
      columnCount: meta.columnCount,
      columns: meta.columns,
      bytesOnDisk: bytes,
    }))
    return
  }

  if (cmd === 'head') {
    const id = getFlag(rest, '--dataset-id')
    const n = Math.min(20, Math.max(1, Number(getFlag(rest, '--n') || 5)))
    const { meta, table } = loadDataset(ws, id)
    const rows = table.rows.slice(0, n).map(r => r.map(c => (c === null ? null : c)))
    console.log(json({
      ok: true,
      datasetId: id,
      rowCount: meta.rowCount,
      columns: table.headers,
      rows,
    }))
    return
  }

  if (cmd === 'schema') {
    const id = getFlag(rest, '--dataset-id')
    const { meta } = loadDataset(ws, id)
    console.log(json({
      ok: true,
      datasetId: id,
      rowCount: meta.rowCount,
      columns: meta.columns,
    }))
    return
  }

  if (cmd === 'describe') {
    const id = getFlag(rest, '--dataset-id')
    const colsFilter = getFlag(rest, '--columns')
    const filter = colsFilter ? colsFilter.split(',').map(s => s.trim()) : null
    const { meta, table } = loadDataset(ws, id)
    const stats = []
    for (let c = 0; c < table.headers.length; c++) {
      const name = table.headers[c]
      if (filter && !filter.includes(name)) continue
      let nulls = 0
      const nums = []
      for (const row of table.rows) {
        const v = row[c]
        if (v === null || v === undefined) nulls++
        else if (typeof v === 'number') nums.push(v)
      }
      let min = null, max = null, mean = null, std = null, q25 = null, q50 = null, q75 = null
      let q01 = null, q10 = null, q90 = null, q99 = null
      if (nums.length) {
        const s = [...nums].sort((a, b) => a - b)
        min = s[0]
        max = s[s.length - 1]
        mean = s.reduce((a, b) => a + b, 0) / s.length
        if (s.length > 1) {
          const v = s.reduce((a, b) => a + (b - mean) ** 2, 0) / (s.length - 1)
          std = Math.sqrt(v)
        }
        const pct = p => s[Math.min(s.length - 1, Math.round((s.length - 1) * p))]
        q25 = pct(0.25); q50 = pct(0.5); q75 = pct(0.75)
        q01 = pct(0.01); q10 = pct(0.10); q90 = pct(0.90); q99 = pct(0.99)
      }
      stats.push({
        name,
        dtype: meta.columns.find(x => x.name === name)?.dtype ?? 'utf8',
        nullCount: nulls,
        min, max, mean, std, q25, q50, q75, q01, q10, q90, q99,
      })
    }
    console.log(json({ ok: true, datasetId: id, stats }))
    return
  }

  if (cmd === 'agg') {
    const id = getFlag(rest, '--dataset-id')
    const specRaw = getFlag(rest, '--spec')
    const outLabel = getFlag(rest, '--out')
    if (!specRaw) fail(2, 'analysis/invalid-args: --spec required')
    let spec
    try { spec = JSON.parse(specRaw) } catch { fail(2, 'analysis/invalid-args: bad spec json') }
    const { table } = loadDataset(ws, id)
    const idx = name => {
      const i = table.headers.indexOf(name)
      if (i < 0) fail(2, `analysis/invalid-args: unknown column ${name}`)
      return i
    }
    let keep = table.rows.map((_, i) => i)
    for (const f of spec.filter || []) {
      const ci = idx(f.column)
      keep = keep.filter(i => {
        const cell = table.rows[i][ci]
        switch (f.op) {
          case 'is_null': return cell === null || cell === undefined
          case 'not_null': return !(cell === null || cell === undefined)
          case 'eq': return cell == f.value
          case 'ne': return cell != f.value
          case 'gt': return Number(cell) > Number(f.value)
          case 'gte': return Number(cell) >= Number(f.value)
          case 'lt': return Number(cell) < Number(f.value)
          case 'lte': return Number(cell) <= Number(f.value)
          case 'contains': return String(cell ?? '').includes(String(f.value))
          default: fail(2, `analysis/invalid-args: bad op ${f.op}`)
        }
      })
    }

    // Working table after filter + mutate
    let workHeaders = [...table.headers]
    let workRows = keep.map(i => [...table.rows[i]])
    const evalExpr = (expr, row, colIndex) => {
      // tiny whitelist evaluator for mock tests
      if (!/^[A-Za-z0-9_\s+\-*/().]+$/.test(expr)) fail(2, 'analysis/invalid-args: illegal mutate expr')
      if (/[A-Za-z_]\w*\s*\(/.test(expr)) fail(2, 'analysis/invalid-args: function calls not allowed')
      const tokens = expr.match(/[A-Za-z_]\w*|\d+\.?\d*|[+\-*/()]/g) || []
      let pos = 0
      const peek = () => tokens[pos]
      const next = () => tokens[pos++]
      const primary = () => {
        const t = next()
        if (t === undefined) fail(2, 'analysis/invalid-args: bad expr')
        if (t === '(') {
          const v = add()
          if (next() !== ')') fail(2, 'analysis/invalid-args: expected )')
          return v
        }
        if (/^\d/.test(t)) return Number(t)
        if (/^[A-Za-z_]/.test(t)) {
          const ci = colIndex[t]
          if (ci === undefined) fail(2, `analysis/invalid-args: unknown column in expr: ${t}`)
          return row[ci]
        }
        if (t === '-') return -primary()
        fail(2, `analysis/invalid-args: bad token ${t}`)
      }
      const mul = () => {
        let v = primary()
        while (peek() === '*' || peek() === '/') {
          const op = next()
          const r = primary()
          const a = Number(v), b = Number(r)
          if (!Number.isFinite(a) || !Number.isFinite(b)) return null
          v = op === '*' ? a * b : (b === 0 ? null : a / b)
        }
        return v
      }
      const add = () => {
        let v = mul()
        while (peek() === '+' || peek() === '-') {
          const op = next()
          const r = mul()
          const a = Number(v), b = Number(r)
          if (!Number.isFinite(a) || !Number.isFinite(b)) return null
          v = op === '+' ? a + b : a - b
        }
        return v
      }
      return add()
    }
    if (Array.isArray(spec.mutate)) {
      for (const m of spec.mutate) {
        if (!m || typeof m.as !== 'string' || typeof m.expr !== 'string') {
          fail(2, 'analysis/invalid-args: mutate needs as+expr')
        }
        if (workHeaders.includes(m.as)) fail(2, `analysis/invalid-args: mutate column collides: ${m.as}`)
        const colIndex = Object.fromEntries(workHeaders.map((h, i) => [h, i]))
        for (const row of workRows) {
          row.push(evalExpr(m.expr, row, colIndex))
        }
        workHeaders.push(m.as)
      }
    }
    const work = { headers: workHeaders, rows: workRows }
    const widx = name => {
      const i = work.headers.indexOf(name)
      if (i < 0) fail(2, `analysis/invalid-args: unknown column ${name}`)
      return i
    }

    const limit = Math.min(Number(spec.limit || 1000), 5000)
    const groupBy = spec.groupBy || []
    const aggs = spec.aggs || []
    let parseFailCount = 0
    let outHeaders, outRows
    if (!groupBy.length && !aggs.length) {
      const select = spec.select || work.headers
      const idxs = select.map(widx)
      outHeaders = select
      outRows = work.rows.slice(0, limit).map(r => idxs.map(ci => r[ci]))
    } else {
      const gKeys = groupBy.map(g => {
        if (typeof g === 'string') return { kind: 'col', name: g, ci: widx(g) }
        if (g && g.dateColumn && g.period) {
          if (!['day', 'week', 'month'].includes(g.period)) fail(2, `analysis/invalid-args: bad period ${g.period}`)
          return { kind: 'time', name: `${g.dateColumn}_${g.period}`, ci: widx(g.dateColumn), period: g.period }
        }
        fail(2, 'analysis/invalid-args: bad groupBy entry')
      })
      const parseTs = (s, period) => {
        const t = String(s).trim()
        const m = t.match(/^(\d{4})-(\d{2})-(\d{2})(?:[T ](\d{2}):(\d{2}):(\d{2}))?(?:Z|z|[+-]\d{2}:?\d{2})?$/)
        if (!m) return null
        let y = +m[1], mo = +m[2], d = +m[3]
        if (m[4] !== undefined) {
          // apply crude offset if present (UTC default)
          const off = t.match(/([+-])(\d{2}):?(\d{2})$/)
          if (off) {
            let mins = (+m[4]) * 60 + (+m[5]) + (off[1] === '-' ? -1 : 1) * ((+off[2]) * 60 + (+off[3]))
            // shift day only for simplicity
            const dayDelta = Math.floor(mins / 1440)
            // normalize via Date UTC
            const dt = new Date(Date.UTC(y, mo - 1, d, +m[4], +m[5]))
            // subtract offset minutes to get UTC
            dt.setUTCMinutes(dt.getUTCMinutes() - (off[1] === '-' ? -1 : 1) * ((+off[2]) * 60 + (+off[3])))
            y = dt.getUTCFullYear(); mo = dt.getUTCMonth() + 1; d = dt.getUTCDate()
            void dayDelta
          }
        }
        if (period === 'month') return `${String(y).padStart(4, '0')}-${String(mo).padStart(2, '0')}`
        const dt = new Date(Date.UTC(y, mo - 1, d))
        if (period === 'day') {
          return `${dt.getUTCFullYear()}-${String(dt.getUTCMonth() + 1).padStart(2, '0')}-${String(dt.getUTCDate()).padStart(2, '0')}`
        }
        // week: Monday
        const dow = (dt.getUTCDay() + 6) % 7
        dt.setUTCDate(dt.getUTCDate() - dow)
        return `${dt.getUTCFullYear()}-${String(dt.getUTCMonth() + 1).padStart(2, '0')}-${String(dt.getUTCDate()).padStart(2, '0')}`
      }
      const groups = new Map()
      for (const row of work.rows) {
        const key = gKeys.map(g => {
          if (g.kind === 'col') return row[g.ci]
          const cell = row[g.ci]
          if (cell === null || cell === undefined || cell === '') return null
          const b = parseTs(cell, g.period)
          if (b === null) { parseFailCount++; return null }
          return b
        })
        const k = JSON.stringify(key)
        if (!groups.has(k)) groups.set(k, { key, members: [] })
        groups.get(k).members.push(row)
      }
      outHeaders = [...gKeys.map(g => g.name), ...aggs.map(a => a.as || (a.column ? `${a.column}_${a.fn}` : a.fn))]
      outRows = []
      for (const g of groups.values()) {
        const row = [...g.key]
        for (const a of aggs) {
          if (a.fn === 'count') {
            row.push(g.members.length)
            continue
          }
          const ci = widx(a.column)
          const nums = g.members.map(r => r[ci]).filter(v => typeof v === 'number')
          let v = null
          if (a.fn === 'sum') v = nums.reduce((x, y) => x + y, 0)
          else if (a.fn === 'mean') v = nums.length ? nums.reduce((x, y) => x + y, 0) / nums.length : null
          else if (a.fn === 'min') v = nums.length ? Math.min(...nums) : null
          else if (a.fn === 'max') v = nums.length ? Math.max(...nums) : null
          else if (a.fn === 'median') {
            if (!nums.length) v = null
            else {
              const s = [...nums].sort((a, b) => a - b)
              const m = Math.floor(s.length / 2)
              v = s.length % 2 ? s[m] : (s[m - 1] + s[m]) / 2
            }
          } else if (a.fn === 'std') {
            if (nums.length < 2) v = null
            else {
              const mean = nums.reduce((x, y) => x + y, 0) / nums.length
              v = Math.sqrt(nums.reduce((x, y) => x + (y - mean) ** 2, 0) / (nums.length - 1))
            }
          } else fail(2, `analysis/invalid-args: bad fn ${a.fn}`)
          row.push(v)
        }
        outRows.push(row)
      }
      if (spec.orderBy?.length) {
        const order = spec.orderBy.map(o => ({ i: outHeaders.indexOf(o.column), desc: !!o.desc }))
        outRows.sort((a, b) => {
          for (const o of order) {
            const av = a[o.i], bv = b[o.i]
            const cmp = (av > bv ? 1 : av < bv ? -1 : 0) * (o.desc ? -1 : 1)
            if (cmp) return cmp
          }
          return 0
        })
      }
      outRows = outRows.slice(0, limit)
    }
    let outDatasetId = null
    let truncated = false
    if (outLabel) {
      outDatasetId = newId()
      writeDataset(ws, outDatasetId, { headers: outHeaders, rows: outRows }, {
        sourceKind: 'csv',
        sourcePath: `agg:${id}`,
        label: outLabel,
        producedBy: 'agg',
        parentDatasetId: id,
      })
    }
    const includeRows = !outDatasetId && outRows.length <= 20
    truncated = !includeRows
    console.log(json({
      ok: true,
      datasetId: id,
      outDatasetId,
      rowCount: outRows.length,
      columns: outHeaders,
      rows: includeRows ? outRows : [],
      truncated,
      ...(parseFailCount > 0 ? { parseFailCount } : {}),
    }))
    return
  }

  if (cmd === 'values') {
    const id = getFlag(rest, '--dataset-id')
    const column = getFlag(rest, '--column')
    const limit = Math.min(100, Math.max(1, Number(getFlag(rest, '--limit') || 20)))
    if (!column) fail(2, 'analysis/invalid-args: --column required')
    const { meta, table } = loadDataset(ws, id)
    const ci = table.headers.indexOf(column)
    if (ci < 0) fail(2, `analysis/invalid-args: unknown column ${column}`)
    let nullCount = 0
    const counts = new Map()
    for (const row of table.rows) {
      const v = row[ci]
      if (v === null || v === undefined) nullCount++
      else counts.set(v, (counts.get(v) || 0) + 1)
    }
    const nonNull = Math.max(1, table.rows.length - nullCount)
    const top = [...counts.entries()]
      .map(([value, count]) => ({ value, count, ratio: count / nonNull }))
      .sort((a, b) => b.count - a.count || String(a.value).localeCompare(String(b.value)))
      .slice(0, limit)
    console.log(json({
      ok: true,
      datasetId: id,
      column,
      dtype: meta.columns.find(x => x.name === column)?.dtype ?? 'utf8',
      rowCount: meta.rowCount,
      nullCount,
      nunique: counts.size,
      limit,
      top,
    }))
    return
  }

  if (cmd === 'sample') {
    const id = getFlag(rest, '--dataset-id')
    const n = Math.min(20, Math.max(1, Number(getFlag(rest, '--n') || 10)))
    const seed = Number(getFlag(rest, '--seed') ?? 42)
    const stratifyBy = getFlag(rest, '--stratify-by')
    const { meta, table } = loadDataset(ws, id)
    const pop = table.rows.length
    // SplitMix64
    const nextU64 = (state) => {
      state.s = (state.s + 0x9e3779b97f4a7c15n) & 0xffffffffffffffffn
      let z = state.s
      z = ((z ^ (z >> 30n)) * 0xbf58476d1ce4e5b9n) & 0xffffffffffffffffn
      z = ((z ^ (z >> 27n)) * 0x94d049bb133111ebn) & 0xffffffffffffffffn
      return z ^ (z >> 31n)
    }
    const sampleFrom = (indices, take, seedNum) => {
      if (take >= indices.length) return [...indices]
      const idx = [...indices]
      const state = { s: BigInt(seedNum) & 0xffffffffffffffffn }
      for (let i = 0; i < take; i++) {
        const r = nextU64(state) % BigInt(idx.length - i)
        const j = i + Number(r)
        ;[idx[i], idx[j]] = [idx[j], idx[i]]
      }
      return idx.slice(0, take).sort((a, b) => a - b)
    }
    let indices
    let method = 'random'
    if (stratifyBy) {
      method = 'stratified'
      const si = table.headers.indexOf(stratifyBy)
      if (si < 0) fail(2, `analysis/invalid-args: unknown column ${stratifyBy}`)
      const groups = new Map()
      table.rows.forEach((row, i) => {
        const k = row[si] === null || row[si] === undefined ? '__null__' : String(row[si])
        if (!groups.has(k)) groups.set(k, [])
        groups.get(k).push(i)
      })
      indices = []
      let gs = seed
      for (const members of groups.values()) {
        const take = Math.min(members.length, Math.floor((members.length * n) / Math.max(1, pop)))
        gs = (gs * 0x9e3779b97f4a7c15 + members.length) >>> 0
        for (const p of sampleFrom(members, take, gs)) indices.push(p)
      }
      // top up if under-allocated
      if (indices.length < Math.min(n, pop)) {
        const chosen = new Set(indices)
        for (let i = 0; i < pop && indices.length < Math.min(n, pop); i++) {
          if (!chosen.has(i)) { indices.push(i); chosen.add(i) }
        }
      }
      indices.sort((a, b) => a - b)
      if (n >= pop) indices = Array.from({ length: pop }, (_, i) => i)
    } else {
      indices = sampleFrom(Array.from({ length: pop }, (_, i) => i), n, seed)
    }
    const rows = indices.map(i => table.rows[i])
    console.log(json({
      ok: true,
      datasetId: id,
      rowCount: meta.rowCount,
      columns: table.headers,
      rows,
      sampling: {
        method,
        n: indices.length,
        seed,
        stratifyBy: stratifyBy ?? null,
        population: pop,
      },
    }))
    return
  }

  if (cmd === 'join') {
    const leftId = getFlag(rest, '--left')
    const rightId = getFlag(rest, '--right')
    const onRaw = getFlag(rest, '--on')
    const how = getFlag(rest, '--how') || 'inner'
    const outLabel = getFlag(rest, '--out')
    if (!leftId || !rightId || !onRaw) fail(2, 'analysis/invalid-args: --left --right --on required')
    if (how !== 'inner' && how !== 'left' && how !== 'anti' && how !== 'semi') {
      fail(2, 'analysis/invalid-args: --how must be inner|left|anti|semi')
    }
    const leftOnly = how === 'anti' || how === 'semi'
    const on = onRaw.split(',').map(s => s.trim()).filter(Boolean)
    if (!on.length) fail(2, 'analysis/invalid-args: --on must list at least one column')
    const left = loadDataset(ws, leftId)
    const right = loadDataset(ws, rightId)
    const maxRows = Number(process.env.DSH_ANALYZE_MAX_ROWS || 5000000)
    const lKey = on.map(c => {
      const i = left.table.headers.indexOf(c)
      if (i < 0) fail(2, `analysis/invalid-args: unknown column ${c} on left`)
      return i
    })
    const rKey = on.map(c => {
      const i = right.table.headers.indexOf(c)
      if (i < 0) fail(2, `analysis/invalid-args: unknown column ${c} on right`)
      return i
    })
    const outHeaders = [...left.table.headers]
    const names = new Set(outHeaders)
    const rightPayload = []
    if (!leftOnly) {
      right.table.headers.forEach((h, i) => {
        if (rKey.includes(i)) return
        let name = h
        while (names.has(name)) name = name + '_r'
        names.add(name)
        outHeaders.push(name)
        rightPayload.push(i)
      })
    }
    const keyOf = (row, idxs) => idxs.map(i => row[i] === undefined ? null : row[i])
    const rightMap = new Map()
    right.table.rows.forEach((row, ri) => {
      const k = JSON.stringify(keyOf(row, rKey))
      if (k.includes('null')) return
      if (!rightMap.has(k)) rightMap.set(k, [])
      rightMap.get(k).push(ri)
    })
    const outRows = []
    for (const lrow of left.table.rows) {
      const k = keyOf(lrow, lKey)
      if (k.some(v => v === null)) {
        if (how === 'left') outRows.push([...lrow, ...rightPayload.map(() => null)])
        else if (how === 'anti') outRows.push([...lrow])
        if (outRows.length > maxRows) fail(3, 'analysis/limit: join result too large')
        continue
      }
      const matches = rightMap.get(JSON.stringify(k)) || []
      if (matches.length === 0) {
        if (how === 'left') outRows.push([...lrow, ...rightPayload.map(() => null)])
        else if (how === 'anti') outRows.push([...lrow])
        if (outRows.length > maxRows) fail(3, 'analysis/limit: join result too large')
        continue
      }
      if (how === 'semi') {
        outRows.push([...lrow])
        if (outRows.length > maxRows) fail(3, 'analysis/limit: join result too large')
        continue
      }
      if (how === 'anti') continue
      for (const ri of matches) {
        outRows.push([...lrow, ...rightPayload.map(ci => right.table.rows[ri][ci] ?? null)])
        if (outRows.length > maxRows) fail(3, 'analysis/limit: join result too large')
      }
    }
    const newIdVal = newId()
    writeDataset(ws, newIdVal, { headers: outHeaders, rows: outRows }, {
      sourceKind: 'join',
      sourcePath: `join:${leftId}+${rightId}`,
      label: outLabel,
      producedBy: 'join',
      parentDatasetId: leftId,
    })
    console.log(json({
      ok: true,
      datasetId: newIdVal,
      rowCount: outRows.length,
      columnCount: outHeaders.length,
      columns: outHeaders.map((name, idx) => ({
        name,
        dtype: inferDtype(outRows.map(r => r[idx])),
      })),
      leftDatasetId: leftId,
      rightDatasetId: rightId,
      how,
    }))
    return
  }

  if (cmd === 'list') {
    const { datasets } = analysisDirs(ws)
    const items = readdirSync(datasets)
      .filter(n => n.endsWith('.meta.json'))
      .sort()
      .map(n => {
        const meta = JSON.parse(readFileSync(join(datasets, n), 'utf8'))
        return {
          datasetId: meta.datasetId,
          rowCount: meta.rowCount,
          columnCount: meta.columnCount,
          label: meta.label ?? null,
          producedBy: meta.producedBy,
          parentDatasetId: meta.parentDatasetId ?? null,
          sourceKind: meta.sourceKind,
        }
      })
    console.log(json({ ok: true, datasets: items }))
    return
  }

  if (cmd === 'export') {
    const id = getFlag(rest, '--dataset-id')
    const output = getFlag(rest, '--output')
    if (!output) fail(2, 'analysis/invalid-args: --output required')
    const { meta, table } = loadDataset(ws, id)
    const { outputs } = analysisDirs(ws)
    let finalPath = within(ws, output)
    if (!finalPath.startsWith(outputs + sep) && finalPath !== outputs) {
      finalPath = join(outputs, finalPath.split(/[\\/]/).pop() || `${id}.csv`)
    }
    const header = table.headers.join(',')
    const lines = [header]
    for (const row of table.rows) {
      lines.push(row.map(c => (c === null || c === undefined ? '' : String(c))).join(','))
    }
    writeFileSync(finalPath, lines.join('\n') + '\n', 'utf8')
    const bytes = statSync(finalPath).size
    const rel = finalPath.startsWith(ws + sep) ? finalPath.slice(ws.length + 1) : finalPath
    console.log(json({ ok: true, outputPath: rel, rowCount: meta.rowCount, bytes }))
    return
  }

  if (cmd === 'rm') {
    const id = getFlag(rest, '--dataset-id')
    const { datasets } = analysisDirs(ws)
    const metaPath = join(datasets, `${id}.meta.json`)
    if (!existsSync(metaPath)) fail(5, 'analysis/unknown-dataset')
    unlinkSync(metaPath)
    try { unlinkSync(join(datasets, `${id}.csv`)) } catch {}
    console.log(json({ ok: true, datasetId: id, removed: true }))
    return
  }

  if (cmd === 'profile') {
    const id = getFlag(rest, '--dataset-id')
    if (!id) fail(2, 'analysis/invalid-args: --dataset-id required')
    const hcRaw = getFlag(rest, '--high-cardinality-threshold')
    let hc = 200
    if (hcRaw !== undefined) {
      hc = Number(hcRaw)
      if (!Number.isInteger(hc) || hc < 1) fail(2, 'analysis/invalid-args: --high-cardinality-threshold must be integer >= 1')
    }
    const { meta, table } = loadDataset(ws, id)
    const rowCount = table.rows.length
    const columns = []
    const warnings = []
    if (rowCount === 0) warnings.push({ code: 'empty_table' })
    for (let c = 0; c < table.headers.length; c++) {
      const name = table.headers[c]
      const dtype = meta.columns.find(x => x.name === name)?.dtype ?? 'utf8'
      let nulls = 0
      let numericCount = 0
      let boolCount = 0
      const seen = new Set()
      const samples = []
      let hasBool = false, hasI64 = false, hasF64 = false, hasStr = false
      let hasNumStr = false, hasNonNumStr = false
      for (const row of table.rows) {
        const v = row[c]
        if (v === null || v === undefined) { nulls++; continue }
        seen.add(JSON.stringify(v))
        if (samples.length < 3 && !samples.some(s => JSON.stringify(s) === JSON.stringify(v))) samples.push(v)
        if (typeof v === 'boolean') { boolCount++; hasBool = true }
        else if (typeof v === 'number') {
          numericCount++
          if (Number.isInteger(v)) hasI64 = true; else hasF64 = true
        } else {
          hasStr = true
          if (v.trim() !== '' && Number.isFinite(Number(v.trim()))) { numericCount++; hasNumStr = true }
          else hasNonNumStr = true
        }
      }
      const nonNull = rowCount - nulls
      const nunique = seen.size
      const highCardinality = nunique > hc
      let typeStability = 'empty'
      if (nonNull > 0) {
        if (hasStr && !hasBool && !hasI64 && !hasF64) typeStability = 'string'
        else if (hasBool && !hasStr && !hasI64 && !hasF64) typeStability = 'bool'
        else if ((hasI64 || hasF64 || hasNumStr) && (hasBool || hasNonNumStr)) typeStability = 'mixed_numeric'
        else if (hasBool && hasStr) typeStability = 'mixed'
        else if (hasI64 && !hasF64 && !hasStr) typeStability = 'int'
        else if (hasF64 || (hasI64 && hasStr)) typeStability = 'float'
        else typeStability = 'mixed'
      }
      const nullRate = rowCount > 0 ? nulls / rowCount : 0
      const uniqueRate = nonNull > 0 ? nunique / nonNull : 0
      const numericShare = rowCount > 0 ? numericCount / rowCount : 0
      columns.push({
        name,
        dtype,
        nullCount: nulls,
        nullRate: Number(nullRate.toFixed(6)),
        nunique,
        uniqueRate: Number(uniqueRate.toFixed(6)),
        highCardinality,
        typeStability,
        numericCount,
        numericShare: Number(numericShare.toFixed(6)),
        boolCount,
        sampleValues: samples,
      })
      if (rowCount > 0 && nulls === rowCount) warnings.push({ code: 'all_null', column: name })
      else if (nullRate >= 0.5) warnings.push({ code: 'mostly_null', column: name })
      if (highCardinality) warnings.push({ code: 'high_cardinality', column: name, nunique, threshold: hc })
      if (typeStability === 'mixed' || typeStability === 'mixed_numeric') warnings.push({ code: 'mixed_types', column: name })
    }
    const suspectKeys = []
    for (let c = 0; c < table.headers.length && suspectKeys.length < 10; c++) {
      const name = table.headers[c]
      let nulls = 0
      const seen = new Set()
      for (const row of table.rows) {
        const v = row[c]
        if (v === null || v === undefined) nulls++
        else seen.add(JSON.stringify(v))
      }
      if (rowCount > 0 && nulls === 0 && seen.size === rowCount) {
        suspectKeys.push({ columns: [name], reason: 'unique_non_null', nullCount: 0, nunique: rowCount })
      }
    }
    let warnOut = warnings
    if (warnOut.length > 50) {
      const total = warnOut.length
      warnOut = warnOut.slice(0, 49)
      warnOut.push({ code: 'warnings_truncated', emitted: 50, total })
    }
    console.log(json({ ok: true, datasetId: id, rowCount, columnCount: table.headers.length, columns, suspectKeys, warnings: warnOut }))
    return
  }

  if (cmd === 'dedup') {
    const id = getFlag(rest, '--dataset-id')
    if (!id) fail(2, 'analysis/invalid-args: --dataset-id required')
    const keyRaw = getFlag(rest, '--key-columns')
    const sampleLimit = parseSampleLimit(getFlag(rest, '--sample-limit'), 5)
    const { meta, table } = loadDataset(ws, id)
    const rowCount = table.rows.length
    let keyIdx = []
    if (keyRaw !== undefined) {
      keyIdx = keyRaw.split(',').map(s => s.trim()).filter(Boolean)
      if (!keyIdx.length) fail(2, 'analysis/invalid-args: --key-columns must list at least one column')
      if (keyIdx.length > 16) fail(2, 'analysis/invalid-args: --key-columns supports at most 16 columns')
      const uniq = new Set()
      keyIdx = keyIdx.map(name => {
        if (uniq.has(name)) fail(2, `analysis/invalid-args: duplicate key column ${name}`)
        uniq.add(name)
        const i = table.headers.indexOf(name)
        if (i < 0) fail(2, `analysis/invalid-args: unknown column ${name}`)
        return i
      })
    }
    const groups = new Map()
    for (let ri = 0; ri < table.rows.length; ri++) {
      const row = table.rows[ri]
      // Map keys must be primitives; always stringify the key payload.
      const key = keyIdx.length
        ? JSON.stringify(keyIdx.map(ci => row[ci] ?? null))
        : JSON.stringify(row.map(c => (c === undefined ? null : c)))
      let g = groups.get(key)
      if (!g) {
        g = {
          count: 0,
          first: ri,
          keyVals: keyIdx.length ? keyIdx.map(ci => row[ci] ?? null) : null,
          samples: [],
        }
        groups.set(key, g)
      }
      g.count++
      if (g.samples.length < 3) g.samples.push(ri)
    }
    const distinct = groups.size
    const duplicateRowCount = Math.max(0, rowCount - distinct)
    let maxMultiplicity = 1
    let duplicateKeyCount = 0
    const dupGroups = []
    for (const g of groups.values()) {
      if (g.count > maxMultiplicity) maxMultiplicity = g.count
      if (g.count >= 2) {
        duplicateKeyCount++
        dupGroups.push(g)
      }
    }
    dupGroups.sort((a, b) => b.count - a.count || a.first - b.first)
    const totalDupGroups = dupGroups.length
    const samples = dupGroups.slice(0, sampleLimit).map(g => ({
      key: g.keyVals,
      count: g.count,
      rows: g.samples.map(ri => table.rows[ri].map(c => (c === undefined ? null : c))),
    }))
    const samplesTruncated = totalDupGroups > sampleLimit
    if (!keyIdx.length) {
      console.log(json({
        ok: true,
        datasetId: id,
        rowCount,
        mode: 'exact',
        keyColumns: null,
        duplicateRowCount,
        duplicateRowRate: Number((duplicateRowCount / Math.max(rowCount, 1)).toFixed(6)),
        distinctRowCount: distinct,
        duplicateRowCountGroups: totalDupGroups,
        maxMultiplicity,
        samples,
        samplesTruncated,
      }))
    } else {
      console.log(json({
        ok: true,
        datasetId: id,
        rowCount,
        mode: 'keyed',
        keyColumns: keyRaw.split(',').map(s => s.trim()).filter(Boolean),
        duplicateRowCount,
        duplicateRowRate: Number((duplicateRowCount / Math.max(rowCount, 1)).toFixed(6)),
        distinctKeyCount: distinct,
        duplicateKeyCount,
        maxMultiplicity,
        samples,
        samplesTruncated,
      }))
    }
    return
  }

  if (cmd === 'outliers') {
    const id = getFlag(rest, '--dataset-id')
    if (!id) fail(2, 'analysis/invalid-args: --dataset-id required')
    const columnsRaw = getFlag(rest, '--columns')
    if (!columnsRaw) fail(2, 'analysis/invalid-args: --columns required')
    const columnNames = columnsRaw.split(',').map(s => s.trim()).filter(Boolean)
    if (!columnNames.length) fail(2, 'analysis/invalid-args: --columns must list at least one column')
    if (columnNames.length > 32) fail(2, 'analysis/invalid-args: --columns supports at most 32 columns')
    const method = getFlag(rest, '--method') || 'iqr'
    if (!['iqr', 'zscore', 'mad'].includes(method)) fail(2, 'analysis/invalid-args: --method must be iqr|zscore|mad')
    const defaultK = method === 'iqr' ? 1.5 : 3.0
    let k = defaultK
    const kRaw = getFlag(rest, '--k')
    if (kRaw !== undefined) {
      k = Number(kRaw)
      if (!Number.isFinite(k) || k <= 0) fail(2, 'analysis/invalid-args: --k must be a finite number > 0')
    }
    const sampleLimit = parseSampleLimit(getFlag(rest, '--sample-limit'), 10)
    const outLabel = getFlag(rest, '--out')
    const { meta, table } = loadDataset(ws, id)
    const rowCount = table.rows.length
    const colResults = []
    const anyOutlier = new Set()
    for (const name of columnNames) {
      const ci = table.headers.indexOf(name)
      if (ci < 0) fail(2, `analysis/invalid-args: unknown column ${name}`)
      const dtype = meta.columns.find(x => x.name === name)?.dtype ?? 'utf8'
      let nullCount = 0
      const series = []
      for (let ri = 0; ri < table.rows.length; ri++) {
        const v = table.rows[ri][ci]
        if (v === null || v === undefined) nullCount++
        else if (typeof v === 'number' && Number.isFinite(v)) series.push({ ri, n: v })
      }
      const numericCount = series.length
      const nums = series.map(x => x.n).sort((a, b) => a - b)
      const pct = (arr, p) => {
        if (!arr.length) return NaN
        if (arr.length === 1) return arr[0]
        return arr[Math.min(arr.length - 1, Math.round((arr.length - 1) * p))]
      }
      let lower = null, upper = null
      let outlierFlags = []
      let degenerateIqr = false, degenerateStd = false, degenerateMad = false
      if (numericCount >= 2) {
        if (method === 'iqr') {
          const q1 = pct(nums, 0.25)
          const q3 = pct(nums, 0.75)
          const iqr = q3 - q1
          if (iqr === 0) {
            degenerateIqr = true
            lower = q1; upper = q3
          } else {
            lower = q1 - k * iqr
            upper = q3 + k * iqr
            outlierFlags = series.filter(x => x.n < lower || x.n > upper)
          }
        } else if (method === 'zscore') {
          const mean = nums.reduce((a, b) => a + b, 0) / nums.length
          const std = Math.sqrt(nums.reduce((a, b) => a + (b - mean) ** 2, 0) / (nums.length - 1))
          if (!std || !Number.isFinite(std)) {
            degenerateStd = true
            lower = mean; upper = mean
          } else {
            lower = mean - k * std
            upper = mean + k * std
            outlierFlags = series.filter(x => Math.abs(x.n - mean) / std > k)
          }
        } else {
          const med = pct(nums, 0.5)
          const devs = nums.map(x => Math.abs(x - med)).sort((a, b) => a - b)
          const mad = pct(devs, 0.5)
          const scale = 1.4826 * mad
          if (!mad || !scale || !Number.isFinite(scale)) {
            degenerateMad = true
            lower = med; upper = med
          } else {
            lower = med - k * scale
            upper = med + k * scale
            outlierFlags = series.filter(x => Math.abs(x.n - med) / scale > k)
          }
        }
      }
      outlierFlags.sort((a, b) => a.ri - b.ri)
      for (const f of outlierFlags) anyOutlier.add(f.ri)
      const samples = outlierFlags.slice(0, sampleLimit).map(f => ({
        rowIndex: f.ri,
        value: f.n,
        row: table.rows[f.ri].map(c => (c === undefined ? null : c)),
      }))
      colResults.push({
        name,
        dtype,
        numericCount,
        nullCount,
        lowerBound: lower === null || !Number.isFinite(lower) ? null : lower,
        upperBound: upper === null || !Number.isFinite(upper) ? null : upper,
        outlierCount: outlierFlags.length,
        outlierRate: Number((outlierFlags.length / Math.max(numericCount, 1)).toFixed(6)),
        degenerateIqr,
        degenerateStd,
        degenerateMad,
        samples,
      })
    }
    let outDatasetId = null
    let outRowCount
    if (outLabel !== undefined) {
      const outId = newId()
      const outRows = [...anyOutlier].sort((a, b) => a - b).map(ri => table.rows[ri])
      const maxRows = envU64('DSH_ANALYZE_MAX_ROWS', 'INV_ANALYSIS_MAX_ROWS', 5_000_000)
      if (outRows.length > maxRows) fail(3, 'analysis/limit: outlier dataset too large')
      writeDataset(ws, outId, { headers: table.headers, rows: outRows }, {
        sourceKind: 'outliers',
        sourcePath: `outliers:${id}`,
        label: outLabel,
        producedBy: 'outliers',
        parentDatasetId: id,
      })
      outDatasetId = outId
      outRowCount = outRows.length
    }
    const payload = {
      ok: true,
      datasetId: id,
      rowCount,
      method,
      k,
      outDatasetId,
      columns: colResults,
    }
    if (outLabel !== undefined) payload.outRowCount = outRowCount
    console.log(json(payload))
    return
  }

  if (cmd === 'corr') {
    const id = getFlag(rest, '--dataset-id')
    if (!id) fail(2, 'analysis/invalid-args: --dataset-id required')
    const columnsRaw = getFlag(rest, '--columns')
    if (!columnsRaw) fail(2, 'analysis/invalid-args: --columns required')
    const columns = columnsRaw.split(',').map(s => s.trim()).filter(Boolean)
    if (columns.length < 2 || columns.length > 16) fail(2, 'analysis/invalid-args: --columns must list 2..16 columns')
    const method = getFlag(rest, '--method') || 'pearson'
    if (method !== 'pearson' && method !== 'spearman') fail(2, 'analysis/invalid-args: --method must be pearson|spearman')
    const pairwise = getFlag(rest, '--pairwise') || 'complete'
    if (pairwise !== 'complete' && pairwise !== 'omit') fail(2, 'analysis/invalid-args: --pairwise must be complete|omit')
    let minNumeric = 2
    const minRaw = getFlag(rest, '--min-numeric')
    if (minRaw !== undefined) {
      minNumeric = Number(minRaw)
      if (!Number.isInteger(minNumeric) || minNumeric < 2) fail(2, 'analysis/invalid-args: --min-numeric must be integer >= 2')
    }
    const { meta, table } = loadDataset(ws, id)
    const colIdx = columns.map(name => {
      const i = table.headers.indexOf(name)
      if (i < 0) fail(2, `analysis/invalid-args: unknown column ${name}`)
      return i
    })
    const maps = colIdx.map((ci, idx) => {
      const m = new Map()
      table.rows.forEach((row, ri) => {
        const cell = row[ci]
        if (cell === null || cell === undefined) return
        let n = null
        if (typeof cell === 'number') n = cell
        else if (typeof cell === 'string' && cell.trim() !== '' && Number.isFinite(Number(cell))) n = Number(cell)
        if (n !== null && Number.isFinite(n)) m.set(ri, n)
      })
      if (m.size === 0) fail(2, `analysis/invalid-args: column ${columns[idx]} is not numeric-capable`)
      return m
    })
    const listwise = pairwise === 'omit'
      ? (() => {
          let mask = null
          for (const m of maps) {
            const set = new Set(m.keys())
            if (!mask) mask = set
            else {
              const next = new Set()
              for (const k of mask) if (set.has(k)) next.add(k)
              mask = next
            }
          }
          return mask
        })()
      : null

    function midRanks(values) {
      const n = values.length
      const idx = [...Array(n).keys()].sort((a, b) => values[a] - values[b])
      const ranks = new Array(n)
      let i = 0
      while (i < n) {
        let j = i
        while (j + 1 < n && values[idx[j + 1]] === values[idx[i]]) j++
        const avg = ((i + 1) + (j + 1)) / 2
        for (let k = i; k <= j; k++) ranks[idx[k]] = avg
        i = j + 1
      }
      return ranks
    }
    function pearson(xs, ys) {
      const n = xs.length
      if (n < 2) return { r: null, degenerate: false }
      const mx = xs.reduce((a, b) => a + b, 0) / n
      const my = ys.reduce((a, b) => a + b, 0) / n
      let sxx = 0, syy = 0, sxy = 0
      for (let i = 0; i < n; i++) {
        const dx = xs[i] - mx
        const dy = ys[i] - my
        sxx += dx * dx
        syy += dy * dy
        sxy += dx * dy
      }
      if (sxx === 0 || syy === 0) return { r: null, degenerate: true }
      const denom = Math.sqrt(sxx * syy)
      if (!denom || !Number.isFinite(denom)) return { r: null, degenerate: true }
      const r = sxy / denom
      if (!Number.isFinite(r)) return { r: null, degenerate: true }
      return { r: Math.max(-1, Math.min(1, r)), degenerate: false }
    }
    const pairs = []
    const totalPairs = columns.length * (columns.length - 1) / 2
    for (let i = 0; i < columns.length; i++) {
      for (let j = i + 1; j < columns.length; j++) {
        let xs = [], ys = []
        if (listwise) {
          const rows = [...listwise].sort((a, b) => a - b)
          for (const ri of rows) {
            if (maps[i].has(ri) && maps[j].has(ri)) {
              xs.push(maps[i].get(ri))
              ys.push(maps[j].get(ri))
            }
          }
        } else {
          const smaller = maps[i].size <= maps[j].size ? maps[i] : maps[j]
          for (const ri of [...smaller.keys()].sort((a, b) => a - b)) {
            if (maps[i].has(ri) && maps[j].has(ri)) {
              xs.push(maps[i].get(ri))
              ys.push(maps[j].get(ri))
            }
          }
        }
        const n = xs.length
        let r, degenerate
        if (n < minNumeric) {
          r = null
          degenerate = false
        } else if (method === 'spearman') {
          const out = pearson(midRanks(xs), midRanks(ys))
          r = out.r
          degenerate = out.degenerate
        } else {
          const out = pearson(xs, ys)
          r = out.r
          degenerate = out.degenerate
        }
        pairs.push({ columns: [columns[i], columns[j]], n, r, degenerate })
      }
    }
    pairs.sort((a, b) => {
      const ar = a.r === null ? -1 : Math.abs(a.r)
      const br = b.r === null ? -1 : Math.abs(b.r)
      if (ar !== br) return br - ar
      if (a.n !== b.n) return b.n - a.n
      return 0
    })
    const truncated = pairs.length > 50
    pairs.length = Math.min(pairs.length, 50)
    console.log(json({
      ok: true,
      datasetId: id,
      rowCount: meta.rowCount,
      method,
      pairwise,
      minNumeric,
      columns,
      pairCount: totalPairs,
      pairs,
      truncated,
    }))
    return
  }

  if (cmd === 'crosstab') {
    const id = getFlag(rest, '--dataset-id')
    if (!id) fail(2, 'analysis/invalid-args: --dataset-id required')
    const rowsName = (getFlag(rest, '--rows') || '').trim()
    const colsName = (getFlag(rest, '--cols') || '').trim()
    if (!rowsName) fail(2, 'analysis/invalid-args: --rows required')
    if (!colsName) fail(2, 'analysis/invalid-args: --cols required')
    if (rowsName === colsName) fail(2, 'analysis/invalid-args: --rows and --cols must be different columns')
    let top = 20
    const topRaw = getFlag(rest, '--top')
    if (topRaw !== undefined) {
      top = Number(topRaw)
      if (!Number.isInteger(top) || top < 1 || top > 50) fail(2, 'analysis/invalid-args: --top must be integer 1..=50')
    }
    let minCount = 1
    const minRaw = getFlag(rest, '--min-count')
    if (minRaw !== undefined) {
      minCount = Number(minRaw)
      if (!Number.isInteger(minCount) || minCount < 1) fail(2, 'analysis/invalid-args: --min-count must be integer >= 1')
    }
    const dropNullRaw = getFlag(rest, '--drop-null')
    let dropNull = true
    if (dropNullRaw !== undefined) {
      if (dropNullRaw === 'true') dropNull = true
      else if (dropNullRaw === 'false') dropNull = false
      else fail(2, 'analysis/invalid-args: --drop-null must be true|false')
    }
    const { meta, table } = loadDataset(ws, id)
    const ri = table.headers.indexOf(rowsName)
    const ci = table.headers.indexOf(colsName)
    if (ri < 0) fail(2, `analysis/invalid-args: unknown column ${rowsName}`)
    if (ci < 0) fail(2, `analysis/invalid-args: unknown column ${colsName}`)
    const cellCounts = new Map()
    const rowMarg = new Map()
    const colMarg = new Map()
    const rowFirst = new Map()
    const colFirst = new Map()
    let nullRowCount = 0
    let seq = 0
    const keyOf = (v) => {
      if (v === null || v === undefined) return dropNull ? null : '__null__'
      return JSON.stringify(v)
    }
    for (const row of table.rows) {
      const rv = row[ri]
      const cv = row[ci]
      if (dropNull && (rv === null || rv === undefined || cv === null || cv === undefined)) {
        nullRowCount++
        continue
      }
      const rk = keyOf(rv)
      const ck = keyOf(cv)
      const ck2 = rk === null || ck === null ? null : `${rk}|${ck}`
      if (ck2 === null) continue
      cellCounts.set(ck2, (cellCounts.get(ck2) || 0) + 1)
      rowMarg.set(rk, (rowMarg.get(rk) || 0) + 1)
      colMarg.set(ck, (colMarg.get(ck) || 0) + 1)
      if (!rowFirst.has(rk)) rowFirst.set(rk, seq)
      if (!colFirst.has(ck)) colFirst.set(ck, seq)
      seq++
    }
    const sortCates = (marg, first) => {
      const keys = [...marg.keys()]
      keys.sort((a, b) => {
        const d = (marg.get(b) || 0) - (marg.get(a) || 0)
        if (d !== 0) return d
        return (first.get(a) ?? Infinity) - (first.get(b) ?? Infinity)
      })
      return keys
    }
    let rowCates = sortCates(rowMarg, rowFirst)
    let colCates = sortCates(colMarg, colFirst)
    const distinctRows = rowCates.length
    const distinctCols = colCates.length
    rowCates = rowCates.slice(0, top)
    colCates = colCates.slice(0, top)
    let truncated = distinctRows > top || distinctCols > top
    const matrix = rowCates.map(() => colCates.map(() => 0))
    let cellCount = 0
    for (const [ck, n] of cellCounts) {
      const [rk, ckv] = ck.split('|')
      const i = rowCates.findIndex(x => x === rk)
      const j = colCates.findIndex(x => x === ckv)
      if (i < 0 || j < 0) {
        truncated = true
        continue
      }
      matrix[i][j] += n
      cellCount += n
    }
    const rowTotals = matrix.map(row => row.reduce((a, b) => a + b, 0))
    const colTotals = colCates.map((_, j) => matrix.reduce((a, row) => a + row[j], 0))
    const grandTotal = rowTotals.reduce((a, b) => a + b, 0)
    const decode = (k) => {
      if (k === '__null__') return '__null__'
      return JSON.parse(k)
    }
    const topCells = []
    for (let i = 0; i < matrix.length; i++) {
      for (let j = 0; j < matrix[i].length; j++) {
        const n = matrix[i][j]
        if (n >= minCount && n > 0) topCells.push({ row: decode(rowCates[i]), col: decode(colCates[j]), count: n })
      }
    }
    topCells.sort((a, b) => b.count - a.count)
    topCells.length = Math.min(topCells.length, 50)
    console.log(json({
      ok: true,
      datasetId: id,
      rowCount: meta.rowCount,
      rowColumn: rowsName,
      colColumn: colsName,
      rowCategories: rowCates.map(decode),
      colCategories: colCates.map(decode),
      matrix,
      rowTotals,
      colTotals,
      grandTotal,
      cellCount,
      truncated,
      nullRowCount,
      topCells,
    }))
    return
  }

  fail(2, `analysis/invalid-args: unknown command ${cmd}`)
}

main().catch((e) => fail(1, String(e?.message || e)))
