export const ARTIFACT_SCHEMA_VERSION = 1

export type CoverageFormat = 'auto' | 'istanbul' | 'llvm-cov-json' | 'lcov'
export type CoverageKind = 'lines' | 'statements' | 'functions' | 'branches'

export interface CoverageValue {
  covered: number
  total: number
  percent: number
}

export interface CoverageSnapshot {
  language: string
  values: Partial<Record<CoverageKind, CoverageValue>>
}

export interface CoverageOtlpOptions {
  branchClass: string
  serviceName: string
  serviceNamespace: string
  timestamp: Date
}

interface OtlpAttribute {
  key: string
  value: { stringValue: string }
}

function finiteNumber(value: unknown, label: string): number {
  if (typeof value !== 'number' || !Number.isFinite(value)) {
    throw new Error(`${label} must be a finite number`)
  }
  return value
}

function coverageValue(coveredValue: unknown, totalValue: unknown, percentValue: unknown, label: string): CoverageValue {
  const covered = finiteNumber(coveredValue, `${label}.covered`)
  const total = finiteNumber(totalValue, `${label}.total`)
  if (!Number.isInteger(covered) || !Number.isInteger(total) || covered < 0 || total < 0 || covered > total) {
    throw new Error(`${label} coverage counts are invalid`)
  }

  const calculated = total === 0 ? 100 : (covered / total) * 100
  const percent = typeof percentValue === 'number' && Number.isFinite(percentValue) ? percentValue : calculated
  if (percent < 0 || percent > 100) throw new Error(`${label}.percent must be between 0 and 100`)
  return { covered, total, percent }
}

function parseIstanbul(value: unknown, language: string): CoverageSnapshot {
  const total = (value as { total?: Record<string, unknown> } | null)?.total
  if (total === undefined || total === null || typeof total !== 'object') {
    throw new Error('Istanbul report is missing total coverage')
  }

  const values: Partial<Record<CoverageKind, CoverageValue>> = {}
  for (const kind of ['lines', 'statements', 'functions', 'branches'] as const) {
    const entry = total[kind] as { covered?: unknown; total?: unknown; pct?: unknown } | undefined
    if (entry === undefined) throw new Error(`Istanbul report is missing total.${kind}`)
    values[kind] = coverageValue(entry.covered, entry.total, entry.pct, kind)
  }
  return { language, values }
}

function parseLlvmCovJson(value: unknown, language: string): CoverageSnapshot {
  const report = value as {
    data?: { totals?: Record<string, { count?: unknown; covered?: unknown; percent?: unknown }> }[]
  } | null
  const totals = report?.data?.[0]?.totals
  if (totals === undefined) throw new Error('LLVM coverage report is missing data[0].totals')

  const values: Partial<Record<CoverageKind, CoverageValue>> = {}
  for (const kind of ['lines', 'functions', 'branches'] as const) {
    const entry = totals[kind]
    if (entry !== undefined) values[kind] = coverageValue(entry.covered, entry.count, entry.percent, kind)
  }
  if (values.lines === undefined) throw new Error('LLVM coverage report is missing line totals')
  return { language, values }
}

interface LcovFile {
  lines: Map<number, number>
  functions: Map<string, number>
  branches: Map<string, number>
}

