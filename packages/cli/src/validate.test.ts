import assert from 'node:assert/strict'
import { mkdtemp, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { test } from 'node:test'
import { validateArtifactDirectory } from './validate.js'

async function artifact(body: unknown, schema = '1'): Promise<string> {
  const directory = await mkdtemp(join(tmpdir(), 'kartero-validate-'))
  await writeFile(join(directory, 'schema_version'), `${schema}\n`)
  await writeFile(join(directory, 'metrics.otlp.json'), JSON.stringify(body))
  return directory
}

function body(metrics: unknown[], resourceAttributes: unknown[] = []): unknown {
  return {
    resourceMetrics: [
      {
        resource: { attributes: resourceAttributes },
        scopeMetrics: [{ scope: { name: 'test', version: '1' }, metrics }],
      },
    ],
  }
}

const gauge = {
  name: 'ci.coverage.percent',
  unit: '%',
  gauge: { dataPoints: [{ asDouble: 88.5, timeUnixNano: '1', attributes: [] }] },
}

async function rejects(metrics: unknown[], fragment: string): Promise<void> {
  const directory = await artifact(body(metrics))
  try {
    await assert.rejects(validateArtifactDirectory(directory), (error: Error) => {
      assert.match(error.message, new RegExp(fragment))
      return true
    })
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
}

test('accepts a gauge, a sum and a histogram', async () => {
  const directory = await artifact(
    body([
      gauge,
      {
        name: 'kache.cache.uploads',
        sum: {
          aggregationTemporality: 'AGGREGATION_TEMPORALITY_CUMULATIVE',
          isMonotonic: true,
          dataPoints: [{ asInt: '4', timeUnixNano: '1', attributes: [] }],
        },
      },
      {
        name: 'ci.job.duration',
        histogram: {
          aggregationTemporality: 1,
          dataPoints: [
            { count: '1', sum: 12, bucketCounts: ['0', '1'], explicitBounds: [30], timeUnixNano: '1', attributes: [] },
          ],
        },
      },
    ])
  )
  try {
    await validateArtifactDirectory(directory)
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})

test('refuses an instrument the collector will not deliver', async () => {
  await rejects([{ name: 'ci.job.duration', summary: { dataPoints: [{ count: '1' }] } }], 'delivers nothing else')
})

test('refuses two instruments on one metric', async () => {
  await rejects([{ ...gauge, sum: { aggregationTemporality: 2, dataPoints: [{ asInt: '1' }] } }], 'more than one instrument')
})

test('refuses a sum without a temporality', async () => {
  await rejects([{ name: 'kache.cache.uploads', sum: { dataPoints: [{ asInt: '1', attributes: [] }] } }], 'aggregationTemporality')
})

test('refuses a histogram whose buckets do not match its bounds', async () => {
  await rejects(
    [
      {
        name: 'ci.job.duration',
        histogram: {
          aggregationTemporality: 1,
          dataPoints: [{ count: '1', sum: 12, bucketCounts: ['0', '1'], explicitBounds: [30, 60], attributes: [] }],
        },
      },
    ],
    'bucket counts'
  )
})

test('refuses an empty data point list', async () => {
  await rejects([{ name: 'ci.coverage.percent', gauge: { dataPoints: [] } }], 'no gauge data points')
})

test('refuses producer-supplied Kartero identity on a point', async () => {
  await rejects(
    [
      {
        name: 'ci.coverage.percent',
        gauge: { dataPoints: [{ asDouble: 1, attributes: [{ key: 'vcs.repository.url.full', value: { stringValue: 'x' } }] }] },
      },
    ],
    'reserved for Kartero'
  )
})

test('refuses producer-supplied Kartero identity on the resource', async () => {
  const directory = await artifact(body([gauge], [{ key: 'cicd.pipeline.name', value: { stringValue: 'x' } }]))
  try {
    await assert.rejects(validateArtifactDirectory(directory), /reserved for Kartero/)
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})

test('refuses an unsupported schema version', async () => {
  const directory = await artifact(body([gauge]), '2')
  try {
    await assert.rejects(validateArtifactDirectory(directory), /unsupported schema_version/)
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})
