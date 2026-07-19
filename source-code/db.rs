use crate::state::{RollbackEntry, State, VersionInfo, WrapperNames};
use miette::{IntoDiagnostic, Result};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

pub fn db_path() -> String {
    format!("{}/hpm.db", crate::db_dir())
}

const SCHEMA: &str = "
    PRAGMA journal_mode = WAL;
    PRAGMA foreign_keys = ON;

    CREATE TABLE IF NOT EXISTS packages (
        name                TEXT NOT NULL,
        version             TEXT NOT NULL,
        checksum            TEXT NOT NULL DEFAULT '',
        pinned              INTEGER NOT NULL DEFAULT 0,
        manually_installed  INTEGER NOT NULL DEFAULT 1,
        installed_at        INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (name, version)
    );

    CREATE TABLE IF NOT EXISTS package_required_by (
        name TEXT NOT NULL, version TEXT NOT NULL, dependent TEXT NOT NULL,
        PRIMARY KEY (name, version, dependent)
    );
    CREATE TABLE IF NOT EXISTS package_depends_on (
        name TEXT NOT NULL, version TEXT NOT NULL, dependency TEXT NOT NULL,
        PRIMARY KEY (name, version, dependency)
    );
    CREATE TABLE IF NOT EXISTS package_conflicts_with (
        name TEXT NOT NULL, version TEXT NOT NULL, conflict TEXT NOT NULL,
        PRIMARY KEY (name, version, conflict)
    );

    -- Snapshoty (rollback) trzymane jako blob JSON per-snapshot: to zamrożone
    -- kopie przeszłego stanu, nikt ich nie odpytuje relacyjnie, więc pełna
    -- normalizacja tylko dodałaby ryzyko bez korzyści. `packages` powyżej —
    -- czyli stan BIEŻĄCY, ten który realnie ma znaczenie do zapytań — jest
    -- w pełni relacyjny.
    CREATE TABLE IF NOT EXISTS snapshots (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        timestamp     INTEGER NOT NULL,
        description   TEXT NOT NULL,
        packages_json TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS wrapper_names (
        key          TEXT PRIMARY KEY,
        wrapper_name TEXT NOT NULL
    );
";

pub fn open() -> Result<Connection> {
    let path = db_path();
    if let Some(parent) = Path::new(&path).parent() {
        fs::create_dir_all(parent).into_diagnostic()?;
    }
    let is_new = !Path::new(&path).exists();
    let mut conn = Connection::open(&path).into_diagnostic()?;
    conn.execute_batch(SCHEMA).into_diagnostic()?;
    if is_new {
        migrate_legacy_json(&mut conn)?;
    }
    Ok(conn)
}

/// Jednorazowa migracja z state.json / wrapper-names.json (hpm < 0.9, albo
/// hpm 0.9 przed tą aktualizacją) do świeżo utworzonej bazy SQLite. Stare
/// pliki JSON są przemianowane na `.migrated`, nie kasowane.
fn migrate_legacy_json(conn: &mut Connection) -> Result<()> {
    let old_state_path = format!("{}/state.json", crate::db_dir());
    if let Ok(data) = fs::read(&old_state_path) {
        if let Ok(old_state) = serde_json::from_slice::<State>(&data) {
            eprintln!("  → Migrating state.json into hpm.db (one-time, 0.9)...");
            write_state(conn, &old_state)?;
            let _ = fs::rename(&old_state_path, format!("{}.migrated", old_state_path));
        }
    }

    let old_wrapper_path = format!("{}/wrapper-names.json", crate::db_dir());
    if let Ok(data) = fs::read(&old_wrapper_path) {
        if let Ok(old_wn) = serde_json::from_slice::<WrapperNames>(&data) {
            write_wrapper_names(conn, &old_wn)?;
            let _ = fs::rename(&old_wrapper_path, format!("{}.migrated", old_wrapper_path));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

pub fn load_state() -> Result<State> {
    let conn = open()?;

    let mut packages: HashMap<String, HashMap<String, VersionInfo>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT name, version, checksum, pinned, manually_installed, installed_at FROM packages"
        ).into_diagnostic()?;
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)? != 0,
                row.get::<_, i64>(4)? != 0,
                row.get::<_, i64>(5)? as u64,
            ))
        }).into_diagnostic()?;

        for row in rows {
            let (name, version, checksum, pinned, manually_installed, installed_at) = row.into_diagnostic()?;
            let info = VersionInfo {
                checksum, pinned, manually_installed,
                required_by: HashSet::new(), depends_on: HashSet::new(), conflicts_with: HashSet::new(),
                installed_at,
            };
            packages.entry(name).or_default().insert(version, info);
        }
    }

    load_relation(&conn, "package_required_by", "dependent",
        |packages: &mut HashMap<String, HashMap<String, VersionInfo>>, name, version, value| {
            if let Some(vi) = packages.get_mut(name).and_then(|v| v.get_mut(version)) {
                vi.required_by.insert(value.to_string());
            }
        }, &mut packages)?;
    load_relation(&conn, "package_depends_on", "dependency",
        |packages: &mut HashMap<String, HashMap<String, VersionInfo>>, name, version, value| {
            if let Some(vi) = packages.get_mut(name).and_then(|v| v.get_mut(version)) {
                vi.depends_on.insert(value.to_string());
            }
        }, &mut packages)?;
    load_relation(&conn, "package_conflicts_with", "conflict",
        |packages: &mut HashMap<String, HashMap<String, VersionInfo>>, name, version, value| {
            if let Some(vi) = packages.get_mut(name).and_then(|v| v.get_mut(version)) {
                vi.conflicts_with.insert(value.to_string());
            }
        }, &mut packages)?;

    let mut history = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT timestamp, description, packages_json FROM snapshots ORDER BY id"
        ).into_diagnostic()?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)? as u64, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
        }).into_diagnostic()?;
        for row in rows {
            let (timestamp, description, packages_json) = row.into_diagnostic()?;
            if let Ok(snapshot) = serde_json::from_str(&packages_json) {
                history.push(RollbackEntry { timestamp, description, snapshot });
            }
        }
    }

    Ok(State { packages, history })
}

