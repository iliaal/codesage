//! Per-language rules that map an imported / included / called name to one or
//! more [`TrustBoundary`] tags. Single source of truth: the boundary
//! derivation engine in [`crate::trust_boundary`] reads these tables.
//!
//! The rule set is deliberately conservative. False positives (a file imports
//! `reqwest` but only uses its `Url` type, not its client) are accepted as a
//! cost for keeping the engine simple and import-graph-driven. The risk score
//! cares about *boundary count*, not perfect attribution, so a small amount
//! of over-tagging is fine.
//!
//! Adding entries: pick the most specific prefix that uniquely identifies the
//! capability. For Rust paths use `module::sub` with no trailing `::`; the
//! engine matches `name == prefix` or `name starts with prefix + "::"`. For C
//! includes use the exact header path as it appears between `<…>` / `"…"`.
//! For PHP/Python/Go/JS, use the module/function name as the parser records
//! it in `refs.to_name`.

use codesage_protocol::{Language, TrustBoundary};

/// How a rule matches against a recorded reference name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchMode {
    /// Exact equality on the full `to_name`.
    Exact,
    /// `to_name == prefix` or `to_name starts with prefix + "::"`. Used for
    /// scoped paths like Rust `tokio::net`.
    PrefixDoubleColon,
    /// `to_name == prefix` or `to_name starts with prefix + "\\"`. Used for
    /// PHP namespaces like `GuzzleHttp\Client`.
    PrefixBackslash,
    /// `to_name == prefix` or `to_name starts with prefix + "/"`. Used for
    /// Go import paths like `net/http` and JS subpath imports.
    PrefixSlash,
    /// `to_name == prefix` or `to_name starts with prefix + "."`. Used for
    /// Python dotted modules like `os.path`.
    PrefixDot,
    /// Match if the recorded name *contains* the substring. Use sparingly —
    /// only when the captured form varies across PHP function vs static call
    /// vs method call sites that all share the same suffix.
    Contains,
}

/// One rule: a name pattern + the boundaries it implies.
#[derive(Debug, Clone, Copy)]
pub struct TrustBoundaryRule {
    pub pattern: &'static str,
    pub mode: MatchMode,
    pub boundaries: &'static [TrustBoundary],
}

const fn rule(
    pattern: &'static str,
    mode: MatchMode,
    boundaries: &'static [TrustBoundary],
) -> TrustBoundaryRule {
    TrustBoundaryRule {
        pattern,
        mode,
        boundaries,
    }
}

const NETWORK: &[TrustBoundary] = &[TrustBoundary::Network];
const NETWORK_API: &[TrustBoundary] = &[TrustBoundary::Network, TrustBoundary::ExternalApi];
const FS: &[TrustBoundary] = &[TrustBoundary::Filesystem];
const EXEC: &[TrustBoundary] = &[TrustBoundary::ProcessExec];
const SECRETS: &[TrustBoundary] = &[TrustBoundary::Secrets];
const SECRETS_NET: &[TrustBoundary] = &[TrustBoundary::Secrets, TrustBoundary::Network];
const DB: &[TrustBoundary] = &[TrustBoundary::Database];
const USER_INPUT: &[TrustBoundary] = &[TrustBoundary::UserInput];
const SERIALIZATION: &[TrustBoundary] = &[TrustBoundary::Serialization];
const CONCURRENCY: &[TrustBoundary] = &[TrustBoundary::Concurrency];

