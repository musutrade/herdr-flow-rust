"""Read-only final inventory for explicitly pinned databases and native data root.

No process/service manipulation or writes. IDs must already come from the owned
runtime and complete private cwd inventories. Missing/unsupported input fails.
"""
import json
import os
from pathlib import Path
import sqlite3
import sys
import time

spec = json.load(sys.stdin)
sessions = spec["sessions"]
assert sessions and len(sessions) <= 256
end = time.monotonic() + 30
assert 0 < len(spec["databases"]) <= 32
# Installed native schema contract (0.165.0); optional tables/columns are
# recorded when present. These are references, not arbitrary UUID-looking text.
relations = {
    "threads": {"id", "attached_thread_parent_id"},
    "thread_dynamic_tools": {"thread_id"},
    "thread_spawn_edges": {"parent_thread_id", "child_thread_id"},
    "thread_attachments": {"thread_id"},
    "guardian_review_feedback": {"thread_id"},
    "thread_read_receipts": {"thread_id"},
    "thread_turns": {"thread_id"},
    "thread_items": {"thread_id"},
    "thread_history_projection_state": {"thread_id"},
    "thread_realtime_items": {"thread_id"},
    "logs": {"thread_id"},
    "stage1_outputs": {"thread_id"},
    "queued_items": {"thread_id"},
    "queued_thread_revisions": {"thread_id"},
    "thread_goals": {"thread_id"},
    "thread_goal_continuation_deferrals": {"thread_id"},
}
# rollout_migration_state.last_checked_thread_id is a cursor; jobs.ownership_token
# is not a native thread identity. Neither is task deletion authority.
results = []
for raw in spec["databases"]:
    entry = {"path": raw, "json_relations": []} if isinstance(raw, str) else raw
    path = Path(entry["path"])
    assert path.is_absolute() and path.is_file() and path.resolve() == path
    db = sqlite3.connect(path.as_uri() + "?mode=ro", uri=True, timeout=2)
    db.execute("PRAGMA query_only=ON")
    db.set_progress_handler(lambda: int(time.monotonic() > end), 10000)
    tables = db.execute("SELECT name FROM sqlite_master WHERE type='table'").fetchall()
    assert len(tables) <= 256
    checks = []
    schema = {}
    quote = lambda name: '"' + name.replace('"', '""') + '"'
    for (table,) in tables:
        if table.startswith("sqlite_"):
            continue
        columns = db.execute("PRAGMA table_info(" + quote(table) + ")").fetchall()
        assert len(columns) <= 256
        schema[table] = [column[1] for column in columns]
        for column in columns:
            name = column[1]
            # Exact typed relations only. Arbitrary log/message text mentioning a
            # task UUID remains outside task ownership and must never be deleted.
            if name not in relations.get(table, set()):
                continue
            matches = sum(db.execute("SELECT count(*) FROM " + quote(table) + " WHERE " + quote(name) + " = ?", (session,)).fetchone()[0] for session in sessions)
            checks.append({"table": table, "column": name, "relation": "exact native session ID", "matches": matches})
            assert matches == 0, f"owned session association remains: {path}:{table}:{name}"
    # JSON relations require an explicitly reviewed schema/path contract. Do not
    # scan freeform serialized messages for UUID substrings.
    for relation in entry.get("json_relations", []):
        assert relation["meaning"] == "native-session-association" and relation["source"]
        table, column, json_path = relation["table"], relation["column"], relation["path"]
        assert table in schema and column in schema[table]
        assert json_path.startswith("$.") and "*" not in json_path
        query = "SELECT count(*) FROM " + quote(table) + " WHERE json_extract(" + quote(column) + ", ?) = ?"
        matches = sum(db.execute(query, (json_path, session)).fetchone()[0] for session in sessions)
        checks.append({"table": table, "column": column, "path": json_path, "relation": "configured exact JSON session association", "matches": matches})
        assert matches == 0, f"owned session association remains: {path}:{table}:{column}:{json_path}"
    assert time.monotonic() < end, "SQL inventory budget exhausted"
    results.append({"database": str(path), "query_only": True, "schema": schema, "typed_association_checks": checks})
    db.close()

roots = [Path(spec["native_data"]) / name for name in ("sessions", "archived_sessions", "shell_snapshots")]
store = Path(spec["herdr_store"])
files = [store]
for name in ("session-snapshots", "session-backups"):
    root = store.parent / name
    if root.exists():
        for path in root.rglob("*"):
            assert len(files) <= 1024 and time.monotonic() < end, "Herdr saved inventory bound"
            if path.is_file():
                files.append(path)
visited = 0
for root in roots:
    if not root.exists():
        continue
    for directory, _, names in os.walk(root):
        for name in names:
            visited += 1
            assert visited <= 100000 and time.monotonic() < end, "artifact inventory bound"
            owned_name = any((name == f"{session}.sh" if root.name == "shell_snapshots" else name.startswith("rollout-") and name.endswith(f"-{session}.jsonl")) for session in sessions)
            assert not owned_name, f"owned rollout/shell artifact remains: {directory}/{name}"
for path in files:
    assert path.stat().st_size <= 32 * 1024 * 1024
    value = json.loads(path.read_bytes())
    assert value["version"] == 3 and isinstance(value["workspaces"], list), "unsupported Herdr saved schema"
    for workspace in value["workspaces"]:
        for tab in workspace["tabs"]:
            for pane in tab["panes"].values():
                association = pane.get("agent_session")
                if association and association.get("agent") == "codex":
                    assert association.get("value") not in sessions, f"owned Herdr saved association remains: {path}"
print(json.dumps({"databases": results, "artifact_roots": [str(p) for p in roots], "files_visited": visited, "herdr_saved_files": [str(p) for p in files], "owned_associations_absent": True}))
