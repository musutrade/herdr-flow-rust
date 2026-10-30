use super::{
    cli_output::{CliResult, Failure},
    Options, Page,
};
use std::{collections::BTreeMap, path::PathBuf, time::Duration};

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) enum Command {
    Init,
    Tui,
    Observe,
    Register,
    Projects,
    Add,
    Report,
    Tasks,
    Task,
    Attach,
    Bindings,
    Backup,
}

pub(super) struct Invocation {
    pub command: Command,
    pub options: Options,
    pub flags: BTreeMap<String, String>,
}
impl Invocation {
    pub fn required(&self, key: &str) -> CliResult<&str> {
        self.flags
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| Failure::usage(format!("missing {key}")))
    }
    pub fn id(&self, key: &str) -> CliResult<i64> {
        let value = number(self.required(key)?)?;
        if value <= 0 {
            return Err(Failure::usage(format!("{key} must be positive")));
        }
        Ok(value)
    }
    pub fn page(&self) -> CliResult<Page> {
        let after_id = self
            .flags
            .get("--after-id")
            .map(|v| number(v))
            .transpose()?
            .unwrap_or(0);
        let limit = self
            .flags
            .get("--limit")
            .map(|v| number(v))
            .transpose()?
            .unwrap_or(100);
        if after_id < 0 || !(1..=100).contains(&limit) {
            return Err(Failure::usage(
                "invalid page; after-id >= 0 and limit 1..100 required",
            ));
        }
        Ok(Page {
            after_id,
            limit: limit as u32,
        })
    }
}

pub(super) fn parse(args: &[String]) -> CliResult<Invocation> {
    let (words, mut flags, forbidden) = tokens(args)?;
    let command = command(&words)?;
    let options = storage(&mut flags, forbidden)?;
    for key in flags.keys() {
        if !allowed(command).contains(&key.as_str()) {
            return Err(Failure::usage(format!(
                "unsupported option {key} for this command"
            )));
        }
    }
    Ok(Invocation {
        command,
        options,
        flags,
    })
}

type Tokens = (Vec<String>, BTreeMap<String, String>, Vec<PathBuf>);
fn tokens(args: &[String]) -> CliResult<Tokens> {
    let mut words = Vec::new();
    let mut flags = BTreeMap::new();
    let mut forbidden = Vec::new();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if !arg.starts_with("--") {
            words.push(arg.clone());
            continue;
        }
        if ["--once", "--watch", "--show"].contains(&arg.as_str()) {
            insert(&mut flags, &mut forbidden, arg, "true")?;
            continue;
        }
        let value = iter
            .next()
            .ok_or_else(|| Failure::usage(format!("missing value for {arg}")))?;
        insert(&mut flags, &mut forbidden, arg, value)?;
    }
    Ok((words, flags, forbidden))
}
fn insert(
    flags: &mut BTreeMap<String, String>,
    forbidden: &mut Vec<PathBuf>,
    key: &str,
    value: &str,
) -> CliResult<()> {
    if key == "--forbid" {
        forbidden.push(PathBuf::from(value));
        return Ok(());
    }
    if flags.insert(key.into(), value.into()).is_some() {
        return Err(Failure::usage(format!("duplicate option {key}")));
    }
    Ok(())
}
fn command(words: &[String]) -> CliResult<Command> {
    let words: Vec<_> = words.iter().map(String::as_str).collect();
    match words.as_slice() {
        ["init"] => Ok(Command::Init),
        ["tui"] => Ok(Command::Tui),
        ["observe"] => Ok(Command::Observe),
        ["backup"] => Ok(Command::Backup),
        [group, action] => grouped(group, action),
        _ => Err(Failure::usage("unknown light command; use light --help")),
    }
}
fn grouped(group: &str, action: &str) -> CliResult<Command> {
    let routes = [
        ("project", "register", Command::Register),
        ("project", "list", Command::Projects),
        ("task", "add", Command::Add),
        ("task", "report", Command::Report),
        ("task", "list", Command::Tasks),
        ("task", "show", Command::Task),
        ("binding", "attach", Command::Attach),
        ("binding", "show", Command::Bindings),
    ];
    routes
        .iter()
        .find(|(g, a, _)| *g == group && *a == action)
        .map(|(_, _, c)| *c)
        .ok_or_else(|| Failure::usage("unknown light command; use light --help"))
}
fn allowed(command: Command) -> &'static [&'static str] {
    match command {
        Command::Init => &[],
        Command::Tui => &["--project", "--batch", "--limit"],
        Command::Observe => &[
            "--once",
            "--watch",
            "--show",
            "--project",
            "--task",
            "--binding",
            "--socket",
            "--service",
            "--interval-ms",
            "--timeout-ms",
            "--stale-ms",
            "--samples",
            "--bark-key-file",
            "--bark-endpoint",
        ],
        Command::Register => &["--root", "--label"],
        Command::Projects => &["--after-id", "--limit"],
        Command::Add | Command::Report | Command::Attach => &["--input"],
        Command::Tasks => &["--project", "--batch", "--after-id", "--limit"],
        Command::Task | Command::Bindings => &["--project", "--task", "--after-id", "--limit"],
        Command::Backup => &["--output-root"],
    }
}
fn number(s: &str) -> CliResult<i64> {
    s.parse().map_err(|_| Failure::usage("expected an integer"))
}
fn storage(flags: &mut BTreeMap<String, String>, forbidden: Vec<PathBuf>) -> CliResult<Options> {
    let root = match flags.remove("--data-root") {
        Some(v) => PathBuf::from(v),
        None => default_root()?,
    };
    if root.as_os_str().is_empty() {
        return Err(Failure::usage("data root must not be empty"));
    }
    let timeout = flags
        .remove("--busy-timeout-ms")
        .map(|s| number(&s))
        .transpose()?
        .unwrap_or(250);
    if !(0..=5000).contains(&timeout) {
        return Err(Failure::usage("busy-timeout-ms must be 0..5000"));
    }
    let mut options = Options::new(root);
    options.forbidden_directories = forbidden;
    options.busy_timeout = Duration::from_millis(timeout as u64);
    Ok(options)
}
fn default_root() -> CliResult<PathBuf> {
    if let Some(value) = std::env::var_os("XDG_STATE_HOME").filter(|v| !v.is_empty()) {
        return absolute(PathBuf::from(value)).map(|p| p.join("herdr-light"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| Failure::usage("set --data-root or an absolute HOME"))?;
    Ok(absolute(PathBuf::from(home))?.join(".local/state/herdr-light"))
}
fn absolute(p: PathBuf) -> CliResult<PathBuf> {
    if !p.is_absolute() {
        return Err(Failure::usage("HOME/XDG_STATE_HOME must be absolute"));
    }
    Ok(p)
}
