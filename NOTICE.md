# Third-party notices

Parts of `rust/src/base/{config,sandbox,tools}.rs` are derived from
[nakasyou/local-mcp](https://github.com/nakasyou/local-mcp), by Shotaro Nakamura.
The original source was taken from commit
`21025d048f54cc9f948c26ac42fa36183dc453c2`. This identifies the origin of the
adapted code; it is not a build-time source download or snapshot dependency.

Copyright (c) 2026 Shotaro Nakamura

The upstream MIT license and copyright notice are preserved verbatim in
[licenses/local-mcp-MIT.txt](licenses/local-mcp-MIT.txt).
At that commit, upstream's LICENSE file contains MIT terms, while its
Cargo.toml labels the package `Apache-2.0`. This repository records that
metadata discrepancy and preserves the supplied license text; it does not
rewrite the upstream notice or claim that upstream resolved the discrepancy.

dev-session-mcp is independently maintained. The local-mcp author has not
endorsed this project, and this is not an official local-mcp release.
See [the upstream relationship and maintenance plan](docs/UPSTREAM.md).

Original additions in this repository use the root [MIT LICENSE](LICENSE).
Cargo dependencies retain their respective licenses and notices.
