//! Whether a `brief` payload is worth serving *again* in a session.
//!
//! Replaying 383 real Claude Code transcripts against `codesage brief` measured
//! the problem this exists to solve: on files an agent actually edits the
//! payload is not rare (40-70% of source edits produce one), and 58-80% of what
//! it produces within a session is a repeat. One session emitted 117 payloads,
//! about 4567 tokens, for 44 distinct facts. Per-fire cost was never the issue;
//! cumulative context is.
//!
//! Three gates, checked in order, each answering a different repeat:
//!
//! - **budget** — a hard per-session ceiling. Once spent, the session is silent.
//! - **dedup** by (path, payload) — the same fact about the same file, which is
//!   the common case since the payload derives from committed history and does
//!   not move while the agent edits.
//! - **cooldown** per path — a payload for a path that changed slightly, e.g.
//!   because the agent committed mid-session and the hooks reindexed. Dedup
//!   cannot catch that one; without a cooldown a commit re-arms every path.
//!
//! State is per session, lives in the runtime dir, and never touches the
//! project — the same rule that made [`codesage_storage::Database::open_read_only`]
//! necessary.
//!
//! **A gate that cannot read its own state fails closed.** Silence is always
//! safe; noise is what makes an unrequested channel get ignored permanently, and
//! that failure is not recoverable by fixing the bug later.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

/// Tokens a single session may spend on served briefs.
///
/// Replaying the corpus through the gate below: the heaviest single session
/// spent ~1072 tokens, the median ~166. This leaves about 40% headroom over the
/// heaviest observed, and no session in the corpus reached it — the budget is a
/// backstop against a session unlike any measured, not the working mechanism.
/// Dedup is the working mechanism. For scale, the same corpus ungated put 4567
/// tokens into one session.
const SESSION_TOKEN_BUDGET: usize = 1500;

/// Seconds before the same path may be served again, whatever the payload says.
const PATH_COOLDOWN_SECS: u64 = 900;

/// Session state older than this is deleted. A session that ran a day ago
/// cannot be resumed into, and the runtime dir is not a place to accumulate.
const STATE_TTL_SECS: u64 = 86_400;

/// Characters per token. The replay measurements that set the budget above used
/// the same divisor, so the two are consistent even though both are estimates.
const CHARS_PER_TOKEN: usize = 4;

/// Append-only record of every fire, silent ones included.
const FIRE_LOG: &str = "brief-fires.jsonl";

/// Rotate past this, keeping one previous generation. At the ~120 bytes a line
/// costs, this holds on the order of 35k fires — more than the 11331 the whole
/// replay corpus contains.
const FIRE_LOG_MAX_BYTES: u64 = 4 * 1024 * 1024;

#[derive(Default, Serialize, Deserialize)]
struct GateState {
    tokens: usize,
    /// `path` + NUL + payload digest -> first served at.
    served: HashMap<String, u64>,
    /// `path` -> last served at, for the cooldown.
    last_path: HashMap<String, u64>,
}

/// Why a fire did or did not reach the agent.
///
/// The reason matters as much as the outcome. ~90% of fires are silent, and a
/// scorer that cannot tell "there was nothing to say" from "we suppressed a
/// repeat" cannot tell a well-targeted surface from an over-firing one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    /// Reached the agent.
    Served,
    /// Nothing to say about this file.
    Empty,
    /// This exact payload already went out for this path.
    Repeat,
    /// This path went out too recently, whatever the payload now says.
    Cooldown,
    /// The session's token budget is spent.
    Budget,
    /// Gate state could not be read or written, so nothing was served.
    Unavailable,
    /// The payload could not be built at all. Distinct from `Empty`: one is a
    /// file with nothing to say, the other is a broken path, and a denominator
    /// that conflates them hides a regression as a quiet surface.
    Error,
}

impl Decision {
    fn as_str(self) -> &'static str {
        match self {
            Decision::Served => "served",
            Decision::Empty => "empty",
            Decision::Repeat => "repeat",
            Decision::Cooldown => "cooldown",
            Decision::Budget => "budget",
            Decision::Unavailable => "unavailable",
            Decision::Error => "error",
        }
    }
}

