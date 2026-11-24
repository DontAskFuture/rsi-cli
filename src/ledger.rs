use crate::models::{
    ArtifactSnapshot, EvolutionMemory, EvolutionRun, RunStatus, StepAttempt,
};
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

/// Persistent local SQLite ledger stored under `~/.rsi/ledger.db` (or `--state-dir`).
pub struct Ledger {
    conn: Connection,
}

pub fn default_state_dir() -> PathBuf {
    if let Ok(home) = std::env::var("RSI_HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home);
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        return PathBuf::from(home).join(".rsi");
    }
    PathBuf::from(".rsi")
}

impl Ledger {
    pub fn open(state_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("failed to create state dir {}", state_dir.display()))?;
        let db_path = state_dir.join("ledger.db");
        let conn = Connection::open(&db_path)
            .with_context(|| format!("failed to open sqlite ledger {}", db_path.display()))?;
        let ledger = Self { conn };
        ledger.init_schema()?;
        Ok(ledger)
    }

    fn init_schema(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS runs (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                repo_url TEXT NOT NULL,
                status TEXT NOT NULL,
                started_at TEXT NOT NULL,
                payload_json TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS artifacts (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                git_ref TEXT NOT NULL,
                commit_sha TEXT NOT NULL,
                payload_json TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS steps (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                step_number INTEGER NOT NULL,
                outcome TEXT NOT NULL,
                payload_json TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS memories (
                id TEXT PRIMARY KEY,
                run_id TEXT NOT NULL,
                step_number INTEGER NOT NULL,
                payload_json TEXT NOT NULL
            );
            "#,
        )?;
        Ok(())
    }

    pub fn upsert_run(&self, run: &EvolutionRun) -> Result<()> {
        let json = serde_json::to_string(run)?;
        self.conn.execute(
            r#"
            INSERT INTO runs (id, name, repo_url, status, started_at, payload_json)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6)
            ON CONFLICT(id) DO UPDATE SET
                status = excluded.status,
                payload_json = excluded.payload_json
            "#,
            params![
                run.id,
                run.name,
                run.repo_url,
                run.status.to_string(),
                run.started_at,
                json
            ],
        )?;
        Ok(())
    }

    pub fn get_run(&self, run_id: &str) -> Result<Option<EvolutionRun>> {
        let row: Option<String> = self
            .conn
            .query_row(
                "SELECT payload_json FROM runs WHERE id = ?1",
                params![run_id],
                |r| r.get(0),
            )
            .optional()?;
        match row {
            Some(s) => Ok(Some(serde_json::from_str(&s)?)),
            None => Ok(None),
        }
    }

    pub fn latest_run(&self) -> Result<Option<EvolutionRun>> {
        let row: Option<String> = self
            .conn
            .query_row(
                "SELECT payload_json FROM runs ORDER BY started_at DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .optional()?;
        match row {
            Some(s) => Ok(Some(serde_json::from_str(&s)?)),
            None => Ok(None),
        }
    }

    pub fn insert_artifact(&self, art: &ArtifactSnapshot) -> Result<()> {
        let json = serde_json::to_string(art)?;
        self.conn.execute(
            "INSERT OR REPLACE INTO artifacts (id, run_id, git_ref, commit_sha, payload_json) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![art.id, art.run_id, art.git_ref, art.commit_sha, json],
        )?;
        Ok(())
    }

    pub fn get_artifact(&self, artifact_id: &str) -> Result<Option<ArtifactSnapshot>> {
        let row: Option<String> = self
            .conn
            .query_row(
                "SELECT payload_json FROM artifacts WHERE id = ?1",
                params![artifact_id],
                |r| r.get(0),
            )
            .optional()?;
        match row {
            Some(s) => Ok(Some(serde_json::from_str(&s)?)),
            None => Ok(None),
        }
    }

    pub fn insert_step(&self, step: &StepAttempt) -> Result<()> {
        let json = serde_json::to_string(step)?;
        self.conn.execute(
            "INSERT OR REPLACE INTO steps (id, run_id, step_number, outcome, payload_json) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                step.id,
                step.run_id,
                step.step_number as i64,
                step.outcome.to_string(),
                json
            ],
        )?;
        Ok(())
    }

    pub fn list_steps(&self, run_id: &str) -> Result<Vec<StepAttempt>> {
        let mut stmt = self.conn.prepare(
            "SELECT payload_json FROM steps WHERE run_id = ?1 ORDER BY step_number ASC",
        )?;
        let rows = stmt.query_map(params![run_id], |r| r.get::<_, String>(0))?;
        let mut steps = Vec::new();
        for row in rows {
            steps.push(serde_json::from_str(&row?)?);
        }
        Ok(steps)
    }

    pub fn insert_memory(&self, mem: &EvolutionMemory) -> Result<()> {
        let json = serde_json::to_string(mem)?;
        self.conn.execute(
            "INSERT OR REPLACE INTO memories (id, run_id, step_number, payload_json) VALUES (?1, ?2, ?3, ?4)",
            params![mem.id, mem.run_id, mem.step_number as i64, json],
        )?;
        Ok(())
    }

    pub fn list_memories(&self, run_id: &str) -> Result<Vec<EvolutionMemory>> {
        let mut stmt = self.conn.prepare(
            "SELECT payload_json FROM memories WHERE run_id = ?1 ORDER BY step_number ASC",
        )?;
        let rows = stmt.query_map(params![run_id], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(serde_json::from_str(&row?)?);
        }
        Ok(out)
    }
}

pub fn run_status_event(status: RunStatus) -> Option<crate::models::NotificationEvent> {
    use crate::models::NotificationEvent;
    match status {
        RunStatus::GoalReached => Some(NotificationEvent::GoalReached),
        RunStatus::MaxStepsReached => Some(NotificationEvent::MaxStepsReached),
        RunStatus::AbortedOnBlocker => Some(NotificationEvent::AbortedOnBlocker),
        RunStatus::Stalled => Some(NotificationEvent::Stalled),
        RunStatus::Running => None,
    }
}