/// Rust crates and `std::` modules. Match mode is `PrefixDoubleColon` so a
/// bare `use reqwest::Client;` and `use reqwest::header::HeaderMap;` both
/// resolve to the `reqwest` rule.
const RUST_RULES: &[TrustBoundaryRule] = &[
    // network
    rule("reqwest", MatchMode::PrefixDoubleColon, NETWORK_API),
    rule("hyper", MatchMode::PrefixDoubleColon, NETWORK),
    rule("ureq", MatchMode::PrefixDoubleColon, NETWORK_API),
    rule("isahc", MatchMode::PrefixDoubleColon, NETWORK_API),
    rule("surf", MatchMode::PrefixDoubleColon, NETWORK_API),
    rule("awc", MatchMode::PrefixDoubleColon, NETWORK_API),
    rule("tokio::net", MatchMode::PrefixDoubleColon, NETWORK),
    rule("std::net", MatchMode::PrefixDoubleColon, NETWORK),
    rule("tonic", MatchMode::PrefixDoubleColon, NETWORK_API),
    rule("axum", MatchMode::PrefixDoubleColon, NETWORK),
    rule("actix_web", MatchMode::PrefixDoubleColon, NETWORK),
    rule("warp", MatchMode::PrefixDoubleColon, NETWORK),
    rule("rocket", MatchMode::PrefixDoubleColon, NETWORK),
    // filesystem
    rule("std::fs", MatchMode::PrefixDoubleColon, FS),
    rule("tokio::fs", MatchMode::PrefixDoubleColon, FS),
    rule("async_fs", MatchMode::PrefixDoubleColon, FS),
    rule("memmap2", MatchMode::PrefixDoubleColon, FS),
    rule("walkdir", MatchMode::PrefixDoubleColon, FS),
    rule("ignore", MatchMode::PrefixDoubleColon, FS),
    rule("notify", MatchMode::PrefixDoubleColon, FS),
    // process-exec
    rule("std::process", MatchMode::PrefixDoubleColon, EXEC),
    rule("tokio::process", MatchMode::PrefixDoubleColon, EXEC),
    rule("subprocess", MatchMode::PrefixDoubleColon, EXEC),
    rule("duct", MatchMode::PrefixDoubleColon, EXEC),
    rule("nix::unistd", MatchMode::PrefixDoubleColon, EXEC),
    // secrets / crypto / env
    rule("std::env", MatchMode::PrefixDoubleColon, SECRETS),
    rule("dotenv", MatchMode::PrefixDoubleColon, SECRETS),
    rule("dotenvy", MatchMode::PrefixDoubleColon, SECRETS),
    rule("ring", MatchMode::PrefixDoubleColon, SECRETS),
    rule("rustls", MatchMode::PrefixDoubleColon, SECRETS_NET),
    rule("openssl", MatchMode::PrefixDoubleColon, SECRETS_NET),
    rule("sodium", MatchMode::PrefixDoubleColon, SECRETS),
    rule("sodiumoxide", MatchMode::PrefixDoubleColon, SECRETS),
    rule("argon2", MatchMode::PrefixDoubleColon, SECRETS),
    rule("bcrypt", MatchMode::PrefixDoubleColon, SECRETS),
    rule("jsonwebtoken", MatchMode::PrefixDoubleColon, SECRETS),
    // database
    rule("sqlx", MatchMode::PrefixDoubleColon, DB),
    rule("rusqlite", MatchMode::PrefixDoubleColon, DB),
    rule("diesel", MatchMode::PrefixDoubleColon, DB),
    rule("sea_orm", MatchMode::PrefixDoubleColon, DB),
    rule("tokio_postgres", MatchMode::PrefixDoubleColon, DB),
    rule("mongodb", MatchMode::PrefixDoubleColon, DB),
    rule("redis", MatchMode::PrefixDoubleColon, DB),
    rule("mysql_async", MatchMode::PrefixDoubleColon, DB),
    // user-input
    rule("clap", MatchMode::PrefixDoubleColon, USER_INPUT),
    rule("structopt", MatchMode::PrefixDoubleColon, USER_INPUT),
    rule("argh", MatchMode::PrefixDoubleColon, USER_INPUT),
    rule("pico_args", MatchMode::PrefixDoubleColon, USER_INPUT),
    // serialization (untrusted-input deserialization)
    rule("serde_yaml", MatchMode::PrefixDoubleColon, SERIALIZATION),
    rule("serde_xml_rs", MatchMode::PrefixDoubleColon, SERIALIZATION),
    rule("quick_xml", MatchMode::PrefixDoubleColon, SERIALIZATION),
    rule("bincode", MatchMode::PrefixDoubleColon, SERIALIZATION),
    // concurrency
    rule(
        "std::sync::Mutex",
        MatchMode::PrefixDoubleColon,
        CONCURRENCY,
    ),
    rule(
        "std::sync::RwLock",
        MatchMode::PrefixDoubleColon,
        CONCURRENCY,
    ),
    rule(
        "std::sync::atomic",
        MatchMode::PrefixDoubleColon,
        CONCURRENCY,
    ),
    rule("parking_lot", MatchMode::PrefixDoubleColon, CONCURRENCY),
    rule(
        "crossbeam_channel",
        MatchMode::PrefixDoubleColon,
        CONCURRENCY,
    ),
];

