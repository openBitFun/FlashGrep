# Branch Switch Incremental Index Validation

Validation date: 2026-07-24

Binary: `target/release/flashgrep` built from the current worktree.

## Automated Gates

- `cargo test`: 226 library tests passed, 1 pre-existing ignored test; 34 e2e, 5 managed-daemon, and 5 MCP integration tests passed.
- `cargo clippy --all-targets --all-features -- -D warnings`: passed.
- `node --check mcp/server.js`: passed.

## Synthetic 100k Repository

The repository contained 100,000 tracked Rust files at H0. Four sequential commits modified exactly
1, 256, 1,000, and 10,000 distinct tracked files. Storage and the release daemon were isolated from
the flashgrep source worktree.

| Transition | Git diff paths | New live delta docs | New delete records | Base align time | Cumulative delta bytes |
| --- | ---: | ---: | ---: | ---: | ---: |
| Initial H0 | 100,000 files | 100,000 indexed | 0 | 2.75 s | 0 |
| H0 -> H1 | 1 | 1 | 1 | <= 0.22 s | 115 |
| H1 -> H2 | 256 | 256 | 256 | 1.05 s | 29,555 |
| H2 -> H3 | 1,000 | 1,000 | 1,000 | 1.20 s | 144,555 |
| H3 -> H4 | 10,000 | 10,000 | 10,000 | 3.02 s | 1,294,555 |

The active H4 manifest retained the original foundation segment IDs and contained live delta segment
document counts `[1, 256, 1000, 10000]`. No transition enumerated or tokenized the other 100,000
tracked files. Search commands issued during the 256/1,000/10,000 advances had median end-to-end CLI
latencies of 104.6/99.1/97.5 ms. Peak daemon RSS across initial build and all advances was 344,064 KiB.

Switching from H4 back to cached H2 restored the exact original H2 manifest ID. This created no new
manifest and restored `base_delta_depth=2`, proving cache activation performed zero base tokenization.

## Chromium Repository

The supplied `/Users/user/workspace/chromium-src` checkout is a depth-1 snapshot, so validation used a
detached temporary worktree at its existing commit and created two local child commits. The user's
`main` worktree and its modified `.gitignore` were not changed.

- Chromium tracked files: 493,140.
- Indexable documents at the base commit: 444,146.
- Initial full build baseline: 135.07 s.
- Peak daemon RSS: 3,158,624 KiB.
- Peak index directory size sampled during the run: 5,033,388 KiB.

The first child commit modified 1,000 real `.cc` files. Base alignment took 4.42 s and produced one
live delta with 1,000 documents plus one delete delta with 1,000 records (13,369,702 bytes).

The second child commit modified another 10,000 real `.cc` files. Refresh and base alignment ran
concurrently with 260 scoped searches:

- total alignment time: 28.64 s;
- new live delta documents: 10,000;
- new delete records: 10,000;
- cumulative delta bytes: 131,203,187;
- median/max search latency: 97.0/504.5 ms, including CLI process startup;
- observed backends: transition `rg_fallback`, then `indexed_workspace_view`, then `indexed_clean`;
- every sample with overlay aligned to the target while base still referenced the parent used
  `indexed_workspace_view` and returned the expected result.

Switching back to the original Chromium commit restored `manifest-0000000000000001`, the exact
initial manifest. The manifest count remained three, `base_delta_depth` returned to zero, and no base
tokenization or candidate publication occurred. The 4.98 s wall time was workspace overlay reconcile
for the 11,000 changed worktree paths, not base reindexing.

## Memory And Append Mode

The performance runs include daemon RSS for full readers and searches. The append-mode invariant is
also covered directly by `manifest_only_reopen_does_not_read_segment_documents`, whose segment store
fails if durable document reads occur. This proves base delta publication does not materialize the
full durable document history in the writer.
