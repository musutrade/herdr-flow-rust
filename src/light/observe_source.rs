use super::{
    observe_state::{populate, Snapshot},
    observe_wire::Wire,
    Binding,
};
use serde_json::{json, Value};
use std::{io, path::PathBuf, time::Duration};

pub(super) struct Source {
    pub socket: PathBuf,
    pub timeout: Duration,
}
impl Source {
    fn call(&self, method: &str, params: Value) -> io::Result<(String, Value)> {
        let mut wire = Wire::connect(&self.socket, self.timeout)?;
        let instance = wire.instance()?;
        let value = wire.request(method, params)?;
        let result = response(value)?;
        Ok((instance, result))
    }
    pub fn read(&self, binding: Binding, now: u64, ttl: u64) -> Snapshot {
        let mut snapshot = Snapshot::unknown(binding, now, ttl, "source_unavailable");
        if !qualified_location(&snapshot.binding) {
            snapshot.reason = "registered_location_not_fully_qualified".into();
            return snapshot;
        }
        if self.fill(&mut snapshot).is_err() {
            snapshot.observed_state = "unknown".into();
            snapshot.reason = "source_unavailable_or_changed".into();
        }
        snapshot
    }
    fn fill(&self, snapshot: &mut Snapshot) -> io::Result<()> {
        let (instance, pong) = self.call("ping", json!({}))?;
        snapshot.server_version = pong["version"].as_str().map(str::to_owned);
        let (current, info) = self.call(
            "agent.get",
            json!({"target":snapshot.binding.details.herdr_location.pane}),
        )?;
        if instance != current {
            return Err(io::ErrorKind::Interrupted.into());
        }
        snapshot.service_instance = Some(instance);
        populate(snapshot, &info["agent"]);
        Ok(())
    }
    pub fn subscribe(&self, binding: &Binding) -> io::Result<Wire> {
        if !qualified_location(binding) {
            return Err(io::ErrorKind::InvalidInput.into());
        }
        let mut wire = Wire::connect(&self.socket, self.timeout)?;
        wire.instance()?;
        let reply=response(wire.request("events.subscribe",json!({"subscriptions":[{"type":"pane.agent_status_changed","pane_id":binding.details.herdr_location.pane}]}))?)?;
        if reply["type"] != "subscription_started" {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(wire)
    }
}
fn response(value: Value) -> io::Result<Value> {
    if value["id"] != "light-observe" || value.get("error").is_some() {
        return Err(io::ErrorKind::InvalidData.into());
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| io::ErrorKind::InvalidData.into())
}

fn qualified_location(binding: &Binding) -> bool {
    let location = &binding.details.herdr_location;
    let Some((workspace, pane)) = location.pane.split_once(':') else {
        return false;
    };
    !workspace.is_empty()
        && workspace == location.workspace
        && !pane.is_empty()
        && !pane.contains(':')
}
