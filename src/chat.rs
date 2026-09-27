//! The project chat: posting, reading, acknowledging.
//!
//! Delivery is fan-out on write: posting creates one receipt per recipient, and
//! every "what's new for me" question is answered from receipts. A receipt is
//! *delivered* once its message has been shown to the agent (by a hook or
//! `inbox`); a `request` additionally stays *pending* until the agent replies to
//! it or acks it.

use std::collections::BTreeSet;

use anyhow::{Result, bail};
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::agents::{self, Agent};
use crate::db::now_ms;
use crate::format;

pub const DEFAULT_ROOM: &str = "general";
pub const MAX_BODY_CHARS: usize = 4000;

const RATE_WINDOW_MS: i64 = 5 * 60 * 1000;
const RATE_LIMIT: i64 = 20;
const DUPLICATE_WINDOW_MS: i64 = 10 * 60 * 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Intent {
    /// You need an answer: the recipients are reminded until they reply or ack.
    Request,
    /// FYI / status update: reply only if useful.
    #[default]
    Inform,
    /// Acknowledgement: no reply expected.
    Ack,
}

impl Intent {
    pub fn as_str(self) -> &'static str {
        match self {
            Intent::Request => "request",
            Intent::Inform => "inform",
            Intent::Ack => "ack",
        }
    }

    fn parse(s: &str) -> Self {
        match s {
            "request" => Intent::Request,
            "ack" => Intent::Ack,
            _ => Intent::Inform,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub id: i64,
    pub sender_id: i64,
    pub sender: String,
    pub room: Option<String>,
    pub recipient_id: Option<i64>,
    pub recipient: Option<String>,
    pub thread_id: Option<i64>,
    pub reply_to: Option<i64>,
    pub intent: Intent,
    pub body: String,
    pub created_at: i64,
}

impl Message {
    /// "#general" or "@codex-1".
    pub fn destination(&self) -> String {
        match (&self.room, &self.recipient) {
            (Some(room), _) => format!("#{room}"),
            (None, Some(to)) => format!("@{to}"),
            (None, None) => "?".to_string(),
        }
    }

    /// One header line plus the body, e.g.
    /// `#12 14:03 @codex-1 → #general [request, re #10]: body`.
    pub fn render(&self, max_body: usize) -> String {
        let mut tags = Vec::new();
        if self.intent != Intent::Inform {
            tags.push(self.intent.as_str().to_string());
        }
        if let Some(parent) = self.reply_to {
            tags.push(format!("re #{parent}"));
        }
        let tags = if tags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", tags.join(", "))
        };
        let body = format::truncate(self.body.trim(), max_body);
        let more = if body.chars().count() < self.body.trim().chars().count() {
            format!(
                " (truncated; inbox(thread={}) shows all)",
                self.thread_id.unwrap_or(self.id)
            )
        } else {
            String::new()
        };
        format!(
            "#{} {} @{} → {}{}: {}{}",
            self.id,
            format::clock(self.created_at),
            self.sender,
            self.destination(),
            tags,
            format::indent_continuation(&body, "    "),
            more
        )
    }
}

const SELECT: &str = "SELECT m.id, m.sender_id, s.handle, m.room, m.recipient_id, r.handle,
                             m.thread_id, m.reply_to, m.intent, m.body, m.created_at
                      FROM messages m
                      JOIN agents s ON s.id = m.sender_id
                      LEFT JOIN agents r ON r.id = m.recipient_id";

fn from_row(row: &Row) -> rusqlite::Result<Message> {
    let intent: String = row.get(8)?;
    Ok(Message {
        id: row.get(0)?,
        sender_id: row.get(1)?,
        sender: row.get(2)?,
        room: row.get(3)?,
        recipient_id: row.get(4)?,
        recipient: row.get(5)?,
        thread_id: row.get(6)?,
        reply_to: row.get(7)?,
        intent: Intent::parse(&intent),
        body: row.get(9)?,
        created_at: row.get(10)?,
    })
}

#[derive(Debug, Default, Clone)]
pub struct NewMessage {
    pub body: String,
    pub intent: Intent,
    /// Direct message to this handle ("@codex-1" or "codex-1").
    pub to: Option<String>,
    pub room: Option<String>,
    pub reply_to: Option<i64>,
}

#[derive(Debug)]
pub struct Posted {
    pub message: Message,
    /// Handles that received a receipt.
    pub recipients: Vec<String>,
    /// @mentions that matched nobody in the project.
    pub unknown_mentions: Vec<String>,
}

