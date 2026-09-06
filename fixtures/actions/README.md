# CI metrics fixtures, v1

Recorded GitHub Actions API payloads for the CI metrics collector, tracked in
Zondax/kunobi-frontend#3544. The collector does not exist yet; these fixtures land first so its
derivation rules can be written and tested against real payloads rather than against assumptions.

Each metric the collector will emit is a pure function of two payload shapes, the run attempt and its
job list, so this corpus is the entire test surface. The payloads are recorded rather than
hand-written because several rules exist only because the real API behaves in ways nobody would
guess. A hand-written fixture would encode what we expect GitHub to send instead of what it sends.

Regenerate with `scripts/ci/capture-ci-metrics-fixtures.sh`, which can be run from anywhere in the
repository. Do not hand-edit the JSON. Every payload is validated before it replaces a committed
fixture, so a run that has aged out of Actions retention fails the capture instead of overwriting a
good fixture with a 404 body.

`manifest.json` maps each fixture stem to its run id and attempt number. The job payloads carry an
attempt number but no run id, so that mapping is the only machine-readable link from a job list back
to its run. Tests that need an attempt pair should group by `run_id` and sort by `attempt` rather
than parsing file names.

## What is scrubbed, and what is kept

The allow-list projections in the capture script are the actual protection: a field absent from them
cannot reach the repository. Removed there are the head SHA, the actor login, every URL, the PR
title, and `runner_name`, which becomes a `runner_present` boolean because whether a runner ever
picked the job up is all the derivation asks. `head_branch` keeps the trunk names `dev`, `main` and `pre`, which are repository structure rather
than identity, and is replaced with a constant otherwise, because a topic branch name can carry a
ticket reference or a personal name. Keeping the trunk names is what lets the branch classification
be tested against a real payload instead of only against synthetic input.

Kept deliberately: `id`, `run_number`, `workflow_id`, `path`, `event`, `status`, `updated_at`,
`repository.full_name`, every step `name`, and per-job `labels`. None is sensitive, and keeping them
lets a test assert that the collector does **not** emit the ones its contract forbids. Note that
`labels` carries runner-pool identifiers such as `zondax-runners`, `mac-mini` and `self-hosted`,
which is how `runner_pool` will be derived; individual machine names do not appear.

## The fixtures

| Fixture | What it demonstrates |
|---|---|
| `first-attempt-success` | The baseline. All 21 jobs ran, none skipped, none failed. Any rule whose happy path is untested against this is untested. |
| `first-attempt-failed` | A first attempt that failed, with five genuine job failures. Its three skipped jobs are comment-posting jobs skipped because their upstream job failed, which is a different cause from a path-filter skip and must not be conflated with one. |
| `path-filter-skipped` | A run where the path filter matched nothing: `changes` concluded `success`, the run concluded `success`, and 14 jobs skipped, including the four reusable callers under their bare ids. Structurally distinguishable from `supersede-cancelled` only by those two success conclusions, which is why both fixtures are needed. |
| `push-event-gated` | A completed `push` run. `E2E-only filter detected` is skipped because it gates on `github.event_name == 'pull_request'`, so this is the only fixture where a skip is caused by the triggering event rather than by a path filter or an upstream job. |
| `rerun-failed-jobs` | Attempt 2 of a run, with 15 of 21 jobs carried forward. Each carried-forward job reports the *new* attempt's `created_at` against the *original* attempt's `started_at`, so a plain subtraction yields negative queue times ranging from 3 to 46 minutes in this payload. Every job reports the new `run_attempt`, carried forward or not, which is why the timestamp inversion and not `run_attempt` is the discriminator. |
| `supersede-cancelled` | A run cancelled by `cancel-in-progress`: 11 cancelled jobs, and `CI Gate` concluding `failure` while the run itself concluded `cancelled`. The cause is known from run history, not from the payload, which retains no field separating a concurrency cancel from a manual one. |
| `flake-recovered-a1` / `-a2` | Attempt 1 failed, attempt 2 succeeded, same run. The only pair in the corpus that should produce a flake. |
| `cancelled-then-success-a1` / `-a2` | Attempt 1 cancelled, attempt 2 succeeded. Must not be classified as a flake. Without this pair, a classifier keyed on "the previous attempt was not a success" looks correct. |
| `three-attempts-a1` / `-a3` | A run that failed on all three attempts, for catching up across more than one unseen attempt. Attempt 2 of this same run is committed as `rerun-failed-jobs`, named for the behaviour it shows rather than its position; `manifest.json` records the relationship. |

## Behaviours the corpus records that the rules have to handle

- The inversion in carried-forward jobs appears in every attempt above the first, not only in
  `rerun-failed-jobs`: 15 of 21 there, 16 in `cancelled-then-success-a2`, and 17 in both
  `flake-recovered-a2` and `three-attempts-a3`.
- A reusable caller's job name changes with what happened to it. Skipped, it appears as the bare
  caller id. Run, it appears as a composite, and sometimes gains a third segment: the same logical
  job is `ts-checks-linux / post-coverage-comment` when skipped and
  `ts-checks-linux / post-coverage-comment / post-comment` when it runs. Both forms occur inside
  single fixtures and across attempt pairs, so a name-keyed join sees a job appear and disappear.
- When a reusable caller is skipped, its nested children are **absent** from the job list rather
  than present and skipped. That is why `path-filter-skipped` and `supersede-cancelled` have 17
  jobs where other fixtures have 21.
- A job that never reached a runner has an empty `steps` array. Every zero-step job in the corpus
  has `runner_present: false`, so any rule reading `steps` has to exclude them rather than treat
  them as a job that did no work.
- `created_at` equals `run_started_at` on every first attempt here. On the attempt endpoint the two
  differ by about two seconds on later attempts. The large divergence that motivates an
  attempt-scoped clock is a property of the *run list* endpoint, whose `created_at` stays pinned to
  the first attempt; no fixture records that endpoint.

## Known gaps

None of these can be manufactured honestly, so they are listed rather than synthesised.

- **No rerun-all attempt.** Every rerun in sampled history is a rerun-failed-jobs, so the branch
  where every job re-executes has no recorded payload.
- **No attempt whose own conclusion is `cancelled`.** Every attempt above the first here concluded
  `success` or `failure`, so a rerun superseded by a newer push is unrepresented.
- **No `timed_out` job**, and no job in any state other than `completed`.
- **No `[e2e-only]` marker failure.** `E2E-only filter detected` never actually fails in this
  corpus, so its deliberate-failure mode is unrecorded.
- **No in-progress run**, so null `started_at` and `completed_at` have no payload. This one needs a
  capture taken deliberately during a live run.
- **No run list payload**, which is where the pinned-`created_at` trap lives.
- **No paginated job list.** The largest run here has 21 jobs against a 100 ceiling, and the capture
  script rejects a truncated capture rather than recording one. A page boundary is not the kind of
  API surprise this corpus exists to guard against, so a hand-built two-page payload is acceptable
  for that one rule.

These fixtures also depend on GitHub retaining the runs they came from. Once those run ids start
returning 404, the corpus becomes unregenerable and the committed JSON is the only copy. The capture
script fails loudly in that case rather than overwriting anything.
