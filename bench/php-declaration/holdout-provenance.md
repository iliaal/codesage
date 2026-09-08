# Source-grounded path-ranking holdout

The 32 cases were frozen before running or inspecting experimental rankings. The author read the two bead descriptions and target source, but did not inspect search implementation changes or existing result captures. The original measured JSON SHA-256 was `0ddbd1c47a530b177f79cd714ce1acafab97d359bcf4476f763fb8e9bde42bf7`.

For publication, local absolute project paths were replaced with their final path component: `monolog`, `laravel-framework`, or `php-src`. The published `holdout.json` SHA-256 is `1f5430ebced8ccc700a8acf01c4710caee1ca31e50e36339553882b1c852c97a`. This deterministic normalization changes only the `project` values. All 32 queries, expected-file lists, categories, and case ordering are unchanged. The evaluator already performed this same normalization before the measured pipeline calls; its effective inputs match the published cases exactly.

The retained result and control captures preserve the original `cases_sha256` and add `normalized_cases_sha256` for the published input. Their rankings were not regenerated or relabeled as a new run. Resolve each project key to your clone with `--project KEY=PATH`.

## Repositories

| Project key | Source revision |
|---|---|
| `monolog` | `68b974809baff3f071893de61447212e9e688ee7` |
| `laravel-framework` | `c1bc8e758a63ab3e404ead77f7fd85ca1dbd3c01` |
| `php-src` | `1364de0472b859917f5bc0be9193bd8068fc2061` |

All expected source files were clean against these revisions. All three repositories have `.codesage/index.db`; index existence alone does not establish semantic freshness. Verify index freshness and embedding configuration before measuring.

## Selection and limits

Cases 1–8 request PHP behavior; 9–16 deliberately request legitimate interfaces, contracts, or facades. Cases 17–24 request Unix or shared C behavior; 25–32 explicitly request Windows behavior on a Linux host. Source location, function bodies, and declared methods determine expected files. A hit means that at least one expected file appears, not that every relevant implementation has been enumerated. Keep category results separate: an aggregate improvement must not hide declaration-target or foreign-platform regressions.

The sample is manually selected and small. PHP cases reuse two repositories named in the original bead, so they provide new-query evidence rather than unseen-repository generalization. Every declaration control uses an explicit declaration word; implicit declaration intent remains untested. PHP facades can contain executable behavior, as the Event fake control demonstrates.

The C sample adds php-src outside the bead's libuv evidence. It does not satisfy a three-repository generalization claim: Redis and nginx were not located in the searched repository roots. Guzzle was also not located. Searches covered the local benchmark clones, reference mirrors, general repository collection, and `/tmp` to depth three; some system-owned temporary directories were unreadable. This is a search limitation, not proof those repositories are absent everywhere. The php-src project also has older evaluation corpora elsewhere; no claim is made that this repository has never been benchmarked. Cases here were authored without reading those corpora.

Some host cases exercise shared C files with conditional platform branches, rather than Unix/Windows mirror directories. They check preservation of legitimate shared behavior. Explicit Windows controls check opt-out behavior, not relevance of implicit platform intent. These cases cannot establish Windows-host mirror behavior, all-Windows uniform-demotion behavior, or justify enabling platform demotion by default. Preserve the bead's default-off gate unless broader evidence supports a separate decision.

## Source evidence

Case numbers are one-based JSON order. Paths below are relative to the corresponding project above.

| Cases | Source evidence |
|---|---|
| 1–2 | Monolog `src/Monolog/Logger.php:332`: `addRecord` limits recursive depth, initializes a record, and applies processors while walking handlers. |
| 3 | Monolog `src/Monolog/Handler/StreamHandler.php:135`: `write` opens the stream and takes `LOCK_EX` when locking is enabled. |
| 4 | Monolog `src/Monolog/Handler/BufferHandler.php:91`: `flush` sends its buffer to the wrapped handler's `handleBatch`. |
| 5 | Laravel `src/Illuminate/Cache/ArrayStore.php:113`: `put` stores the value and calls `calculateExpiration`. |
| 6–7 | Laravel `src/Illuminate/Events/Dispatcher.php:320` stops on a false listener response; `setupWildcardListen` at line 156 stores the listener and clears cached wildcard matches. |
| 8 | Laravel `src/Illuminate/Validation/Validator.php:460`: `passes` loops rules and checks `stopOnFirstFailure`. |
| 9 | Monolog `src/Monolog/Handler/HandlerInterface.php:34`: declarations and the `handle` contract document bubbling semantics. |
| 10 | Monolog `src/Monolog/Processor/ProcessorInterface.php:26`: `__invoke` accepts and returns a log record. |
| 11 | Monolog `src/Monolog/Formatter/FormatterInterface.php:29`: `format` and `formatBatch` declarations. |
| 12 | Laravel `src/Illuminate/Contracts/Cache/Store.php:23`: `many` and timed `put` declarations. |
| 13 | Laravel `src/Illuminate/Contracts/Queue/Queue.php:21`: pending, delayed, and reserved size declarations. |
| 14 | Laravel `src/Illuminate/Contracts/Validation/Validator.php:25`: `validated`, `fails`, and `failed` declarations. |
| 15 | Laravel `src/Illuminate/Support/Facades/Cache.php:83`: accessor returns the cache container key. |
| 16 | Laravel `src/Illuminate/Support/Facades/Event.php:54`: `fake` replaces the dispatcher with `EventFake`. |
| 17–18 | php-src `ext/pcntl/pcntl.c:266` calls `fork`; `pcntl_sigprocmask` at line 946 validates and applies signal mask operations. |
| 19 | php-src `ext/posix/posix.c:170`: user identity functions call POSIX UID APIs. |
| 20 | php-src `ext/sockets/sockets.c:2425`: `socket_create_pair` reaches the native `socketpair` call. |
| 21 | php-src `ext/standard/proc_open.c:1208`: subprocess creation consumes descriptor specifications and the pipes output argument. |
| 22 | php-src `main/network.c:919`: connection setup resolves host addresses and accepts a timeout. |
| 23 | php-src `main/streams/plain_wrapper.c:1100`: directory stream read calls `readdir` and copies the entry name. |
| 24 | php-src `ext/standard/file.c:206`: `flock` parses its operation and optional would-block output. |
| 25 | php-src `win32/select.c:32`: select emulation manages socket sets and Windows handles. |
| 26–27 | php-src `win32/time.c:50` implements `gettimeofday`; `usleep` at line 66 uses `CreateWaitableTimer` and `WaitForSingleObject`. |
| 28 | php-src `win32/signal.c:102`: console control handler registration parses a callback and calls `SetConsoleCtrlHandler`. |
| 29 | php-src `win32/codepage.c:80`: UTF-8 conversion invokes wide-character conversion with `CP_UTF8`. |
| 30 | php-src `win32/ioutil.c:384`: wide-character unlink opens the path with deletion access. |
| 31 | php-src `win32/sendmail.c:188`: `TSendMail` accepts the SMTP host, headers, recipient, and message data. |
| 32 | php-src `win32/sockets.c:25`: socket-pair emulation creates sockets, binds, listens, and establishes the second endpoint. |

No source repository, ledger record, Git state, or ranking configuration was changed while authoring this holdout.
