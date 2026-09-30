import assert from 'node:assert/strict'
import { mkdtemp, readFile, rm, writeFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { test } from 'node:test'
import { BOUNDS, validateArtifactDirectory } from './validate.js'

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

// The collector's half of this is `structure_bounds_match_the_ones_the_cli_checks`
// in tests/cli_contract.rs.
test('checks the bounds the collector enforces', async () => {
  const contract = JSON.parse(await readFile('../../fixtures/contract/bounds.json', 'utf8'))
  assert.deepEqual({ ...BOUNDS }, contract)
})

const attribute = (n: number) => ({ key: `k${n}`, value: { stringValue: 'v' } })
const scope = { scope: { name: 'test', version: '1' }, metrics: [gauge] }
const resourceOf = (scopes: unknown[], attributes: unknown[] = []) => ({ resource: { attributes }, scopeMetrics: scopes })
const times = <T>(n: number, make: (index: number) => T): T[] => Array.from({ length: n }, (_, index) => make(index))
const histogram = (buckets: number) => ({
  name: 'ci.job.duration',
  histogram: {
    aggregationTemporality: 1,
    dataPoints: [
      {
        count: '1',
        sum: 1,
        bucketCounts: times(buckets, () => '0'),
        explicitBounds: times(buckets - 1, (index) => index),
        attributes: [],
      },
    ],
  },
})

// Each entry is a payload exactly at a bound, which must pass, and the same
// payload one past it, which the collector refuses whole.
const atTheBound: [string, (n: number) => unknown, number, RegExp][] = [
  ['resourceMetrics entries', (n) => ({ resourceMetrics: times(n, () => resourceOf([scope])) }), BOUNDS.resourceMetrics, /5 resourceMetrics entries; the collector accepts 4/],
  ['scopes in a resource', (n) => ({ resourceMetrics: [resourceOf(times(n, () => scope))] }), BOUNDS.scopesPerResource, /9 scopeMetrics entries; the collector accepts 8/],
  ['resource attributes', (n) => ({ resourceMetrics: [resourceOf([scope], times(n, attribute))] }), BOUNDS.attributes, /resource has 33 attributes; the collector accepts 32/],
  [
    'point attributes',
    (n) => body([{ ...gauge, gauge: { dataPoints: [{ asDouble: 1, attributes: times(n, attribute) }] } }]),
    BOUNDS.attributes,
    /ci\.coverage\.percent has 33 attributes; the collector accepts 32/,
  ],
  ['histogram buckets', (n) => body([histogram(n)]), BOUNDS.bucketsPerPoint, /65 buckets; the collector accepts 64/],
]

for (const [what, payload, bound, refusal] of atTheBound) {
  test(`accepts ${bound} ${what} and refuses one more`, async () => {
    const within = await artifact(payload(bound))
    const beyond = await artifact(payload(bound + 1))
    try {
      await validateArtifactDirectory(within)
      await assert.rejects(validateArtifactDirectory(beyond), refusal)
    } finally {
      await rm(within, { recursive: true, force: true })
      await rm(beyond, { recursive: true, force: true })
    }
  })
}

test('refuses a metrics file over the byte bound before parsing it', async () => {
  const directory = await mkdtemp(join(tmpdir(), 'kartero-validate-'))
  await writeFile(join(directory, 'schema_version'), '1\n')
  // Not JSON: a parse error here would mean the size was checked too late.
  await writeFile(join(directory, 'metrics.otlp.json'), Buffer.alloc(BOUNDS.jsonBytes + 1, 0x20))
  try {
    await assert.rejects(validateArtifactDirectory(directory), /16777217 bytes; the collector accepts 16777216/)
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
})
