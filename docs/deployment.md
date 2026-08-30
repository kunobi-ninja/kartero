# Deploy Kartero

The chart is published as an OCI artifact:

```bash
helm show values oci://registry-1.docker.io/zondax/kartero --version 0.3.0
helm install kartero oci://registry-1.docker.io/zondax/kartero \
  --version 0.3.0 --namespace signoz
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

## Archive diagnostic artifacts

Off by default. Turn it on in Helm; the running Deployment (`kartero run`) then
copies matching GitHub artifacts on the same interval as collect. There is
nothing to cron and no extra command to invoke in the cluster. The prefix for
kache benches is `bench` (`bench-firefox`, not `telemetry-otlp-v1-*`).

It does not replace GitHub's 30-day artifact retention until you enable it and
point it at a bucket you own.

```yaml
archive:
  enabled: true
  artifactPrefix: bench
  bucket: kache-bench-archive
  keyPrefix: kache/bench
  endpoint: https://<accountid>.r2.cloudflarestorage.com
  region: auto
  maxBytes: 33554432
  existingSecret: kartero-archive
  accessKeyIdKey: access-key-id
  secretAccessKeyKey: secret-access-key
```

The archive Secret is separate from the GitHub token. Objects land at
`{keyPrefix}/{owner}/{repo}/{run_id}/{attempt}/{artifact}.zip`. Kartero does
not serve or query that bucket.

For a private image, set `image.repository`, `image.tag`, and
`imagePullSecrets`. Cluster-level registry proxies do not change the chart's
credential model.