/// PHP. `to_name` for `use` imports is the full namespaced path. For function
/// and static calls the `to_name` is also recorded (e.g. `exec`, `mysqli_query`).
const PHP_RULES: &[TrustBoundaryRule] = &[
    // network — HTTP clients + cURL family
    rule("GuzzleHttp", MatchMode::PrefixBackslash, NETWORK_API),
    rule(
        "Symfony\\Component\\HttpClient",
        MatchMode::PrefixBackslash,
        NETWORK_API,
    ),
    rule("Psr\\Http\\Client", MatchMode::PrefixBackslash, NETWORK),
    rule("Http\\Client", MatchMode::PrefixBackslash, NETWORK),
    rule("React\\Http", MatchMode::PrefixBackslash, NETWORK),
    rule("Amp\\Http", MatchMode::PrefixBackslash, NETWORK),
    rule("curl_exec", MatchMode::Exact, NETWORK_API),
    rule("curl_init", MatchMode::Exact, NETWORK_API),
    rule("curl_multi_exec", MatchMode::Exact, NETWORK_API),
    rule(
        "file_get_contents",
        MatchMode::Exact,
        &[TrustBoundary::Filesystem, TrustBoundary::Network],
    ),
    rule("fsockopen", MatchMode::Exact, NETWORK),
    rule("stream_socket_client", MatchMode::Exact, NETWORK),
    rule("socket_create", MatchMode::Exact, NETWORK),
    // filesystem
    rule("fopen", MatchMode::Exact, FS),
    rule("fread", MatchMode::Exact, FS),
    rule("fwrite", MatchMode::Exact, FS),
    rule("file_put_contents", MatchMode::Exact, FS),
    rule("file", MatchMode::Exact, FS),
    rule("unlink", MatchMode::Exact, FS),
    rule("rename", MatchMode::Exact, FS),
    rule("copy", MatchMode::Exact, FS),
    rule("mkdir", MatchMode::Exact, FS),
    rule("rmdir", MatchMode::Exact, FS),
    rule("SplFileObject", MatchMode::Exact, FS),
    rule(
        "Symfony\\Component\\Filesystem",
        MatchMode::PrefixBackslash,
        FS,
    ),
    // process-exec
    rule("exec", MatchMode::Exact, EXEC),
    rule("system", MatchMode::Exact, EXEC),
    rule("passthru", MatchMode::Exact, EXEC),
    rule("shell_exec", MatchMode::Exact, EXEC),
    rule("proc_open", MatchMode::Exact, EXEC),
    rule("popen", MatchMode::Exact, EXEC),
    rule("pcntl_exec", MatchMode::Exact, EXEC),
    rule(
        "Symfony\\Component\\Process",
        MatchMode::PrefixBackslash,
        EXEC,
    ),
    // secrets / env
    rule("getenv", MatchMode::Exact, SECRETS),
    rule("password_hash", MatchMode::Exact, SECRETS),
    rule("password_verify", MatchMode::Exact, SECRETS),
    rule("openssl_encrypt", MatchMode::Exact, SECRETS),
    rule("openssl_decrypt", MatchMode::Exact, SECRETS),
    rule("openssl_sign", MatchMode::Exact, SECRETS),
    rule("hash_hmac", MatchMode::Exact, SECRETS),
    rule("sodium_crypto", MatchMode::Contains, SECRETS),
    // database
    rule("PDO", MatchMode::Exact, DB),
    rule("mysqli", MatchMode::Exact, DB),
    rule("mysqli_query", MatchMode::Exact, DB),
    rule("mysqli_connect", MatchMode::Exact, DB),
    rule("pg_query", MatchMode::Exact, DB),
    rule("pg_connect", MatchMode::Exact, DB),
    rule("sqlite_open", MatchMode::Exact, DB),
    rule("Doctrine\\DBAL", MatchMode::PrefixBackslash, DB),
    rule("Doctrine\\ORM", MatchMode::PrefixBackslash, DB),
    rule("Illuminate\\Database", MatchMode::PrefixBackslash, DB),
    rule("Cake\\Database", MatchMode::PrefixBackslash, DB),
    // user-input (request superglobals + framework request bags)
    rule("filter_input", MatchMode::Exact, USER_INPUT),
    rule(
        "Illuminate\\Http\\Request",
        MatchMode::PrefixBackslash,
        USER_INPUT,
    ),
    rule(
        "Symfony\\Component\\HttpFoundation\\Request",
        MatchMode::PrefixBackslash,
        USER_INPUT,
    ),
    rule(
        "Psr\\Http\\Message\\ServerRequestInterface",
        MatchMode::PrefixBackslash,
        USER_INPUT,
    ),
    // serialization
    rule("unserialize", MatchMode::Exact, SERIALIZATION),
    rule(
        "Symfony\\Component\\Yaml",
        MatchMode::PrefixBackslash,
        SERIALIZATION,
    ),
    rule("simplexml_load_string", MatchMode::Exact, SERIALIZATION),
    rule("simplexml_load_file", MatchMode::Exact, SERIALIZATION),
    rule("DOMDocument", MatchMode::Exact, SERIALIZATION),
];

