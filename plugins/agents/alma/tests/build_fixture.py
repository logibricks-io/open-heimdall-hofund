#!/usr/bin/env python3
"""Build chat_threads.db for Alma session reader golden tests."""

import sqlite3
from pathlib import Path

SCHEMA = """
CREATE TABLE workspaces (id TEXT PRIMARY KEY, path TEXT NOT NULL, name TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
CREATE TABLE chat_threads (id TEXT PRIMARY KEY, title TEXT NOT NULL, model TEXT, metadata TEXT DEFAULT '{}',
 created_at TEXT NOT NULL, updated_at TEXT NOT NULL, is_incognito INTEGER DEFAULT 0, workspace_id TEXT, parent_thread_id TEXT);
CREATE TABLE chat_messages (id TEXT, thread_id TEXT NOT NULL, parent_id TEXT, slot_id TEXT, depth TEXT NOT NULL, message TEXT NOT NULL,
 timestamp TEXT NOT NULL, metadata TEXT DEFAULT '{}', created_at TEXT NOT NULL, updated_at TEXT NOT NULL, parent_tool_call_id TEXT);
CREATE INDEX idx_messages_version_info ON chat_messages(thread_id, timestamp, id, slot_id, created_at);
CREATE TABLE usage_records (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, thread_id TEXT NOT NULL, model TEXT, provider_id TEXT,
 date TEXT NOT NULL, input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0, cached_input_tokens INTEGER DEFAULT 0,
 cache_write_input_tokens INTEGER DEFAULT 0, reasoning_tokens INTEGER DEFAULT 0, total_tokens INTEGER DEFAULT 0,
 timestamp TEXT NOT NULL, created_at TEXT NOT NULL);
CREATE TABLE aux_usage_records (id TEXT PRIMARY KEY, purpose TEXT NOT NULL, model TEXT, provider_id TEXT, thread_id TEXT,
 date TEXT NOT NULL, input_tokens INTEGER DEFAULT 0, output_tokens INTEGER DEFAULT 0, cached_input_tokens INTEGER DEFAULT 0,
 cache_write_input_tokens INTEGER DEFAULT 0, reasoning_tokens INTEGER DEFAULT 0, total_tokens INTEGER DEFAULT 0,
 timestamp TEXT NOT NULL, created_at TEXT NOT NULL);
"""

ROOT = Path(__file__).resolve().parent / "fixtures" / "root"
DB = ROOT / "chat_threads.db"


