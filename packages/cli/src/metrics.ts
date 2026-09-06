/**
 * Build a Kartero artifact from arbitrary metric points.
 *
 * `coverage` and `gauge` cover producers that report one thing about the job
 * they run in. A producer that sweeps an API afterwards reports many things
 * about many runs, in all three instruments, each at the instant it describes
 * — and until now had to hand-roll OTLP JSON to do it. Hand-rolled copies of
 * this wire format drift: delta temporality is a bare `1`, a histogram is
 * silently wrong when its bucket counts and bounds disagree, and nothing
 * complains until a dashboard is empty.
 */
import { mkdir, writeFile } from 'node:fs/promises'
import { join } from 'node:path'
import { ARTIFACT_SCHEMA_VERSION } from './coverage.js'

/** AGGREGATION_TEMPORALITY_DELTA / _CUMULATIVE. */
const TEMPORALITY = { delta: 1, cumulative: 2 } as const

export type Instrument = 'gauge' | 'counter' | 'histogram'
export type Temporality = keyof typeof TEMPORALITY

export interface MetricPoint {
  metric: string
  instrument: Instrument
  unit: string
  value: number
  attributes: Record<string, string>
  /** The instant the point describes, not the moment it was collected. */
  observedAt: Date
  /** Histogram only. Ascending, finite, and without the overflow bound. */
  bounds?: readonly number[]
  /** Counters and histograms. Defaults to delta. */
  temporality?: Temporality
}

export interface ArtifactOptions {
  resource: Record<string, string>
  scope: { name: string; version: string }
}

interface OtlpAttribute {
  key: string
  value: { stringValue: string }
}

export interface OtlpMetricsPayload {
  resourceMetrics: {
    resource: { attributes: OtlpAttribute[] }
    scopeMetrics: { scope: { name: string; version: string }; metrics: unknown[] }[]
  }[]
}

/**
 * Kartero stamps repository and pipeline identity from the trusted GitHub run
 * and discards whatever arrived, so a producer that sets these has written
 * something that will be thrown away.
 */
function attribute(key: string, value: string): OtlpAttribute {
  if (key === '') throw new Error('attribute key must not be empty')
  if (key.startsWith('cicd.') || key.startsWith('vcs.')) {
    throw new Error(`attribute ${key} is reserved for Kartero`)
  }
  return { key, value: { stringValue: value } }
}

function attributes(record: Record<string, string>): OtlpAttribute[] {
  return Object.entries(record).map(([key, value]) => attribute(key, value))
}

function nanos(at: Date): string {
  const time = at.getTime()
  if (!Number.isFinite(time)) throw new Error('observedAt is not a valid date')
  return String(BigInt(time) * 1_000_000n)
}

/**
 * Place one observation into explicit buckets.
 *
 * OTLP requires exactly one more bucket count than bound, the last being the
 * overflow above the final bound. A payload that gets this wrong is accepted
 * by most backends and misdescribes every observation in it.
 */
export function bucketise(value: number, bounds: readonly number[]): string[] {
  if (!Number.isFinite(value)) throw new Error(`refusing to bucket a non-finite value: ${value}`)
  const counts = new Array<number>(bounds.length + 1).fill(0)
  const index = bounds.findIndex((bound) => value <= bound)
  counts[index === -1 ? bounds.length : index] = 1
  return counts.map(String)
}

function assertBounds(bounds: readonly number[] | undefined, metric: string): readonly number[] {
  if (bounds === undefined || bounds.length === 0) {
    throw new Error(`${metric} is a histogram and needs explicit bounds`)
  }
  if (!bounds.every((bound) => Number.isFinite(bound))) {
    throw new Error(`${metric} has a non-finite bucket bound`)
  }
  if (bounds.some((bound, index) => index > 0 && bound <= (bounds[index - 1] as number))) {
    throw new Error(`${metric} bucket bounds must ascend`)
  }
  return bounds
}

/**
 * One metric carries one instrument and one unit. Two points disagreeing on
 * either describe different things under one name, and OTLP has no way to say
 * so — the backend keeps whichever arrived first.
 */
function group(points: readonly MetricPoint[]): Map<string, MetricPoint[]> {
  const byMetric = new Map<string, MetricPoint[]>()
  for (const point of points) {
    if (point.metric === '') throw new Error('metric name must not be empty')
    if (point.unit === '') throw new Error(`${point.metric} must declare a unit`)
    const existing = byMetric.get(point.metric)
    if (existing === undefined) {
      byMetric.set(point.metric, [point])
      continue
    }
    const first = existing[0] as MetricPoint
    if (first.instrument !== point.instrument) {
      throw new Error(`${point.metric} is both ${first.instrument} and ${point.instrument}`)
    }
    if (first.unit !== point.unit) {
      throw new Error(`${point.metric} is both ${first.unit} and ${point.unit}`)
    }
    existing.push(point)
  }
  return byMetric
}

function metricFor(name: string, points: readonly MetricPoint[]): unknown {
  const first = points[0] as MetricPoint
  const { unit, instrument } = first
  const temporality = TEMPORALITY[first.temporality ?? 'delta']

  if (instrument === 'gauge') {
    return {
      name,
      unit,
      gauge: {
        dataPoints: points.map((point) => ({
          asDouble: point.value,
          timeUnixNano: nanos(point.observedAt),
          attributes: attributes(point.attributes),
        })),
      },
    }
  }

  if (instrument === 'counter') {
    return {
      name,
      unit,
      sum: {
        dataPoints: points.map((point) => {
          const at = nanos(point.observedAt)
          return {
            asInt: String(Math.trunc(point.value)),
            startTimeUnixNano: at,
            timeUnixNano: at,
            attributes: attributes(point.attributes),
          }
        }),
        aggregationTemporality: temporality,
        isMonotonic: true,
      },
    }
  }

  const bounds = assertBounds(first.bounds, name)
  return {
    name,
    unit,
    histogram: {
      dataPoints: points.map((point) => {
        const at = nanos(point.observedAt)
        return {
          count: '1',
          sum: point.value,
          bucketCounts: bucketise(point.value, assertBounds(point.bounds ?? bounds, name)),
          explicitBounds: [...(point.bounds ?? bounds)],
          startTimeUnixNano: at,
          timeUnixNano: at,
          attributes: attributes(point.attributes),
        }
      }),
      aggregationTemporality: temporality,
    },
  }
}

/**
 * Every point carries its own timestamp, which is what lets one artifact hold
 * a whole sweep: a point for a run that finished three days ago lands three
 * days ago rather than at the moment of collection.
 */
export function buildMetricsOtlp(points: readonly MetricPoint[], options: ArtifactOptions): OtlpMetricsPayload {
  if (points.length === 0) throw new Error('an artifact needs at least one point')
  const metrics = [...group(points)].map(([name, group]) => metricFor(name, group))
  return {
    resourceMetrics: [
      {
        resource: { attributes: attributes(options.resource) },
        scopeMetrics: [{ scope: options.scope, metrics }],
      },
    ],
  }
}

/**
 * Write the two files Kartero expects at the root of the uploaded artifact.
 *
 * `wx` rather than an overwrite: a directory that already holds an artifact
 * usually means two producers writing to one path, and silently keeping the
 * second is worse than failing.
 */
export async function writeArtifact(directory: string, payload: unknown): Promise<void> {
  await mkdir(directory, { recursive: true })
  await writeFile(join(directory, 'metrics.otlp.json'), `${JSON.stringify(payload)}\n`, { flag: 'wx' })
  await writeFile(join(directory, 'schema_version'), `${ARTIFACT_SCHEMA_VERSION}\n`, { flag: 'wx' })
}