/// Decide and record in one step. Pure, so the ordering of the three gates is
/// testable without a filesystem.
fn decide(state: &mut GateState, path: &str, payload: &str, now: u64) -> Decision {
    let cost = payload.len().div_ceil(CHARS_PER_TOKEN);
    if state.tokens + cost > SESSION_TOKEN_BUDGET {
        return Decision::Budget;
    }

    let key = format!("{path}\0{:x}", digest(payload));
    if state.served.contains_key(&key) {
        return Decision::Repeat;
    }

    if let Some(prev) = state.last_path.get(path)
        && now.saturating_sub(*prev) < PATH_COOLDOWN_SECS
    {
        return Decision::Cooldown;
    }

    state.served.insert(key, now);
    state.last_path.insert(path.to_string(), now);
    state.tokens += cost;
    Decision::Served
}

/// FNV-1a. The digest only has to separate payloads within one session's state
/// file, so a 64-bit non-cryptographic hash with no dependency is the right
/// size of tool.
fn digest(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Session ids arrive from a hook payload, so they are untrusted input on a
/// path that becomes a filename. Keep only characters that cannot traverse.
fn sanitize(session: &str) -> String {
    let cleaned: String = session
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(64)
        .collect();
    if cleaned.is_empty() {
        format!("{:x}", digest(session))
    } else {
        cleaned
    }
}

fn state_path(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("brief-{}.json", sanitize(session)))
}

/// Delete session state past its TTL. Called only when a session's own state is
/// created, so this is once per session rather than once per fire.
fn prune(dir: &Path, now: u64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("brief-") || !name.ends_with(".json") {
            continue;
        }
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| now.saturating_sub(d.as_secs()) > STATE_TTL_SECS)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Should this payload be served for `path` in `session`, and if not, why?
///
/// [`Decision::Unavailable`] on any I/O or parse failure, per the fail-closed
/// rule above. Recording is part of the answer: [`Decision::Served`] has already
/// been charged against the budget, so the caller must serve what it asked about.
pub(crate) fn evaluate(dir: &Path, session: &str, path: &str, payload: &str) -> Decision {
    if payload.is_empty() {
        return Decision::Empty;
    }
    if std::fs::create_dir_all(dir).is_err() {
        return Decision::Unavailable;
    }

    let file = state_path(dir, session);
    let existing = std::fs::read_to_string(&file).ok();
    // A corrupt state file starts over rather than disabling the gate for the
    // rest of the session; the budget it forgets is bounded by one session.
    let mut state: GateState = existing
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_default();

    let now = now_secs();
    if existing.is_none() {
        prune(dir, now);
    }

    let decision = decide(&mut state, path, payload, now);
    if decision != Decision::Served {
        return decision;
    }

    // Write to a session-and-pid-unique temp then rename, so a concurrent fire
    // in the same session cannot observe a half-written file. Two fires racing
    // can still lose one update; the cost of that is one duplicate payload,
    // which is why this does not take a lock.
    let Ok(encoded) = serde_json::to_string(&state) else {
        return Decision::Unavailable;
    };
    let tmp = file.with_extension(format!("tmp{}", std::process::id()));
    if std::fs::write(&tmp, encoded).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Decision::Unavailable;
    }
    if std::fs::rename(&tmp, &file).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return Decision::Unavailable;
    }
    Decision::Served
}

