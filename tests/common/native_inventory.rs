//! Observations for one real delivery/restart round; campaign proof is separate.
use super::resource_inventory::{collect, InventorySpec};
use herdr_flow::{
    model::J,
    task_env::{TaskEnvironment, WorktreeRegistration},
};
use serde_json::json;
use std::{
    fs,
    path::{Path, PathBuf},
};

pub struct NativeInventory {
    base: PathBuf,
    queue: PathBuf,
    registry: PathBuf,
    task: String,
    instance: String,
    workspace: PathBuf,
    temporary: PathBuf,
    runtimes: Vec<PathBuf>,
    worktrees: Vec<WorktreeRegistration>,
    open_files: Vec<PathBuf>,
    excluded: Vec<PathBuf>,
    expected_tasks: Vec<String>,
    notifications: usize,
}

impl NativeInventory {
    pub fn new(
        env: &TaskEnvironment,
        queue: &Path,
        base: &Path,
        excluded: Vec<PathBuf>,
        runtimes: Vec<PathBuf>,
        mut expected_tasks: Vec<String>,
        notifications: usize,
    ) -> Self {
        expected_tasks.sort();
        Self {
            base: base.into(),
            queue: queue.into(),
            registry: base.join("registry"),
            task: env.task_id.clone(),
            instance: env.instance_id.clone(),
            workspace: env.workspace_root.clone(),
            temporary: env.temp_root.clone(),
            runtimes,
            worktrees: env.worktree_registrations.clone(),
            open_files: env.open_files.clone(),
            excluded,
            expected_tasks,
            notifications,
        }
    }

    fn report(&self, stage: &str, expected: &J) -> J {
        let envs = self.registry.join("envs");
        let plans = self.registry.join("plans");
        let mut report = collect(&InventorySpec {
            base: &self.base,
            queue: &self.queue,
            registry_root: &self.registry,
            envs_dir: &envs,
            plans_dir: &plans,
            task_id: &self.task,
            instance_id: &self.instance,
            workspace_root: &self.workspace,
            temp_root: &self.temporary,
            runtime_roots: &self.runtimes,
            worktrees: &self.worktrees,
            open_files: &self.open_files,
            excluded: &self.excluded,
        });
        report["expected_identity"] = expected.clone();
        let bytes = serde_json::to_vec_pretty(&report).unwrap();
        assert!(bytes.len() <= 1024 * 1024, "inventory report exceeds 1MiB");
        fs::write(self.base.join(format!("inventory-{stage}.json")), bytes).unwrap();
        report
    }

    fn queue_keys(&self) -> J {
        let state: J =
            serde_json::from_slice(&fs::read(self.queue.join("queue.json")).unwrap()).unwrap();
        let mut keys = json!({});
        for section in ["tasks", "waits", "events", "deliveries"] {
            keys[section] = json!(state[section]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>());
        }
        keys
    }

    pub fn before(&self, confirmed: &J) -> J {
        assert_eq!(confirmed["phase"], "confirmed");
        assert!(confirmed["id"].as_str().is_some_and(|id| !id.is_empty()));
        assert_eq!(confirmed["artifact"]["id"], confirmed["receipt"]["commit"]);
        let expected = json!({"candidate": confirmed["artifact"]["id"],
            "intent": confirmed["id"], "receipt": confirmed["receipt"],
            "queue_keys": self.queue_keys()});
        self.before_expected(expected)
    }

    pub fn before_unknown(&self, journal: &J) -> J {
        assert_eq!(journal["phase"], "unknown");
        assert!(journal["receipt"].is_null());
        assert!(journal["id"].as_str().is_some_and(|id| !id.is_empty()));
        assert!(journal["artifact"]["id"]
            .as_str()
            .is_some_and(|id| !id.is_empty()));
        let expected = json!({"candidate": journal["artifact"]["id"],
            "intent": journal["id"], "receipt": J::Null,
            "observation": "unknown", "queue_keys": self.queue_keys()});
        self.before_expected(expected)
    }