function parseLcov(text: string, language: string): CoverageSnapshot {
  const files = new Map<string, LcovFile>()
  let current: LcovFile | undefined

  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim()
    if (line.startsWith('SF:')) {
      const path = line.slice(3)
      if (path === '') throw new Error('LCOV contains an empty SF record')
      current = files.get(path) ?? { lines: new Map(), functions: new Map(), branches: new Map() }
      files.set(path, current)
      continue
    }
    if (line === 'end_of_record') {
      current = undefined
      continue
    }
    if (current === undefined) continue

    if (line.startsWith('DA:')) {
      const [lineNumberRaw, hitsRaw] = line.slice(3).split(',')
      const lineNumber = Number(lineNumberRaw)
      const hits = Number(hitsRaw)
      if (!Number.isInteger(lineNumber) || lineNumber <= 0 || !Number.isFinite(hits) || hits < 0) {
        throw new Error(`invalid LCOV line record: ${line}`)
      }
      current.lines.set(lineNumber, Math.max(current.lines.get(lineNumber) ?? 0, hits))
      continue
    }
    if (line.startsWith('FN:')) {
      const separator = line.indexOf(',')
      if (separator > 3) current.functions.set(line.slice(separator + 1), current.functions.get(line.slice(separator + 1)) ?? 0)
      continue
    }
    if (line.startsWith('FNDA:')) {
      const separator = line.indexOf(',')
      const hits = Number(line.slice(5, separator))
      const name = line.slice(separator + 1)
      if (separator <= 5 || name === '' || !Number.isFinite(hits) || hits < 0) {
        throw new Error(`invalid LCOV function record: ${line}`)
      }
      current.functions.set(name, Math.max(current.functions.get(name) ?? 0, hits))
      continue
    }
    if (line.startsWith('BRDA:')) {
      const [lineNumber, block, branch, takenRaw] = line.slice(5).split(',')
      if (!lineNumber || block === undefined || branch === undefined || takenRaw === undefined) {
        throw new Error(`invalid LCOV branch record: ${line}`)
      }
      const hits = takenRaw === '-' ? 0 : Number(takenRaw)
      if (!Number.isFinite(hits) || hits < 0) throw new Error(`invalid LCOV branch record: ${line}`)
      const key = `${lineNumber}:${block}:${branch}`
      current.branches.set(key, Math.max(current.branches.get(key) ?? 0, hits))
    }
  }

  if (files.size === 0) throw new Error('LCOV report has no source files')
  const values: Partial<Record<CoverageKind, CoverageValue>> = {}
  const aggregate = (select: (file: LcovFile) => Map<unknown, number>, kind: CoverageKind): void => {
    let covered = 0
    let total = 0
    for (const file of files.values()) {
      for (const hits of select(file).values()) {
        total += 1
        if (hits > 0) covered += 1
      }
    }
    if (total > 0) values[kind] = coverageValue(covered, total, undefined, kind)
  }
  aggregate((file) => file.lines, 'lines')
  aggregate((file) => file.functions, 'functions')
  aggregate((file) => file.branches, 'branches')
  if (values.lines === undefined) throw new Error('LCOV report has no DA line records')
  return { language, values }
}

export function parseCoverage(text: string, format: CoverageFormat = 'auto', language?: string): CoverageSnapshot {
  let selected = format
  let json: unknown
  if (format === 'auto' || format === 'istanbul' || format === 'llvm-cov-json') {
    try {
      json = JSON.parse(text)
    } catch (error) {
      if (format !== 'auto') throw new Error(`coverage report is not valid JSON: ${String(error)}`)
    }
  }

  if (selected === 'auto') {
    const object = json as Record<string, unknown> | undefined
    if (object?.total !== undefined) selected = 'istanbul'
    else if (object?.type === 'llvm.coverage.json.export' || object?.data !== undefined) selected = 'llvm-cov-json'
    else if (/^SF:/m.test(text) && /^end_of_record\s*$/m.test(text)) selected = 'lcov'
    else throw new Error('could not detect coverage format; use --format')
  }

  if (selected === 'istanbul') return parseIstanbul(json, language ?? 'typescript')
  if (selected === 'llvm-cov-json') return parseLlvmCovJson(json, language ?? 'rust')
  return parseLcov(text, language ?? 'unknown')
}

function attribute(key: string, value: string): OtlpAttribute {
  return { key, value: { stringValue: value } }
}

export function buildCoverageOtlp(snapshot: CoverageSnapshot, options: CoverageOtlpOptions): unknown {
  if (!Number.isFinite(options.timestamp.getTime())) throw new Error('timestamp is invalid')
  const timeUnixNano = String(BigInt(options.timestamp.getTime()) * 1_000_000n)
  const points = Object.entries(snapshot.values) as [CoverageKind, CoverageValue][]
  if (points.length === 0) throw new Error('coverage snapshot has no values')

  const attributesFor = (kind: CoverageKind): OtlpAttribute[] => [
    attribute('language', snapshot.language),
    attribute('kind', kind),
    attribute('branch_class', options.branchClass),
  ]
  const metrics = [
    {
      name: 'ci.coverage.percent',
      unit: '%',
      gauge: {
        dataPoints: points.map(([kind, value]) => ({ asDouble: value.percent, timeUnixNano, attributes: attributesFor(kind) })),
      },
    },
    {
      name: 'ci.coverage.covered',
      unit: '{item}',
      gauge: {
        dataPoints: points.map(([kind, value]) => ({ asInt: String(value.covered), timeUnixNano, attributes: attributesFor(kind) })),
      },
    },
    {
      name: 'ci.coverage.total',
      unit: '{item}',
      gauge: {
        dataPoints: points.map(([kind, value]) => ({ asInt: String(value.total), timeUnixNano, attributes: attributesFor(kind) })),
      },
    },
  ]

  return {
    resourceMetrics: [
      {
        resource: {
          attributes: [
            attribute('service.namespace', options.serviceNamespace),
            attribute('service.name', options.serviceName),
            attribute('deployment.environment', 'ci'),
            attribute('telemetry.plane', 'engineering'),
          ],
        },
        scopeMetrics: [{ scope: { name: '@kunobi/kartero', version: String(ARTIFACT_SCHEMA_VERSION) }, metrics }],
      },
    ],
  }
}