/// One line per fire, appended to `brief-fires.jsonl` in the runtime dir.
///
/// This exists because a silent fire leaves no trace anywhere else. About 90% of
/// real fires emit nothing, and a transcript only ever records what an agent was
/// shown — so without this the denominator of any efficacy measurement is
/// unrecoverable after the fact, and an un-measured surface is indistinguishable
/// from one that does nothing.
///
/// The digest is over the rendered payload, which is the only part that survives
/// verbatim into a transcript. That is what lets a replayed firing settle the
/// row it belongs to instead of creating a second one.
///
/// Best-effort throughout: a fire is not worth failing over, and the log must
/// never turn into a reason the payload path errors.
pub(crate) fn log_fire(
    dir: &Path,
    session: &str,
    project: &Path,
    path: &str,
    decision: Decision,
    payload: &str,
) {
    // Own the directory rather than assuming a prior call made it. `evaluate`
    // returns Empty before it creates anything, and `create(true)` does not make
    // parents — so without this the log drops every fire before the session's
    // first non-silent one, which is exactly the population it exists to count.
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let log = dir.join(FIRE_LOG);
    rotate_if_large(&log);

    let mut line = format!(
        r#"{{"t":{},"s":"{}","p":{},"f":{},"d":"{}""#,
        now_secs(),
        sanitize(session),
        escape(&project.to_string_lossy()),
        escape(path),
        decision.as_str(),
    );
    if !payload.is_empty() {
        line.push_str(&format!(
            r#","h":"{:x}","tok":{}"#,
            digest(payload),
            payload.len().div_ceil(CHARS_PER_TOKEN)
        ));
    }
    line.push_str("}\n");

    use std::io::Write;
    if let Ok(mut fh) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
    {
        let _ = fh.write_all(line.as_bytes());
    }
}

/// Keep one previous generation, matching the daemon log's convention.
fn rotate_if_large(log: &Path) {
    let too_big = std::fs::metadata(log)
        .map(|m| m.len() > FIRE_LOG_MAX_BYTES)
        .unwrap_or(false);
    if too_big {
        let _ = std::fs::rename(log, log.with_extension("jsonl.1"));
    }
}