/// C: matches the include path verbatim, as recorded between `<...>` or `"..."`.
/// Tree-sitter strips the brackets/quotes, so the rule patterns are bare paths.
const C_RULES: &[TrustBoundaryRule] = &[
    // network
    rule("sys/socket.h", MatchMode::Exact, NETWORK),
    rule("netinet/in.h", MatchMode::Exact, NETWORK),
    rule("netinet/tcp.h", MatchMode::Exact, NETWORK),
    rule("netdb.h", MatchMode::Exact, NETWORK),
    rule("arpa/inet.h", MatchMode::Exact, NETWORK),
    rule("curl/curl.h", MatchMode::Exact, NETWORK_API),
    rule("microhttpd.h", MatchMode::Exact, NETWORK),
    // filesystem
    rule("fcntl.h", MatchMode::Exact, FS),
    rule("sys/stat.h", MatchMode::Exact, FS),
    rule("dirent.h", MatchMode::Exact, FS),
    rule("ftw.h", MatchMode::Exact, FS),
    rule("sys/mman.h", MatchMode::Exact, FS),
    // process-exec
    rule("sys/wait.h", MatchMode::Exact, EXEC),
    rule("spawn.h", MatchMode::Exact, EXEC),
    rule("sys/exec.h", MatchMode::Exact, EXEC),
    // unistd.h is fs+exec — keep both, since callers can rely on either
    rule(
        "unistd.h",
        MatchMode::Exact,
        &[TrustBoundary::Filesystem, TrustBoundary::ProcessExec],
    ),
    // secrets / crypto
    rule("openssl/ssl.h", MatchMode::Exact, SECRETS_NET),
    rule("openssl/evp.h", MatchMode::Exact, SECRETS),
    rule("openssl/sha.h", MatchMode::Exact, SECRETS),
    rule("openssl/rand.h", MatchMode::Exact, SECRETS),
    rule("openssl/aes.h", MatchMode::Exact, SECRETS),
    rule("sodium.h", MatchMode::Exact, SECRETS),
    rule("mbedtls/ssl.h", MatchMode::Exact, SECRETS_NET),
    rule("crypt.h", MatchMode::Exact, SECRETS),
    // database
    rule("sqlite3.h", MatchMode::Exact, DB),
    rule("mysql.h", MatchMode::Exact, DB),
    rule("mysql/mysql.h", MatchMode::Exact, DB),
    rule("libpq-fe.h", MatchMode::Exact, DB),
    rule("postgresql/libpq-fe.h", MatchMode::Exact, DB),
    // concurrency
    rule("pthread.h", MatchMode::Exact, CONCURRENCY),
    rule("stdatomic.h", MatchMode::Exact, CONCURRENCY),
    rule("threads.h", MatchMode::Exact, CONCURRENCY),
    rule("semaphore.h", MatchMode::Exact, CONCURRENCY),
    // call-site rules (when refs.kind = 'call' captures these)
    rule("getenv", MatchMode::Exact, SECRETS),
    rule("setenv", MatchMode::Exact, SECRETS),
    rule("execve", MatchMode::Exact, EXEC),
    rule("execvp", MatchMode::Exact, EXEC),
    rule("execlp", MatchMode::Exact, EXEC),
    rule("fork", MatchMode::Exact, EXEC),
    rule("system", MatchMode::Exact, EXEC),
    rule("popen", MatchMode::Exact, EXEC),
];

