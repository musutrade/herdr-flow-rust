use super::*;
#[path = "delivery_publication.rs"]
mod publication;

fn begin(f: &Fixture, id: &str) {
    begin_scope(f, id, "goal");
}

fn begin_scope(f: &Fixture, id: &str, criterion: &str) {
    let mut s = queue::load_queue(&f.queue).unwrap();
    mutate(
        &mut s,
        json!({"kind":"stage","id":id,"scope":format!("deliver {id}"),"criteria":[criterion]}),
    );
    queue::save(&f.queue, &mut s).unwrap();
}

fn approve(f: &mut Fixture, name: &str, artifact: &J) {
    let mut s = queue::load_queue(&f.queue).unwrap();
    mutate(
        &mut s,
        json!({"kind":"candidate","candidate":artifact["id"],"reason":"independent current-scope review"}),
    );
    mutate(
        &mut s,
        json!({"kind":"activity","activity":name,"role":"acceptance","input":artifact["id"],"session_epoch":1,
        "result_wait":{"id":name,"epoch":1,"assignment":name,"owner":"reviewer","kind":"signal","timeout_seconds":30,
        "reason":"review result","condition":"done","producer":{"adapter":"codex","session":name}}}),
    );
    let checks: Vec<J> = s["tasks"][name]["acceptance"].as_object().unwrap().keys()
        .map(|id| json!({"id":id,"criterion":id,"source":"request","status":"passed","evidence":"inspected current scope"})).collect();
    let mut report = json!({"conclusion":"passed","scope":"current declared scope","checks":checks,"gaps":[],"reuses":[],"resolutions":[]});
    let reviewer = json!({"adapter":"codex","session":name});
    if checks.len() > 1 {
        report["checks"] = json!([checks[0]]);
        assert!(
            acceptance::submit(&mut s, name, &reviewer, &report).is_err(),
            "stage-only evidence must not pass overall acceptance"
        );
        report["checks"] = json!(checks);
    }
    let verdict = acceptance::submit(&mut s, name, &reviewer, &report).unwrap();
    let mut e = json!({"id":name,"task":name,"wait":name,"acknowledged":true,"outcome":"complete"});
    let wait = s["waits"][name].clone();
    progress::attach(&mut s, &mut e, &wait, 2.0).unwrap();
    s["events"][name] = e;
    let spec = json!({"task":name,"state_version":s["tasks"][name]["state_version"],"event":name,"verdict":verdict["id"],
        "next_action":{"kind":"delivery","owner":"customer","reason":"current scope passed","condition":"authorized endpoint"}});
    progress::apply(&mut s, &spec, 3.0).unwrap();
    f.spec["state_version"] = s["tasks"]["job"]["state_version"].clone();
    queue::save(&f.queue, &mut s).unwrap();
}

fn artifact(f: &Fixture) -> J {
    let s = queue::load_queue(&f.queue).unwrap();
    s["tasks"]["job"]["artifacts"][s["tasks"]["job"]["candidate"].as_str().unwrap()]["object"]
        .clone()
}

fn confirmed(f: &mut Fixture, name: &str, a: &J) -> String {
    begin_scope(
        f,
        name,
        if name == "stage-two" {
            "evidence"
        } else {
            "goal"
        },
    );
    approve(f, name, a);
    f.prepare();
    assert_eq!(
        delivery::execute(&mut f.env, &delivery::local::Local).unwrap()["phase"],
        "confirmed"
    );
    f.env.delivery["id"].as_str().unwrap().to_owned()
}