fn escape(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> GateState {
        GateState::default()
    }

    #[test]
    fn the_same_payload_for_the_same_path_is_served_once() {
        let mut s = state();
        assert_eq!(
            decide(&mut s, "a.rs", "hotspot: 90%", 1000),
            Decision::Served
        );
        assert_eq!(
            decide(&mut s, "a.rs", "hotspot: 90%", 1000),
            Decision::Repeat
        );
        // Still suppressed long after the cooldown: the fact has not changed,
        // so re-serving it is pure repetition.
        assert_eq!(
            decide(
                &mut s,
                "a.rs",
                "hotspot: 90%",
                1000 + PATH_COOLDOWN_SECS * 10
            ),
            Decision::Repeat
        );
    }

    #[test]
    fn a_changed_payload_waits_out_the_cooldown() {
        let mut s = state();
        assert_eq!(
            decide(&mut s, "a.rs", "hotspot: 90%", 1000),
            Decision::Served
        );
        // A mid-session commit plus reindex can shift the payload. Within the
        // cooldown that is still noise.
        assert_eq!(
            decide(
                &mut s,
                "a.rs",
                "hotspot: 91%",
                1000 + PATH_COOLDOWN_SECS - 1
            ),
            Decision::Cooldown
        );
        assert_eq!(
            decide(&mut s, "a.rs", "hotspot: 91%", 1000 + PATH_COOLDOWN_SECS),
            Decision::Served
        );
    }

    #[test]
    fn other_paths_are_unaffected_by_a_paths_cooldown() {
        let mut s = state();
        assert_eq!(decide(&mut s, "a.rs", "x", 1000), Decision::Served);
        assert_eq!(decide(&mut s, "b.rs", "x", 1000), Decision::Served);
    }

    #[test]
    fn the_budget_is_a_hard_ceiling() {
        let mut s = state();
        let big = "x".repeat(SESSION_TOKEN_BUDGET * CHARS_PER_TOKEN);
        assert_eq!(decide(&mut s, "a.rs", &big, 1000), Decision::Served);
        assert_eq!(s.tokens, SESSION_TOKEN_BUDGET);
        // Nothing more this session, however cheap or novel.
        assert_eq!(decide(&mut s, "b.rs", "y", 9999), Decision::Budget);
    }

    #[test]
    fn a_payload_larger_than_the_whole_budget_is_never_served() {
        let mut s = state();
        let huge = "x".repeat((SESSION_TOKEN_BUDGET + 1) * CHARS_PER_TOKEN);
        assert_eq!(decide(&mut s, "a.rs", &huge, 1000), Decision::Budget);
        assert_eq!(s.tokens, 0, "a refused payload must not charge the budget");
    }

    #[test]
    fn session_ids_cannot_escape_the_runtime_dir() {
        let dir = Path::new("/run/user/1000/codesage");
        let escaped = state_path(dir, "../../../etc/passwd");
        assert_eq!(escaped.parent().unwrap(), dir);
        assert!(!escaped.to_string_lossy().contains(".."));
        // An id with nothing usable still yields a stable, distinct file.
        assert_ne!(state_path(dir, "///"), state_path(dir, "!!!"));
    }

    #[test]
    fn state_round_trips_through_the_runtime_dir() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        assert_eq!(
            evaluate(p, "sess-1", "a.rs", "hotspot: 90%"),
            Decision::Served
        );
        assert_eq!(
            evaluate(p, "sess-1", "a.rs", "hotspot: 90%"),
            Decision::Repeat
        );
        // A different session starts with its own budget and its own history.
        assert_eq!(
            evaluate(p, "sess-2", "a.rs", "hotspot: 90%"),
            Decision::Served
        );
        // An empty payload is never a fire.
        assert_eq!(evaluate(p, "sess-3", "a.rs", ""), Decision::Empty);
    }

    #[test]
    fn a_corrupt_state_file_starts_over_rather_than_disabling_the_gate() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::write(state_path(p, "sess"), "{not json").unwrap();
        assert_eq!(evaluate(p, "sess", "a.rs", "x"), Decision::Served);
        assert_eq!(evaluate(p, "sess", "a.rs", "x"), Decision::Repeat);
    }

    #[test]
    fn the_fire_log_records_silent_fires_and_names_the_reason() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let proj = Path::new("/home/x/proj");

        for (path, payload) in [("a.rs", "hotspot: 90%"), ("b.rs", "")] {
            let d = evaluate(p, "sess", path, payload);
            log_fire(p, "sess", proj, path, d, payload);
        }
        // The repeat is the whole reason this log exists: it never reaches a
        // transcript, so nothing else can count it.
        let d = evaluate(p, "sess", "a.rs", "hotspot: 90%");
        log_fire(p, "sess", proj, "a.rs", d, "hotspot: 90%");

        let raw = std::fs::read_to_string(p.join(FIRE_LOG)).unwrap();
        let lines: Vec<&str> = raw.lines().collect();
        assert_eq!(lines.len(), 3, "every fire is a line: {raw}");
        assert!(lines[0].contains(r#""d":"served""#), "{}", lines[0]);
        assert!(lines[1].contains(r#""d":"empty""#), "{}", lines[1]);
        assert!(lines[2].contains(r#""d":"repeat""#), "{}", lines[2]);

        // A served line carries the payload digest, which is the join key back
        // to a transcript; a silent one has no payload to hash.
        assert!(lines[0].contains(r#""h":"#));
        assert!(!lines[1].contains(r#""h":"#));

        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
            assert_eq!(v["p"], "/home/x/proj");
        }
    }

    #[test]
    fn a_silent_fire_is_logged_even_as_the_first_fire_in_a_fresh_runtime_dir() {
        let dir = tempfile::tempdir().unwrap();
        // Nothing has created this yet, which is the state a hook's very first
        // fire meets. The first fires of a session are overwhelmingly silent, so
        // a log that needs someone else to make its directory loses exactly the
        // records it exists for.
        let p = dir.path().join("codesage");
        assert!(!p.exists());

        let d = evaluate(&p, "sess", "a.rs", "");
        assert_eq!(d, Decision::Empty);
        log_fire(&p, "sess", Path::new("/proj"), "a.rs", d, "");

        let raw = std::fs::read_to_string(p.join(FIRE_LOG)).expect("log must exist");
        assert!(raw.contains(r#""d":"empty""#), "{raw}");
    }

    #[test]
    fn a_path_with_quotes_still_produces_valid_json() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        let nasty = r#"src/we"ird\path.rs"#;
        log_fire(p, "s", Path::new("/p"), nasty, Decision::Served, "x");
        let raw = std::fs::read_to_string(p.join(FIRE_LOG)).unwrap();
        let v: serde_json::Value = serde_json::from_str(raw.trim()).expect("valid JSON");
        assert_eq!(v["f"], nasty);
    }
}
