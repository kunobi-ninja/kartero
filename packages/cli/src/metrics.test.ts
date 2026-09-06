import assert from 'node:assert/strict'
import { mkdtemp, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { test } from 'node:test'
import { bucketise, buildMetricsOtlp, writeArtifact, type MetricPoint } from './metrics.js'

const OPTIONS = {
  resource: { 'service.namespace': 'kunobi', 'service.name': 'github-actions-ci' },
  scope: { name: 'ci-metrics-collector', version: '1' },
}

const AT = new Date('2026-09-06T12:00:00Z')

function point(overrides: Partial<MetricPoint> = {}): MetricPoint {
  return {
    metric: 'ci.run.attempts',
    instrument: 'counter',
    unit: '1',
    value: 1,
    attributes: { branch_class: 'trunk_dev' },
    observedAt: AT,
    ...overrides,
  }
}

function metrics(payload: ReturnType<typeof buildMetricsOtlp>): Record<string, any>[] {
  return payload.resourceMetrics[0]?.scopeMetrics[0]?.metrics as Record<string, any>[]
}

test('a counter is a monotonic delta sum with an integer value', () => {
  const built = metrics(buildMetricsOtlp([point()], OPTIONS))[0]
  assert.equal(built?.name, 'ci.run.attempts')
  assert.equal(built?.sum.aggregationTemporality, 1)
  assert.equal(built?.sum.isMonotonic, true)
  assert.equal(built?.sum.dataPoints[0].asInt, '1')
  assert.equal(built?.sum.dataPoints[0].timeUnixNano, '1788696000000000000')
})

test('a gauge carries a double and no temporality', () => {
  const built = metrics(buildMetricsOtlp([point({ metric: 'ci.collector.last_success', instrument: 'gauge', value: 12.5 })], OPTIONS))[0]
  assert.equal(built?.gauge.dataPoints[0].asDouble, 12.5)
  assert.equal(built?.sum, undefined)
})

test('a histogram buckets its observation against explicit bounds', () => {
  const built = metrics(
    buildMetricsOtlp([point({ metric: 'ci.job.duration', instrument: 'histogram', unit: 's', value: 42, bounds: [30, 60] })], OPTIONS)
  )[0]
  const dp = built?.histogram.dataPoints[0]
  assert.equal(dp.count, '1')
  assert.equal(dp.sum, 42)
  assert.deepEqual(dp.explicitBounds, [30, 60])
  // One more count than bound, and 42 lands in the 30..60 bucket.
  assert.deepEqual(dp.bucketCounts, ['0', '1', '0'])
  assert.equal(dp.bucketCounts.length, dp.explicitBounds.length + 1)
})

test('cumulative is available when a producer means it', () => {
  const built = metrics(buildMetricsOtlp([point({ temporality: 'cumulative' })], OPTIONS))[0]
  assert.equal(built?.sum.aggregationTemporality, 2)
})

test('points keep their own instants, so one artifact spans a sweep', () => {
  const older = new Date('2026-09-03T09:00:00Z')
  const built = metrics(buildMetricsOtlp([point(), point({ observedAt: older })], OPTIONS))[0]
  const stamps = built?.sum.dataPoints.map((p: Record<string, string>) => p.timeUnixNano)
  assert.equal(new Set(stamps).size, 2)
})

test('one metric cannot be two instruments or two units', () => {
  assert.throws(() => buildMetricsOtlp([point(), point({ instrument: 'gauge' })], OPTIONS), /both counter and gauge/)
  assert.throws(() => buildMetricsOtlp([point(), point({ unit: 's' })], OPTIONS), /both 1 and s/)
})

test('a histogram without usable bounds is refused', () => {
  const histogram = { metric: 'ci.job.duration', instrument: 'histogram' as const, unit: 's' }
  assert.throws(() => buildMetricsOtlp([point({ ...histogram })], OPTIONS), /needs explicit bounds/)
  assert.throws(() => buildMetricsOtlp([point({ ...histogram, bounds: [60, 30] })], OPTIONS), /must ascend/)
  assert.throws(() => buildMetricsOtlp([point({ ...histogram, bounds: [Number.POSITIVE_INFINITY] })], OPTIONS), /non-finite/)
})

test('Kartero-stamped identity is refused wherever it appears', () => {
  assert.throws(() => buildMetricsOtlp([point({ attributes: { 'vcs.ref.head.name': 'dev' } })], OPTIONS), /reserved for Kartero/)
  assert.throws(
    () => buildMetricsOtlp([point()], { ...OPTIONS, resource: { 'cicd.pipeline.name': 'CI' } }),
    /reserved for Kartero/
  )
})

test('an empty sweep is refused rather than written as an empty artifact', () => {
  assert.throws(() => buildMetricsOtlp([], OPTIONS), /at least one point/)
})

test('bucketise puts an overflow observation in the last bucket', () => {
  assert.deepEqual(bucketise(5, [10, 20]), ['1', '0', '0'])
  assert.deepEqual(bucketise(15, [10, 20]), ['0', '1', '0'])
  assert.deepEqual(bucketise(99, [10, 20]), ['0', '0', '1'])
  assert.throws(() => bucketise(Number.NaN, [10]), /non-finite/)
})

test('writeArtifact emits both files and refuses to overwrite', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'kartero-metrics-'))
  try {
    const payload = buildMetricsOtlp([point()], OPTIONS)
    await writeArtifact(directory, payload)
    assert.equal((await readFile(join(directory, 'schema_version'), 'utf8')).trim(), '1')
    const body = JSON.parse(await readFile(join(directory, 'metrics.otlp.json'), 'utf8'))
    assert.equal(body.resourceMetrics[0].scopeMetrics[0].scope.name, 'ci-metrics-collector')
    await assert.rejects(writeArtifact(directory, payload), /EEXIST/)
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})
