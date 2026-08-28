#!/usr/bin/env node

import { mkdir, readFile, writeFile } from 'node:fs/promises'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'
import { ARTIFACT_SCHEMA_VERSION, buildCoverageOtlp, parseCoverage, type CoverageFormat } from './coverage.js'
import { validateArtifactDirectory } from './validate.js'

const USAGE = `Usage:
  kartero coverage --input REPORT [--output DIR] [--format FORMAT]
  kartero validate --input DIR
  kartero --version

Coverage formats: auto, istanbul, llvm-cov-json, lcov
`

function parseFlags(args: string[]): Map<string, string> {
  const flags = new Map<string, string>()
  for (let index = 0; index < args.length; index += 1) {
    const flag = args[index]
    if (flag === undefined || !flag.startsWith('--')) throw new Error(`unexpected argument ${flag ?? ''}`)
    const value = args[index + 1]
    if (value === undefined || value.startsWith('--')) throw new Error(`${flag} needs a value`)
    if (flags.has(flag)) throw new Error(`${flag} was provided more than once`)
    flags.set(flag, value)
    index += 1
  }
  return flags
}

function required(flags: Map<string, string>, name: string): string {
  const value = flags.get(name)
  if (value === undefined || value === '') throw new Error(`${name} is required`)
  return value
}

function branchClass(explicit: string | undefined): string {
  if (explicit !== undefined) return explicit
  const branch = process.env.GITHUB_REF_NAME
  if (branch === 'dev') return 'trunk_dev'
  if (branch === 'main') return 'trunk_main'
  if (process.env.GITHUB_EVENT_NAME === 'pull_request') return 'pull_request'
  return 'other'
}

async function packageVersion(): Promise<string> {
  const path = join(dirname(fileURLToPath(import.meta.url)), '..', 'package.json')
  return (JSON.parse(await readFile(path, 'utf8')) as { version: string }).version
}

async function coverage(args: string[]): Promise<void> {
  const flags = parseFlags(args)
  const known = new Set([
    '--input',
    '--output',
    '--format',
    '--language',
    '--branch-class',
    '--service-name',
    '--service-namespace',
    '--timestamp',
  ])
  for (const flag of flags.keys()) if (!known.has(flag)) throw new Error(`unknown option ${flag}`)

  const input = required(flags, '--input')
  const output = flags.get('--output') ?? 'telemetry'
  const format = (flags.get('--format') ?? 'auto') as CoverageFormat
  if (!['auto', 'istanbul', 'llvm-cov-json', 'lcov'].includes(format)) throw new Error(`unknown coverage format ${format}`)
  const timestamp = flags.has('--timestamp') ? new Date(required(flags, '--timestamp')) : new Date()
  const snapshot = parseCoverage(await readFile(input, 'utf8'), format, flags.get('--language'))
  const payload = buildCoverageOtlp(snapshot, {
    branchClass: branchClass(flags.get('--branch-class')),
    serviceName: flags.get('--service-name') ?? 'github-actions-ci',
    serviceNamespace: flags.get('--service-namespace') ?? 'kunobi',
    timestamp,
  })

  await mkdir(output)
  await writeFile(join(output, 'metrics.otlp.json'), `${JSON.stringify(payload)}\n`, { flag: 'wx' })
  await writeFile(join(output, 'schema_version'), `${ARTIFACT_SCHEMA_VERSION}\n`, { flag: 'wx' })
  console.log(`wrote Kartero coverage artifact to ${output}`)
}

async function main(): Promise<void> {
  const [command, ...args] = process.argv.slice(2)
  if (command === '--version' || command === '-v') {
    console.log(await packageVersion())
    return
  }
  if (command === '--help' || command === '-h' || command === undefined) {
    process.stdout.write(USAGE)
    return
  }
  if (command === 'coverage') {
    await coverage(args)
    return
  }
  if (command === 'validate') {
    const flags = parseFlags(args)
    if ([...flags.keys()].some((flag) => flag !== '--input')) throw new Error('validate accepts only --input')
    const input = required(flags, '--input')
    await validateArtifactDirectory(input)
    console.log(`valid Kartero artifact: ${input}`)
    return
  }
  throw new Error(`unknown command ${command}`)
}

main().catch((error: unknown) => {
  console.error(`kartero: ${error instanceof Error ? error.message : String(error)}`)
  process.exitCode = 1
})
