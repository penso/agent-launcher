# Repository Security Advisories

`GitHubSecuritySource` is registered alongside the ordinary GitHub source. Its
`SourceKey` is unchanged; `cache_key()` is `security:{source_key.canonical()}` and
`is_confidential()` is true. Results are full replacements with empty checkpoints.
The runtime must route these records to memory only and clear them on sync errors.
This crate does not persist advisory records or log response bodies.

## API And Authentication

The adapter uses the [repository advisory REST API](https://docs.github.com/en/rest/security-advisories/repository-advisories),
verified against the documentation on 2026-09-13. Requests use API version
`2022-11-28`, `api.github.com` for GitHub.com, and `/api/v3` on enterprise hosts.
It reuses ordinary GitHub client construction and host-specific token resolution,
but rejects all redirects, including same-origin redirects.

Authentication is mandatory, even for an empty inventory. Unpublished advisory
access requires a repository security manager or administrator, or an advisory
collaborator. Classic tokens need `repo` or `repository_advisories:read` for reads;
fork creation also requires GitHub to authorize the mutation. Preparation requires
an explicit `permissions.push: true` on the verified fork, including when the
permissions field is otherwise absent. HTTP errors use static, sanitized reasons.

Listing explicitly requests `state=triage` and `state=draft`, following the Link
header's `before` or `after` cursor. There is no numbered-page fallback. Next links
must preserve the API origin, repository path and state, and use only validated
query parameters. Published, closed and withdrawn records are excluded. Missing
or unknown states, malformed records, repeated cursors, unsafe links and any page
failure discard the entire inventory.

## Confirmed Preparation

`prepare_security(key, create_fork)` must only run after explicit confirmation.
It validates exact source coordinates and the GHSA identifier, refetches the
advisory, and requires `draft`. Triage reports must be accepted manually on GitHub;
this adapter never patches state.

If the advisory has no fork, `create_fork=true` permits one POST to the advisory's
`/forks` endpoint. The documented 202 response is asynchronous; its payload,
including any temporary clone token, is ignored. The adapter polls the advisory
and the repository named by its `private_fork` field. A 404 repository response
is retried only after this call successfully requested creation. Other access
errors fail immediately. Ambiguous POST failures are not retried; a subsequent
confirmed call starts by refetching the advisory.

Dispatch preparation verifies the linked ID, private flags, exact host and full
repository name, non-source repository name, non-archived/non-disabled status,
push permission, and valid default branch. API-provided clone URLs are ignored.
Only validated host/name coordinates and the default branch leave this adapter,
along with the fresh issue. No publishing, merging, public PR creation, or
collaborator changes are implemented.

## Bounds And Tests

- At most 100 pages total across both states, at most 100 records per page.
- At most 8 MiB per response, checked before allocation and during streamed reads.
- 30-second request/body timeouts and a 120-second full inventory deadline.
- Five-minute preparation deadline and five-second polling interval.
- No partial inventory is returned when a bound is reached.

`cargo test -p agent-launcher-issues -p agent-launcher-core` runs local synthetic
HTTP fixtures. Coverage includes cursor pagination, state filtering, missing
fields, sanitized permission errors, redirects, response/page limits, existing
forks, 202/404 provisioning, ambiguous POST failure, preparation deadlines, and
identity/privacy/host/permission/ref rejection. These tests do not query or mutate
live private advisories. Actual GitHub/GHES deployment compatibility and real
five-minute provisioning are not exercised by the local fixtures.
