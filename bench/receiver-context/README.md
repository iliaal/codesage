# TypeScript receiver contract retrieval

Use this opt-in utility to retrieve a method declaration through TypeScript receiver types and imports. For example, `api.users.create()` can lead through an imported callback fixture, `ApiHelpers.users`, and `UserApiHelper.create`. A same-named function elsewhere does not establish that link.

The utility uses TypeScript's [compiler API](https://github.com/microsoft/TypeScript/wiki/Using-the-Compiler-API), pinned to 5.9.2. It produces declaration candidates and source provenance for the experimental Whetstone context selector. It does not change CodeSage's Rust graph, database, MCP tools, or review defaults. No model runs are involved.

## Run it

You need Node.js, Git, a checkout at the reviewed commit, and a trusted TypeScript installation outside that checkout. Install dependencies in a separate directory with package scripts disabled:

```bash
receiver_deps=$(mktemp -d)
npm install --prefix "$receiver_deps" --ignore-scripts --no-audit --no-fund \
  --save-exact typescript@5.9.2
```

If the receiver type depends on external declarations, install the project's exact dependency versions in that directory too. This utility neither downloads dependencies nor invents missing declarations. `--dependency-root` supplies a fallback for package resolution when a module is missing from the checkout. The receipt lists those fallbacks and loaded package versions; check them against the reviewed manifest and lockfile.

From the CodeSage repository, run:

```bash
node bench/receiver-context/resolve.mjs \
  --project /absolute/reviewed-checkout \
  --head FULL_REVIEWED_COMMIT_SHA \
  --typescript "$receiver_deps/node_modules/typescript/lib/typescript.js" \
  --dependency-root "$receiver_deps" \
  --tsconfig path/to/tsconfig.json \
  --file path/to/changed-test.ts \
  --output /absolute/new-receipt.json
```

Repeat `--file` for each reviewed TypeScript file. The output must not exist. Use a source checkout without untracked `node_modules`: every compiler-read file inside the checkout must match Git HEAD. Keep installed dependency declarations in the separate trusted directory. Omit `--dependency-root` when the analysis needs no external packages. Omit `--tsconfig` only when the recorded default options fit the project: strict checking, ES2021, CommonJS, Node10 resolution, and no automatic ambient type packages.

The program starts from the explicit files and follows their imports. It loads compiler options and inherited configuration from `--tsconfig`, but does not build project references or use the configuration's full file list. Dependency fallback is an analysis configuration, not a reproduction of the project's installed environment. Declaration errors are recorded even when another receiver resolves successfully.

To add the result to Whetstone's existing experimental packet, save it as `<case-id>.json` and pass `--receiver-context-directory /absolute/receipts` to `distillery/benchmarks/aacr/symbol_context.py`. That selector requires its prepared review inputs and a fresh CodeSage structural index at the same head. It verifies the receipt and matches each target against an indexed symbol before applying the existing packet budget. Without the option, selection stays unchanged.

## What the result establishes

`calls[]` identifies calls by repository path, LF-based line number, UTF-8 byte column, and token. A `candidate` has exactly one in-project function or method implementation declaration. `receiver_chain` records each receiver's inferred type and declaration locations. `module_resolutions`, `reads`, compiler identity, options, package versions, and diagnostics retain the analysis inputs. Project files read by the compiler must match Git HEAD; all read files and the compiler carry SHA-256 hashes.

The resolver abstains on `any`, unknown or union/intersection receivers, unresolved type parameters, dynamic keys, signature-only declarations, and unsupported receiver expressions. Aliased imports, re-exports, lexical shadowing, and contextual callback types follow the compiler's rules. A typed shadow can resolve to a different method; an untyped shadow must remain unresolved.

These are declaration candidates, not proven runtime targets. Overrides, monkey-patching, casts, inaccurate declarations, and other runtime behavior can differ. A receipt is a local analysis artifact, not an authenticated authority. Missing dependencies and diagnostics make the analysis partial; a successful process does not establish a clean TypeScript build or complete call coverage.

The host reads source, JSON configuration, and external declaration files. It does not load target plugins, run package scripts, emit code, or execute the reviewed application. Inputs are bounded to 1,024 read files, 64 MiB, and 10,000 recorded call sites; exceeding a bound fails without writing the receipt. These bounds are not a CPU or memory sandbox for TypeScript itself.

## Verification

```bash
TYPESCRIPT_PATH="$receiver_deps/node_modules/typescript/lib/typescript.js" \
  node --test bench/receiver-context/test-resolve.mjs
node --check bench/receiver-context/resolve.mjs
```

Tests use temporary Git repositories and the real compiler. They cover generic callback inference, import aliases and barrels, homonyms, shadowing, ambiguity, missing declarations, exact source positions, and provenance refusals.

On n8n development commit `ebe680fbcacd9210aa219ab01d98012ac1d6faae`, TypeScript 5.9.2 and official Playwright 1.54.2 declarations resolve all four `api.users.create()` calls in `user-service.spec.ts` to `UserApiHelper.create`. The integrated 6,754-byte packet includes the complete method containing the default surname and return value. The partial checkout produces 36 compiler diagnostics, retained in the receipt. Without Playwright declarations, those calls remain unresolved. This is retrieval validation on a known development case, not evidence of improved reviewer accuracy; no additional model comparison has been run.

The default selector's output remains byte-identical on the five original development cases. A separate synthetic integration probe with an actual CodeSage index verifies conversion from its Unicode character columns to the compiler receipt's UTF-8 byte columns. The Node suite has 13 passing tests; Whetstone's preparation and selector suite has 43. Rust workspace tests are outside this benchmark-only change; no Rust source or shipped dependency changed.
