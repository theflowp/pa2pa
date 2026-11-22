use anyhow::Result;
use chrono::{DateTime, Utc};
use pa2pa_crypto::{EncryptedEnvelope, Identity};
use pa2pa_proto::Attachment;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredMessage {
    pub id: String,
    pub thread_id: String,
    pub in_reply_to: Option<String>,
    /// "inbound" or "outbound"
    pub direction: String,
    pub peer_alias: String,
    pub peer_fingerprint: String,
    pub kind: String,
    pub subject: Option<String>,
    pub body: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
    pub sender_agent: Option<String>,
    pub auto_turn: u32,
    /// "unread", "read", "replied", "pending_approval", "denied", "sent"
    pub status: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub peer_alias: String,
    pub peer_fingerprint: String,
    pub subject: Option<String>,
    pub last_message: String,
    pub last_direction: String,
    pub last_at: DateTime<Utc>,
    pub message_count: i64,
    pub unread_count: i64,
    pub pending_approvals: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub id: String,
    pub message_id: String,
    pub thread_id: String,
    pub peer_alias: String,
    pub peer_message: String,
    pub reason: String,
    pub proposed_action: Option<String>,
    pub proposed_reply: Option<String>,
    /// "pending", "approved", "denied"
    pub status: String,
    pub decision_note: Option<String>,
    pub created_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OutboxEntry {
    pub message_id: String,
    pub peer_alias: String,
    pub peer_fingerprint: String,
    pub envelope: EncryptedEnvelope,
    pub attempts: u32,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub next_retry_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct InboxCounts {
    pub unread_messages: i64,
    pub pending_approvals: i64,
    pub pending_outbox: i64,
}

pub struct Store {
    conn: Mutex<Connection>,
    identity: Option<Identity>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_identity(path, None)
    }

    pub fn open_encrypted(path: &Path, identity: Identity) -> Result<Self> {
        Self::open_with_identity(path, Some(identity))
    }

    pub fn open_with_identity(path: &Path, identity: Option<Identity>) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS messages (
                 id TEXT PRIMARY KEY,
                 thread_id TEXT NOT NULL,
                 in_reply_to TEXT,
                 direction TEXT NOT NULL,
                 peer_alias TEXT NOT NULL,
                 peer_fingerprint TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 subject TEXT,
                 body TEXT NOT NULL,
                 attachments_json TEXT,
                 sender_agent TEXT,
                 auto_turn INTEGER NOT NULL DEFAULT 0,
                 status TEXT NOT NULL,
                 created_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_messages_thread ON messages(thread_id, created_at);
             CREATE INDEX IF NOT EXISTS idx_messages_status ON messages(status, created_at);
             CREATE TABLE IF NOT EXISTS approvals (
                 id TEXT PRIMARY KEY,
                 message_id TEXT NOT NULL,
                 thread_id TEXT NOT NULL,
                 peer_alias TEXT NOT NULL,
                 peer_message TEXT NOT NULL,
                 reason TEXT NOT NULL,
                 proposed_action TEXT,
                 proposed_reply TEXT,
                 status TEXT NOT NULL,
                 decision_note TEXT,
                 created_at TEXT NOT NULL,
                 decided_at TEXT
             );
             CREATE INDEX IF NOT EXISTS idx_approvals_status ON approvals(status, created_at);
             CREATE TABLE IF NOT EXISTS outbox (
                 message_id TEXT PRIMARY KEY,
                 peer_alias TEXT NOT NULL,
                 peer_fingerprint TEXT NOT NULL,
                 envelope_json TEXT NOT NULL,
                 attempts INTEGER NOT NULL DEFAULT 0,
                 last_error TEXT,
                 created_at TEXT NOT NULL,
                 next_retry_at TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS idx_outbox_retry ON outbox(next_retry_at);",
        )?;
        // Backwards-compatible migration for existing DBs without attachments_json
        let _ = conn.execute("ALTER TABLE messages ADD COLUMN attachments_json TEXT", []);
        Ok(Self {
            conn: Mutex::new(conn),
            identity,
        })
    }

    fn enc(&self, text: &str) -> Result<String> {
        if let Some(ref id) = self.identity {
            Ok(id.encrypt_local_text(text)?)
        } else {
            Ok(text.to_string())
        }
    }

    fn enc_opt(&self, text: Option<&str>) -> Result<Option<String>> {
        match text {
            Some(s) => Ok(Some(self.enc(s)?)),
            None => Ok(None),
        }
    }

    fn dec(&self, stored: String) -> String {
        if let Some(ref id) = self.identity {
            id.decrypt_local_text(&stored).unwrap_or(stored)
        } else {
            stored
        }
    }

    fn dec_opt(&self, stored: Option<String>) -> Option<String> {
        stored.map(|s| self.dec(s))
    }

    fn decrypt_message(&self, mut msg: StoredMessage, raw_attachments: Option<String>) -> StoredMessage {
        msg.subject = self.dec_opt(msg.subject);
        msg.body = self.dec(msg.body);
        if let Some(raw_att) = raw_attachments {
            let dec_json = self.dec(raw_att);
            msg.attachments = serde_json::from_str(&dec_json).unwrap_or_default();
        }
        msg
    }

    fn decrypt_approval(&self, mut rec: ApprovalRecord) -> ApprovalRecord {
        rec.peer_message = self.dec(rec.peer_message);
        rec.proposed_reply = self.dec_opt(rec.proposed_reply);
        rec
    }

    pub fn insert_message(&self, msg: &StoredMessage) -> Result<bool> {
        let enc_subject = self.enc_opt(msg.subject.as_deref())?;
        let enc_body = self.enc(&msg.body)?;
        let enc_attachments = if msg.attachments.is_empty() {
            None
        } else {
            let json = serde_json::to_string(&msg.attachments)?;
            Some(self.enc(&json)?)
        };

        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let rows = conn.execute(
            "INSERT OR IGNORE INTO messages
             (id, thread_id, in_reply_to, direction, peer_alias, peer_fingerprint, kind, subject, body, attachments_json, sender_agent, auto_turn, status, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                msg.id,
                msg.thread_id,
                msg.in_reply_to,
                msg.direction,
                msg.peer_alias,
                msg.peer_fingerprint,
                msg.kind,
                enc_subject,
                enc_body,
                enc_attachments,
                msg.sender_agent,
                msg.auto_turn as i64,
                msg.status,
                msg.created_at.to_rfc3339(),
            ],
        )?;
        Ok(rows > 0)
    }

    pub fn update_message_status(&self, id: &str, status: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "UPDATE messages SET status = ?1 WHERE id = ?2",
            params![status, id],
        )?;
        Ok(())
    }

    pub fn mark_thread_read(&self, thread_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "UPDATE messages SET status = 'read' WHERE thread_id = ?1 AND direction = 'inbound' AND status = 'unread'",
            params![thread_id],
        )?;
        Ok(())
    }

    pub fn get_message(&self, id: &str) -> Result<Option<StoredMessage>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT id, thread_id, in_reply_to, direction, peer_alias, peer_fingerprint, kind, subject, body, attachments_json, sender_agent, auto_turn, status, created_at
             FROM messages WHERE id = ?1",
        )?;
        let res = stmt.query_row(params![id], row_to_message).optional()?;
        Ok(res.map(|(m, att)| self.decrypt_message(m, att)))
    }

    pub fn list_inbox(&self, unread_only: bool, limit: usize) -> Result<Vec<StoredMessage>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let sql = if unread_only {
            "SELECT id, thread_id, in_reply_to, direction, peer_alias, peer_fingerprint, kind, subject, body, attachments_json, sender_agent, auto_turn, status, created_at
             FROM messages WHERE direction = 'inbound' AND status IN ('unread', 'pending_approval')
             ORDER BY created_at DESC LIMIT ?1"
        } else {
            "SELECT id, thread_id, in_reply_to, direction, peer_alias, peer_fingerprint, kind, subject, body, attachments_json, sender_agent, auto_turn, status, created_at
             FROM messages WHERE direction = 'inbound'
             ORDER BY created_at DESC LIMIT ?1"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map(params![limit as i64], row_to_message)?;
        let mut out = Vec::new();
        for r in rows {
            let (m, att) = r?;
            out.push(self.decrypt_message(m, att));
        }
        Ok(out)
    }

    pub fn list_thread_messages(&self, thread_id: &str) -> Result<Vec<StoredMessage>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT id, thread_id, in_reply_to, direction, peer_alias, peer_fingerprint, kind, subject, body, attachments_json, sender_agent, auto_turn, status, created_at
             FROM messages WHERE thread_id = ?1 ORDER BY created_at ASC",
        )?;
        let rows = stmt.query_map(params![thread_id], row_to_message)?;
        let mut out = Vec::new();
        for r in rows {
            let (m, att) = r?;
            out.push(self.decrypt_message(m, att));
        }
        Ok(out)
    }

    pub fn list_threads(&self, limit: usize) -> Result<Vec<ThreadSummary>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT
                m.thread_id,
                m.peer_alias,
                m.peer_fingerprint,
                (SELECT subject FROM messages s WHERE s.thread_id = m.thread_id AND s.subject IS NOT NULL ORDER BY s.created_at ASC LIMIT 1),
                m.body,
                m.direction,
                m.created_at,
                (SELECT COUNT(*) FROM messages c WHERE c.thread_id = m.thread_id),
                (SELECT COUNT(*) FROM messages u WHERE u.thread_id = m.thread_id AND u.direction = 'inbound' AND u.status IN ('unread', 'pending_approval')),
                (SELECT COUNT(*) FROM approvals a WHERE a.thread_id = m.thread_id AND a.status = 'pending')
             FROM messages m
             INNER JOIN (
                 SELECT thread_id, MAX(created_at) AS max_at FROM messages GROUP BY thread_id
             ) latest ON latest.thread_id = m.thread_id AND latest.max_at = m.created_at
             GROUP BY m.thread_id
             ORDER BY m.created_at DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            let ts: String = row.get(6)?;
            let last_at = DateTime::parse_from_rfc3339(&ts)
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or_else(|_| Utc::now());
            Ok(ThreadSummary {
                thread_id: row.get(0)?,
                peer_alias: row.get(1)?,
                peer_fingerprint: row.get(2)?,
                subject: row.get(3)?,
                last_message: row.get(4)?,
                last_direction: row.get(5)?,
                last_at,
                message_count: row.get(7)?,
                unread_count: row.get(8)?,
                pending_approvals: row.get(9)?,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            let mut t = r?;
            t.subject = self.dec_opt(t.subject);
            t.last_message = self.dec(t.last_message);
            out.push(t);
        }
        Ok(out)
    }

    pub fn create_approval(
        &self,
        message_id: &str,
        thread_id: &str,
        peer_alias: &str,
        peer_message: &str,
        reason: &str,
        proposed_action: Option<&str>,
        proposed_reply: Option<&str>,
    ) -> Result<ApprovalRecord> {
        let rec = ApprovalRecord {
            id: Uuid::new_v4().to_string()[..8].to_string(),
            message_id: message_id.to_string(),
            thread_id: thread_id.to_string(),
            peer_alias: peer_alias.to_string(),
            peer_message: peer_message.to_string(),
            reason: reason.to_string(),
            proposed_action: proposed_action.map(|s| s.to_string()),
            proposed_reply: proposed_reply.map(|s| s.to_string()),
            status: "pending".to_string(),
            decision_note: None,
            created_at: Utc::now(),
            decided_at: None,
        };
        let enc_peer_msg = self.enc(&rec.peer_message)?;
        let enc_prop_reply = self.enc_opt(rec.proposed_reply.as_deref())?;

        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT INTO approvals
             (id, message_id, thread_id, peer_alias, peer_message, reason, proposed_action, proposed_reply, status, decision_note, created_at, decided_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                rec.id,
                rec.message_id,
                rec.thread_id,
                rec.peer_alias,
                enc_peer_msg,
                rec.reason,
                rec.proposed_action,
                enc_prop_reply,
                rec.status,
                rec.decision_note,
                rec.created_at.to_rfc3339(),
                Option::<String>::None,
            ],
        )?;
        Ok(rec)
    }

    pub fn list_approvals(&self, status: Option<&str>) -> Result<Vec<ApprovalRecord>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut out = Vec::new();
        if let Some(st) = status {
            let mut stmt = conn.prepare(
                "SELECT id, message_id, thread_id, peer_alias, peer_message, reason, proposed_action, proposed_reply, status, decision_note, created_at, decided_at
                 FROM approvals WHERE status = ?1 ORDER BY created_at DESC",
            )?;
            let rows = stmt.query_map(params![st], row_to_approval)?;
            for r in rows {
                out.push(self.decrypt_approval(r?));
            }
        } else {
            let mut stmt = conn.prepare(
                "SELECT id, message_id, thread_id, peer_alias, peer_message, reason, proposed_action, proposed_reply, status, decision_note, created_at, decided_at
                 FROM approvals ORDER BY created_at DESC LIMIT 100",
            )?;
            let rows = stmt.query_map([], row_to_approval)?;
            for r in rows {
                out.push(self.decrypt_approval(r?));
            }
        }
        Ok(out)
    }

    pub fn get_approval(&self, id: &str) -> Result<Option<ApprovalRecord>> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT id, message_id, thread_id, peer_alias, peer_message, reason, proposed_action, proposed_reply, status, decision_note, created_at, decided_at
             FROM approvals WHERE id = ?1",
        )?;
        Ok(stmt
            .query_row(params![id], row_to_approval)
            .optional()?
            .map(|r| self.decrypt_approval(r)))
    }

    pub fn decide_approval(
        &self,
        id: &str,
        status: &str,
        decision_note: Option<&str>,
    ) -> Result<Option<ApprovalRecord>> {
        let now = Utc::now().to_rfc3339();
        let changed = {
            let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
            conn.execute(
                "UPDATE approvals SET status = ?1, decision_note = ?2, decided_at = ?3 WHERE id = ?4",
                params![status, decision_note, now, id],
            )?
        };
        if changed == 0 {
            return Ok(None);
        }
        self.get_approval(id)
    }

    pub fn enqueue_outbox(
        &self,
        peer_alias: &str,
        peer_fingerprint: &str,
        envelope: &EncryptedEnvelope,
        last_error: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now();
        let next = now + chrono::Duration::seconds(5);
        let env_json = serde_json::to_string(envelope)?;
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "INSERT OR REPLACE INTO outbox
             (message_id, peer_alias, peer_fingerprint, envelope_json, attempts, last_error, created_at, next_retry_at)
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7)",
            params![
                envelope.message_id,
                peer_alias,
                peer_fingerprint,
                env_json,
                last_error,
                now.to_rfc3339(),
                next.to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub fn list_due_outbox(&self, limit: usize) -> Result<Vec<OutboxEntry>> {
        let now = Utc::now().to_rfc3339();
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut stmt = conn.prepare(
            "SELECT message_id, peer_alias, peer_fingerprint, envelope_json, attempts, last_error, created_at, next_retry_at
             FROM outbox WHERE next_retry_at <= ?1 ORDER BY next_retry_at ASC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![now, limit as i64], |row| {
            let env_str: String = row.get(3)?;
            let created_str: String = row.get(6)?;
            let next_str: String = row.get(7)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                env_str,
                row.get::<_, i64>(4)? as u32,
                row.get::<_, Option<String>>(5)?,
                created_str,
                next_str,
            ))
        })?;
        let mut out = Vec::new();
        for r in rows {
            let (message_id, peer_alias, peer_fingerprint, env_str, attempts, last_error, c_str, n_str) = r?;
            if let Ok(envelope) = serde_json::from_str::<EncryptedEnvelope>(&env_str) {
                let created_at = DateTime::parse_from_rfc3339(&c_str)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                let next_retry_at = DateTime::parse_from_rfc3339(&n_str)
                    .map(|d| d.with_timezone(&Utc))
                    .unwrap_or_else(|_| Utc::now());
                out.push(OutboxEntry {
                    message_id,
                    peer_alias,
                    peer_fingerprint,
                    envelope,
                    attempts,
                    last_error,
                    created_at,
                    next_retry_at,
                });
            }
        }
        Ok(out)
    }

    pub fn mark_outbox_retry(&self, message_id: &str, attempts: u32, err: &str) -> Result<()> {
        let delay_secs = (5i64 * (1i64 << attempts.min(4))).min(60);
        let next = (Utc::now() + chrono::Duration::seconds(delay_secs)).to_rfc3339();
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute(
            "UPDATE outbox SET attempts = ?1, last_error = ?2, next_retry_at = ?3 WHERE message_id = ?4",
            params![attempts as i64, err, next, message_id],
        )?;
        Ok(())
    }

    pub fn remove_outbox(&self, message_id: &str) -> Result<()> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        conn.execute("DELETE FROM outbox WHERE message_id = ?1", params![message_id])?;
        Ok(())
    }

    pub fn purge_expired_outbox(&self, max_age_days: i64) -> Result<usize> {
        let cutoff = (Utc::now() - chrono::Duration::days(max_age_days)).to_rfc3339();
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        Ok(conn.execute("DELETE FROM outbox WHERE created_at < ?1", params![cutoff])?)
    }

    pub fn counts(&self) -> Result<InboxCounts> {
        let conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        let unread_messages: i64 = conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE direction = 'inbound' AND status IN ('unread', 'pending_approval')",
            [],
            |r| r.get(0),
        )?;
        let pending_approvals: i64 = conn.query_row(
            "SELECT COUNT(*) FROM approvals WHERE status = 'pending'",
            [],
            |r| r.get(0),
        )?;
        let pending_outbox: i64 = conn.query_row("SELECT COUNT(*) FROM outbox", [], |r| r.get(0))?;
        Ok(InboxCounts {
            unread_messages,
            pending_approvals,
            pending_outbox,
        })
    }
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<(StoredMessage, Option<String>)> {
    let ts: String = row.get(13)?;
    let created_at = DateTime::parse_from_rfc3339(&ts)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    let att_json: Option<String> = row.get(9)?;
    Ok((
        StoredMessage {
            id: row.get(0)?,
            thread_id: row.get(1)?,
            in_reply_to: row.get(2)?,
            direction: row.get(3)?,
            peer_alias: row.get(4)?,
            peer_fingerprint: row.get(5)?,
            kind: row.get(6)?,
            subject: row.get(7)?,
            body: row.get(8)?,
            attachments: Vec::new(),
            sender_agent: row.get(10)?,
            auto_turn: row.get::<_, i64>(11)? as u32,
            status: row.get(12)?,
            created_at,
        },
        att_json,
    ))
}

fn row_to_approval(row: &rusqlite::Row<'_>) -> rusqlite::Result<ApprovalRecord> {
    let created_str: String = row.get(10)?;
    let decided_str: Option<String> = row.get(11)?;
    let created_at = DateTime::parse_from_rfc3339(&created_str)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now());
    let decided_at = decided_str.and_then(|s| {
        DateTime::parse_from_rfc3339(&s)
            .ok()
            .map(|d| d.with_timezone(&Utc))
    });
    Ok(ApprovalRecord {
        id: row.get(0)?,
        message_id: row.get(1)?,
        thread_id: row.get(2)?,
        peer_alias: row.get(3)?,
        peer_message: row.get(4)?,
        reason: row.get(5)?,
        proposed_action: row.get(6)?,
        proposed_reply: row.get(7)?,
        status: row.get(8)?,
        decision_note: row.get(9)?,
        created_at,
        decided_at,
    })
}