/// C++. Inherits the C include rules (engine merges both lists for `.cpp`/`.h`
/// files classified as C++). The extras here are STL/Boost-specific.
const CPP_EXTRA_RULES: &[TrustBoundaryRule] = &[
    // STL
    rule("filesystem", MatchMode::Exact, FS),
    rule("fstream", MatchMode::Exact, FS),
    rule("iostream", MatchMode::Exact, USER_INPUT),
    rule("system_error", MatchMode::Exact, EXEC),
    // Boost / asio
    rule("boost/asio.hpp", MatchMode::Exact, NETWORK),
    rule("boost/asio/", MatchMode::PrefixSlash, NETWORK),
    rule("asio.hpp", MatchMode::Exact, NETWORK),
    rule("asio/", MatchMode::PrefixSlash, NETWORK),
    // pqxx
    rule("pqxx/pqxx", MatchMode::Exact, DB),
    rule("pqxx/", MatchMode::PrefixSlash, DB),
    // std namespaced call-sites (when ::-form recorded)
    rule("std::system", MatchMode::PrefixDoubleColon, EXEC),
    rule("std::getenv", MatchMode::PrefixDoubleColon, SECRETS),
    rule("std::filesystem", MatchMode::PrefixDoubleColon, FS),
];

/// Python. `to_name` captures the dotted module path (or function name for
/// calls). `PrefixDot` makes `os.environ` match the `os` rule and `os.path`
/// match without listing both.
const PYTHON_RULES: &[TrustBoundaryRule] = &[
    // network
    rule("requests", MatchMode::PrefixDot, NETWORK_API),
    rule("urllib", MatchMode::PrefixDot, NETWORK),
    rule("urllib3", MatchMode::PrefixDot, NETWORK),
    rule("httpx", MatchMode::PrefixDot, NETWORK_API),
    rule("aiohttp", MatchMode::PrefixDot, NETWORK_API),
    rule("socket", MatchMode::PrefixDot, NETWORK),
    rule("http", MatchMode::PrefixDot, NETWORK),
    rule("flask", MatchMode::PrefixDot, NETWORK),
    rule("fastapi", MatchMode::PrefixDot, NETWORK),
    rule("starlette", MatchMode::PrefixDot, NETWORK),
    rule("django", MatchMode::PrefixDot, NETWORK),
    rule("tornado", MatchMode::PrefixDot, NETWORK),
    // filesystem
    rule("os.path", MatchMode::PrefixDot, FS),
    rule("pathlib", MatchMode::PrefixDot, FS),
    rule("shutil", MatchMode::PrefixDot, FS),
    rule("tempfile", MatchMode::PrefixDot, FS),
    rule("glob", MatchMode::PrefixDot, FS),
    // process-exec
    rule("subprocess", MatchMode::PrefixDot, EXEC),
    rule("multiprocessing", MatchMode::PrefixDot, EXEC),
    // secrets / crypto / env
    rule("os.environ", MatchMode::PrefixDot, SECRETS),
    rule("getpass", MatchMode::PrefixDot, SECRETS),
    rule("hashlib", MatchMode::PrefixDot, SECRETS),
    rule("hmac", MatchMode::PrefixDot, SECRETS),
    rule("secrets", MatchMode::PrefixDot, SECRETS),
    rule("cryptography", MatchMode::PrefixDot, SECRETS),
    rule("Crypto", MatchMode::PrefixDot, SECRETS),
    rule("nacl", MatchMode::PrefixDot, SECRETS),
    rule("jwt", MatchMode::PrefixDot, SECRETS),
    rule("ssl", MatchMode::PrefixDot, SECRETS_NET),
    // database
    rule("sqlite3", MatchMode::PrefixDot, DB),
    rule("psycopg2", MatchMode::PrefixDot, DB),
    rule("psycopg", MatchMode::PrefixDot, DB),
    rule("MySQLdb", MatchMode::PrefixDot, DB),
    rule("pymysql", MatchMode::PrefixDot, DB),
    rule("aiomysql", MatchMode::PrefixDot, DB),
    rule("aiopg", MatchMode::PrefixDot, DB),
    rule("sqlalchemy", MatchMode::PrefixDot, DB),
    rule("redis", MatchMode::PrefixDot, DB),
    rule("pymongo", MatchMode::PrefixDot, DB),
    // user-input
    rule("argparse", MatchMode::PrefixDot, USER_INPUT),
    rule("click", MatchMode::PrefixDot, USER_INPUT),
    rule("typer", MatchMode::PrefixDot, USER_INPUT),
    rule("sys.argv", MatchMode::PrefixDot, USER_INPUT),
    // serialization
    rule("pickle", MatchMode::PrefixDot, SERIALIZATION),
    rule("yaml", MatchMode::PrefixDot, SERIALIZATION),
    rule("xml", MatchMode::PrefixDot, SERIALIZATION),
    rule("marshal", MatchMode::PrefixDot, SERIALIZATION),
    rule("shelve", MatchMode::PrefixDot, SERIALIZATION),
    // concurrency
    rule("threading", MatchMode::PrefixDot, CONCURRENCY),
    rule("asyncio", MatchMode::PrefixDot, CONCURRENCY),
];