pub fn post(
    conn: &mut Connection,
    project_id: i64,
    sender: &Agent,
    new: NewMessage,
) -> Result<Posted> {
    let body = new.body.trim().to_string();
    if body.is_empty() {
        bail!("message body is empty");
    }
    if body.chars().count() > MAX_BODY_CHARS {
        bail!(
            "message is longer than {MAX_BODY_CHARS} characters; keep chat messages short and \
             put long content in a shared note (remember) that you link to"
        );
    }

    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let now = now_ms();

    if !sender.is_human() {
        let recent: i64 = tx.query_row(
            "SELECT count(*) FROM messages WHERE sender_id = ?1 AND created_at > ?2",
            params![sender.id, now - RATE_WINDOW_MS],
            |r| r.get(0),
        )?;
        if recent >= RATE_LIMIT {
            bail!(
                "you have posted {recent} messages in the last 5 minutes; slow down and batch \
                 updates into fewer, fuller messages"
            );
        }
        let duplicate: Option<i64> = tx
            .query_row(
                "SELECT id FROM messages WHERE sender_id = ?1 AND body = ?2 AND created_at > ?3",
                params![sender.id, body, now - DUPLICATE_WINDOW_MS],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = duplicate {
            bail!("you already posted this exact message as #{id}");
        }
    }

    let parent = match new.reply_to {
        Some(id) => Some(
            load(&tx, project_id, id)?
                .ok_or_else(|| anyhow::anyhow!("there is no message #{id} in this project"))?,
        ),
        None => None,
    };

    // Destination: explicit `to`, else explicit room, else the parent's, else #general.
    let (room, recipient) = match (&new.to, &new.room, &parent) {
        (Some(to), _, _) => {
            let Some(target) = agents::find(&tx, project_id, to)? else {
                bail!(
                    "{} is not in this project; call who to list participants",
                    at(to)
                );
            };
            if target.id == sender.id {
                bail!("you can't send a direct message to yourself");
            }
            (None, Some(target))
        }
        (None, Some(room), _) => (Some(normalize_room(room)?), None),
        (None, None, Some(p)) => match (&p.room, p.recipient_id) {
            (Some(room), _) => (Some(room.clone()), None),
            (None, Some(recipient_id)) => {
                let other = if p.sender_id == sender.id {
                    recipient_id
                } else {
                    p.sender_id
                };
                (None, Some(agents::get(&tx, other)?))
            }
            (None, None) => (Some(DEFAULT_ROOM.to_string()), None),
        },
        (None, None, None) => (Some(DEFAULT_ROOM.to_string()), None),
    };

    let thread_id = parent.as_ref().map(|p| p.thread_id.unwrap_or(p.id));
    tx.execute(
        "INSERT INTO messages (project_id, sender_id, room, recipient_id, thread_id, reply_to,
                               intent, body, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            project_id,
            sender.id,
            room,
            recipient.as_ref().map(|r| r.id),
            thread_id,
            new.reply_to,
            new.intent.as_str(),
            body,
            now
        ],
    )?;
    let message_id = tx.last_insert_rowid();

    // Receipts: the DM recipient, or every online agent for a room message, plus
    // anyone @mentioned or being replied to. The human reads everything via
    // `chitchat tail`.
    let everyone = agents::list(&tx, project_id)?;
    let mentions = parse_mentions(&body);
    let mut unknown_mentions = Vec::new();
    for m in &mentions {
        if !everyone.iter().any(|a| &a.handle == m) {
            unknown_mentions.push(format!("@{m}"));
        }
    }
    let mut recipients = Vec::new();
    for agent in &everyone {
        if agent.id == sender.id || agent.is_human() {
            continue;
        }
        // A reply always reaches the author of the message it answers, even if
        // they're offline right now: they're the one waiting for it.
        let direct = recipient.as_ref().is_some_and(|r| r.id == agent.id)
            || parent.as_ref().is_some_and(|p| p.sender_id == agent.id);
        let mentioned = direct || mentions.contains(&agent.handle);
        let in_room = room.is_some() && agent.is_online();
        if direct || mentioned || in_room {
            tx.execute(
                "INSERT INTO receipts (message_id, agent_id, mentioned) VALUES (?1, ?2, ?3)",
                params![message_id, agent.id, mentioned],
            )?;
            recipients.push(agent.handle.clone());
        }
    }

    // Replying to a message settles it for the replier.
    if let Some(parent) = &parent {
        tx.execute(
            "UPDATE receipts SET acked_at = ?3, delivered_at = coalesce(delivered_at, ?3)
             WHERE message_id = ?1 AND agent_id = ?2 AND acked_at IS NULL",
            params![parent.id, sender.id, now],
        )?;
    }

    let message = load(&tx, project_id, message_id)?.expect("just inserted");
    tx.commit()?;
    Ok(Posted {
        message,
        recipients,
        unknown_mentions,
    })
}