#[test]
fn two_stage_git_deliveries_keep_one_parent_then_require_fresh_overall_acceptance() {
    let mut f = Fixture::new(true);
    let instance = f.env.instance_id.clone();
    let a = artifact(&f);
    let mut q = queue::load_queue(&f.queue).unwrap();
    mutate(
        &mut q,
        json!({"kind":"revise","goal":"deliver correct content and supporting evidence",
        "basis":{"original":["request"],"changes":[],"specifications":[],"endpoint":"customer endpoint","confirmation":"user"},
        "acceptance":[{"id":"goal","description":"correct content","required":true},
            {"id":"evidence","description":"supporting evidence included","required":true}]}),
    );
    queue::save(&f.queue, &mut q).unwrap();
    let first = confirmed(&mut f, "stage-one", &a);
    assert!(delivery::finish(&mut f.env).is_err());
    let receipt = f.env.delivery["receipt"].clone();
    let checkpoint = delivery::complete_stage(&mut f.env, &f.queue, &first).unwrap();
    assert_eq!(checkpoint["phase"], "complete");
    assert_eq!(checkpoint["delivery"]["receipt"], receipt);
    let mut s = queue::load_queue(&f.queue).unwrap();
    assert_eq!(s["tasks"]["job"]["control"], "running");
    assert_eq!(s["tasks"]["job"]["scope_epoch"], 2);
    assert!(f.env.delivery.is_null());
    assert!(s["tasks"]["job"]["approved"].is_null());
    assert!(s["tasks"]["job"]["candidate"].is_null());
    assert!(!lineage::current(&s, &s["tasks"]["stage-one"]));
    f.spec["state_version"] = s["tasks"]["job"]["state_version"].clone();
    assert!(delivery::prepare(&mut f.env, &f.queue, &f.spec, &delivery::local::Local).is_err());
    std::fs::write(f.env.workspace_root.join("A"), "B").unwrap();
    git_cmd(&f.env.workspace_root, &["commit", "-am", "B"]);
    let b = json!({"adapter":"git","id":git_cmd(&f.env.workspace_root,&["rev-parse","HEAD"]),"location":f.env.workspace_root,"input":a["id"]});
    mutate(&mut s, json!({"kind":"artifact","artifact":b}));
    queue::save(&f.queue, &mut s).unwrap();
    f.spec["expected"] = a["id"].clone();
    let second = confirmed(&mut f, "stage-two", &b);
    let frozen = queue::load_queue(&f.queue).unwrap();
    let current = f.env.delivery.clone();
    // A repeated old acknowledgement cannot clear a later stage's journal.
    assert_eq!(
        delivery::complete_stage(&mut f.env, &f.queue, &first).unwrap(),
        checkpoint
    );
    assert_eq!(queue::load_queue(&f.queue).unwrap(), frozen);
    assert_eq!(f.env.delivery, current);
    delivery::complete_stage(&mut f.env, &f.queue, &second).unwrap();
    assert_eq!(f.env.instance_id, instance);
    assert!(f.env.workspace_root.exists());
    assert!(is_registered("job").unwrap());
    assert_eq!(
        git_cmd(&f.dir.path().join("customer"), &["rev-parse", "main"]),
        b["id"].as_str().unwrap()
    );
    assert!(delivery::complete_stage(&mut f.env, &f.queue, &first).is_err());
    f.spec["expected"] = b["id"].clone();
    approve(&mut f, "overall-review", &b);
    f.prepare();
    assert!(f.env.delivery["stage"].is_null());
    assert!(delivery::complete_stage(&mut f.env, &f.queue, "wrong").is_err());
    delivery::execute(&mut f.env, &delivery::local::Local).unwrap();
    delivery::finish(&mut f.env).unwrap();
    f.clean();
    let s = queue::load_queue(&f.queue).unwrap();
    for task in ["stage-one", "stage-two", "overall-review"] {
        assert!(s["tasks"][task].is_null());
    }
}

#[test]
fn stage_checkpoint_resumes_each_persisted_prefix_without_external_resubmission() {
    for prefix in 0..3 {
        let mut f = Fixture::new(false);
        let a = artifact(&f);
        let id = confirmed(&mut f, "draft", &a);
        let intent = f.env.delivery.clone();
        let mut s = queue::load_queue(&f.queue).unwrap();
        s["tasks"]["job"]["stage_delivery"] = json!({"phase":"continuing","delivery":intent});
        queue::save(&f.queue, &mut s).unwrap();
        if prefix > 0 {
            f.env.delivery = J::Null;
        }
        if prefix == 1 {
            f.env.persist().unwrap();
        }
        if prefix != 2 {
            f.env.release_ownership();
            f.env = restore_environment("job").unwrap();
        }
        // Prefix 2 retains a cleared handle with the old durable intent, as
        // after a persist error. Retrying must actually persist the empty journal.
        let checkpoint = delivery::complete_stage(&mut f.env, &f.queue, &id).unwrap();
        assert_eq!(checkpoint["delivery"], intent);
        assert_eq!(checkpoint["phase"], "complete");
        f.env.release_ownership();
        f.env = restore_environment("job").unwrap();
        assert!(f.env.delivery.is_null());
        let after = queue::load_queue(&f.queue).unwrap();
        assert_eq!(
            delivery::complete_stage(&mut f.env, &f.queue, &id).unwrap(),
            checkpoint
        );
        assert_eq!(queue::load_queue(&f.queue).unwrap(), after);
        assert_eq!(
            std::fs::read_to_string(f.dir.path().join("customer")).unwrap(),
            "A"
        );
    }
}