/// Go: `to_name` is the import path string (quotes stripped by the parser).
const GO_RULES: &[TrustBoundaryRule] = &[
    // network
    rule("net", MatchMode::PrefixSlash, NETWORK),
    rule("net/http", MatchMode::PrefixSlash, NETWORK),
    rule("net/rpc", MatchMode::PrefixSlash, NETWORK),
    rule("net/url", MatchMode::PrefixSlash, NETWORK),
    rule(
        "google.golang.org/grpc",
        MatchMode::PrefixSlash,
        NETWORK_API,
    ),
    rule("github.com/gin-gonic/gin", MatchMode::PrefixSlash, NETWORK),
    rule("github.com/gofiber/fiber", MatchMode::PrefixSlash, NETWORK),
    rule("github.com/labstack/echo", MatchMode::PrefixSlash, NETWORK),
    // filesystem
    rule("io/ioutil", MatchMode::PrefixSlash, FS),
    rule("io/fs", MatchMode::PrefixSlash, FS),
    rule("path/filepath", MatchMode::PrefixSlash, FS),
    // process-exec
    rule("os/exec", MatchMode::PrefixSlash, EXEC),
    rule("syscall", MatchMode::PrefixSlash, EXEC),
    // secrets / env
    rule("crypto", MatchMode::PrefixSlash, SECRETS),
    rule("crypto/tls", MatchMode::PrefixSlash, SECRETS_NET),
    rule("crypto/rand", MatchMode::PrefixSlash, SECRETS),
    rule("golang.org/x/crypto", MatchMode::PrefixSlash, SECRETS),
    // database
    rule("database/sql", MatchMode::PrefixSlash, DB),
    rule("github.com/jmoiron/sqlx", MatchMode::PrefixSlash, DB),
    rule("github.com/lib/pq", MatchMode::PrefixSlash, DB),
    rule("github.com/go-sql-driver/mysql", MatchMode::PrefixSlash, DB),
    rule("github.com/jackc/pgx", MatchMode::PrefixSlash, DB),
    rule("github.com/go-redis/redis", MatchMode::PrefixSlash, DB),
    rule("go.mongodb.org/mongo-driver", MatchMode::PrefixSlash, DB),
    rule("gorm.io/gorm", MatchMode::PrefixSlash, DB),
    // user-input
    rule("flag", MatchMode::Exact, USER_INPUT),
    rule("github.com/spf13/cobra", MatchMode::PrefixSlash, USER_INPUT),
    rule("github.com/urfave/cli", MatchMode::PrefixSlash, USER_INPUT),
    // serialization
    rule("encoding/xml", MatchMode::PrefixSlash, SERIALIZATION),
    rule("encoding/gob", MatchMode::PrefixSlash, SERIALIZATION),
    rule("gopkg.in/yaml.v2", MatchMode::PrefixSlash, SERIALIZATION),
    rule("gopkg.in/yaml.v3", MatchMode::PrefixSlash, SERIALIZATION),
    // concurrency
    rule("sync", MatchMode::Exact, CONCURRENCY),
    rule("sync/atomic", MatchMode::PrefixSlash, CONCURRENCY),
];