fn at(handle: &str) -> String {
    format!("@{}", handle.trim().trim_start_matches('@'))
}

fn normalize_room(room: &str) -> Result<String> {
    let room = room.trim().trim_start_matches('#').to_ascii_lowercase();
    let valid = (1..=32).contains(&room.len())
        && room
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !valid {
        bail!("room names are 1-32 characters: lowercase letters, digits, '-' and '_'");
    }
    Ok(room)
}

/// Lowercased handles after "@" that isn't part of a word (so emails don't count).
pub fn parse_mentions(body: &str) -> BTreeSet<String> {
    let chars: Vec<char> = body.chars().collect();
    let mut out = BTreeSet::new();
    for (i, &c) in chars.iter().enumerate() {
        if c != '@' || (i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_')) {
            continue;
        }
        let handle: String = chars[i + 1..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric() || **c == '-')
            .collect();
        let handle = handle.trim_end_matches('-').to_ascii_lowercase();
        if !handle.is_empty() {
            out.insert(handle);
        }
    }
    out
}

pub fn load(conn: &Connection, project_id: i64, id: i64) -> Result<Option<Message>> {
    Ok(conn
        .query_row(
            &format!("{SELECT} WHERE m.project_id = ?1 AND m.id = ?2"),
            params![project_id, id],
            from_row,
        )
        .optional()?)
}

#[derive(Debug, Clone, Default)]
pub struct InboxQuery {
    /// Only messages not yet shown to me (default). Otherwise recent history.
    pub unread_only: bool,
    pub room: Option<String>,
    /// A whole thread, by any message id in it.
    pub thread: Option<i64>,
    pub limit: usize,
}

#[derive(Debug)]
pub struct Inbox {
    pub messages: Vec<Message>,
    /// Unread messages not included because of the limit.
    pub more_unread: usize,
    /// Requests addressed to me that I haven't answered yet.
    pub pending: Vec<Message>,
}

pub fn inbox(conn: &mut Connection, project_id: i64, me: &Agent, q: &InboxQuery) -> Result<Inbox> {
    let limit = q.limit.clamp(1, 100) as i64;
    let room = q.room.as_deref().map(normalize_room).transpose()?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

    let messages = if let Some(any_id) = q.thread {
        let Some(m) = load(&tx, project_id, any_id)? else {
            bail!("there is no message #{any_id} in this project");
        };
        let root = m.thread_id.unwrap_or(m.id);
        query(
            &tx,
            &format!(
                "{SELECT} WHERE m.project_id = ?1 AND (m.id = ?2 OR m.thread_id = ?2)
                 AND {VISIBLE} ORDER BY m.id LIMIT ?4"
            ),
            params![project_id, root, me.id, limit],
        )?
    } else if q.unread_only {
        query(
            &tx,
            &format!(
                "{SELECT} JOIN receipts rc ON rc.message_id = m.id AND rc.agent_id = ?2
                 WHERE m.project_id = ?1 AND rc.delivered_at IS NULL
                 AND (?3 IS NULL OR m.room = ?3) ORDER BY m.id LIMIT ?4"
            ),
            params![project_id, me.id, room, limit],
        )?
    } else {
        let mut recent = query(
            &tx,
            &format!(
                "{SELECT} WHERE m.project_id = ?1 AND {VISIBLE}
                 AND (?2 IS NULL OR m.room = ?2) ORDER BY m.id DESC LIMIT ?4"
            ),
            params![project_id, room, me.id, limit],
        )?;
        recent.reverse();
        recent
    };

    mark_delivered(&tx, me.id, messages.iter().map(|m| m.id))?;
    let more_unread = unread_count(&tx, me.id)?.total;
    let pending = pending_requests(&tx, project_id, me.id)?;
    tx.commit()?;
    Ok(Inbox {
        messages,
        more_unread,
        pending,
    })
}

/// Room messages, plus direct messages to or from me. `?3` must bind my agent id.
const VISIBLE: &str = "(m.room IS NOT NULL OR m.sender_id = ?3 OR m.recipient_id = ?3)";

