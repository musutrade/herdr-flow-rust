//! Explicit live campaigns. Each child has its own process environment and queue.
//! Failures stop a sequential campaign; never replay an ambiguous task.
use super::*;
use std::time::Instant;

const CASE: &str = "upstream_live_review::live_upstream_coordinator_full_task";

#[test]
#[ignore = "explicit existing endpoint/models; twenty consecutive real local tasks"]
fn twenty_consecutive_real_tasks_leave_no_task_resources() {
    let base = common::tempdir().unwrap().keep();
    eprintln!("sequential campaign evidence: {}", base.display());
    let mut results = Vec::new();
    for index in 0..20 {
        let start = Instant::now();
        let mut child = launch(&base, index);
        let status = child.wait().unwrap();
        results.push(json!({"index":index,"success":status.success(),"seconds":start.elapsed().as_secs_f64()}));
        util::atomic_write_json(
            &base.join("campaign.json"),
            &json!({"requested":20,"results":results}),
        )
        .unwrap();
        assert!(
            status.success(),
            "round {index} failed; preserve its state, no retry"
        );
    }
}

#[test]
#[ignore = "explicit existing endpoint/models; two real tasks in independent queues sharing only upstream"]
fn two_independent_real_queues_finish_without_cross_cleanup() {
    let base = common::tempdir().unwrap().keep();
    eprintln!("parallel campaign evidence: {}", base.display());
    let start = Instant::now();
    let mut first = launch(&base, 0);
    let mut second = launch(&base, 1);
    let a = first.wait().unwrap();
    let first_seconds = start.elapsed().as_secs_f64();
    let b = second.wait().unwrap();
    util::atomic_write_json(
        &base.join("campaign.json"),
        &json!({
            "scope":"two independent queues; not shared-queue claim backpressure",
            "first_success":a.success(),"second_success":b.success(),
            "first_seconds":first_seconds,"total_seconds":start.elapsed().as_secs_f64()
        }),
    )
    .unwrap();
    assert!(
        a.success() && b.success(),
        "preserve failed children; no retry"
    );
}

fn launch(base: &Path, index: usize) -> std::process::Child {
    let log = fs::File::create(base.join(format!("round-{index:02}.log"))).unwrap();
    Command::new(std::env::current_exe().unwrap())
        .args([CASE, "--exact", "--ignored", "--nocapture"])
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .spawn()
        .unwrap()
}
