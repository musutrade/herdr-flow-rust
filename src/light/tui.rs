//! Line-input TUI: retain canonical terminal editing and never change termios/screens.
use super::{
    cli_args::Invocation,
    cli_output::{CliResult, Failure},
    tui_data::{Browser, View},
    tui_view, Options,
};
use std::io::{self, BufRead, IsTerminal, Read, Write};

pub(super) fn run(inv: Invocation) -> CliResult<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(Failure::usage(
            "tui requires terminal stdin/stdout; use record JSON commands for pipes",
        ));
    }
    let mut page = inv.page()?;
    if !inv.flags.contains_key("--limit") {
        page.limit = 10;
    }
    let project = inv
        .flags
        .get("--project")
        .map(|_| inv.id("--project"))
        .transpose()?;
    let mut browser = Browser {
        view: View::Tasks,
        project,
        batch: inv.flags.get("--batch").cloned(),
        task: None,
        page,
        history: Vec::new(),
        ids: Vec::new(),
        next: None,
    };
    let mut input = io::stdin().lock();
    let mut output = io::stdout().lock();
    writeln!(
        output,
        "Herdr light terminal browser. Local data only; type help for commands."
    )
    .map_err(Failure::Output)?;
    interaction(&mut browser, &inv.options, &mut input, &mut output)
}
fn interaction(
    browser: &mut Browser,
    options: &Options,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> CliResult<()> {
    loop {
        let frame = frame(browser, options);
        write!(output, "{frame}light> ")
            .and_then(|_| output.flush())
            .map_err(Failure::Output)?;
        let Some(line) = input_line(input)? else {
            return Ok(());
        };
        if !respond(browser, line.trim(), output)? {
            return Ok(());
        }
    }
}
fn frame(browser: &mut Browser, options: &Options) -> String {
    match browser.load(options) {
        Ok(data) => tui_view::render(browser, &data),
        Err(error) => format!("\nData error: {}\nNothing initialized or repaired. refresh retries reading; quit exits.\n", tui_view::safe(&error.to_string())),
    }
}
fn input_line(input: &mut impl BufRead) -> CliResult<Option<String>> {
    let mut line = String::new();
    let n = input.take(4097).read_line(&mut line)?;
    if n == 0 {
        return Ok(None);
    }
    if n > 4096 {
        return Err(Failure::usage("input line exceeds 4096 bytes"));
    }
    Ok(Some(line))
}
fn respond(browser: &mut Browser, line: &str, output: &mut impl Write) -> CliResult<bool> {
    match command(browser, line) {
        Ok(false) => {
            writeln!(output, "Browser closed.").map_err(Failure::Output)?;
            return Ok(false);
        }
        Ok(true) => {}
        Err(message) => writeln!(output, "Input error: {message}").map_err(Failure::Output)?,
    }
    if line == "help" {
        writeln!(output, "{}", tui_view::HELP).map_err(Failure::Output)?;
    }
    Ok(true)
}
fn command(b: &mut Browser, line: &str) -> Result<bool, &'static str> {
    let (cmd, arg) = line.split_once(' ').unwrap_or((line, ""));
    let arg = arg.trim();
    if !arg.is_empty() {
        return argument(b, cmd, arg).map(|_| true);
    }
    match cmd {
        "quit" | "q" => Ok(false),
        "projects" => {
            b.reset(View::Projects);
            Ok(true)
        }
        "tasks" => {
            b.reset(View::Tasks);
            Ok(true)
        }
        "refresh" | "r" | "help" | "" => Ok(true),
        _ => navigation(b, cmd).map(|_| true),
    }
}
fn navigation(b: &mut Browser, cmd: &str) -> Result<(), &'static str> {
    match cmd {
        "events" => detail(b, View::Events),
        "bindings" => detail(b, View::Bindings),
        "next" | "n" => next(b),
        "prev" | "p" => previous(b),
        _ => Err("unknown/incomplete command; type help"),
    }
}
fn argument(b: &mut Browser, cmd: &str, arg: &str) -> Result<(), &'static str> {
    match cmd {
        "project" => project(b, arg),
        "batch" => batch(b, arg),
        "open" => open(b, id(arg)?),
        _ => Err("unexpected argument; type help"),
    }
}
fn project(b: &mut Browser, arg: &str) -> Result<(), &'static str> {
    b.project = if arg == "all" { None } else { Some(id(arg)?) };
    b.task = None;
    b.reset(View::Tasks);
    Ok(())
}
fn batch(b: &mut Browser, arg: &str) -> Result<(), &'static str> {
    if arg.len() > 256 {
        return Err("batch must be at most 256 bytes");
    }
    b.batch = if arg == "-" { None } else { Some(arg.into()) };
    b.task = None;
    b.reset(View::Tasks);
    Ok(())
}
fn id(arg: &str) -> Result<i64, &'static str> {
    arg.parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or("expected a positive ID")
}
fn open(b: &mut Browser, id: i64) -> Result<(), &'static str> {
    if !b.ids.contains(&id) {
        return Err("ID is not on the displayed page");
    }
    let view = match b.view {
        View::Projects => {
            b.project = Some(id);
            b.task = None;
            View::Tasks
        }
        View::Tasks => {
            b.task = Some(id);
            View::Events
        }
        View::Events => View::Event(id),
        View::Bindings => View::Binding(id),
        _ => return Err("select a list first"),
    };
    b.reset(view);
    Ok(())
}
fn detail(b: &mut Browser, view: View) -> Result<(), &'static str> {
    if b.task.is_none() {
        return Err("open a task first");
    }
    b.reset(view);
    Ok(())
}
fn next(b: &mut Browser) -> Result<(), &'static str> {
    let next = b.next.ok_or("no next page")?;
    b.history.push(b.page.after_id);
    b.page.after_id = next;
    Ok(())
}
fn previous(b: &mut Browser) -> Result<(), &'static str> {
    b.page.after_id = b.history.pop().ok_or("no previous page")?;
    Ok(())
}