/// JavaScript and TypeScript. Module specifiers are recorded as the literal
/// source string (quotes stripped by the parser). Covers node: builtins,
/// process.env, and the popular HTTP / DB clients.
const JS_RULES: &[TrustBoundaryRule] = &[
    // network
    rule("node:http", MatchMode::Exact, NETWORK),
    rule("node:https", MatchMode::Exact, NETWORK),
    rule("node:net", MatchMode::Exact, NETWORK),
    rule("node:dgram", MatchMode::Exact, NETWORK),
    rule("http", MatchMode::Exact, NETWORK),
    rule("https", MatchMode::Exact, NETWORK),
    rule("net", MatchMode::Exact, NETWORK),
    rule("axios", MatchMode::PrefixSlash, NETWORK_API),
    rule("undici", MatchMode::PrefixSlash, NETWORK_API),
    rule("node-fetch", MatchMode::PrefixSlash, NETWORK_API),
    rule("got", MatchMode::PrefixSlash, NETWORK_API),
    rule("ky", MatchMode::PrefixSlash, NETWORK_API),
    rule("express", MatchMode::PrefixSlash, NETWORK),
    rule("fastify", MatchMode::PrefixSlash, NETWORK),
    rule("hono", MatchMode::PrefixSlash, NETWORK),
    rule("koa", MatchMode::PrefixSlash, NETWORK),
    rule("@hapi/hapi", MatchMode::PrefixSlash, NETWORK),
    // filesystem
    rule("node:fs", MatchMode::Exact, FS),
    rule("node:fs/promises", MatchMode::Exact, FS),
    rule("node:path", MatchMode::Exact, FS),
    rule("fs", MatchMode::Exact, FS),
    rule("fs/promises", MatchMode::Exact, FS),
    rule("path", MatchMode::Exact, FS),
    rule("fs-extra", MatchMode::PrefixSlash, FS),
    rule("graceful-fs", MatchMode::PrefixSlash, FS),
    // process-exec
    rule("node:child_process", MatchMode::Exact, EXEC),
    rule("child_process", MatchMode::Exact, EXEC),
    rule("execa", MatchMode::PrefixSlash, EXEC),
    // secrets / env
    rule("node:crypto", MatchMode::Exact, SECRETS),
    rule("crypto", MatchMode::Exact, SECRETS),
    rule("bcrypt", MatchMode::PrefixSlash, SECRETS),
    rule("argon2", MatchMode::PrefixSlash, SECRETS),
    rule("jsonwebtoken", MatchMode::PrefixSlash, SECRETS),
    rule("dotenv", MatchMode::PrefixSlash, SECRETS),
    // database
    rule("pg", MatchMode::PrefixSlash, DB),
    rule("mysql", MatchMode::PrefixSlash, DB),
    rule("mysql2", MatchMode::PrefixSlash, DB),
    rule("sqlite3", MatchMode::PrefixSlash, DB),
    rule("better-sqlite3", MatchMode::PrefixSlash, DB),
    rule("mongodb", MatchMode::PrefixSlash, DB),
    rule("redis", MatchMode::PrefixSlash, DB),
    rule("ioredis", MatchMode::PrefixSlash, DB),
    rule("prisma", MatchMode::PrefixSlash, DB),
    rule("@prisma/client", MatchMode::PrefixSlash, DB),
    rule("drizzle-orm", MatchMode::PrefixSlash, DB),
    rule("kysely", MatchMode::PrefixSlash, DB),
    rule("typeorm", MatchMode::PrefixSlash, DB),
    rule("sequelize", MatchMode::PrefixSlash, DB),
    rule("mongoose", MatchMode::PrefixSlash, DB),
    // user-input
    rule("yargs", MatchMode::PrefixSlash, USER_INPUT),
    rule("commander", MatchMode::PrefixSlash, USER_INPUT),
    rule("minimist", MatchMode::PrefixSlash, USER_INPUT),
    rule("@oclif/core", MatchMode::PrefixSlash, USER_INPUT),
    rule("inquirer", MatchMode::PrefixSlash, USER_INPUT),
    // serialization
    rule("js-yaml", MatchMode::PrefixSlash, SERIALIZATION),
    rule("yaml", MatchMode::PrefixSlash, SERIALIZATION),
    rule("xml2js", MatchMode::PrefixSlash, SERIALIZATION),
    rule("fast-xml-parser", MatchMode::PrefixSlash, SERIALIZATION),
];

/// Returns the language-specific rule table; for C++ also returns the C
/// rules so they apply. Empty slice when the language has no rules yet.
pub fn rules_for(language: Language) -> &'static [&'static [TrustBoundaryRule]] {
    match language {
        Language::Rust => &[RUST_RULES],
        Language::Php => &[PHP_RULES],
        Language::C => &[C_RULES],
        Language::Cpp => &[C_RULES, CPP_EXTRA_RULES],
        Language::Python => &[PYTHON_RULES],
        Language::Go => &[GO_RULES],
        Language::JavaScript | Language::TypeScript => &[JS_RULES],
    }
}

