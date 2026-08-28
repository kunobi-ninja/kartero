import { ARTIFACT_SCHEMA_VERSION } from './coverage.js'

interface OtlpAttribute {
  key: string
  value: { stringValue: string }
}

export interface GaugeOptions {
  attributes: ReadonlyArray<readonly [string, string]>
  serviceName: string
  serviceNamespace: string
  timestamp: Date
  unit: string
}

function attribute(key: string, value: string): OtlpAttribute {
  if (key === '') throw new Error('attribute key must not be empty')
  if (key.startsWith('cicd.') || key.startsWith('vcs.')) {
    throw new Error(`attribute ${key} is reserved for Kartero`)
  }
  return { key, value: { stringValue: value } }
}

function dataValue(raw: string): { asInt: string } | { asDouble: number } {
  if (/^-?(?:0|[1-9][0-9]*)$/.test(raw)) return { asInt: raw }
  const value = Number(raw)
  if (!Number.isFinite(value)) throw new Error('gauge value must be a finite number')
  return { asDouble: value }
}

export function parseAttribute(raw: string): readonly [string, string] {
  const separator = raw.indexOf('=')
  if (separator <= 0) throw new Error(`attribute must use KEY=VALUE: ${raw}`)
  return [raw.slice(0, separator), raw.slice(separator + 1)]
}

export function buildGaugeOtlp(name: string, rawValue: string, options: GaugeOptions): unknown {
  if (name === '') throw new Error('metric name must not be empty')
  if (options.unit === '') throw new Error('metric unit must not be empty')
  if (!Number.isFinite(options.timestamp.getTime())) throw new Error('timestamp is invalid')

  const timeUnixNano = String(BigInt(options.timestamp.getTime()) * 1_000_000n)
  const attributes = options.attributes.map(([key, value]) => attribute(key, value))

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
        scopeMetrics: [
          {
            scope: { name: '@kunobi/kartero', version: String(ARTIFACT_SCHEMA_VERSION) },
            metrics: [
              {
                name,
                unit: options.unit,
                gauge: {
                  dataPoints: [{ ...dataValue(rawValue), timeUnixNano, attributes }],
                },
              },
            ],
          },
        ],
      },
    ],
  }
}
