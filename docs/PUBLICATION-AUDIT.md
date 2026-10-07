# Publication audit — 2026-10-07 UTC

Repository: [gw31415/dev-session-mcp](https://github.com/gw31415/dev-session-mcp).
Source checkpoint: `d30815001346ad9ce10cb2739b579fb3732c5e2f`, tree
`59c19dcdbc335251e6197bc94365147563be0f38`.
The repository was still **private** when this audit was recorded.

## Checked scope

- GitHub's advertised refs contained only `refs/heads/main`; no other branches
  or tags. The source checkpoint retained all 13 reachable commits.
- All 175 unique reachable text blobs were examined, including files deleted
  from the current tree: 1,832,162 bytes total, largest blob 146,742 bytes.
  Commit messages and author/committer metadata were also checked. The
  current tracked source tree contained 41 files before this audit record.
- The bounded inspection limits were 100 commits, 2,000 reachable objects,
  2 MiB per blob and 20 MiB total. No limit was reached and no binary blob
  needed to be skipped.
- Authenticated GitHub collection reads returned zero releases, zero
  issues/PRs and zero workflow runs. Accordingly no release assets or
  issue/PR attachments were present in those collections. No Actions run
  was started. The connector rejected the repository-wide artifacts and
  workflows endpoints; those two indexes were not independently inspected.

## Methods and findings

[Gitleaks 8.30.1](https://github.com/gitleaks/gitleaks/releases/tag/v8.30.1)
was downloaded from its official release and checked against the published
SHA256 digest. It scanned the reachable Git history and an export containing
only the current tracked files, with full output redaction enabled.

One finding in `rust/tests/http_smoke.rs` is an intentionally unsigned JWT
used by the negative authentication test. Its algorithm is `none`, it has
no signature, and the test requires the server to reject it. It is not a
real credential. This finding remains visible rather than being broadly
allowlisted or removed from history.

A separate scan covered every historical blob and commit metadata for
private-key blocks, known API-token formats, literal credential assignments,
credential-bearing URLs, Tunnel IDs, SSH destinations and delivery file IDs.
The file-ID pattern matched the Rust identifier `file_system_sandbox_policy`
in the original and adapted sandbox module; neither is a delivery ID.
URL/IP candidates were reviewed as public documentation/client origins,
documentation placeholders or network-policy test literals. Commit email
metadata used GitHub noreply addresses.

No actual API key, private key, Tunnel credential, private connection
destination or user attachment ID was found in this checked scope. Reports
did not print candidate values. This is a bounded audit, not a guarantee
about objects unreachable from advertised refs or uninspected GitHub areas.

## Publication handoff

The README credits and links upstream in its opening, installation and usage
guidance. Design/deployment documents explain the independent additions.
The whole upstream snapshot and build-time source rewriting were removed;
the four maintained derived modules and byte-identical upstream MIT notice
remain. See [UPSTREAM](UPSTREAM.md) and [NOTICE](../NOTICE.md).

Source changes were saved to main with an expected-head check and ordinary
fast-forward. No history rewrite, force-push, credential rotation or host
configuration change was performed. The real stdio regression passed;
see [VALIDATION](../VALIDATION.md).

The execution environment exposes repository read/write tools for files and
Git objects, but no repository visibility mutation or authenticated browser.
An authorized operator must perform the already requested visibility change
in [repository settings](https://github.com/gw31415/dev-session-mcp/settings),
then verify `visibility=public` and the README through an unauthenticated
read. This document does not claim publication completed.
