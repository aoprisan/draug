//! Sandbox/snapshot registry backed by SQLite (rusqlite).
//!
//! rusqlite is synchronous; the registry's API is synchronous too. Callers
//! in async context must go through `tokio::task::spawn_blocking` (backends
//! do this internally). Operations are single small transactions, so the
//! connection mutex is not a contention concern.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::{Error, ResourceKind, Result};
use crate::limits::ResourceLimits;
use crate::types::{Sandbox, SandboxId, SandboxState, SnapshotId, SnapshotMeta};

const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS sandbox (
    id         TEXT PRIMARY KEY,
    name       TEXT UNIQUE,
    state      TEXT NOT NULL,
    backend    TEXT NOT NULL,
    rootfs     TEXT NOT NULL,
    state_dir  TEXT NOT NULL,
    limits     TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS snapshot (
    id         TEXT PRIMARY KEY,
    sandbox_id TEXT REFERENCES sandbox(id) ON DELETE SET NULL,
    name       TEXT NOT NULL,
    path       TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (sandbox_id, name)
);
"#;

pub struct Registry {
    conn: Mutex<Connection>,
}

impl Registry {
    /// Open (creating if needed) the registry database at `path`.
    /// Enables WAL mode and a busy timeout so concurrent `sbx` invocations
    /// queue instead of failing.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io("create registry dir", e))?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// In-memory registry, for tests.
    pub fn open_in_memory() -> Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Default registry path: `$XDG_DATA_HOME/draug/registry.db`, falling
    /// back to `~/.local/share/draug/registry.db`.
    pub fn default_path() -> PathBuf {
        let base = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
                home.join(".local/share")
            });
        base.join("draug/registry.db")
    }

    // --- sandboxes -------------------------------------------------------

    /// Insert a new sandbox row. Fails with `AlreadyExists` on id or name
    /// collision.
    pub fn insert_sandbox(&self, sb: &Sandbox) -> Result<()> {
        let limits = serde_json::to_string(&sb.limits)
            .map_err(|e| Error::InvalidSpec(format!("unserializable limits: {e}")))?;
        let conn = self.conn.lock().unwrap();
        let res = conn.execute(
            "INSERT INTO sandbox (id, name, state, backend, rootfs, state_dir, limits, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                sb.id.0,
                sb.name,
                sb.state.as_str(),
                sb.backend,
                sb.rootfs.to_string_lossy(),
                sb.state_dir.to_string_lossy(),
                limits,
                sb.created_at,
            ],
        );
        match res {
            Ok(_) => Ok(()),
            Err(e) if is_constraint_violation(&e) => Err(Error::AlreadyExists {
                kind: ResourceKind::Sandbox,
                id: sb.name.clone().unwrap_or_else(|| sb.id.0.clone()),
            }),
            Err(e) => Err(e.into()),
        }
    }

    /// Look up a sandbox by id, or by name if no id matches.
    pub fn get_sandbox(&self, id_or_name: &str) -> Result<Sandbox> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, state, backend, rootfs, state_dir, limits, created_at
             FROM sandbox WHERE id = ?1 OR name = ?1",
        )?;
        stmt.query_row(params![id_or_name], row_to_sandbox)
            .optional()?
            .ok_or_else(|| Error::NotFound {
                kind: ResourceKind::Sandbox,
                id: id_or_name.to_owned(),
            })
    }

    pub fn list_sandboxes(&self) -> Result<Vec<Sandbox>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, name, state, backend, rootfs, state_dir, limits, created_at
             FROM sandbox ORDER BY created_at",
        )?;
        let rows = stmt.query_map([], row_to_sandbox)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn update_sandbox_state(&self, id: &SandboxId, state: SandboxState) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let n = conn.execute(
            "UPDATE sandbox SET state = ?2 WHERE id = ?1",
            params![id.0, state.as_str()],
        )?;
        if n == 0 {
            return Err(Error::NotFound {
                kind: ResourceKind::Sandbox,
                id: id.0.clone(),
            });
        }
        Ok(())
    }

    /// Remove a sandbox row. Idempotent: removing a missing row is Ok, to
    /// match `Backend::destroy`'s contract.
    pub fn remove_sandbox(&self, id: &SandboxId) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM sandbox WHERE id = ?1", params![id.0])?;
        Ok(())
    }

    // --- snapshots -------------------------------------------------------

    pub fn insert_snapshot(&self, snap: &SnapshotMeta) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        let res = conn.execute(
            "INSERT INTO snapshot (id, sandbox_id, name, path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                snap.id.0,
                snap.sandbox_id.as_ref().map(|s| s.0.as_str()),
                snap.name,
                snap.path.to_string_lossy(),
                snap.created_at,
            ],
        );
        match res {
            Ok(_) => Ok(()),
            Err(e) if is_constraint_violation(&e) => Err(Error::AlreadyExists {
                kind: ResourceKind::Snapshot,
                id: snap.name.clone(),
            }),
            Err(e) => Err(e.into()),
        }
    }

    /// Look up a snapshot by id, or by name if no id matches.
    pub fn get_snapshot(&self, id_or_name: &str) -> Result<SnapshotMeta> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, sandbox_id, name, path, created_at
             FROM snapshot WHERE id = ?1 OR name = ?1",
        )?;
        stmt.query_row(params![id_or_name], row_to_snapshot)
            .optional()?
            .ok_or_else(|| Error::NotFound {
                kind: ResourceKind::Snapshot,
                id: id_or_name.to_owned(),
            })
    }

    /// List snapshots, optionally restricted to one sandbox.
    pub fn list_snapshots(&self, sandbox: Option<&SandboxId>) -> Result<Vec<SnapshotMeta>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, sandbox_id, name, path, created_at FROM snapshot
             WHERE ?1 IS NULL OR sandbox_id = ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map(params![sandbox.map(|s| s.0.as_str())], row_to_snapshot)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn remove_snapshot(&self, id: &SnapshotId) -> Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM snapshot WHERE id = ?1", params![id.0])?;
        Ok(())
    }
}