def main() -> None:
    ROOT.mkdir(parents=True, exist_ok=True)
    if DB.exists():
        DB.unlink()
    conn = sqlite3.connect(DB)
    conn.executescript(SCHEMA)
    exec_ = conn.execute
    exec_(
        "INSERT INTO workspaces VALUES (?,?,?,?,?)",
        ("ws1", "/work/alma", "alma", "2026-10-01T09:00:00.000Z", "2026-10-01T09:00:00.000Z"),
    )
    exec_(
        "INSERT INTO chat_threads (id, title, model, created_at, updated_at, workspace_id) VALUES (?,?,?,?,?,?)",
        ("thA", "Fix the build", "claude-subscription:claude-opus-5-5", "2026-10-01T10:00:00.000Z", "2026-10-01T10:00:30.000Z", "ws1"),
    )
    exec_(
        "INSERT INTO chat_threads (id, title, model, created_at, updated_at, workspace_id) VALUES (?,?,?,?,?,?)",
        ("thB", "New Chat", "gemini:gemini-3.1-pro-preview", "2026-10-01T11:00:00.000Z", "2026-10-01T11:00:00.000Z", None),
    )
    exec_(
        "INSERT INTO chat_threads (id, title, created_at, updated_at) VALUES (?,?,?,?)",
        ("thC", "New Chat", "2026-10-01T12:00:00.000Z", "2026-10-01T12:00:00.000Z"),
    )
    exec_(
        "INSERT INTO chat_threads (id, title, created_at, updated_at, is_incognito) VALUES (?,?,?,?,1)",
        ("thD", "Secret", "2026-10-01T13:00:00.000Z", "2026-10-01T13:00:00.000Z"),
    )
    exec_(
        "INSERT INTO chat_threads (id, title, created_at, updated_at, metadata) VALUES (?,?,?,?,?)",
        ("thE", "Daily digest", "2026-10-01T14:00:00.000Z", "2026-10-01T14:00:00.000Z", '{"isCron":true}'),
    )
    exec_(
        "INSERT INTO chat_threads (id, title, created_at, updated_at, metadata) VALUES (?,?,?,?,?)",
        ("thF", "⏰ Cron: daily digest (Retry 1)", "2026-10-01T15:00:00.000Z", "2026-10-01T15:00:00.000Z", "{}"),
    )

    def msg(mid, thread, at, message, meta, parent=""):
        exec_(
            """INSERT INTO chat_messages (id, thread_id, slot_id, depth, message, timestamp, metadata, created_at, updated_at, parent_tool_call_id)
            VALUES (?,?,?,?,?,?,?,?,?,?)""",
            (mid, thread, mid, "0.0", message, at, meta, at, at, parent or None),
        )

    msg(
        "thA--user-1",
        "thA",
        "2026-10-01T10:00:00.000Z",
        '{"id":"u1","role":"user","parts":[{"type":"text","text":"Please fix the build"}]}',
        '{"source":"app"}',
    )
    msg(
        "thB--user-1",
        "thB",
        "2026-10-01T11:00:01.000Z",
        '{"id":"u2","role":"user","parts":[{"type":"text","text":"What   is\\nWAL?"}]}',
        "{}",
    )
    msg(
        "thD--user-1",
        "thD",
        "2026-10-01T13:00:01.000Z",
        '{"id":"u3","role":"user","parts":[{"type":"text","text":"hidden"}]}',
        "{}",
    )
    exec_(
        """INSERT INTO usage_records (id, message_id, thread_id, model, provider_id, date, input_tokens, output_tokens, cached_input_tokens, cache_write_input_tokens, reasoning_tokens, total_tokens, timestamp, created_at) VALUES
        (?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
        (
            "u1",
            "thA--a1",
            "thA",
            "plugin:openai-codex-auth:openai-codex:gpt-6-astra",
            "plugin",
            "2026-10-01",
            1000,
            50,
            600,
            0,
            10,
            1050,
            "2026-10-01T10:00:05.000Z",
            "2026-10-01T10:00:05.000Z",
        ),
    )
    exec_(
        """INSERT INTO usage_records (id, message_id, thread_id, model, provider_id, date, input_tokens, output_tokens, cached_input_tokens, cache_write_input_tokens, reasoning_tokens, total_tokens, timestamp, created_at) VALUES
        (?,?,?,?,?,?,?,?,?,?,?,?,?,?)""",
        (
            "u2",
            "thA--a1",
            "thA",
            "claude-subscription:claude-opus-5-5",
            "claude-subscription",
            "2026-10-01",
            500,
            20,
            100,
            300,
            0,
            520,
            "2026-10-01T10:00:06.000Z",
            "2026-10-01T10:00:06.000Z",
        ),
    )
    exec_(
        """INSERT INTO aux_usage_records (id, purpose, model, provider_id, thread_id, date, input_tokens, output_tokens, timestamp, created_at) VALUES
        (?,?,?,?,?,?,?,?,?,?)""",
        (
            "x1",
            "title",
            "gemini:gemini-3.1-flash-lite-preview",
            "gemini",
            "thB",
            "2026-10-01",
            30,
            5,
            "2026-10-01T11:00:02.000Z",
            "2026-10-01T11:00:02.000Z",
        ),
    )
    conn.commit()
    conn.close()


if __name__ == "__main__":
    main()