/// Does `name` match the rule's pattern under its match mode?
pub fn rule_matches(rule: &TrustBoundaryRule, name: &str) -> bool {
    match rule.mode {
        MatchMode::Exact => name == rule.pattern,
        MatchMode::PrefixDoubleColon => {
            name == rule.pattern
                || (name.starts_with(rule.pattern)
                    && name.as_bytes().get(rule.pattern.len()) == Some(&b':')
                    && name.as_bytes().get(rule.pattern.len() + 1) == Some(&b':'))
        }
        MatchMode::PrefixBackslash => {
            name == rule.pattern
                || (name.starts_with(rule.pattern)
                    && name.as_bytes().get(rule.pattern.len()) == Some(&b'\\'))
        }
        MatchMode::PrefixSlash => {
            name == rule.pattern
                || (name.starts_with(rule.pattern)
                    && name.as_bytes().get(rule.pattern.len()) == Some(&b'/'))
        }
        MatchMode::PrefixDot => {
            name == rule.pattern
                || (name.starts_with(rule.pattern)
                    && name.as_bytes().get(rule.pattern.len()) == Some(&b'.'))
        }
        MatchMode::Contains => name.contains(rule.pattern),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_reqwest_matches_root_and_subpath() {
        let table = rules_for(Language::Rust)[0];
        let reqwest = table
            .iter()
            .find(|r| r.pattern == "reqwest")
            .expect("reqwest rule present");
        assert!(rule_matches(reqwest, "reqwest"));
        assert!(rule_matches(reqwest, "reqwest::Client"));
        assert!(rule_matches(reqwest, "reqwest::header::HeaderMap"));
        assert!(!rule_matches(reqwest, "reqwest_middleware"));
        assert!(!rule_matches(reqwest, "not_reqwest"));
    }

    #[test]
    fn php_namespace_matches_subpaths_only_on_backslash() {
        let table = rules_for(Language::Php)[0];
        let guzzle = table
            .iter()
            .find(|r| r.pattern == "GuzzleHttp")
            .expect("Guzzle rule present");
        assert!(rule_matches(guzzle, "GuzzleHttp"));
        assert!(rule_matches(guzzle, "GuzzleHttp\\Client"));
        assert!(!rule_matches(guzzle, "GuzzleHttpFake"));
    }

    #[test]
    fn c_include_path_exact_match() {
        let table = rules_for(Language::C)[0];
        let sock = table
            .iter()
            .find(|r| r.pattern == "sys/socket.h")
            .expect("sys/socket.h rule present");
        assert!(rule_matches(sock, "sys/socket.h"));
        assert!(!rule_matches(sock, "socket.h"));
        assert!(!rule_matches(sock, "sys/socket.hh"));
    }

    #[test]
    fn cpp_inherits_c_plus_extras() {
        let tables = rules_for(Language::Cpp);
        assert_eq!(tables.len(), 2, "cpp gets both C and CPP rule tables");
        let has_unistd = tables[0].iter().any(|r| r.pattern == "unistd.h");
        let has_filesystem = tables[1].iter().any(|r| r.pattern == "filesystem");
        assert!(has_unistd);
        assert!(has_filesystem);
    }

    #[test]
    fn python_prefix_dot_handles_module_subpaths() {
        let table = rules_for(Language::Python)[0];
        let os_env = table
            .iter()
            .find(|r| r.pattern == "os.environ")
            .expect("os.environ rule present");
        assert!(rule_matches(os_env, "os.environ"));
        assert!(rule_matches(os_env, "os.environ.get"));
        assert!(!rule_matches(os_env, "os.environ_var"));
    }

    #[test]
    fn go_import_path_prefix_slash() {
        let table = rules_for(Language::Go)[0];
        let nh = table
            .iter()
            .find(|r| r.pattern == "net/http")
            .expect("net/http rule present");
        assert!(rule_matches(nh, "net/http"));
        assert!(rule_matches(nh, "net/http/httptest"));
        assert!(!rule_matches(nh, "net/httpx"));
    }

    #[test]
    fn js_node_builtin_exact() {
        let table = rules_for(Language::JavaScript)[0];
        let cp = table
            .iter()
            .find(|r| r.pattern == "node:child_process")
            .expect("node:child_process rule present");
        assert!(rule_matches(cp, "node:child_process"));
        assert!(!rule_matches(cp, "node:child_processx"));
        assert!(!rule_matches(cp, "child_processx"));
    }
}
