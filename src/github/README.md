# GitHub capabilities

Enable the optional `github` Cargo feature to use `fridica_agent::github`.
Fridica bundles this module as `fridica github-mcp`. Hosts own CLI parsing,
placement policy, and default enablement; the library owns tools and transport.
No separate server installation or runtime code downloads are needed.

- [octocrab](https://github.com/XAMPPRocky/octocrab) handles GitHub REST/GraphQL.
  Retries and redirect following are disabled. The CLI fixes the host to GitHub.com.
- [gitoxide](https://github.com/GitoxideLabs/gitoxide) validates object IDs/refs and
  reads local HEAD/branch with isolated configuration inside the worker directory.
- `tools.rs` defines the advertised schemas and read/write tool boundary.
- `api.rs` maps validated arguments to GitHub endpoints, checks review heads, and
  uses non-force Git reference updates for multi-file commits.
- `mod.rs` owns bounded newline-delimited MCP stdio, authentication and dispatch.

`GH_TOKEN` (or `--token-env NAME`) supplies authentication, with target-local
`gh auth token --hostname github.com` as fallback. Credentials are resolved lazily,
so initialization and tool listing work offline. `--read-only` rejects write calls
as well as hiding them. `--probe` only reads authenticated identity.

Multi-file commits accept UTF-8 text, preserve existing regular-file executable
modes, and refuse symlink/submodule replacement or truncated tree listings.
Reviews are pinned to the inspected commit ID; head-check and submission are not
an atomic transaction. Inspect GitHub after uncertain writes before retrying.
There are no merge, release, admin, arbitrary HTTP, shell or force-push tools.

Run `cargo test --features github --test github` from the agent repository. Tests use local
HTTP fixtures, never real GitHub writes. Fridica tests and wheel smoke tests exercise
the bundled executable's stdio interface without credentials.
