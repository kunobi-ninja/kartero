import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { ARTIFACT_SCHEMA_VERSION } from './coverage.js'

/**
 * The instruments the collector delivers. Anything else is dropped at import,
 * which is why the list is checked here: a producer that only finds out from a
 * missing dashboard panel has no way back to the cause.
 */
const INSTRUMENTS = ['gauge', 'sum', 'histogram'] as const

type Instrument = (typeof INSTRUMENTS)[number]

interface OtlpAttribute {
  key?: unknown
}

interface OtlpPoint {
  attributes?: unknown
  bucketCounts?: unknown
  explicitBounds?: unknown
}

type OtlpMetric = { name?: unknown } & Partial<Record<Instrument, { dataPoints?: unknown; aggregationTemporality?: unknown }>>

interface OtlpBody {
  resourceMetrics?: {
    resource?: { attributes?: unknown }
    scopeMetrics?: { metrics?: unknown }[]
  }[]
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value)
}

/**
 * Kartero stamps repository and pipeline identity from the trusted GitHub run,
 * so a producer-supplied one is discarded at import. Refusing it here means the
 * producer finds out while it can still fix the payload.
 */
function checkAttributes(value: unknown, where: string): void {
  if (value === undefined) return
  if (!Array.isArray(value)) throw new Error(`${where} has a non-array attributes field`)
  for (const attribute of value as OtlpAttribute[]) {
    const key = isRecord(attribute) ? attribute.key : undefined
    if (typeof key !== 'string' || key === '') throw new Error(`${where} has an attribute without a key`)
    if (key.startsWith('cicd.') || key.startsWith('vcs.')) {
      throw new Error(`${where} carries ${key}, which is reserved for Kartero`)
    }
  }
}

function instrumentOf(metric: OtlpMetric, label: string): Instrument {
  const present = INSTRUMENTS.filter((instrument) => metric[instrument] !== undefined)
  if (present.length === 0) {
    throw new Error(`${label} carries no gauge, sum or histogram; Kartero delivers nothing else`)
  }
  if (present.length > 1) throw new Error(`${label} carries more than one instrument: ${present.join(', ')}`)
  return present[0] as Instrument
}

function checkTemporality(temporality: unknown, label: string): void {
  const named = temporality === 'AGGREGATION_TEMPORALITY_DELTA' || temporality === 'AGGREGATION_TEMPORALITY_CUMULATIVE'
  const numbered = temporality === 1 || temporality === 2
  if (!named && !numbered) {
    throw new Error(`${label} must declare a delta or cumulative aggregationTemporality`)
  }
}

/**
 * OTLP requires exactly one more bucket count than bound. A mismatch is
 * accepted by most backends and misdescribes every observation rather than
 * being refused, so it has to be caught before upload.
 */
function checkBuckets(point: OtlpPoint, label: string): void {
  const counts = point.bucketCounts
  const bounds = point.explicitBounds
  if (!Array.isArray(counts) || !Array.isArray(bounds)) {
    throw new Error(`${label} is missing bucketCounts or explicitBounds`)
  }
  if (counts.length !== bounds.length + 1) {
    throw new Error(`${label} has ${counts.length} bucket counts for ${bounds.length} bounds`)
  }
}

export async function validateArtifactDirectory(directory: string): Promise<void> {
  const schema = (await readFile(join(directory, 'schema_version'), 'utf8')).trim()
  if (schema !== String(ARTIFACT_SCHEMA_VERSION)) {
    throw new Error(`unsupported schema_version ${JSON.stringify(schema)}`)
  }

  const body = JSON.parse(await readFile(join(directory, 'metrics.otlp.json'), 'utf8')) as OtlpBody
  if (!Array.isArray(body.resourceMetrics) || body.resourceMetrics.length === 0) {
    throw new Error('metrics.otlp.json has no resourceMetrics')
  }

  let metricCount = 0
  for (const resource of body.resourceMetrics) {
    checkAttributes(resource.resource?.attributes, 'resource')
    for (const scope of resource.scopeMetrics ?? []) {
      if (!Array.isArray(scope.metrics)) continue
      for (const metric of scope.metrics as OtlpMetric[]) {
        metricCount += 1
        const name = typeof metric.name === 'string' && metric.name !== '' ? metric.name : undefined
        if (name === undefined) throw new Error('metrics.otlp.json has a metric without a name')

        const instrument = instrumentOf(metric, name)
        const carrier = metric[instrument] ?? {}
        if (instrument !== 'gauge') checkTemporality(carrier.aggregationTemporality, name)

        const points = carrier.dataPoints
        if (!Array.isArray(points) || points.length === 0) {
          throw new Error(`${name} has no ${instrument} data points`)
        }
        for (const point of points as OtlpPoint[]) {
          if (!isRecord(point)) throw new Error(`${name} has a data point that is not an object`)
          if (instrument === 'histogram') checkBuckets(point, name)
          checkAttributes(point.attributes, name)
        }
      }
    }
  }

  if (metricCount === 0) throw new Error('metrics.otlp.json has no metrics')
}
