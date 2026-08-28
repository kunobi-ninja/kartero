import { readFile } from 'node:fs/promises'
import { join } from 'node:path'
import { ARTIFACT_SCHEMA_VERSION } from './coverage.js'

export async function validateArtifactDirectory(directory: string): Promise<void> {
  const schema = (await readFile(join(directory, 'schema_version'), 'utf8')).trim()
  if (schema !== String(ARTIFACT_SCHEMA_VERSION)) {
    throw new Error(`unsupported schema_version ${JSON.stringify(schema)}`)
  }

  const body = JSON.parse(await readFile(join(directory, 'metrics.otlp.json'), 'utf8')) as {
    resourceMetrics?: { scopeMetrics?: { metrics?: unknown[] }[] }[]
  }
  if (!Array.isArray(body.resourceMetrics) || body.resourceMetrics.length === 0) {
    throw new Error('metrics.otlp.json has no resourceMetrics')
  }
  const metricCount = body.resourceMetrics.reduce(
    (resources, resource) => resources + (resource.scopeMetrics ?? []).reduce((scopes, scope) => scopes + (scope.metrics?.length ?? 0), 0),
    0
  )
  if (metricCount === 0) throw new Error('metrics.otlp.json has no metrics')
}
