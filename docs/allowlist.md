# The allowlist

`allowlist.yaml` decides which metric names, attribute keys and attribute
values reach the backend. A name matching nothing in it is dropped.

## Names and families

Names are admitted individually, by family, or both.

```yaml
metric_patterns:
  - 'kache\.(bench|cache|prefetch|ci)\..+'
  - 'ci\.(run|job|coverage|collector)\..+'

metrics:
  - some.exact.name

attribute_patterns:
  - '(service|deployment|telemetry)\..+'

attributes:
  - branch_class
  - job_name
```

Patterns are regular expressions. Two things about them are not left to the
author:

**They are anchored to the whole name.** `ci\..+` is compiled as
`^(?:ci\..+)$`, so it cannot match `anything.ci.whatever`. An unanchored
pattern is the failure worth designing out: it admits far more than it appears
to, in a file whose entire purpose is being readable. A pattern that already
carries its own `^` and `$` keeps working — those assert at the same positions
as the ones added around them.

**They cannot reach what Kartero stamps.** A pattern matching
`cicd.pipeline.run.id` or `vcs.ref.head.name` is refused at load, because the
collector overwrites those from the trusted GitHub run regardless and a
pattern covering them would read as permission that does nothing.

An invalid pattern fails at load with the pattern quoted, rather than silently
matching nothing.

Entries under `metrics` and `attributes` are literal strings, not patterns.
`kache.bench.speedup` there means exactly that name, not a regex in which the
dots are wildcards.

## What a pattern trades away

Admitting `ci\..+` means a producer can open a new `ci.*` series without anyone
approving that specific name. That is a genuine loss of review, and worth
accepting only for a namespace already owned by a trusted producer.

It is a smaller loss than it looks. A producer is already a trusted workflow on
a trusted branch of a configured repository; anyone able to add a metric name
can edit the workflow that sends it. And a new name costs a bounded number of
series — one per attribute combination that already exists.

## Values, which patterns do not relax

```yaml
attribute_values:
  branch_class: [pull_request, trunk_dev, trunk_main, release_pre, other]
  attempt_class: [first, rerun]
```

An attribute listed here may only carry a listed value. A point carrying
anything else is **dropped**, not stripped of the attribute — stripping it
would silently merge the point into a different series, which is harder to
notice than a point going missing.

This is the check worth keeping tight. A new metric name costs a bounded
number of series; an unbounded attribute value costs one series *per run*.

Only vocabularies fixed in a producer's code belong here, so that changing one
means changing a reviewed file. Keys whose values are legitimately open — a job
name from a reviewed alias table, a repository from this deployment's own
source list — are left out and taken on trust.

Declaring values for a key that no entry or pattern admits is refused. That
combination reads as protection which is not there.

## Projects

```yaml
projects:
  - bench-firefox
```

Unlike the vocabularies above, this one is required rather than merely checked.
A `kache.bench.*` point carrying no project, or one that is not listed, is
dropped.

## Changing it

The chart ships `charts/kartero/allowlist.yaml`, and a test fails if it drifts
from the copy at the repository root. Edit both, or copy one over the other.

`tests/cli_contract.rs` pins every name the allowlist admitted before it was
collapsed into patterns, so narrowing a family fails there rather than by a
series quietly disappearing from a dashboard.
