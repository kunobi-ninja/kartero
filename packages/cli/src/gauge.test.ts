import assert from 'node:assert/strict'
import { test } from 'node:test'
import { buildGaugeOtlp, parseAttribute } from './gauge.js'

test('builds an integer gauge with producer attributes', () => {
  const payload = buildGaugeOtlp('kache.bench.verdict.ok', '0', {
    attributes: [
      ['kache.bench.project', 'bench-firefox'],
      ['kache.bench.cache_tool', 'kache'],
    ],
    serviceName: 'github-actions-ci',
    serviceNamespace: 'kunobi',
    timestamp: new Date('2026-08-28T12:00:00Z'),
    unit: '1',
  }) as {
    resourceMetrics: { scopeMetrics: { metrics: { gauge: { dataPoints: Record<string, unknown>[] } }[] }[] }[]
  }
  const point = payload.resourceMetrics[0]?.scopeMetrics[0]?.metrics[0]?.gauge.dataPoints[0]
  assert.equal(point?.asInt, '0')
  assert.equal(point?.timeUnixNano, '1787918400000000000')
  assert.deepEqual(point?.attributes, [
    { key: 'kache.bench.project', value: { stringValue: 'bench-firefox' } },
    { key: 'kache.bench.cache_tool', value: { stringValue: 'kache' } },
  ])
})

test('builds a floating-point gauge', () => {
  const payload = buildGaugeOtlp('example.ratio', '0.5', {
    attributes: [],
    serviceName: 'github-actions-ci',
    serviceNamespace: 'kunobi',
    timestamp: new Date('2026-08-28T12:00:00Z'),
    unit: '1',
  }) as {
    resourceMetrics: { scopeMetrics: { metrics: { gauge: { dataPoints: Record<string, unknown>[] } }[] }[] }[]
  }
  assert.equal(payload.resourceMetrics[0]?.scopeMetrics[0]?.metrics[0]?.gauge.dataPoints[0]?.asDouble, 0.5)
})

test('parses values containing equals signs', () => {
  assert.deepEqual(parseAttribute('example.key=a=b'), ['example.key', 'a=b'])
})

test('rejects invalid values and trusted-envelope attributes', () => {
  const options = {
    attributes: [] as ReadonlyArray<readonly [string, string]>,
    serviceName: 'github-actions-ci',
    serviceNamespace: 'kunobi',
    timestamp: new Date('2026-08-28T12:00:00Z'),
    unit: '1',
  }
  assert.throws(() => buildGaugeOtlp('example.metric', 'NaN', options), /finite number/)
  assert.throws(
    () => buildGaugeOtlp('example.metric', '0', { ...options, attributes: [['cicd.pipeline.name', 'forged']] }),
    /reserved for Kartero/
  )
  assert.throws(() => parseAttribute('missing-separator'), /KEY=VALUE/)
})