fn is_constraint_violation(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(err, _)
            if err.code == rusqlite::ErrorCode::ConstraintViolation
    )
}

fn row_to_sandbox(row: &rusqlite::Row<'_>) -> rusqlite::Result<Sandbox> {
    let state_str: String = row.get(2)?;
    let limits_json: String = row.get(6)?;
    let limits: ResourceLimits = serde_json::from_str(&limits_json).unwrap_or_default();
    Ok(Sandbox {
        id: SandboxId(row.get(0)?),
        name: row.get(1)?,
        state: SandboxState::parse(&state_str).unwrap_or(SandboxState::Stopped),
        backend: row.get(3)?,
        rootfs: PathBuf::from(row.get::<_, String>(4)?),
        state_dir: PathBuf::from(row.get::<_, String>(5)?),
        limits,
        created_at: row.get(7)?,
    })
}

fn row_to_snapshot(row: &rusqlite::Row<'_>) -> rusqlite::Result<SnapshotMeta> {
    Ok(SnapshotMeta {
        id: SnapshotId(row.get(0)?),
        sandbox_id: row.get::<_, Option<String>>(1)?.map(SandboxId),
        name: row.get(2)?,
        path: PathBuf::from(row.get::<_, String>(3)?),
        created_at: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_sandbox(id: &str, name: Option<&str>) -> Sandbox {
        Sandbox {
            id: SandboxId(id.to_owned()),
            name: name.map(str::to_owned),
            state: SandboxState::Creating,
            backend: "ns".to_owned(),
            rootfs: PathBuf::from("/images/base"),
            state_dir: PathBuf::from("/run/draug").join(id),
            limits: ResourceLimits::unlimited(),
            created_at: 1,
        }
    }

    #[test]
    fn sandbox_roundtrip() {
        let reg = Registry::open_in_memory().unwrap();
        reg.insert_sandbox(&sample_sandbox("sb1", Some("dev"))).unwrap();

        let by_id = reg.get_sandbox("sb1").unwrap();
        assert_eq!(by_id.name.as_deref(), Some("dev"));
        let by_name = reg.get_sandbox("dev").unwrap();
        assert_eq!(by_name.id.0, "sb1");

        reg.update_sandbox_state(&SandboxId("sb1".into()), SandboxState::Ready)
            .unwrap();
        assert_eq!(reg.get_sandbox("sb1").unwrap().state, SandboxState::Ready);

        assert!(matches!(
            reg.insert_sandbox(&sample_sandbox("sb1", None)),
            Err(Error::AlreadyExists { .. })
        ));

        reg.remove_sandbox(&SandboxId("sb1".into())).unwrap();
        assert!(matches!(
            reg.get_sandbox("sb1"),
            Err(Error::NotFound { .. })
        ));
        // idempotent remove
        reg.remove_sandbox(&SandboxId("sb1".into())).unwrap();
    }

    #[test]
    fn snapshot_roundtrip() {
        let reg = Registry::open_in_memory().unwrap();
        reg.insert_sandbox(&sample_sandbox("sb1", None)).unwrap();
        let snap = SnapshotMeta {
            id: SnapshotId("sn1".into()),
            sandbox_id: Some(SandboxId("sb1".into())),
            name: "before-tests".into(),
            path: PathBuf::from("/var/lib/draug/snapshots/sn1"),
            created_at: 2,
        };
        reg.insert_snapshot(&snap).unwrap();
        assert_eq!(reg.get_snapshot("before-tests").unwrap().id.0, "sn1");
        assert_eq!(
            reg.list_snapshots(Some(&SandboxId("sb1".into()))).unwrap().len(),
            1
        );

        // destroying the sandbox orphans, not deletes, the snapshot
        reg.remove_sandbox(&SandboxId("sb1".into())).unwrap();
        assert!(reg.get_snapshot("sn1").unwrap().sandbox_id.is_none());
    }
}
