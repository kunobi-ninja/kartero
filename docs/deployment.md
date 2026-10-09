# Deploy Kartero

The chart is published as an OCI artifact:

```bash
helm show values oci://registry-1.docker.io/zondax/kartero --version 0.4.15
helm install kartero oci://registry-1.docker.io/zondax/kartero \
  --version 0.4.15 --namespace signoz
```

Set at least the GitHub owner, repository, workflows, existing Secret, OTLP
endpoint, and storage class for the target cluster:

```yaml
github:
  owner: kunobi-ninja
  repo: kunobi-frontend
  workflows:
    - ci.yml
  trustedBranch: dev
  artifactPrefix: telemetry-otlp-v1
  existingSecret: kartero-github
  existingSecretKey: token

otlp:
  endpoint: http://signoz-otel-collector.signoz.svc.cluster.local:4318

interval: 15m
heartbeatInterval: 1m

ledger:
  path: /var/lib/kartero/ledger.sqlite
  persistence:
    type: pvc
    mountPath: /var/lib/kartero
    size: 1Gi
    storageClassName: ceph-csi-rbd
    accessModes:
      - ReadWriteOnce
```

The chart only references the GitHub Secret. It does not create credentials.
This works with External Secrets, Sealed Secrets, or a manually managed Secret.
The Secret key must contain a fine-grained GitHub token with repository read,
metadata read, and Actions read access.

For example, an External Secrets installation can create the referenced Secret
without putting the token in Helm values:

```yaml
apiVersion: external-secrets.io/v1
kind: ExternalSecret
metadata:
  name: kartero-github
spec:
  secretStoreRef:
    kind: ClusterSecretStore
    name: onepassword
  target:
    name: kartero-github
    creationPolicy: Owner
  data:
    - secretKey: token
      remoteRef:
        key: kartero-github
        property: credential
```

Adapt the store, item, and property names to the cluster's secret provider.

The PVC is the normal production path. An ephemeral ledger forgets delivery
state after a reschedule and can import old artifacts again.

## Several repositories

Set `sources` instead of `github` to collect from more than one repository in a
single Deployment. The two are mutually exclusive; setting both fails at
startup rather than picking one.

```yaml
sources:
  - owner: kunobi-ninja
    repo: kache
    workflows: [bench.yml, ci.yml]
    trustedBranch: main
    existingSecret: kartero-github-kache
    existingSecretKey: token
  - owner: kunobi-ninja
    repo: kunobi-frontend
    workflows: [ci.yaml]
    trustedBranch: dev
    existingSecret: kartero-github-kunobi-frontend
    existingSecretKey: token
```

Each source needs its own Secret, mounted read-only, and its own trusted
branch — `main` for one repository and `dev` for another is the normal case,
not an exception.

With `sources` set the chart renders a config file and the collector reads
that instead of its environment. `github.artifactPrefix` still applies; the
rest of the `github` block is ignored.

`kartero config-check` loads a configuration, prints the sources it resolved,
and exits without contacting anything. It reports whether each token resolved,
never the token itself. Use it to check a config before a rollout:

```bash
KARTERO_CONFIG=/etc/kartero/config/kartero.yaml kartero config-check
```

One instance means one ledger and one allowlist for every source. A second
Deployment is still an option, and costs a second PVC, a second token to
rotate and a second thing to watch.

## Watching a source

A source that cannot be listed is the failure worth alerting on, because it is
the one that looks like nothing. The pod stays Ready, collect passes keep
running on schedule, and every artifact counter sits at zero — which is
exactly what a repository with nothing to collect yet looks like.

```
kartero_source_up{source="owner/repo"} == 0
```

Alert on that being 0 for longer than one `interval`. It is written for every
configured source on every pass, so a source that starts failing moves rather
than stopping, and a series that stops being written means the scrape stopped
rather than the source recovering.

For a family expected daily, alert on stale delivery as well as source health:

