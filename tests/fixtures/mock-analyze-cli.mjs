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
      version: '0.1.2-mock',
      capabilities: ['ingest', 'head', 'schema', 'describe', 'agg', 'list', 'export', 'rm', 'values', 'sample', 'join'],
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
      }
      stats.push({
        name,
        dtype: meta.columns.find(x => x.name === name)?.dtype ?? 'utf8',
        nullCount: nulls,
        min, max, mean, std, q25, q50, q75,
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
    if (how !== 'inner' && how !== 'left') fail(2, 'analysis/invalid-args: --how must be inner|left')
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
    right.table.headers.forEach((h, i) => {
      if (rKey.includes(i)) return
      let name = h
      while (names.has(name)) name = name + '_r'
      names.add(name)
      outHeaders.push(name)
      rightPayload.push(i)
    })
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
        continue
      }
      const matches = rightMap.get(JSON.stringify(k)) || []
      if (matches.length === 0) {
        if (how === 'left') outRows.push([...lrow, ...rightPayload.map(() => null)])
        continue
      }
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

  fail(2, `analysis/invalid-args: unknown command ${cmd}`)
}

main().catch((e) => fail(1, String(e?.message || e)))
