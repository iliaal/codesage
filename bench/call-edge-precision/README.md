# Call-edge precision evaluation

This bounded evaluation checks the receiver/scope decision in `cs-0lhl` and the
observed incompatible-language impact edge. It does not claim complete type
inference or complete call graphs.

## Preregistered gates

Before modifying resolution, pin Prometheus at
`25d678b5021ca12c0c5cdb24046a182b6ca999df`. Index its complete source tree in a
temporary database. Independently inspect the source of each labeled edge.
Require every labeled true edge to survive, every targeted false edge to
disappear, unchanged raw name-reference rows, and identical cold, repeated,
reopened, incremental, and empty-rebuild fixture answers. Unknown receiver or
FFI identity must not become a proven target merely through a matching name.

The candidate language check may do constant work per existing candidate,
without new filesystem reads, database queries, or persistent caches. Accept
at most 20% median query-time growth across ten measured repetitions on the
same machine; below 5 ms absolute growth is measurement noise. Indexing must
retain the existing file/symbol coverage. These bounds apply to this panel,
not all repositories. Retain source-supported C/C++ and JavaScript/TypeScript
interoperation; do not infer arbitrary foreign-function bindings.

Run the real workload explicitly:

```sh
cargo run -p codesage-graph --example call_edge_precision -- \
  prometheus /path/to/pinned/prometheus
cargo run -p codesage-graph --example call_edge_precision -- panel
```

The real workload uses the JavaScript global `parseFloat` call in
`web/ui/mantine-ui/src/lib/formatFloatValue.ts:8` and array `join` calls in
`formatSeries.ts:17,23` as false edges to unrelated Go definitions. The
explicitly imported `escapeString` calls in `formatSeries.ts:14,16,22` are true
controls. These are source labels, independent of CodeSage's returned edges.

`prometheus-oracle.json` pins eight inspected source files and eight independently
labeled edge groups. The executable checks their hashes before indexing. It
compares resolved identities through `find_references`, one-edge traces with
exact endpoint handles and callsite lines, and depth-one impact reasons at the
labeled callsites. Same-file impact is inapplicable because impact excludes
the changed file. A found transitive path does not count as a direct edge.
`cases.json` contains the complete receiver panel. A separate source reviewer
classified the cases and corrected an invalid Rust example, an unspecified
Python external import, and a recursion control that only exercised its caller.

## Disposition

Adopt the compatible-language filter and the C-to-C++ method exclusion. The
filter runs before ambiguity counting and visibility checks, so an incompatible
namesake cannot hide a real same-language candidate. `.h` remains compatible
with C++ methods because project discovery may parse it as C++. The filter adds
no database or filesystem reads and no cache. Python binding resolution and
Rust Cargo resolution retain their existing evidence rules.

Do not adopt broad receiver inference in this change. The generic resolver
caches by caller file and name; it does not retain JavaScript lexical bindings
or Rust receiver types. A name blacklist would remove real user methods, and a
file-wide shadow rule would suppress other unshadowed call sites. Fixing these
requires position-specific binding facts, invalidation, and separate recall
evaluation. The new language filter does not make those guesses sound.

The executed 22-case panel retains all nine labeled true controls. Three false
same-language trace/impact edges remain: JavaScript local shadows, built-in
`Map.get`, and Rust `Option.map`. JavaScript callable parameters remain unknown
but receive a guessed target. Python parameter and factory receivers, and Go
interface receivers, correctly remain unresolved in this panel. True controls
cover Python self/class/import/recursion/scoped rebinding, JavaScript named
exports, a C++ receiver, and Go/Rust local calls. Existing Python tests cover
the larger import-order and binding panel.

Prometheus exposes another remaining limitation: `formatSeries.ts` explicitly
imports `escapeString` from `lib/escapeString.ts`, but the homonym in
`promql/utils.ts` also receives a false direct edge at line 14 and direct impact
reasons at lines 14, 16, and 22. The genuine `escapeString` direct edge survives,
while its `to` handle remains absent because the candidates are ambiguous.
Separately, the path from `formatSeries` through `maybeQuoteLabelName` to
`utils.escapeString` is genuine: its two calls occur at lines 16 and 288.
The evaluation records both legs separately. That legitimate indirect path
does not establish the false direct edge and must survive a later binding fix.
`formatSeries.ts` can therefore legitimately remain a dependent of the utils
file. Whole-file impact membership is not the oracle for this false direct
call; only the labeled `escapeString` callsite reasons are compared.

`results.json` records measured outcomes and costs. The receiver panel checks
repeated, reopened, whitespace-edited, and empty-rebuild parity. Those edits
preserve graph answers and do not prove changed-edge invalidation. The regular
`semantic_caller_edit_replaces_edges_before_restoration` regression changes
the warmed caller from `first()` to `second()`, queries that changed state,
then restores `first()` and queries again. Reference target identities, direct
trace, impact, and reachable tests must all switch and restore; each state
must equal an independent full rebuild. A negative control that withheld
incremental indexing failed on the stale `first()` edge before restoration.
Other regular regressions cover bundles and cross-language test reachability.
Raw name-reference rows remain available. Graph absence is not proof of
unreachability, and arbitrary FFI calls remain unmodeled.

The full Prometheus index retained 951 files, 13,590 symbols, and 120,614
references, with zero failed files and one degraded parse both before and
after the change. Replaying baseline and candidate with the corrected direct
oracle gave three-query warm medians of 12.813 ms and 15.864 ms, respectively,
across ten repetitions in debug builds. The 23.8% relative increase exceeded
the 20% branch of the preregistered gate; the 3.052 ms absolute increase passed
its below-5-ms allowance. Concurrent development load and the small query set
prevent a general performance claim. All four true edge groups retained their direct trace;
three cross-language false groups were rejected and the same-language import
homonym remained, as described above.

Suggested follow-up: retain position-specific JavaScript import and lexical
bindings, first reproduce the two `escapeString` targets and local/parameter
shadows, then require the genuine imported target and unshadowed callers to
survive. Evaluate built-in receiver evidence separately from binding identity;
do not infer a receiver's class from a globally unique method name.
