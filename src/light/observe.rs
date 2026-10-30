use super::{
    cli_args::Invocation,
    cli_output::{self, CliResult, Failure},
    observe_cache::Cache,
    observe_notify,
    observe_source::Source,
    observe_state::Snapshot,
    observe_wire::Wire,
    Binding, Error, Store,
};
use serde_json::{json, Value};
use std::{
    path::PathBuf,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

struct Settings {
    watch: bool,
    interval: u64,
    timeout: u64,
    ttl: u64,
    samples: u64,
}
fn number(inv: &Invocation, key: &str, default: u64, min: u64, max: u64) -> CliResult<u64> {
    let value = inv
        .flags
        .get(key)
        .map(|s| s.parse::<u64>())
        .transpose()
        .map_err(|_| Failure::usage("expected positive integer"))?
        .unwrap_or(default);
    if !(min..=max).contains(&value) {
        return Err(Failure::usage(format!("{key} must be {min}..{max}")));
    }
    Ok(value)
}
fn settings(inv: &Invocation) -> CliResult<Settings> {
    let modes = ["--once", "--watch", "--show"]
        .iter()
        .filter(|k| inv.flags.contains_key(**k))
        .count();
    if modes != 1 {
        return Err(Failure::usage(
            "select exactly one of --once, --watch, --show",
        ));
    }
    let interval = number(inv, "--interval-ms", 1000, 100, 60000)?;
    let timeout = number(inv, "--timeout-ms", 500, 50, 5000)?;
    let ttl = number(inv, "--stale-ms", 15000, 100, 3600000)?;
    if ttl < interval + 2 * timeout {
        return Err(Failure::usage(
            "stale-ms must cover interval plus two request timeouts",
        ));
    }
    let watch = inv.flags.contains_key("--watch");
    if inv.flags.contains_key("--samples") && !watch {
        return Err(Failure::usage("samples is watch-only"));
    }
    Ok(Settings {
        watch,
        interval,
        timeout,
        ttl,
        samples: number(inv, "--samples", u64::MAX, 1, u64::MAX)?,
    })
}
fn selected(store: &Store, inv: &Invocation) -> CliResult<Binding> {
    let task = store.task(inv.id("--task")?)?;
    let binding = store.binding(inv.id("--binding")?)?;
    if task.project_id != inv.id("--project")? || binding.task_id != task.id {
        return Err(Error::NotFound("binding in this project/task").into());
    }
    Ok(binding)
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as u64
}
pub(super) fn run(inv: Invocation) -> CliResult<Value> {
    let settings = settings(&inv)?;
    let mut options = inv.options.clone();
    options.read_only = true;
    let store = Store::open(options)?;
    let binding = selected(&store, &inv)?;
    if inv.flags.contains_key("--show") {
        return show(&inv, &binding);
    }
    let source = source(&inv, &binding, settings.timeout)?;
    check_notification(&inv)?;
    let cache = Cache::open(&inv.options, false)?;
    let _lease = cache.lease(binding.id)?;
    let run = Run {
        inv,
        settings,
        store,
        source,
        cache,
        subscription: None,
        previous: None,
        gap: None,
    };
    run.execute()
}
fn source(inv: &Invocation, binding: &Binding, timeout: u64) -> CliResult<Source> {
    if inv.required("--service")? != binding.details.herdr_location.service {
        return Err(Failure::usage("service must match registered binding"));
    }
    let socket = PathBuf::from(inv.required("--socket")?);
    if !socket.is_absolute() {
        return Err(Failure::usage("socket must be absolute"));
    }
    Ok(Source {
        socket,
        timeout: Duration::from_millis(timeout),
    })
}
fn check_notification(inv: &Invocation) -> CliResult<()> {
    if let Some(endpoint) = inv.flags.get("--bark-endpoint") {
        if !observe_notify::valid_endpoint(endpoint) {
            return Err(Failure::usage(
                "Bark endpoint must be https://HOST/push (http loopback only for substitutes)",
            ));
        }
        if !inv.flags.contains_key("--bark-key-file") {
            return Err(Failure::usage(
                "Bark endpoint requires an external key file",
            ));
        }
    }
    Ok(())
}
fn show(inv: &Invocation, binding: &Binding) -> CliResult<Value> {
    let cache = Cache::open(&inv.options, true)?;
    let mut snapshot = cache
        .get(binding.id)?
        .ok_or(Error::NotFound("cached observation"))?;
    if snapshot.binding != *binding {
        snapshot = Snapshot::unknown(
            binding.clone(),
            now(),
            0,
            "binding_changed_since_observation",
        );
    }
    snapshot.current(now());
    snapshot.check_observer();
    Ok(
        json!({"observation":snapshot,"notifications":cache.notifications(binding.id)?,"liveness":"cached lease, not proof the observer process is alive"}),
    )
}
struct Run {
    inv: Invocation,
    settings: Settings,
    store: Store,
    source: Source,
    cache: Cache,
    subscription: Option<Wire>,
    previous: Option<Snapshot>,
    gap: Option<String>,
}
impl Run {
    fn execute(mut self) -> CliResult<Value> {
        let mut count = 0;
        loop {
            let start = Instant::now();
            let binding = selected(&self.store, &self.inv)?;
            let uninterrupted = self.subscription_ready(&binding);
            let mut snapshot = self.read(binding);
            let uninterrupted = uninterrupted
                && self
                    .subscription
                    .as_ref()
                    .and_then(|wire| wire.instance().ok())
                    == snapshot.service_instance;
            snapshot.reconcile(self.previous.as_ref(), uninterrupted);
            if let Some(gap) = self.gap.take() {
                snapshot.continuity = gap;
                snapshot.completion_transition = false;
            }
            self.cache.put(&snapshot)?;
            let notification = self.notify(&snapshot)?;
            let value = json!({"observation":snapshot,"notification":notification,"notifications":self.cache.notifications(snapshot.binding.id)?});
            count += 1;
            if !self.settings.watch {
                return Ok(value);
            }
            cli_output::write_success(value).map_err(Failure::Output)?;
            self.previous = Some(snapshot);
            if count >= self.settings.samples {
                return self.stop();
            }
            self.wait(start);
        }
    }
    fn read(&self, binding: Binding) -> Snapshot {
        let mut snapshot = if binding.details.session.is_none() {
            Snapshot::unknown(
                binding,
                now(),
                self.settings.ttl,
                "registered_session_missing",
            )
        } else {
            self.source.read(binding, now(), self.settings.ttl)
        };
        snapshot.current(now());
        if self.settings.watch {
            snapshot.observer = "watch_lease_unconfirmed".into();
            let pid = unsafe { libc::getpid() } as u32;
            snapshot.observer_process = super::observe_wire::process_start(pid)
                .ok()
                .map(|start| (pid, start));
        }
        snapshot
    }
    fn subscription_ready(&mut self, binding: &Binding) -> bool {
        if !self.settings.watch || binding.details.session.is_none() {
            return false;
        }
        if self
            .previous
            .as_ref()
            .is_some_and(|p| p.binding != *binding)
        {
            self.subscription = None;
            self.gap = Some("binding_changed".into());
        }
        if self.subscription.is_none() {
            self.subscription = self.source.subscribe(binding).ok();
            if self.subscription.is_none() {
                self.gap = Some("subscription_unavailable".into());
            }
            return false;
        }
        self.previous
            .as_ref()
            .is_some_and(|p| p.binding == *binding)
    }
    fn wait(&mut self, start: Instant) {
        let deadline = start + Duration::from_millis(self.settings.interval);
        for _ in 0..32 {
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                break;
            };
            if !self.wait_event(remaining) {
                break;
            }
        }
        if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
            std::thread::sleep(remaining);
        }
    }
    fn wait_event(&mut self, timeout: Duration) -> bool {
        let Some(wire) = self.subscription.as_mut() else {
            return false;
        };
        let reason = match wire.next(timeout) {
            Ok(None) => return false,
            Ok(Some(value)) => match value.get("error") {
                None => return true,
                Some(error) if error["code"] == "events_lost" => "events_lost",
                Some(_) => "subscription_error",
            },
            Err(_) => "disconnected",
        };
        self.gap = Some(reason.into());
        self.subscription = None;
        false
    }
    fn notify(&mut self, snapshot: &Snapshot) -> CliResult<&'static str> {
        let Some(path) = self.inv.flags.get("--bark-key-file") else {
            return Ok("not_configured");
        };
        if snapshot.observed_state != "blocked"
            || !now()
                .checked_sub(snapshot.observed_at_ms)
                .is_some_and(|age| age <= snapshot.stale_after_ms)
        {
            return Ok("not_needed");
        }
        let key = format!(
            "{:?}:{}",
            snapshot.service_instance,
            serde_json::to_string(&snapshot.binding).unwrap_or_default()
        );
        let (attempt, reason) = self.cache.reserve(snapshot.binding.id, &key, now())?;
        let Some(id) = attempt else { return Ok(reason) };
        let endpoint = self
            .inv
            .flags
            .get("--bark-endpoint")
            .map(String::as_str)
            .unwrap_or("https://api.day.app/push");
        let status = observe_notify::send(
            std::path::Path::new(path),
            endpoint,
            snapshot.binding.task_id,
            snapshot.binding.id,
        );
        self.cache.finish(id, status.status, status.reason)?;
        Ok(status.status)
    }
    fn stop(&mut self) -> CliResult<Value> {
        if let Some(snapshot) = &mut self.previous {
            snapshot.observer = "stopped".into();
            snapshot.observed_state = "unknown".into();
            snapshot.reason = "observer_stopped".into();
            snapshot.completion_transition = false;
            self.cache.put(snapshot)?;
        }
        Ok(json!({"observer":"stopped","reason":"sample_limit","last_observation":self.previous}))
    }
}
