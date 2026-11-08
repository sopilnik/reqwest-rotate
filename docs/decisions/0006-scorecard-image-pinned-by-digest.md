# 0006 The Scorecard image is pinned by digest and moved by hand

Status: accepted

## Context

`.github/workflows/scorecard.yml` runs the OpenSSF Scorecard analysis with `security-events: write`, so what it runs
can upload or hide code-scanning alerts. `ossf/scorecard-action` is a container action: at v2.5.0 its `action.yaml`
says `runs.using: "docker"` and `image: "docker://ghcr.io/ossf/scorecard-action:v2.5.0"`. Pinning the action to a
commit therefore fixes the `action.yaml` and not the image, because a tag on the registry can be moved to another
image.

Dependabot's `github-actions` updater, which moves the other two pins in the workflow, skips a `docker://` reference:
`dependency_for_declarations` in `github_actions/lib/dependabot/github_actions/file_parser.rb` returns early for it,
under the comment `TODO: Support Docker references and path references`.

## Decision

The workflow runs the image directly: `uses: docker://ghcr.io/ossf/scorecard-action@sha256:<digest> # v2.5.0`. The
digest is the manifest digest the registry returns for the `v2.5.0` tag. A `docker://` step does not apply the defaults
of `action.yaml`, so the step passes the inputs the image reads on this run (`results_file`, `results_format`,
`repo_token`, `publish_results`, `file_mode`), and `internal_default_token` as `action.yaml` would: the image reads it
only when `repo_token` is empty or results are published, and neither holds here. `internal_publish_base_url` is left
out: the image reads it only when it publishes results, and `publish_results` is `false`.

Dependabot does not move this pin, so a maintainer does, with a new scorecard-action release:

1. Read the release tag's `action.yaml`: `gh api "repos/ossf/scorecard-action/contents/action.yaml?ref=<tag>" --jq
   .content | base64 -d`. Its `image:` line names the tag to look up. Compare its `inputs:` with the `with:` keys of the
   step.
2. Read the digest of that tag from the registry. Get an anonymous token with
   `curl -sS "https://ghcr.io/token?scope=repository:ossf/scorecard-action:pull"`, then send
   `curl -sSI -H "Authorization: Bearer <token>" -H "Accept: application/vnd.docker.distribution.manifest.v2+json"
   https://ghcr.io/v2/ossf/scorecard-action/manifests/<tag>` and read `docker-content-digest`. At v2.5.0 the answer is
   a single-platform manifest, not an index.
3. Check the digest is the right image. Fetch the manifest by digest, then the config blob it names, and read the labels
   `org.opencontainers.image.version` (the tag) and `org.opencontainers.image.revision` (the commit the tag points to:
   `gh api repos/ossf/scorecard-action/commits/<tag> --jq .sha`). `gh attestation verify
   oci://ghcr.io/ossf/scorecard-action@<digest> -R ossf/scorecard-action` should also pass.
4. Change the digest and the version comment together, in one commit.

## Alternatives

- Use the action by commit (`ossf/scorecard-action@<sha>`), which Dependabot does move. Rejected: the commit leaves the
  image on a tag that can be moved, and that image runs with a token that can write code-scanning results.
- Add a scheduled check for new releases. Not done: one pin does not justify a job of its own.

## Consequences

The Scorecard image stays on v2.5.0 until a maintainer follows the steps above, so it can lag behind the newest
release, and nothing opens a pull request to say so. The pin runs the image without its `action.yaml`, which is not the
documented use of the action; if a release adds or renames an input, the step has to follow it by hand (step 1). If
Dependabot starts to support `docker://` references in workflows, it can move this pin, and a new record supersedes
this one.
