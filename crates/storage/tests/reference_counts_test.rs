//! `reference_counts_for_name_set` (one pass over `refs`) must agree with the
//! batched `reference_counts_for_names` query on every name set.

use std::collections::{HashMap, HashSet};

use codesage_protocol::work::{StopReason, WorkControl};
use rusqlite::{Connection, params};

const POOL: [&str; 9] = ["", "a", "A", "b", "ns\\a", "x.a", "a.b", "c", "unused"];

struct Lcg(u64);

impl Lcg {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 33) as usize) % n
    }
}

fn batched(db: &codesage_storage::Database, names: &HashSet<String>) -> HashMap<String, u32> {
    let names: Vec<String> = names.iter().cloned().collect();
    let mut out = HashMap::new();
    for batch in names.chunks(3) {
        out.extend(db.reference_counts_for_names(batch).unwrap());
    }
    out
}

#[test]
fn name_set_counts_match_the_batched_query() {
    let mut rng = Lcg(0x5eed);
    for round in 0..40 {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("index.db");
        drop(codesage_storage::Database::open(&path).unwrap());
        let conn = Connection::open(&path).unwrap();
        for file in 1..=3 {
            conn.execute(
                "INSERT INTO files (id, path, language, content_hash) VALUES (?1, ?2, 'rust', 'h')",
                params![file, format!("src/f{file}.rs")],
            )
            .unwrap();
        }
        for col in 0..60 {
            let to_name = POOL[rng.next(POOL.len())];
            // Tail equal to the name, empty, or another pool entry.
            let tail = match rng.next(3) {
                0 => to_name,
                1 => "",
                _ => POOL[rng.next(POOL.len())],
            };
            conn.execute(
                "INSERT INTO refs (from_file_id, to_name, to_name_tail, kind, line, col)
                 VALUES (?1, ?2, ?3, 'call', ?4, ?5)",
                params![
                    1 + rng.next(3) as i64,
                    to_name,
                    tail,
                    1 + rng.next(4) as i64,
                    col
                ],
            )
            .unwrap();
        }
        // A BLOB name never equals a bound text name, but its text tail still
        // differs from it and counts.
        conn.execute(
            "INSERT INTO refs (from_file_id, to_name, to_name_tail, kind, line, col)
             VALUES (1, X'61', 'b', 'call', 9, ?1)",
            params![1000 + round],
        )
        .unwrap();
        drop(conn);
        let db = codesage_storage::Database::open(&path).unwrap();
        let names: HashSet<String> = POOL
            .iter()
            .filter(|_| rng.next(3) != 0)
            .map(|name| name.to_string())
            .collect();
        assert_eq!(
            db.reference_counts_for_name_set(&names).unwrap(),
            batched(&db, &names),
            "round {round}, names {names:?}"
        );
    }
}

#[test]
fn name_set_counts_one_site_per_file_and_line_for_name_and_tail() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    drop(codesage_storage::Database::open(&path).unwrap());
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "INSERT INTO files (id, path, language, content_hash) VALUES (1, 'src/a.rs', 'rust', 'h');
         INSERT INTO files (id, path, language, content_hash) VALUES (2, 'src/b.rs', 'rust', 'h');
         INSERT INTO refs (from_file_id, to_name, to_name_tail, kind, line, col)
         VALUES (1, 'm::helper', 'helper', 'call', 3, 4);
         INSERT INTO refs (from_file_id, to_name, to_name_tail, kind, line, col)
         VALUES (1, 'helper', 'helper', 'call', 3, 9);
         INSERT INTO refs (from_file_id, to_name, to_name_tail, kind, line, col)
         VALUES (1, 'helper', 'helper', 'call', 4, 1);
         INSERT INTO refs (from_file_id, to_name, to_name_tail, kind, line, col)
         VALUES (2, 'helper', 'helper', 'call', 3, 4);
         INSERT INTO refs (from_file_id, to_name, to_name_tail, kind, line, col)
         VALUES (2, 'Helper', 'Helper', 'call', 5, 4);",
    )
    .unwrap();
    drop(conn);
    let db = codesage_storage::Database::open(&path).unwrap();
    let names: HashSet<String> = ["helper", "m::helper", "absent"]
        .into_iter()
        .map(String::from)
        .collect();
    let counts = db.reference_counts_for_name_set(&names).unwrap();
    assert_eq!(counts["helper"], 3);
    assert_eq!(counts["m::helper"], 1);
    assert_eq!(counts["absent"], 0);
    assert_eq!(counts.len(), 3);
    assert!(
        db.reference_counts_for_name_set(&HashSet::new())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn cancelled_work_stops_the_name_set_scan() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("index.db");
    drop(codesage_storage::Database::open(&path).unwrap());
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch(
        "INSERT INTO files (id, path, language, content_hash) VALUES (1, 'src/a.rs', 'rust', 'h');
         WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x + 1 FROM n WHERE x < 20000)
         INSERT INTO refs (from_file_id, to_name, to_name_tail, kind, line, col)
         SELECT 1, 'helper', 'helper', 'call', x, 0 FROM n;",
    )
    .unwrap();
    drop(conn);
    let db = codesage_storage::Database::open(&path).unwrap();
    let names: HashSet<String> = HashSet::from(["helper".to_string()]);
    let control = WorkControl::new(None);
    let _scope = control.enter();
    assert_eq!(
        db.reference_counts_for_name_set(&names).unwrap()["helper"],
        20_000
    );
    control.cancel(StopReason::ClientCancelled);
    assert!(db.reference_counts_for_name_set(&names).is_err());
}