fn query(conn: &Connection, sql: &str, params: impl rusqlite::Params) -> Result<Vec<Message>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(params, from_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok(rows)
}

fn mark_delivered(conn: &Connection, agent_id: i64, ids: impl Iterator<Item = i64>) -> Result<()> {
    let now = now_ms();
    let mut stmt = conn.prepare(
        "UPDATE receipts SET delivered_at = ?3
         WHERE message_id = ?1 AND agent_id = ?2 AND delivered_at IS NULL",
    )?;
    for id in ids {
        stmt.execute(params![id, agent_id, now])?;
    }
    Ok(())
}

/// Requests addressed to `agent_id` that are still waiting for an answer.
pub fn pending_requests(conn: &Connection, project_id: i64, agent_id: i64) -> Result<Vec<Message>> {
    query(
        conn,
        &format!(
            "{SELECT} JOIN receipts rc ON rc.message_id = m.id AND rc.agent_id = ?2
             WHERE m.project_id = ?1 AND m.intent = 'request' AND rc.acked_at IS NULL
             ORDER BY m.id"
        ),
        params![project_id, agent_id],
    )
}

/// Marks requests as handled. Returns how many were still pending.
pub fn ack(conn: &Connection, agent_id: i64, ids: &[i64]) -> Result<usize> {
    let now = now_ms();
    let mut stmt = conn.prepare(
        "UPDATE receipts SET acked_at = ?3, delivered_at = coalesce(delivered_at, ?3)
         WHERE message_id = ?1 AND agent_id = ?2 AND acked_at IS NULL",
    )?;
    let mut n = 0;
    for id in ids {
        n += stmt.execute(params![id, agent_id, now])?;
    }
    Ok(n)
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct UnreadCount {
    pub total: usize,
    /// DMs, @mentions and requests.
    pub urgent: usize,
}

pub fn unread_count(conn: &Connection, agent_id: i64) -> Result<UnreadCount> {
    let (total, urgent): (i64, i64) = conn.query_row(
        "SELECT count(*), coalesce(sum(rc.mentioned OR m.intent = 'request'), 0)
         FROM receipts rc JOIN messages m ON m.id = rc.message_id
         WHERE rc.agent_id = ?1 AND rc.delivered_at IS NULL",
        [agent_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok(UnreadCount {
        total: total as usize,
        urgent: urgent as usize,
    })
}

/// Which unread messages a hook should hand to the agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deliver {
    Everything,
    /// DMs, @mentions and requests only (used mid-turn).
    Urgent,
}

/// Takes unread messages for a hook to show, oldest first, until `budget` characters
/// of rendered text are used, and marks them delivered. Returns the rendered
/// messages and how many unread messages are left over.
pub fn take_unread(
    conn: &mut Connection,
    project_id: i64,
    me: &Agent,
    which: Deliver,
    budget: usize,
    max_body: usize,
) -> Result<(Vec<Message>, usize)> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let urgent_only = which == Deliver::Urgent;
    let candidates = query(
        &tx,
        &format!(
            "{SELECT} JOIN receipts rc ON rc.message_id = m.id AND rc.agent_id = ?2
             WHERE m.project_id = ?1 AND rc.delivered_at IS NULL
             AND (NOT ?3 OR rc.mentioned OR m.intent = 'request')
             ORDER BY m.id LIMIT 200"
        ),
        params![project_id, me.id, urgent_only],
    )?;
    let mut used = 0;
    let mut taken = Vec::new();
    for m in candidates {
        let len = m.render(max_body).chars().count() + 1;
        if !taken.is_empty() && used + len > budget {
            break;
        }
        used += len;
        taken.push(m);
    }
    mark_delivered(&tx, me.id, taken.iter().map(|m| m.id))?;
    let left = unread_count(&tx, me.id)?;
    tx.commit()?;
    let left = if urgent_only { left.urgent } else { left.total };
    Ok((taken, left))
}

/// Pending requests the Stop hook hasn't nudged about yet; marks them nudged (and
/// delivered, since the nudge shows them).
pub fn take_unnudged_requests(
    conn: &mut Connection,
    project_id: i64,
    me: &Agent,
) -> Result<Vec<Message>> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let requests = query(
        &tx,
        &format!(
            "{SELECT} JOIN receipts rc ON rc.message_id = m.id AND rc.agent_id = ?2
             WHERE m.project_id = ?1 AND m.intent = 'request'
             AND rc.acked_at IS NULL AND rc.nudged_at IS NULL
             ORDER BY m.id LIMIT 20"
        ),
        params![project_id, me.id],
    )?;
    let now = now_ms();
    for m in &requests {
        tx.execute(
            "UPDATE receipts SET nudged_at = ?3, delivered_at = coalesce(delivered_at, ?3)
             WHERE message_id = ?1 AND agent_id = ?2",
            params![m.id, me.id, now],
        )?;
    }
    tx.commit()?;
    Ok(requests)
}

