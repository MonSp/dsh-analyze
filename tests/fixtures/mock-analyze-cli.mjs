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
    createdAt: metaExtra.createdAt ?? new Date().toISOString(),
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
      version: '0.1.7-mock',
      capabilities: ['ingest', 'head', 'schema', 'describe', 'agg', 'list', 'export', 'rm', 'values', 'sample', 'join', 'profile', 'dedup', 'outliers', 'corr', 'crosstab', 'series', 'union', 'diff', 'pivot'],
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
    const format = getFlag(rest, '--format') || (path.endsWith('.jsonl') ? 'jsonl' : path.endsWith('.json') ? 'json' : 'csv')
    if (format === 'parquet') fail(4, 'analysis/limit: parquet not supported yet (planned, see docs/interface.md)')
    if (format === 'xlsx') fail(4, 'analysis/limit: xlsx not supported (convert to csv/jsonl)')
    if (format === 'jsonl') fail(4, 'analysis/limit: mock CLI supports csv/json only in T0 tests')
    const id = getFlag(rest, '--dataset-id') || newId()
    const text = readFileSync(path, 'utf8')
    let table
    if (format === 'json') {
      let records
      try { records = JSON.parse(text.replace(/^﻿/, '')) } catch { fail(2, 'analysis/invalid-args: bad json file') }
      if (records && !Array.isArray(records) && typeof records === 'object' && Array.isArray(records.records)) {
        records = records.records
      }
      if (!Array.isArray(records)) fail(2, 'analysis/invalid-args: json must be a top-level array or {records:[…]}')
      const headers = []
      const rows = []
      for (let ri = 0; ri < records.length; ri++) {
        const rec = records[ri]
        if (!rec || typeof rec !== 'object' || Array.isArray(rec)) {
          fail(2, `analysis/invalid-args: bad json record at index ${ri}`)
        }
        const row = []
        for (const key of Object.keys(rec)) {
          let ci = headers.indexOf(key)
          if (ci < 0) { headers.push(key); ci = headers.length - 1 }
          row[ci] = rec[key] === undefined ? null : rec[key]
        }
        for (let c = 0; c < headers.length; c++) if (row[c] === undefined) row[c] = null
        rows.push(row)
      }
      table = { headers, rows }
    } else {
      table = parseCsv(text)
    }
    const label = getFlag(rest, '--label')
    const { meta, bytes } = writeDataset(ws, id, table, {
      sourceKind: format === 'json' ? 'json' : 'csv',
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
    const rawN = getFlag(rest, '--n')
    const requestedN = rawN === undefined ? 5 : Number(rawN)
    if (!Number.isInteger(requestedN) || requestedN < 1) fail(2, 'analysis/invalid-args: --n must be integer >= 1')
    const limit = Math.min(requestedN, 100)
    const { meta, table } = loadDataset(ws, id)
    const rows = table.rows.slice(0, limit).map(r => r.map(c => (c === null ? null : c)))
    const warnings = []
    if (requestedN > 100) warnings.push({ code: 'clamp', field: 'n', requested: requestedN, actual: 100 })
    console.log(json({
      ok: true,
      datasetId: id,
      rowCount: meta.rowCount,
      columns: table.headers,
      rows,
      requestedN,
      n: rows.length,
      ...(warnings.length ? { warnings } : {}),
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
    const warnings = []

    // ---- filter / having / count_if predicate trees (R-5) ----
    const evalPredicate = (pred, row, headerList) => {
      const ci = headerList.indexOf(pred.column)
      if (ci < 0) fail(2, `analysis/invalid-args: unknown column ${pred.column}`)
      const cell = row[ci]
      switch (pred.op) {
        case 'is_null': return cell === null || cell === undefined
        case 'not_null': return !(cell === null || cell === undefined)
        case 'eq': return cell == pred.value
        case 'ne': return cell != pred.value
        case 'gt': return Number(cell) > Number(pred.value)
        case 'gte': return Number(cell) >= Number(pred.value)
        case 'lt': return Number(cell) < Number(pred.value)
        case 'lte': return Number(cell) <= Number(pred.value)
        case 'in': return (Array.isArray(pred.value) ? pred.value : [pred.value]).some(v => cell == v)
        case 'contains': return String(cell ?? '').includes(String(pred.value))
        default: fail(2, `analysis/invalid-args: bad op ${pred.op}`)
      }
    }
    const evalFilterNode = (node, row, headerList) => {
      if (Array.isArray(node)) return node.every(item => evalFilterNode(item, row, headerList))
      if (node && typeof node === 'object') {
        const keys = Object.keys(node)
        if ('op' in node) return evalPredicate(node, row, headerList)
        if (keys.length === 1 && 'all' in node) {
          if (!Array.isArray(node.all)) fail(2, 'analysis/invalid-args: filter all must be an array')
          return node.all.every(item => evalFilterNode(item, row, headerList))
        }
        if (keys.length === 1 && 'any' in node) {
          if (!Array.isArray(node.any)) fail(2, 'analysis/invalid-args: filter any must be an array')
          return node.any.some(item => evalFilterNode(item, row, headerList))
        }
        if (keys.length === 1 && 'not' in node) return !evalFilterNode(node.not, row, headerList)
        fail(2, 'analysis/invalid-args: bad filter group')
      }
      fail(2, 'analysis/invalid-args: bad filter entry')
    }

    let keep = table.rows.map((_, i) => i)
    if (spec.filter !== undefined) {
      keep = keep.filter(i => evalFilterNode(spec.filter, table.rows[i], table.headers))
    }

    // ---- topKPerGroup (R-15): row stage after filter, before mutate ----
    if (spec.topKPerGroup) {
      const t = spec.topKPerGroup
      const by = Array.isArray(t.by) ? t.by : []
      if (!by.length || by.length > 8) fail(2, 'analysis/invalid-args: topKPerGroup.by must be 1..8 column names')
      const k = t.k
      if (!Number.isInteger(k) || k < 1 || k > 1000) fail(2, 'analysis/invalid-args: topKPerGroup.k must be integer 1..1000')
      const byIdx = by.map(name => {
        const i = table.headers.indexOf(name)
        if (i < 0) fail(2, `analysis/invalid-args: unknown column ${name}`)
        return i
      })
      let orderByIdx = -1
      let orderDesc = false
      if (t.orderBy && t.orderBy.column) {
        orderByIdx = table.headers.indexOf(t.orderBy.column)
        if (orderByIdx < 0) fail(2, `analysis/invalid-args: unknown column ${t.orderBy.column}`)
        orderDesc = !!t.orderBy.desc
      }
      const groupsMap = new Map()
      for (const i of keep) {
        const key = JSON.stringify(byIdx.map(ci => table.rows[i][ci] ?? null))
        if (!groupsMap.has(key)) groupsMap.set(key, [])
        groupsMap.get(key).push(i)
      }
      const keptSet = new Set()
      for (const members of groupsMap.values()) {
        let ordered = members
        if (orderByIdx >= 0) {
          ordered = [...members].sort((a, b) => {
            const av = table.rows[a][orderByIdx], bv = table.rows[b][orderByIdx]
            const cmp = (av > bv ? 1 : av < bv ? -1 : 0) * (orderDesc ? -1 : 1)
            return cmp !== 0 ? cmp : a - b
          })
        }
        for (const i of ordered.slice(0, k)) keptSet.add(i)
      }
      keep = keep.filter(i => keptSet.has(i))
    }

    // ---- mutate (R-9: to_int/to_str/to_date + string literals) ----
    let workHeaders = [...table.headers]
    let workRows = keep.map(i => [...table.rows[i]])
    const parseWithFmt = (s, fmt) => {
      let re = '^'
      const groups = []
      for (let fi = 0; fi < fmt.length; fi++) {
        if (fmt[fi] === '%') {
          const d = fmt[fi + 1]
          if (!'YmdHMS'.includes(d ?? '')) fail(2, 'analysis/invalid-args: to_date fmt has unsupported directive (only %Y %m %d %H %M %S)')
          const pat = d === 'Y' ? '(\\d{4})' : '(\\d{2})'
          re += pat
          groups.push(d)
          fi++
        } else {
          re += fmt[fi].replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
        }
      }
      re += '$'
      const m = String(s).trim().match(new RegExp(re))
      if (!m) return null
      const parts = {}
      groups.forEach((d, gi) => { parts[d] = +m[gi + 1] })
      const y = parts.Y, mo = parts.m, d = parts.d
      if (!y || !mo || !d) return null
      let out = `${String(y).padStart(4, '0')}-${String(mo).padStart(2, '0')}-${String(d).padStart(2, '0')}`
      if (parts.H !== undefined || parts.M !== undefined || parts.S !== undefined) {
        out += `T${String(parts.H ?? 0).padStart(2, '0')}:${String(parts.M ?? 0).padStart(2, '0')}:${String(parts.S ?? 0).padStart(2, '0')}`
      }
      return out
    }
    const makeEvalExpr = (stats) => (expr, row, colIndex) => {
      if (typeof expr !== 'string' || !expr.trim() || expr.length > 512) {
        fail(2, 'analysis/invalid-args: mutate expr must be non-empty string ≤512')
      }
      if (!/[A-Za-z_]/.test(expr) && !/\d/.test(expr)) {
        fail(2, 'analysis/invalid-args: mutate expr is empty of operands')
      }
      const tokens = []
      let ti = 0
      while (ti < expr.length) {
        const ch = expr[ti]
        if (/\s/.test(ch)) { ti++; continue }
        if (ch === '"') {
          let j = ti + 1
          while (j < expr.length && expr[j] !== '"') { if (expr[j] === '\\') j++; j++ }
          if (j >= expr.length) fail(2, 'analysis/invalid-args: mutate expr has illegal characters')
          tokens.push({ t: 'str', v: expr.slice(ti + 1, j) })
          ti = j + 1
          continue
        }
        if (/[A-Za-z_]/.test(ch)) {
          let j = ti
          while (j < expr.length && /\w/.test(expr[j])) j++
          tokens.push({ t: 'id', v: expr.slice(ti, j) })
          ti = j
          continue
        }
        if (/[0-9]/.test(ch)) {
          let j = ti
          while (j < expr.length && /[0-9.]/.test(expr[j])) j++
          tokens.push({ t: 'num', v: Number(expr.slice(ti, j)) })
          ti = j
          continue
        }
        if ('+-*/(),'.includes(ch)) { tokens.push({ t: 'op', v: ch }); ti++; continue }
        fail(2, 'analysis/invalid-args: mutate expr has illegal characters')
      }
      let pos = 0
      const peek = () => tokens[pos]
      const next = () => tokens[pos++]
      // null = propagate null; NaN = computed null (non-numeric operand)
      const numOrUndef = (v) => {
        if (v === null || v === undefined) return null
        if (typeof v === 'boolean') return NaN
        const n = typeof v === 'number' ? v : Number(String(v).trim())
        return Number.isFinite(n) ? n : NaN
      }
      const parsePrimary = () => {
        const tok = next()
        if (!tok) fail(2, 'analysis/invalid-args: mutate expr has illegal characters')
        if (tok.t === 'num' || tok.t === 'str') return tok.v
        if (tok.t === 'op' && tok.v === '(') {
          const v = parseAdd()
          const c = next()
          if (!c || c.v !== ')') fail(2, 'analysis/invalid-args: mutate expr has illegal characters')
          return v
        }
        if (tok.t === 'id') {
          const nxt = peek()
          if (nxt && nxt.t === 'op' && nxt.v === '(') {
            if (!['to_int', 'to_str', 'to_date'].includes(tok.v)) {
              fail(2, 'analysis/invalid-args: function calls are not allowed in mutate expr')
            }
            next()
            let argc = 0, arg1 = null, fmt = null
            if (!(peek() && peek().t === 'op' && peek().v === ')')) {
              arg1 = parseAdd()
              argc = 1
              const sep = peek()
              if (sep && sep.t === 'op' && sep.v === ',') {
                if (tok.v !== 'to_date') fail(2, `analysis/invalid-args: ${tok.v}(e) takes 1 argument`)
                next()
                const fmtTok = next()
                if (!fmtTok || fmtTok.t !== 'str') fail(2, 'analysis/invalid-args: to_date fmt must be a string literal')
                fmt = fmtTok.v
                argc = 2
              }
            }
            const close = next()
            if (!close || close.v !== ')') fail(2, 'analysis/invalid-args: mutate expr has illegal characters')
            if (tok.v === 'to_date' && argc !== 2) fail(2, 'analysis/invalid-args: to_date(e, fmt) takes 2 arguments')
            if (tok.v !== 'to_date' && argc !== 1) fail(2, `analysis/invalid-args: ${tok.v}(e) takes 1 argument`)
            if (tok.v === 'to_int') {
              if (arg1 === null || arg1 === undefined) return null
              const n = Number(String(arg1).trim())
              if (!Number.isFinite(n)) { stats.cast++; return null }
              return Math.trunc(n)
            }
            if (tok.v === 'to_str') {
              if (arg1 === null || arg1 === undefined) return null
              return String(arg1)
            }
            if (arg1 === null || arg1 === undefined) return null
            const parsed = parseWithFmt(String(arg1), fmt)
            if (parsed === null) { stats.cast++; return null }
            return parsed
          }
          const ci = colIndex[tok.v]
          if (ci === undefined) fail(2, `analysis/invalid-args: unknown column in expr: ${tok.v}`)
          return row[ci]
        }
        if (tok.t === 'op' && tok.v === '-') return parseNeg()
        fail(2, 'analysis/invalid-args: mutate expr has illegal characters')
      }
      const parseNeg = () => {
        const v = parseUnary()
        const n = numOrUndef(v)
        if (n === null) return null
        if (!Number.isFinite(n)) { stats.nullOut++; return null }
        return -n
      }
      const parseUnary = () => {
        const t = peek()
        if (t && t.t === 'op' && t.v === '-') { next(); return parseNeg() }
        return parsePrimary()
      }
      const parseMul = () => {
        let v = parseUnary()
        while (peek() && peek().t === 'op' && (peek().v === '*' || peek().v === '/')) {
          const op = next().v
          const r = parseUnary()
          const a = numOrUndef(v), b = numOrUndef(r)
          if (a === null || b === null) { v = null; continue }
          if (!Number.isFinite(a) || !Number.isFinite(b)) { stats.nullOut++; v = null; continue }
          if (op === '/' && b === 0) { stats.nullOut++; v = null; continue }
          v = op === '*' ? a * b : a / b
        }
        return v
      }
      const parseAdd = () => {
        let v = parseMul()
        while (peek() && peek().t === 'op' && (peek().v === '+' || peek().v === '-')) {
          const op = next().v
          const r = parseMul()
          const a = numOrUndef(v), b = numOrUndef(r)
          if (a === null || b === null) { v = null; continue }
          if (!Number.isFinite(a) || !Number.isFinite(b)) { stats.nullOut++; v = null; continue }
          v = op === '+' ? a + b : a - b
        }
        return v
      }
      return parseAdd()
    }
    if (Array.isArray(spec.mutate)) {
      for (const m of spec.mutate) {
        if (!m || typeof m.as !== 'string' || typeof m.expr !== 'string') {
          fail(2, 'analysis/invalid-args: mutate needs as+expr')
        }
        if (workHeaders.includes(m.as)) fail(2, `analysis/invalid-args: mutate column collides: ${m.as}`)
        const colIndex = Object.fromEntries(workHeaders.map((h, i) => [h, i]))
        const stats = { cast: 0, nullOut: 0 }
        const evalExpr = makeEvalExpr(stats)
        for (const row of workRows) {
          row.push(evalExpr(m.expr, row, colIndex))
        }
        workHeaders.push(m.as)
        if (stats.nullOut > 0) warnings.push({ code: 'mutate-null', column: m.as, count: stats.nullOut })
        if (stats.cast > 0) warnings.push({ code: 'cast-null', column: m.as, count: stats.cast })
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
    let parseFailSample = []
    let outHeaders, outRows
    if (!groupBy.length && !aggs.length) {
      outHeaders = [...work.headers]
      outRows = work.rows.map(r => [...r])
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
          || t.match(/^(\d{4})\/(\d{2})\/(\d{2})(?: (\d{2}):(\d{2}):(\d{2}))?$/)
          || t.match(/^(\d{4})(\d{2})(\d{2})$/)
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
          if (b === null) {
            parseFailCount++
            const s = String(cell)
            if (!parseFailSample.includes(s) && parseFailSample.length < 10) parseFailSample.push(s)
            return null
          }
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
          if (a.fn === 'count_if') {
            if (!a.where) fail(2, 'analysis/invalid-args: count_if requires where')
            row.push(g.members.filter(m => evalFilterNode(a.where, m, work.headers) === true).length)
            continue
          }
          const ci = widx(a.column)
          const vals = g.members.map(r => r[ci]).filter(v => v !== null && v !== undefined)
          if (a.fn === 'count_distinct') {
            row.push(new Set(vals.map(v => JSON.stringify(v))).size)
            continue
          }
          if (a.fn === 'first') {
            row.push(vals.length ? vals[0] : null)
            continue
          }
          if (a.fn === 'last') {
            row.push(vals.length ? vals[vals.length - 1] : null)
            continue
          }
          const nums = vals.filter(v => typeof v === 'number')
          if (a.fn === 'percentile') {
            if (typeof a.p !== 'number' || !(a.p > 0 && a.p < 1)) {
              fail(2, 'analysis/invalid-args: percentile requires p in (0,1)')
            }
            if (!nums.length) { row.push(null); continue }
            const s = [...nums].sort((x, y) => x - y)
            row.push(s[Math.min(s.length - 1, Math.round((s.length - 1) * a.p))])
            continue
          }
          let v = null
          if (a.fn === 'sum') v = nums.reduce((x, y) => x + y, 0)
          else if (a.fn === 'mean') v = nums.length ? nums.reduce((x, y) => x + y, 0) / nums.length : null
          else if (a.fn === 'min') v = nums.length ? Math.min(...nums) : null
          else if (a.fn === 'max') v = nums.length ? Math.max(...nums) : null
          else if (a.fn === 'median') {
            if (!nums.length) v = null
            else {
              const s = [...nums].sort((x, y) => x - y)
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
    }

    // ---- having (R-7): filter result rows before select ----
    if (spec.having !== undefined) {
      outRows = outRows.filter(r => evalFilterNode(spec.having, r, outHeaders))
    }

    // ---- select (R-9): project / rename ----
    if (Array.isArray(spec.select)) {
      const newHeaders = []
      const idxs = []
      const seen = new Set()
      for (const s of spec.select) {
        let src, out
        if (typeof s === 'string') { src = s; out = s }
        else if (s && typeof s === 'object') { src = s.column; out = s.as }
        else fail(2, 'analysis/invalid-args: select entries must be strings or {column,as}')
        const ci = outHeaders.indexOf(src)
        if (ci < 0) fail(2, `analysis/invalid-args: unknown column ${src}`)
        if (seen.has(out)) fail(2, `analysis/invalid-args: duplicate select name ${out}`)
        seen.add(out)
        newHeaders.push(out)
        idxs.push(ci)
      }
      outRows = outRows.map(r => idxs.map(ci => r[ci]))
      outHeaders = newHeaders
    }

    // ---- orderBy + limit on the final columns ----
    if (spec.orderBy?.length) {
      const order = spec.orderBy.map(o => {
        const i = outHeaders.indexOf(o.column)
        if (i < 0) fail(2, `analysis/invalid-args: unknown column ${o.column}`)
        return { i, desc: !!o.desc }
      })
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

    if (parseFailCount > 0) {
      warnings.push({ code: 'date-parse-fail', count: parseFailCount })
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
      ...(parseFailCount > 0 ? { parseFailCount, parseFailSample } : {}),
      ...(warnings.length ? { warnings } : {}),
    }))
    return
  }

  if (cmd === 'values') {
    const id = getFlag(rest, '--dataset-id')
    const column = getFlag(rest, '--column')
    const rawLimit = getFlag(rest, '--limit')
    const requestedLimit = rawLimit === undefined ? 20 : Number(rawLimit)
    if (!Number.isInteger(requestedLimit) || requestedLimit < 1) fail(2, 'analysis/invalid-args: --limit must be integer >= 1')
    const limit = Math.min(requestedLimit, 100)
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
    const warnings = []
    if (requestedLimit > 100) warnings.push({ code: 'clamp', field: 'limit', requested: requestedLimit, actual: 100 })
    console.log(json({
      ok: true,
      datasetId: id,
      column,
      dtype: meta.columns.find(x => x.name === column)?.dtype ?? 'utf8',
      rowCount: meta.rowCount,
      nullCount,
      nunique: counts.size,
      requestedLimit,
      limit,
      top,
      ...(warnings.length ? { warnings } : {}),
    }))
    return
  }

  if (cmd === 'sample') {
    const id = getFlag(rest, '--dataset-id')
    const rawN = getFlag(rest, '--n')
    const requestedN = rawN === undefined ? 10 : Number(rawN)
    if (!Number.isInteger(requestedN) || requestedN < 1) fail(2, 'analysis/invalid-args: --n must be integer >= 1')
    const n = Math.min(requestedN, 200)
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
    const warnings = []
    if (requestedN > 200) warnings.push({ code: 'clamp', field: 'n', requested: requestedN, actual: 200 })
    console.log(json({
      ok: true,
      datasetId: id,
      rowCount: meta.rowCount,
      columns: table.headers,
      rows,
      requestedN,
      n: rows.length,
      sampling: {
        method,
        n: indices.length,
        seed,
        stratifyBy: stratifyBy ?? null,
        population: pop,
      },
      ...(warnings.length ? { warnings } : {}),
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
    const seenLeft = new Set()
    const seenRight = new Set()
    const lKey = on.map(entry => {
      const parts = entry.split(':')
      const name = parts.length === 2 ? parts[0] : entry
      if (parts.length > 2 || (parts.length === 2 && (!parts[0] || !parts[1]))) {
        fail(2, 'analysis/invalid-args: on pairs must be leftKey:rightKey')
      }
      if (seenLeft.has(name)) fail(2, `analysis/invalid-args: duplicate join key ${name}`)
      seenLeft.add(name)
      const i = left.table.headers.indexOf(name)
      if (i < 0) fail(2, `analysis/invalid-args: unknown column ${name} on left`)
      return i
    })
    const rKey = on.map(entry => {
      const parts = entry.split(':')
      const name = parts.length === 2 ? parts[1] : entry
      if (seenRight.has(name)) fail(2, `analysis/invalid-args: duplicate join key ${name}`)
      seenRight.add(name)
      const i = right.table.headers.indexOf(name)
      if (i < 0) fail(2, `analysis/invalid-args: unknown column ${name} on right`)
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
      // Value-based null only — string keys like "null"/"nullable" are valid.
      const parts = keyOf(row, rKey)
      if (parts.some(v => v === null || v === undefined)) return
      const k = JSON.stringify(parts)
      if (!rightMap.has(k)) rightMap.set(k, [])
      rightMap.get(k).push(ri)
    })
    const outRows = []
    for (const lrow of left.table.rows) {
      const k = keyOf(lrow, lKey)
      if (k.some(v => v === null || v === undefined)) {
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
    const labelLike = getFlag(rest, '--label-like')
    const producedBy = getFlag(rest, '--produced-by')
    const sinceRaw = getFlag(rest, '--since')
    let sinceTs = null
    if (sinceRaw !== undefined) {
      const m = sinceRaw.match(/^(\d{4})-(\d{2})-(\d{2})(?:[T ](\d{2}):(\d{2}):(\d{2}))?$/)
      if (!m) fail(2, 'analysis/invalid-args: --since must be YYYY-MM-DD[THH:MM:SS]')
      sinceTs = Date.UTC(+m[1], +m[2] - 1, +m[3], +(m[4] ?? 0), +(m[5] ?? 0), +(m[6] ?? 0))
    }
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
          createdAt: meta.createdAt ?? null,
        }
      })
      .filter(d => {
        if (labelLike !== undefined && !String(d.label ?? '').toLowerCase().includes(labelLike.toLowerCase())) return false
        if (producedBy !== undefined && d.producedBy !== producedBy) return false
        if (sinceTs !== null) {
          if (!d.createdAt) return false
          const ts = Date.parse(d.createdAt)
          if (!Number.isFinite(ts) || ts < sinceTs) return false
        }
        return true
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
    if (new Set(columns).size !== columns.length) fail(2, 'analysis/invalid-args: duplicate column in --columns')
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
        pairs.push({ columns: [columns[i], columns[j]], n, r, degenerate, _i: i, _j: j })
      }
    }
    pairs.sort((a, b) => {
      const ar = a.r === null ? -1 : Math.abs(a.r)
      const br = b.r === null ? -1 : Math.abs(b.r)
      if (ar !== br) return br - ar
      if (a.n !== b.n) return b.n - a.n
      if (a._i !== b._i) return a._i - b._i
      return a._j - b._j
    })
    for (const p of pairs) {
      delete p._i
      delete p._j
    }
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
    // Null uses a non-printable sentinel (not "__null__") so a literal
    // string category cannot collide. Values stay JSON text; the cell
    // key is JSON.stringify([rowKey, colKey]) so pipes in values are safe.
    const NULL_KEY = String.fromCharCode(0)
    const keyOf = (v) => {
      if (v === null || v === undefined) return NULL_KEY
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
      const cellKey = JSON.stringify([rk, ck])
      cellCounts.set(cellKey, (cellCounts.get(cellKey) || 0) + 1)
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
    for (const [cellKey, n] of cellCounts) {
      const [rk, ckv] = JSON.parse(cellKey)
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
      if (k === NULL_KEY) return '__null__'
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

  if (cmd === 'series') {
    const id = getFlag(rest, '--dataset-id')
    const orderBy = getFlag(rest, '--order-by')
    const opsRaw = getFlag(rest, '--ops')
    const outLabel = getFlag(rest, '--out')
    if (!id || !orderBy || !opsRaw) fail(2, 'analysis/invalid-args: --dataset-id --order-by --ops required')
    let opsText = opsRaw
    if (!opsRaw.trimStart().startsWith('[')) {
      const opsPath = within(ws, opsRaw)
      if (!existsSync(opsPath)) fail(2, 'analysis/invalid-args: cannot read ops file')
      opsText = readFileSync(opsPath, 'utf8').replace(/^﻿/, '')
    }
    let ops
    try {
      ops = JSON.parse(opsText)
    } catch {
      fail(2, 'analysis/invalid-args: bad ops JSON')
    }
    if (!Array.isArray(ops) || ops.length === 0) fail(2, 'analysis/invalid-args: ops must list at least one transform')
    if (ops.length > 8) fail(2, 'analysis/invalid-args: ops must be 1..=8 transforms')
    const { meta, table } = loadDataset(ws, id)
    const orderIdx = table.headers.indexOf(orderBy)
    if (orderIdx < 0) fail(2, `analysis/invalid-args: unknown order-by column ${orderBy}`)
    const asRe = /^[A-Za-z_][A-Za-z0-9_]*$/
    const names = new Set(table.headers)
    const colIdxs = []
    for (const op of ops) {
      if (!op || typeof op !== 'object') fail(2, 'analysis/invalid-args: each op must be a JSON object')
      if (!['lag', 'lead', 'diff', 'pct_change', 'rolling'].includes(op.fn)) {
        fail(2, `analysis/invalid-args: unknown fn ${op.fn}`)
      }
      if (typeof op.column !== 'string' || !op.column) fail(2, 'analysis/invalid-args: op requires column')
      if (typeof op.as !== 'string' || !asRe.test(op.as) || op.as.length > 64) {
        fail(2, 'analysis/invalid-args: as must match [A-Za-z_][A-Za-z0-9_]* (<=64)')
      }
      if (names.has(op.as)) fail(2, `analysis/invalid-args: as column collides: ${op.as}`)
      names.add(op.as)
      const ci = table.headers.indexOf(op.column)
      if (ci < 0) fail(2, `analysis/invalid-args: unknown column ${op.column}`)
      colIdxs.push(ci)
      if (op.fn === 'rolling') {
        if (op.periods !== undefined && op.periods !== null) fail(2, 'analysis/invalid-args: rolling takes window, not periods')
        const window = Number(op.window)
        if (!Number.isInteger(window) || window < 1 || window > 10000) {
          fail(2, 'analysis/invalid-args: window must be 1..=10000')
        }
        if (!['mean', 'sum', 'min', 'max'].includes(op.agg)) {
          fail(2, 'analysis/invalid-args: rolling agg must be mean|sum|min|max')
        }
      } else {
        if (op.window !== undefined && op.window !== null) fail(2, `analysis/invalid-args: fn ${op.fn} does not take window`)
        const periods = op.periods === undefined || op.periods === null ? 1 : Number(op.periods)
        if (!Number.isInteger(periods) || periods < 1 || periods > 10000) {
          fail(2, 'analysis/invalid-args: periods must be 1..=10000')
        }
      }
    }

    const typeRank = v => {
      if (v === null || v === undefined) return 5
      if (typeof v === 'boolean') return 0
      if (typeof v === 'number') return 1
      return 2
    }
    const cmpVal = (a, b) => {
      const ra = typeRank(a)
      const rb = typeRank(b)
      if (ra !== rb) return ra - rb
      if (ra === 5) return 0
      if (ra === 0) return (a === b ? 0 : a ? 1 : -1)
      if (ra === 1) return a === b ? 0 : a < b ? -1 : 1
      return a < b ? -1 : a > b ? 1 : 0
    }
    const order = table.rows.map((_, i) => i).sort((i, j) => {
      const c = cmpVal(table.rows[i][orderIdx] ?? null, table.rows[j][orderIdx] ?? null)
      return c !== 0 ? c : i - j
    })
    const sorted = order.map(i => table.rows[i])
    const n = sorted.length
    const finite = v => {
      if (v === null || v === undefined || typeof v === 'boolean') return null
      const num = typeof v === 'number' ? v : Number(String(v).trim())
      return Number.isFinite(num) ? num : null
    }
    const newCols = ops.map((op, k) => {
      const ci = colIdxs[k]
      const col = new Array(n).fill(null)
      if (op.fn === 'lag') {
        const p = op.periods === undefined || op.periods === null ? 1 : Number(op.periods)
        for (let i = 0; i < n; i++) col[i] = i < p ? null : sorted[i - p][ci] ?? null
      } else if (op.fn === 'lead') {
        const p = op.periods === undefined || op.periods === null ? 1 : Number(op.periods)
        for (let i = 0; i < n; i++) {
          const j = i + p
          col[i] = j >= n ? null : sorted[j][ci] ?? null
        }
      } else if (op.fn === 'diff') {
        const p = op.periods === undefined || op.periods === null ? 1 : Number(op.periods)
        for (let i = 0; i < n; i++) {
          if (i < p) continue
          const c = finite(sorted[i][ci])
          const prev = finite(sorted[i - p][ci])
          if (c !== null && prev !== null) col[i] = c - prev
        }
      } else if (op.fn === 'pct_change') {
        const p = op.periods === undefined || op.periods === null ? 1 : Number(op.periods)
        for (let i = 0; i < n; i++) {
          if (i < p) continue
          const c = finite(sorted[i][ci])
          const prev = finite(sorted[i - p][ci])
          if (c !== null && prev !== null && prev !== 0) col[i] = (c - prev) / prev
        }
      } else if (op.fn === 'rolling') {
        const window = Number(op.window)
        for (let i = 0; i < n; i++) {
          const start = Math.max(0, i - window + 1)
          const nums = []
          for (let j = start; j <= i; j++) {
            const v = finite(sorted[j][ci])
            if (v !== null) nums.push(v)
          }
          if (nums.length < window) continue
          if (op.agg === 'mean') col[i] = nums.reduce((a, b) => a + b, 0) / nums.length
          else if (op.agg === 'sum') col[i] = nums.reduce((a, b) => a + b, 0)
          else if (op.agg === 'min') col[i] = Math.min(...nums)
          else col[i] = Math.max(...nums)
        }
      }
      return col
    })
    const outHeaders = [...table.headers, ...ops.map(o => o.as)]
    const outRows = sorted.map((row, i) => [...row, ...newCols.map(col => col[i])])
    const newIdVal = newId()
    writeDataset(ws, newIdVal, { headers: outHeaders, rows: outRows }, {
      sourceKind: 'series',
      sourcePath: `series:${id}`,
      label: outLabel,
      producedBy: 'series',
      parentDatasetId: id,
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
      sourceDatasetId: id,
      orderBy,
      opCount: ops.length,
    }))
    return
  }

  if (cmd === 'union') {
    const leftId = getFlag(rest, '--left')
    const rightId = getFlag(rest, '--right')
    const policy = getFlag(rest, '--columns-policy') || 'strict'
    const outLabel = getFlag(rest, '--out')
    if (!leftId || !rightId) fail(2, 'analysis/invalid-args: --left --right required')
    if (policy !== 'strict' && policy !== 'intersect') {
      fail(2, 'analysis/invalid-args: --columns-policy must be strict|intersect')
    }
    const left = loadDataset(ws, leftId)
    const right = loadDataset(ws, rightId)
    let outHeaders
    if (policy === 'strict') {
      const lset = new Set(left.table.headers)
      const rset = new Set(right.table.headers)
      const missingRight = left.table.headers.filter(h => !rset.has(h))
      const missingLeft = right.table.headers.filter(h => !lset.has(h))
      if (missingRight.length || missingLeft.length) {
        const bits = [
          ...missingRight.map(h => `left-only ${h}`),
          ...missingLeft.map(h => `right-only ${h}`),
        ]
        fail(2, `analysis/invalid-args: union column mismatch: ${bits.join(', ')}`)
      }
      outHeaders = [...left.table.headers]
    } else {
      const rset = new Set(right.table.headers)
      outHeaders = left.table.headers.filter(h => rset.has(h))
      if (!outHeaders.length) fail(2, 'analysis/invalid-args: union column mismatch: no shared columns')
    }
    const project = (tbl) => {
      const idxs = outHeaders.map(h => tbl.headers.indexOf(h))
      return tbl.rows.map(r => idxs.map(ci2 => (ci2 < 0 ? null : r[ci2])))
    }
    const lRows = project(left.table)
    const rRows = project(right.table)
    const outRows = [...lRows, ...rRows]
    const maxRows = Number(process.env.DSH_ANALYZE_MAX_ROWS || 5000000)
    if (outRows.length > maxRows) fail(3, 'analysis/limit: union result too large')
    const newIdVal = newId()
    writeDataset(ws, newIdVal, { headers: outHeaders, rows: outRows }, {
      sourceKind: 'union',
      sourcePath: `union:${leftId}+${rightId}`,
      label: outLabel,
      producedBy: 'union',
      parentDatasetId: leftId,
    })
    console.log(json({
      ok: true,
      datasetId: newIdVal,
      rowCount: outRows.length,
      leftRows: lRows.length,
      rightRows: rRows.length,
      columns: outHeaders.map((name, idx) => ({
        name,
        dtype: inferDtype(outRows.map(r => r[idx])),
      })),
    }))
    return
  }

  if (cmd === 'diff') {
    const leftId = getFlag(rest, '--left')
    const rightId = getFlag(rest, '--right')
    const onRaw = getFlag(rest, '--on')
    const rawSample = getFlag(rest, '--sample-limit')
    const requestedSample = rawSample === undefined ? 5 : Number(rawSample)
    if (!Number.isInteger(requestedSample) || requestedSample < 1) {
      fail(2, 'analysis/invalid-args: --sample-limit must be integer 1..=20')
    }
    const sampleLimit = Math.min(requestedSample, 20)
    if (!leftId || !rightId) fail(2, 'analysis/invalid-args: --left --right required')
    const left = loadDataset(ws, leftId)
    const right = loadDataset(ws, rightId)
    const warnings = []
    if (requestedSample > 20) {
      warnings.push({ code: 'clamp', field: 'sampleLimit', requested: requestedSample, actual: 20 })
    }
    const shared = left.table.headers.filter(h => right.table.headers.includes(h))
    const rowObj = (tbl, row) => Object.fromEntries(tbl.headers.map((h, i) => [h, row[i] ?? null]))
    let onlyLeft = 0, onlyRight = 0, changed = 0
    const samples = { onlyLeft: [], onlyRight: [], changed: [] }
    if (onRaw === undefined) {
      // Multiset comparison over shared columns; changed stays 0 (R-8b).
      const keyOf = (tbl, row) => JSON.stringify(shared.map(h => row[tbl.headers.indexOf(h)] ?? null))
      const rightByKey = new Map()
      right.table.rows.forEach((row, ri) => {
        const k = keyOf(right.table, row)
        if (!rightByKey.has(k)) rightByKey.set(k, [])
        rightByKey.get(k).push(ri)
      })
      const consumed = new Set()
      for (const row of left.table.rows) {
        const k = keyOf(left.table, row)
        const q = rightByKey.get(k)
        if (q && q.length) {
          consumed.add(q.shift())
        } else {
          onlyLeft++
          if (samples.onlyLeft.length < sampleLimit) samples.onlyLeft.push(rowObj(left.table, row))
        }
      }
      right.table.rows.forEach((row, ri) => {
        if (consumed.has(ri)) return
        onlyRight++
        if (samples.onlyRight.length < sampleLimit) samples.onlyRight.push(rowObj(right.table, row))
      })
    } else {
      const entries = onRaw.split(',').map(s => s.trim()).filter(Boolean)
      if (!entries.length || entries.length > 8) fail(2, 'analysis/invalid-args: --on must list 1..8 key entries')
      const seen = new Set()
      const pairs = entries.map(entry => {
        const parts = entry.split(':')
        const ln = parts.length === 2 ? parts[0] : entry
        const rn = parts.length === 2 ? parts[1] : entry
        if (parts.length > 2 || !ln || !rn) fail(2, 'analysis/invalid-args: on pairs must be leftKey:rightKey')
        if (seen.has(ln)) fail(2, `analysis/invalid-args: duplicate join key ${ln}`)
        seen.add(ln)
        const li = left.table.headers.indexOf(ln)
        if (li < 0) fail(2, `analysis/invalid-args: unknown column ${ln} on left`)
        const ri = right.table.headers.indexOf(rn)
        if (ri < 0) fail(2, `analysis/invalid-args: unknown column ${rn} on right`)
        return { li, ri, ln }
      })
      const keyLeftNames = new Set(pairs.map(p => p.ln))
      const nonKeyShared = shared.filter(h => !keyLeftNames.has(h))
      const rightByKey = new Map()
      right.table.rows.forEach((row, ri) => {
        const key = pairs.map(p => row[p.ri] ?? null)
        if (key.some(v => v === null || v === undefined)) return // null keys never match
        const k = JSON.stringify(key)
        if (!rightByKey.has(k)) rightByKey.set(k, [])
        rightByKey.get(k).push(ri)
      })
      const consumed = new Set()
      for (const row of left.table.rows) {
        const key = pairs.map(p => row[p.li] ?? null)
        const q = key.some(v => v === null || v === undefined) ? null : rightByKey.get(JSON.stringify(key))
        if (!q || !q.length) {
          onlyLeft++
          if (samples.onlyLeft.length < sampleLimit) samples.onlyLeft.push(rowObj(left.table, row))
          continue
        }
        const ri = q.shift()
        consumed.add(ri)
        const rrow = right.table.rows[ri]
        const diffColumns = []
        for (const h of nonKeyShared) {
          const li = left.table.headers.indexOf(h)
          const rj = right.table.headers.indexOf(h)
          if ((row[li] ?? null) !== (rrow[rj] ?? null)) diffColumns.push(h)
        }
        if (diffColumns.length) {
          changed++
          if (samples.changed.length < sampleLimit) {
            samples.changed.push({
              key: Object.fromEntries(pairs.map(p => [p.ln, row[p.li] ?? null])),
              diffColumns,
              left: rowObj(left.table, row),
              right: rowObj(right.table, rrow),
            })
          }
        }
      }
      right.table.rows.forEach((row, ri) => {
        if (consumed.has(ri)) return
        onlyRight++
        if (samples.onlyRight.length < sampleLimit) samples.onlyRight.push(rowObj(right.table, row))
      })
    }
    console.log(json({
      ok: true,
      datasetId: newId(),
      onlyLeft,
      onlyRight,
      changed,
      samples,
      ...(warnings.length ? { warnings } : {}),
    }))
    return
  }

  if (cmd === 'pivot') {
    const id = getFlag(rest, '--dataset-id')
    const indexRaw = getFlag(rest, '--index')
    const columnsName = (getFlag(rest, '--columns') || '').trim()
    const valuesName = (getFlag(rest, '--values') || '').trim()
    const agg = getFlag(rest, '--agg')
    const outLabel = getFlag(rest, '--out')
    if (!id || !indexRaw || !columnsName || !valuesName || !agg) {
      fail(2, 'analysis/invalid-args: --dataset-id --index --columns --values --agg required')
    }
    if (!['sum', 'mean', 'min', 'max', 'count'].includes(agg)) {
      fail(2, 'analysis/invalid-args: --agg must be sum|mean|min|max|count')
    }
    const { table } = loadDataset(ws, id)
    const indexNames = indexRaw.split(',').map(s => s.trim()).filter(Boolean)
    if (!indexNames.length || indexNames.length > 16) {
      fail(2, 'analysis/invalid-args: --index must list 1..16 columns')
    }
    const indexIdx = indexNames.map(name => {
      const i = table.headers.indexOf(name)
      if (i < 0) fail(2, `analysis/invalid-args: unknown column ${name}`)
      return i
    })
    const pcol = table.headers.indexOf(columnsName)
    if (pcol < 0) fail(2, `analysis/invalid-args: unknown column ${columnsName}`)
    const vcol = table.headers.indexOf(valuesName)
    if (vcol < 0) fail(2, `analysis/invalid-args: unknown column ${valuesName}`)
    // Distinct pivot values in first-seen order; >50 → analysis/limit (R-16b).
    const pivotKeys = []
    const pivotNames = new Map()
    for (const row of table.rows) {
      const v = row[pcol]
      const k = JSON.stringify(v ?? null)
      if (!pivotNames.has(k)) {
        if (pivotKeys.length >= 50) fail(3, 'analysis/limit: pivot columns exceed 50')
        pivotKeys.push(k)
        pivotNames.set(k, v === null || v === undefined ? 'null' : String(v))
      }
    }
    const rowKeys = []
    const rowGroups = new Map()
    for (const row of table.rows) {
      const rk = JSON.stringify(indexIdx.map(ci2 => row[ci2] ?? null))
      if (!rowGroups.has(rk)) {
        rowGroups.set(rk, { key: indexIdx.map(ci2 => row[ci2] ?? null), cells: new Map() })
        rowKeys.push(rk)
      }
      const pk = JSON.stringify(row[pcol] ?? null)
      const g = rowGroups.get(rk)
      if (!g.cells.has(pk)) g.cells.set(pk, [])
      g.cells.get(pk).push(row[vcol] === undefined ? null : row[vcol])
    }
    const outHeaders = [...indexNames, ...pivotKeys.map(k => pivotNames.get(k))]
    const outRows = []
    for (const rk of rowKeys) {
      const g = rowGroups.get(rk)
      const row = [...g.key]
      for (const pk of pivotKeys) {
        const vals = g.cells.get(pk) ?? []
        if (!vals.length) {
          row.push(null) // missing cell → Null
          continue
        }
        if (agg === 'count') {
          row.push(vals.length)
          continue
        }
        const nums = vals.filter(v => typeof v === 'number')
        let cell = null
        if (nums.length) {
          if (agg === 'sum') cell = nums.reduce((x, y) => x + y, 0)
          else if (agg === 'mean') cell = nums.reduce((x, y) => x + y, 0) / nums.length
          else if (agg === 'min') cell = Math.min(...nums)
          else if (agg === 'max') cell = Math.max(...nums)
        }
        row.push(cell)
      }
      outRows.push(row)
    }
    const maxRows = Number(process.env.DSH_ANALYZE_MAX_ROWS || 5000000)
    if (outRows.length > maxRows) fail(3, 'analysis/limit: pivot result too large')
    const newIdVal = newId()
    writeDataset(ws, newIdVal, { headers: outHeaders, rows: outRows }, {
      sourceKind: 'pivot',
      sourcePath: `pivot:${id}`,
      label: outLabel,
      producedBy: 'pivot',
      parentDatasetId: id,
    })
    console.log(json({
      ok: true,
      datasetId: newIdVal,
      rowCount: outRows.length,
      columns: outHeaders.map((name, idx) => ({
        name,
        dtype: inferDtype(outRows.map(r => r[idx])),
      })),
      indexColumns: indexNames,
      pivotColumn: columnsName,
    }))
    return
  }

  fail(2, `analysis/invalid-args: unknown command ${cmd}`)
}

main().catch((e) => fail(1, String(e?.message || e)))