```promql
time() - kartero_source_last_delivery_timestamp_seconds{source="owner/repo",family="ci.probe"} > 26 * 3600
```

Also alert when that series is absent after onboarding. `source_up` only proves
that GitHub runs were listed; it cannot prove a producer uploaded an artifact
or that the allowlist admitted its metrics. `kartero_otlp_rejected_points_total`
counts points rejected in an OTLP partial-success response, and
`kartero_otlp_response_issues_total` counts success responses whose body could
not be checked. Neither causes a full-body retry, which could duplicate
accepted delta points.

A producer can also upload an artifact Kartero refuses for its contents: one
over the size bounds, one that will not open, an unknown `schema_version`, a
payload outside the OTLP structure bounds, or one the backend answers 400 or
413 to. Nothing in it is delivered, and the producer's job has already passed.

```promql
increase(kartero_artifacts_total{outcome="rejected"}[24h]) > 0
```

Alert on that. `outcome="skipped"` cannot stand in for it: it counts every
artifact a pass had nothing to do for, including each one delivered earlier,
so it is never zero. The pod log names the artifact and the reason
(`artifact payload rejected`). In the OTLP backend the same count is
`kartero.collect.artifacts` with `kartero.artifact.outcome = rejected`.

In the OTLP backend, the freshness gauge is
`kartero.collect.source_last_delivery`, with `kartero.source` and
`kartero.metric.family` attributes. It carries the same Unix timestamp, so a
missing or old `ci.probe` series is visible in SigNoz without Prometheus
scraping. `kartero.collect.pending_metrics_bytes` shows the retained replay
backlog there.

`kartero_source_listing_failures_total{source, kind}` says which kind of
failure. Three kinds will not resolve on their own, and all are logged at
ERROR naming the source; ordinary transport failures stay at WARN.

| `kind` | Status | Usually means |
| --- | --- | --- |
| `not_found` | 404 | The repository or workflow file does not exist, or the token cannot see the repository. GitHub answers 404 rather than 403 for a private repository a token cannot see, so a missing grant and a missing file are indistinguishable — check the token first. |
| `unauthorized` | 401 | The token has expired or been revoked. |
| `forbidden` | 403 | An organisation approval or a permission has been withdrawn. |

A 403 carrying an exhausted quota is a secondary rate limit and stays
transient, told apart by `x-ratelimit-remaining` or `Retry-After`. Treating
one as permanent would flip a healthy source to misconfigured during a busy
hour.

## Before a token expires

```
kartero_source_token_expires_timestamp_seconds{source} - time() < 14 * 86400
```

GitHub reports a token's expiry on every response that carries it, so this is
recorded on each pass, whether or not the listing succeeded — knowing a token
is days from lapsing is most useful while it still works.

The series is **absent for tokens that never expire**, which is a real answer
rather than a missing one. Absence therefore means either no expiry or no pass
yet; `kartero_source_up` distinguishes those.

It is an absolute instant rather than a countdown on purpose. A "seconds
remaining" gauge is wrong the moment scraping stops and right only by
accident, so subtract `time()` at query time.

The same reaches OTLP as `kartero.collect.source_token_expires`, in seconds,
with the source in `kartero.source`.

Readiness deliberately does not fail on this. The Prometheus endpoint is
served by the same process, so making the pod unready would remove it from
scrape discovery and hide the very metrics that diagnose the problem.

The same signal reaches the OTLP backend as `kartero.collect.source_up`, with
the source in the `kartero.source` attribute, and
`kartero.collect.sources_misconfigured` counts them per pass.

## Wait for the store

A collector answers 2xx once it has queued a request, not once its store has
the data. If the store is down, the collector accepts the request and drops it
later. Kartero has already recorded the artifact as delivered by then, so it
never sends it again.

Set `otlp.readinessUrl` to a URL that answers 2xx only while the store can
take writes:

```yaml
otlp:
  endpoint: http://signoz-otel-collector.signoz.svc.cluster.local:4318
  readinessUrl: http://signoz.signoz.svc.cluster.local:8080/api/v1/health?live=1
```

For SigNoz this is the query service, which runs `SELECT 1` against ClickHouse
when the request carries `live`. `kubectl -n signoz get svc` shows the service
name for your release. Keep `?live=1`: without it the endpoint answers 2xx
while ClickHouse is down. The collector's own health check only describes the
collector, so it does not help either. The check sends no credentials, and the
URL is logged as written, so keep tokens out of it.

Collect asks the URL at the start of each pass and again before each delivery.
Only a 2xx within 10 seconds counts, and a redirect is not followed. On any
other answer:

- No collected data is sent or recorded for the rest of that pass.
  Artifacts, derived attempts and withheld payloads stay as they were, and the
  next pass asks again.
- Every source is still listed, so `kartero_source_up` and token expiry stay
  current.
- The archive pass runs as usual, and `/readyz` still answers ok. Kartero's
  own heartbeat and pass telemetry still go to the collector.

A blocked pass counts as `kartero_collect_passes_total{outcome="blocked"}`
rather than `error`, and `kartero_otlp_delivery_blocked` stays 1 until a check
succeeds. Alert when it has been 1 for longer than three intervals, for
example with `for: 3h` at the default interval:

```promql
kartero_otlp_delivery_blocked == 1
```

`kartero_otlp_readiness_checks_total{outcome}` counts the answers. If it stays
flat with the URL set, the setting did not reach the pod. `unavailable` is a
5xx, 408, 429, timeout or failed connection, logged at WARN. Usually that is
the store being down, which clears by itself, but a wrong host name, a wrong
port or an untrusted certificate fails the same way, before any answer, so the
blocked alert above is the one that catches a wrong URL. `misconfigured` is a
redirect or any other 4xx. Waiting will not clear it, so it is logged at ERROR
and is worth its own alert:

```promql
increase(kartero_otlp_readiness_checks_total{outcome="misconfigured"}[2h]) > 0
```

Alerts evaluated in the same store cannot fire while it is down. The pod log
has the reason either way.

Waiting has a horizon for artifacts and derived CI metrics. A pass lists runs
created since the UTC date `lookback` ago, which with the default `24h` means
runs from the last 24 to 48 hours, and a run that ages out of that window
while delivery waits is not looked at again. After a longer outage, raise
`lookback` to cover it, within GitHub's artifact retention, until the backlog
is delivered. Payloads withheld by the allowlist are kept in the ledger
instead, so they wait up to their 30-day replay horizon whatever `lookback`
is.

The check narrows the window in which data is lost; it does not close it.
Data the collector accepted just before the store failed can still be dropped,
and a store that answers `SELECT 1` while it cannot write, for example with a
full disk, passes the check. A persistent sending queue in the collector covers
more of that, and for every producer.

## Archive diagnostic artifacts

Off by default. Turn it on in Helm; the running Deployment (`kartero run`) then
copies matching GitHub artifacts onto a volume on the same interval as collect.
There is nothing to cron and no extra command to invoke in the cluster. The
prefix for kache benches is `bench` (`bench-firefox`, not `telemetry-otlp-v1-*`).

Files land at `{path}/{owner}/{repo}/{run_id}/{attempt}/{artifact}.zip`. Give
the archive PVC enough space for the nights you want to keep. The chart prunes
files after 30 days by default; set `archive.retentionDays` to another positive
number to change that horizon.

```yaml
archive:
  enabled: true
  artifactPrefix: bench
  maxBytes: 33554432
  path: /var/lib/kartero-archive
  persistence:
    type: pvc
    size: 50Gi
    storageClassName: ceph-csi-rbd
    accessModes:
      - ReadWriteOnce
```

For a private image, set `image.repository`, `image.tag`, and
`imagePullSecrets`. Cluster-level registry proxies do not change the chart's
credential model.
