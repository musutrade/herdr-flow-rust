//! Host-only child registration for the exact independently accepted delivery.
use super::*;

pub(crate) fn register(s: &mut J, name: &str, request: &J, epoch: i64, now: f64) -> Result<J> {
    let spec = json!({"task":name,"state_version":request["state_version"]});
    let parent = load_parent(s, &spec)?;
    crate::delivery::validate_approval(&parent)?;
    let id = field(request, "id")?;
    let activity = format!("publish-{id}");
    let input = parent["candidate"].clone();
    let artifact = artifact_record(&parent, &input)?;
    let mut link = activity_link(&parent, &json!({"session_epoch":1}), "publication", &input)?;
    link["round"] = parent["acceptance_round"].clone();
    link["approval"] = parent["approved"].clone();
    link["delivery"] = json!(id);
    let wait = host_wait(&activity, artifact, &link, epoch);
    let child_spec = json!({"activity":activity});
    let child = register_activity(s, &child_spec, parent, wait, link, now)?;
    // CI duration never converts a pending publication into a business failure.
    s["waits"][&activity]["deadline"] = J::Null;
    Ok(child)
}

fn host_wait(name: &str, artifact: &J, link: &J, epoch: i64) -> J {
    json!({"id":name,"task":name,"assignment":name,"epoch":epoch,
        "owner":"publisher","kind":"signal","timeout_seconds":86400,
        "reason":"authorized remote publication","condition":"host-verified publication result",
        "candidate":artifact["id"],"artifact_adapter":artifact["adapter"],"lineage":link})
}

pub(super) fn current(parent: &J, child: &J, link: &J) -> bool {
    let name = child["task"].as_str().unwrap_or_default();
    parent["activities"][name] == *link
        && parent["candidate"] == child["candidate"]
        && parent["acceptance_round"] == link["round"]
        && parent["approved"] == link["approval"]
        && !link["approval"].is_null()
        && delivery_current(parent, link)
}

fn delivery_current(parent: &J, link: &J) -> bool {
    if parent["control"] == "delivering" {
        return parent["delivery"] == link["delivery"] && !link["delivery"].is_null();
    }
    parent["control"] == "running" && parent["delivery"].is_null()
}

/// A publisher holds its target through postmerge policy and host confirmation.
/// Ordinary PR owners retain their existing confirmed-landing release boundary.
pub(crate) fn delivery_confirmed(child: &J) -> bool {
    if child["lineage"]["role"] != "publication" {
        return true;
    }
    let saved = &child["publication_delivery"];
    saved["phase"] == "confirmed"
        && saved["id"] == child["lineage"]["delivery"]
        && saved["task"] == child["task"]
        && saved["requirement"] == child["lineage"]["requirement"]
        && saved["id"].as_str().is_some_and(|id| !id.is_empty())
        && receipt_matches(&saved["receipt"], &child["pr"]["landing"])
}

pub(crate) fn receipt_matches(receipt: &J, landing: &J) -> bool {
    ["head", "commit", "repo", "base_ref", "number"]
        .iter()
        .all(|key| receipt.get(key).is_some() && receipt[key] == landing[key])
}