fn load_relation(
    conn: &Connection, table: &str, value_col: &str,
    apply: impl Fn(&mut HashMap<String, HashMap<String, VersionInfo>>, &str, &str, &str),
    packages: &mut HashMap<String, HashMap<String, VersionInfo>>,
) -> Result<()> {
    let mut stmt = conn.prepare(&format!("SELECT name, version, {} FROM {}", value_col, table)).into_diagnostic()?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
    }).into_diagnostic()?;
    for row in rows {
        let (name, version, value) = row.into_diagnostic()?;
        apply(packages, &name, &version, &value);
    }
    Ok(())
}

pub fn save_state(state: &State) -> Result<()> {
    let mut conn = open()?;
    write_state(&mut conn, state)
}

fn write_state(conn: &mut Connection, state: &State) -> Result<()> {
    let tx = conn.transaction().into_diagnostic()?;

    tx.execute("DELETE FROM packages", []).into_diagnostic()?;
    tx.execute("DELETE FROM package_required_by", []).into_diagnostic()?;
    tx.execute("DELETE FROM package_depends_on", []).into_diagnostic()?;
    tx.execute("DELETE FROM package_conflicts_with", []).into_diagnostic()?;
    tx.execute("DELETE FROM snapshots", []).into_diagnostic()?;

    for (name, versions) in &state.packages {
        for (version, info) in versions {
            tx.execute(
                "INSERT INTO packages (name, version, checksum, pinned, manually_installed, installed_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![
                    name, version, info.checksum,
                    info.pinned as i64, info.manually_installed as i64, info.installed_at as i64
                ],
            ).into_diagnostic()?;

            for dependent in &info.required_by {
                tx.execute(
                    "INSERT OR IGNORE INTO package_required_by (name, version, dependent) VALUES (?1, ?2, ?3)",
                    rusqlite::params![name, version, dependent],
                ).into_diagnostic()?;
            }
            for dependency in &info.depends_on {
                tx.execute(
                    "INSERT OR IGNORE INTO package_depends_on (name, version, dependency) VALUES (?1, ?2, ?3)",
                    rusqlite::params![name, version, dependency],
                ).into_diagnostic()?;
            }
            for conflict in &info.conflicts_with {
                tx.execute(
                    "INSERT OR IGNORE INTO package_conflicts_with (name, version, conflict) VALUES (?1, ?2, ?3)",
                    rusqlite::params![name, version, conflict],
                ).into_diagnostic()?;
            }
        }
    }

    for entry in &state.history {
        let packages_json = serde_json::to_string(&entry.snapshot).into_diagnostic()?;
        tx.execute(
            "INSERT INTO snapshots (timestamp, description, packages_json) VALUES (?1, ?2, ?3)",
            rusqlite::params![entry.timestamp as i64, entry.description, packages_json],
        ).into_diagnostic()?;
    }

    tx.commit().into_diagnostic()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// WrapperNames
// ---------------------------------------------------------------------------

pub fn load_wrapper_names() -> Result<WrapperNames> {
    let conn = open()?;
    let mut names = HashMap::new();
    let mut stmt = conn.prepare("SELECT key, wrapper_name FROM wrapper_names").into_diagnostic()?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    }).into_diagnostic()?;
    for row in rows {
        let (k, v) = row.into_diagnostic()?;
        names.insert(k, v);
    }
    Ok(WrapperNames { names })
}

pub fn save_wrapper_names(wn: &WrapperNames) -> Result<()> {
    let conn = open()?;
    write_wrapper_names(&conn, wn)
}

fn write_wrapper_names(conn: &Connection, wn: &WrapperNames) -> Result<()> {
    conn.execute("DELETE FROM wrapper_names", []).into_diagnostic()?;
    for (k, v) in &wn.names {
        conn.execute(
            "INSERT OR REPLACE INTO wrapper_names (key, wrapper_name) VALUES (?1, ?2)",
            rusqlite::params![k, v],
        ).into_diagnostic()?;
    }
    Ok(())
}