#[test]
fn stage_continuation_requires_observed_receipt_and_exact_frozen_scope() {
    let mut f = Fixture::new(false);
    let a = artifact(&f);
    begin(&f, "draft");
    approve(&mut f, "draft", &a);
    f.prepare();
    let id = f.env.delivery["id"].as_str().unwrap().to_owned();
    assert!(delivery::complete_stage(&mut f.env, &f.queue, &id).is_err());
    delivery::execute(&mut f.env, &Uncertain).unwrap();
    assert!(delivery::complete_stage(&mut f.env, &f.queue, &id).is_err());
    std::fs::write(f.dir.path().join("customer"), "A").unwrap();
    delivery::reconcile(&mut f.env, &delivery::local::Local).unwrap();
    let confirmed = f.env.delivery.clone();
    let s = queue::load_queue(&f.queue).unwrap();
    for field in ["receipt", "stage", "instance", "queue"] {
        f.env.delivery[field] = J::Null;
        assert!(
            delivery::complete_stage(&mut f.env, &f.queue, &id).is_err(),
            "{field}"
        );
        assert_eq!(queue::load_queue(&f.queue).unwrap(), s);
        f.env.delivery = confirmed.clone();
    }
    for field in ["stage", "approved", "delivery"] {
        let mut q = s.clone();
        q["tasks"]["job"][field] = J::Null;
        queue::save(&f.queue, &mut q).unwrap();
        assert!(
            delivery::complete_stage(&mut f.env, &f.queue, &id).is_err(),
            "{field}"
        );
        assert_eq!(f.env.delivery, confirmed);
    }
    let mut q = s;
    queue::save(&f.queue, &mut q).unwrap();
    delivery::complete_stage(&mut f.env, &f.queue, &id).unwrap();
}

#[test]
fn stage_cli_reacquires_the_same_environment_and_rejects_changed_resume_journal() {
    let mut f = Fixture::new(false);
    let a = artifact(&f);
    let id = confirmed(&mut f, "draft", &a);
    let intent = f.env.delivery.clone();
    let mut s = queue::load_queue(&f.queue).unwrap();
    s["tasks"]["job"]["stage_delivery"] = json!({"phase":"continuing","delivery":intent});
    queue::save(&f.queue, &mut s).unwrap();
    f.env.delivery["receipt"]["unexpected"] = json!(true);
    assert!(delivery::complete_stage(&mut f.env, &f.queue, &id).is_err());
    assert_eq!(queue::load_queue(&f.queue).unwrap(), s);
    assert!(delivery::complete_stage(&mut f.env, &f.queue, "different").is_err());
    f.env.delivery = intent;
    f.env.release_ownership();
    let spec = f.dir.path().join("stage-complete.json");
    util::atomic_write_json(&spec, &json!({"delivery":id})).unwrap();
    let options = [
        "stage-complete",
        "--task",
        "job",
        "--queue",
        f.queue.to_str().unwrap(),
        "--spec",
        spec.to_str().unwrap(),
    ];
    assert_eq!(delivery::cli::cli(&options.map(str::to_owned)), 0);
    f.env = restore_environment("job").unwrap();
    assert!(f.env.delivery.is_null());
    assert_eq!(
        queue::load_queue(&f.queue).unwrap()["tasks"]["job"]["stage_delivery"]["phase"],
        "complete"
    );
}