    fn before_expected(&self, expected: J) -> J {
        let r = self.report("before", &expected);
        let structural_errors: Vec<_> = r["errors"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|error| {
                !error
                    .as_str()
                    .is_some_and(|text| text.starts_with("process "))
            })
            .collect();
        assert!(
            structural_errors.is_empty(),
            "live structural inventory errors: {structural_errors:?}"
        );
        assert_eq!(r["registry"]["entry"]["instance"], self.instance);
        assert_eq!(r["registry"]["entry"]["task"], self.task);
        assert_eq!(
            r["registry"]["entry"]["workspace_root"],
            json!(self.workspace)
        );
        assert_eq!(r["registry"]["entry"]["temp_root"], json!(self.temporary));
        assert_eq!(r["registry"]["occupied_slots"], 1);
        assert_eq!(r["roots"]["workspace"]["exists"], true);
        assert_eq!(r["roots"]["temp"]["exists"], true);
        assert_eq!(r["state"]["exists"], true);
        assert_eq!(r["cleanup"]["exists"], false);
        assert_eq!(
            r["runtime_roots"].as_array().unwrap().len(),
            self.runtimes.len()
        );
        assert!(r["runtime_roots"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["exists"] == true));
        assert!(r["processes"]["count"].as_u64().unwrap() > 0);
        let q = &r["queue_entries"];
        assert_eq!(q["owned"]["tasks"]["keys"], json!(self.expected_tasks));
        for (section, count) in [
            ("tasks", self.expected_tasks.len()),
            ("waits", self.notifications),
            ("events", self.notifications),
            ("deliveries", self.notifications),
        ] {
            assert_eq!(
                q["owned"][section]["total"],
                json!(count),
                "owned {section}"
            );
            assert_eq!(q["owned"][section]["truncated"], false);
            assert_eq!(
                q["owned"][section]["keys"], expected["queue_keys"][section],
                "exact {section} identities"
            );
            assert_eq!(q["unknown"][section]["total"], 0, "unknown {section}");
            assert_eq!(q["unrelated"][section]["total"], 0, "unrelated {section}");
        }
        assert_eq!(q["mixed"]["deliveries"]["total"], 0);
        // Per-process completeness and outside paths remain in the report;
        // a momentary sample is not evidence of complete process discovery.
        eprintln!(
            "native inventory before: {} marked process samples",
            r["processes"]["count"]
        );
        expected
    }

    pub fn after(&self, expected: &J) {
        assert_eq!(
            fs::symlink_metadata(&self.queue).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        let r = self.report("after", expected);
        let errors = r["errors"].as_array().unwrap();
        assert_eq!(
            errors.len(),
            1,
            "unexpected post-finish observations: {errors:?}"
        );
        let prefix = format!("queue read {}:", self.queue.join("queue.json").display());
        assert!(errors[0].as_str().unwrap().starts_with(&prefix));
        assert!(r["queue_entries"]["error"].is_string());
        assert_eq!(
            errors[0],
            format!("{prefix} {}", r["queue_entries"]["error"].as_str().unwrap())
        );
        assert_eq!(r["registry"]["registered"], false);
        assert_eq!(r["registry"]["entry"], J::Null);
        assert_eq!(r["registry"]["occupied_slots"], 0);
        for section in ["state", "cleanup"] {
            assert_eq!(r[section]["exists"], false);
        }
        for root in ["workspace", "temp"] {
            assert_eq!(r["roots"][root]["exists"], false);
        }
        assert_eq!(r["processes"]["count"], 0);
        assert_eq!(r["processes"]["truncated"], false);
        assert_eq!(r["base_usage"]["truncated"], false);
        for section in ["runtime_roots", "open_files"] {
            assert!(r[section]
                .as_array()
                .unwrap()
                .iter()
                .all(|v| v["exists"] == false));
        }
        for tree in r["worktrees"].as_array().unwrap() {
            assert_eq!(tree["path"]["exists"], false);
        }
        eprintln!("native inventory after: owned paths absent, zero marked matches; discovery limitations retained");
    }
}
