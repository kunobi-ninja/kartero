import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import { test } from 'node:test'
import { buildCoverageOtlp, parseCoverage } from './coverage.js'

test('parses Istanbul summary coverage', () => {
  const snapshot = parseCoverage(
    JSON.stringify({
      total: {
        lines: { total: 100, covered: 87, pct: 87 },
        statements: { total: 110, covered: 88, pct: 80 },
        functions: { total: 20, covered: 15, pct: 75 },
        branches: { total: 40, covered: 28, pct: 70 },
      },
    })
  )
  assert.equal(snapshot.language, 'typescript')
  assert.deepEqual(snapshot.values.lines, { total: 100, covered: 87, percent: 87 })
  assert.deepEqual(snapshot.values.branches, { total: 40, covered: 28, percent: 70 })
})

test('parses LLVM JSON coverage', () => {
  const snapshot = parseCoverage(
    JSON.stringify({
      type: 'llvm.coverage.json.export',
      data: [{ totals: { lines: { count: 200, covered: 179, percent: 89.5 } } }],
    })
  )
  assert.deepEqual(snapshot.values.lines, { total: 200, covered: 179, percent: 89.5 })
})

test('parses LCOV lines, functions, and branches', () => {
  const snapshot = parseCoverage(
    ['SF:/src/a.ts', 'FN:1,one', 'FNDA:1,one', 'DA:1,1', 'DA:2,0', 'BRDA:2,0,0,1', 'BRDA:2,0,1,-', 'end_of_record'].join('\n'),
    'lcov',
    'typescript'
  )
  assert.deepEqual(snapshot.values.lines, { total: 2, covered: 1, percent: 50 })
  assert.deepEqual(snapshot.values.functions, { total: 1, covered: 1, percent: 100 })
  assert.deepEqual(snapshot.values.branches, { total: 2, covered: 1, percent: 50 })
})

test('builds the allowlisted OTLP metric contract', async () => {
  const snapshot = parseCoverage(
    JSON.stringify({
      type: 'llvm.coverage.json.export',
      data: [{ totals: { lines: { count: 200, covered: 179, percent: 89.5 } } }],
    })
  )
  const payload = buildCoverageOtlp(snapshot, {
    branchClass: 'trunk_dev',
    serviceName: 'github-actions-ci',
    serviceNamespace: 'kunobi',
    timestamp: new Date('2026-08-28T12:00:00Z'),
  }) as {
    resourceMetrics: { scopeMetrics: { metrics: { name: string; gauge: { dataPoints: unknown[] } }[] }[] }[]
  }
  const metrics = payload.resourceMetrics[0]?.scopeMetrics[0]?.metrics
  assert.deepEqual(metrics?.map((metric) => metric.name), ['ci.coverage.percent', 'ci.coverage.covered', 'ci.coverage.total'])
  assert.equal(metrics?.every((metric) => metric.gauge.dataPoints.length === 1), true)
  const expected = JSON.parse(await readFile('../../fixtures/coverage/expected/metrics.otlp.json', 'utf8'))
  assert.deepEqual(payload, expected)
})

test('rejects malformed coverage counts', () => {
  assert.throws(
    () =>
      parseCoverage(
        JSON.stringify({
          type: 'llvm.coverage.json.export',
          data: [{ totals: { lines: { count: 10, covered: 12, percent: 120 } } }],
        })
      ),
    /coverage counts are invalid/
  )
})