/// Every message in the project after `after_id` (for `chitchat tail`).
pub fn history(
    conn: &Connection,
    project_id: i64,
    after_id: i64,
    room: Option<&str>,
    limit: usize,
) -> Result<Vec<Message>> {
    let room = room.map(normalize_room).transpose()?;
    let mut rows = query(
        conn,
        &format!(
            "{SELECT} WHERE m.project_id = ?1 AND m.id > ?2 AND (?3 IS NULL OR m.room = ?3)
             ORDER BY m.id DESC LIMIT ?4"
        ),
        params![project_id, after_id, room, limit as i64],
    )?;
    rows.reverse();
    Ok(rows)
}

/// Messages matching a full-text query that `me` can see.
pub fn search(
    conn: &Connection,
    project_id: i64,
    me: &Agent,
    fts_query: &str,
    limit: usize,
) -> Result<Vec<(Message, String)>> {
    let mut stmt = conn.prepare(&format!(
        "{SELECT} JOIN messages_fts f ON f.rowid = m.id
         WHERE messages_fts MATCH ?2 AND m.project_id = ?1 AND {VISIBLE}
         ORDER BY bm25(messages_fts) LIMIT ?4"
    ))?;
    let rows = stmt
        .query_map(params![project_id, fts_query, me.id, limit as i64], |row| {
            from_row(row)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .map(|m| {
            let snippet = format::truncate(&m.body.replace('\n', " "), 160);
            (m, snippet)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::tests::{caller, proc, project};

    struct World {
        _dir: tempfile::TempDir,
        conn: Connection,
        project: i64,
    }

    fn world() -> World {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open(&dir.path().join("t.db")).unwrap();
        let project = project(&conn);
        World {
            _dir: dir,
            conn,
            project,
        }
    }

    /// Agents need a live client process to receive room messages.
    fn live_agent(w: &mut World, vendor: &str, child: &std::process::Child) -> Agent {
        let info = crate::procs::info(child.id()).unwrap();
        agents::resolve(&mut w.conn, w.project, &caller(vendor, Some(&info), None)).unwrap()
    }

    fn sleeper() -> std::process::Child {
        std::process::Command::new("sleep")
            .arg("60")
            .spawn()
            .unwrap()
    }

    fn say(w: &mut World, from: &Agent, body: &str) -> Posted {
        post(
            &mut w.conn,
            w.project,
            from,
            NewMessage {
                body: body.into(),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn mentions_ignore_emails_and_trailing_punctuation() {
        let m = parse_mentions("hey @Codex-1, see @claude-2. mail me@example.com @-");
        assert_eq!(m.into_iter().collect::<Vec<_>>(), ["claude-2", "codex-1"]);
    }

    #[test]
    fn room_messages_reach_online_agents_and_requests_stay_pending_until_answered() {
        let mut w = world();
        let (mut p1, mut p2) = (sleeper(), sleeper());
        let claude = live_agent(&mut w, "claude", &p1);
        let codex = live_agent(&mut w, "codex", &p2);
        // An agent whose process is gone gets nothing.
        let gone = agents::resolve(
            &mut w.conn,
            w.project,
            &caller("codex", Some(&proc(999_998)), None),
        )
        .unwrap();

        let posted = post(
            &mut w.conn,
            w.project,
            &claude,
            NewMessage {
                body: "Can someone review the schema? @codex-1".into(),
                intent: Intent::Request,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(posted.recipients, ["codex-1"]);
        assert_eq!(
            unread_count(&w.conn, codex.id).unwrap(),
            UnreadCount {
                total: 1,
                urgent: 1
            }
        );
        assert_eq!(unread_count(&w.conn, gone.id).unwrap().total, 0);

        let inbox = inbox(
            &mut w.conn,
            w.project,
            &codex,
            &InboxQuery {
                unread_only: true,
                limit: 10,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(inbox.messages.len(), 1);
        assert_eq!(inbox.pending.len(), 1);
        assert_eq!(unread_count(&w.conn, codex.id).unwrap().total, 0);

        // Replying settles the request for the replier and threads the reply.
        let reply = post(
            &mut w.conn,
            w.project,
            &codex,
            NewMessage {
                body: "Looks good.".into(),
                reply_to: Some(posted.message.id),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(reply.message.thread_id, Some(posted.message.id));
        assert_eq!(reply.message.room.as_deref(), Some("general"));
        assert!(
            pending_requests(&w.conn, w.project, codex.id)
                .unwrap()
                .is_empty()
        );
        assert_eq!(unread_count(&w.conn, claude.id).unwrap().total, 1);

        for p in [&mut p1, &mut p2] {
            p.kill().unwrap();
            p.wait().unwrap();
        }
    }

    #[test]
    fn replies_to_direct_messages_go_back_to_the_sender() {
        let mut w = world();
        let (mut p1, mut p2) = (sleeper(), sleeper());
        let claude = live_agent(&mut w, "claude", &p1);
        let codex = live_agent(&mut w, "codex", &p2);

        let dm = post(
            &mut w.conn,
            w.project,
            &claude,
            NewMessage {
                body: "psst".into(),
                to: Some("@codex-1".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(dm.message.destination(), "@codex-1");
        let back = post(
            &mut w.conn,
            w.project,
            &codex,
            NewMessage {
                body: "yes?".into(),
                reply_to: Some(dm.message.id),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(back.message.destination(), "@claude-1");
        assert_eq!(back.recipients, ["claude-1"]);

        for p in [&mut p1, &mut p2] {
            p.kill().unwrap();
            p.wait().unwrap();
        }
    }

    #[test]
    fn replies_reach_the_asker_even_when_offline() {
        let mut w = world();
        let mut p = sleeper();
        let codex = live_agent(&mut w, "codex", &p);
        // The asker's process has exited (e.g. a finished `claude -p` run).
        let asker = agents::resolve(
            &mut w.conn,
            w.project,
            &caller("claude", Some(&proc(999_997)), Some("s1")),
        )
        .unwrap();
        let q = say(&mut w, &asker, "anyone?");
        assert!(q.recipients.contains(&"codex-1".to_string()));
        let a = post(
            &mut w.conn,
            w.project,
            &codex,
            NewMessage {
                body: "me".into(),
                reply_to: Some(q.message.id),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(a.recipients, ["claude-1"]);
        assert_eq!(
            unread_count(&w.conn, asker.id).unwrap(),
            UnreadCount {
                total: 1,
                urgent: 1
            }
        );
        p.kill().unwrap();
        p.wait().unwrap();
    }

    #[test]
    fn duplicates_and_floods_are_rejected() {
        let mut w = world();
        let me = agents::resolve(
            &mut w.conn,
            w.project,
            &caller("claude", Some(&proc(1)), None),
        )
        .unwrap();
        say(&mut w, &me, "hello");
        let dup = post(
            &mut w.conn,
            w.project,
            &me,
            NewMessage {
                body: "hello".into(),
                ..Default::default()
            },
        );
        assert!(dup.unwrap_err().to_string().contains("already posted"));
        for i in 0..19 {
            say(&mut w, &me, &format!("update {i}"));
        }
        let flood = post(
            &mut w.conn,
            w.project,
            &me,
            NewMessage {
                body: "one more".into(),
                ..Default::default()
            },
        );
        assert!(flood.unwrap_err().to_string().contains("slow down"));
    }

    #[test]
    fn take_unread_respects_budget_and_urgency() {
        let mut w = world();
        let (mut p1, mut p2) = (sleeper(), sleeper());
        let claude = live_agent(&mut w, "claude", &p1);
        let codex = live_agent(&mut w, "codex", &p2);
        say(&mut w, &claude, "fyi one");
        say(&mut w, &claude, "@codex-1 urgent two");
        say(&mut w, &claude, "fyi three");

        let (urgent, left) =
            take_unread(&mut w.conn, w.project, &codex, Deliver::Urgent, 10_000, 500).unwrap();
        assert_eq!(urgent.len(), 1);
        assert_eq!(left, 0);
        let (rest, left) =
            take_unread(&mut w.conn, w.project, &codex, Deliver::Everything, 1, 500).unwrap();
        assert_eq!(rest.len(), 1, "at least one message even if over budget");
        assert_eq!(left, 1);

        for p in [&mut p1, &mut p2] {
            p.kill().unwrap();
            p.wait().unwrap();
        }
    }
}
