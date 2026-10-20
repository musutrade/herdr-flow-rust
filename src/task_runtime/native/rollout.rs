//! Exact-turn recovery for runtimes which do not implement paginated history.
use super::*;
use std::io::{BufRead, BufReader};

pub(super) fn read(root: &Path, state: &J, turn: &J) -> Result<Option<J>> {
    let file = open_rollout(root, state)?;
    let mut scan = Scan {
        state,
        turn,
        session: false,
        context: false,
        result: None,
    };
    for line in BufReader::new(file).split(b'\n') {
        let bytes = line?;
        if bytes.is_empty() {
            continue;
        }
        let Ok(row) = serde_json::from_slice::<J>(&bytes) else {
            break;
        };
        scan.row(&row)?;
    }
    ensure(scan.session, "Codex rollout session metadata missing")?;
    Ok(scan.result)
}
fn open_rollout(root: &Path, state: &J) -> Result<std::fs::File> {
    let path = PathBuf::from(required(state, "transcript")?);
    let path = util::resolve_path(&path);
    ensure(
        util::is_relative_to(&path, &root.join("c")),
        "Codex rollout escaped private home",
    )?;
    let file = std::fs::File::open(path)?;
    ensure(
        file.metadata()?.len() <= 16 * 1024 * 1024,
        "Codex rollout exceeds collection bound",
    )?;
    Ok(file)
}

struct Scan<'a> {
    state: &'a J,
    turn: &'a J,
    session: bool,
    context: bool,
    result: Option<J>,
}
impl Scan<'_> {
    fn row(&mut self, row: &J) -> Result<()> {
        let payload = &row["payload"];
        match row["type"].as_str() {
            Some("session_meta") => self.metadata(payload),
            Some("turn_context") => self.context(payload),
            Some("event_msg") => self.event(payload),
            _ => Ok(()),
        }
    }
    fn metadata(&mut self, value: &J) -> Result<()> {
        ensure(
            value["id"] == self.state["session"] && value["cwd"] == self.state["cwd"],
            "Codex rollout belongs to another session/workspace",
        )?;
        self.session = true;
        Ok(())
    }
    fn context(&mut self, value: &J) -> Result<()> {
        if value["turn_id"] != *self.turn {
            return Ok(());
        }
        ensure(
            value["cwd"] == self.state["cwd"] && value["model"] == self.state["model"]["id"],
            "Codex turn model/directory changed",
        )?;
        self.context = true;
        Ok(())
    }
    fn event(&mut self, value: &J) -> Result<()> {
        if value["turn_id"] != *self.turn {
            return Ok(());
        }
        let status = match value["type"].as_str() {
            Some("task_complete") => "completed",
            Some("turn_aborted") => "interrupted",
            _ => return Ok(()),
        };
        ensure(
            self.session && self.context,
            "Codex terminal event lacks exact session/turn context",
        )?;
        self.result = Some(
            json!({"id":self.turn,"status":status,"items":[{"type":"agentMessage","text":value["last_agent_message"]}],"source":"native_rollout"}),
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_an_exact_session_model_and_terminal_turn_can_finish() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("c")).unwrap();
        let path = root.join("c/rollout.jsonl");
        let state = json!({"session":"s","cwd":"/work","model":{"id":"m"},"transcript":path});
        let rows = [
            json!({"type":"session_meta","payload":{"id":"s","cwd":"/work"}}),
            json!({"type":"turn_context","payload":{"turn_id":"t","cwd":"/work","model":"m"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"t","last_agent_message":"verified"}}),
        ];
        let text = rows.iter().map(J::to_string).collect::<Vec<_>>().join("\n");
        std::fs::write(&path, &text).unwrap();
        assert_eq!(
            read(root, &state, &json!("t")).unwrap().unwrap()["status"],
            "completed"
        );
        assert!(read(root, &state, &json!("other")).unwrap().is_none());
        std::fs::write(&path, format!("{}\n{{", rows[0])).unwrap();
        assert!(read(root, &state, &json!("t")).unwrap().is_none());
        for wrong in [
            text.replace("\"s\"", "\"wrong\""),
            text.replace("\"m\"", "\"other-model\""),
            format!("{}\n{}", rows[0], rows[2]),
        ] {
            std::fs::write(&path, wrong).unwrap();
            assert!(read(root, &state, &json!("t")).is_err());
        }
        std::fs::write(&path, text.replace("task_complete", "turn_aborted")).unwrap();
        assert_eq!(
            read(root, &state, &json!("t")).unwrap().unwrap()["status"],
            "interrupted"
        );
        std::fs::write(&path, text.replace("task_complete", "token_count")).unwrap();
        assert!(read(root, &state, &json!("t")).unwrap().is_none());
        let mut foreign = state.clone();
        foreign["transcript"] = json!(root.join("outside"));
        assert!(read(root, &foreign, &json!("t")).is_err());
    }
}
