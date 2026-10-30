# Local terminal browser

Run `herdr-flow light --data-root /absolute/private/data tui`. This is one line-input TUI, chosen for the existing terminal team's browsing needs. It uses ordinary terminal line editing: type a command and press Enter. No additional UI dependency, web server, observer or background refresh is started. The installed tool is not replaced by building this feature.

The first view lists tasks across projects. Optional `--project ID`, `--batch TEXT` and `--limit 1..100` select initial filters and page size (default 10). Storage root, explicit forbidden paths and busy timeout use the existing record CLI rules. Missing storage is an error, never implicit initialization.

| Command | Action |
|---|---|
| `projects` | List projects; `open ID` selects a displayed project |
| `tasks` | List tasks using the current project/batch filters |
| `project ID` / `project all` | Filter one project or all projects; reset pagination |
| `batch TEXT` / `batch -` | Exact batch label or all batches; reset pagination |
| `open ID` | Open a displayed task, event, or participant binding |
| `events` / `bindings` | Browse the selected task's report history or participants |
| `next` / `prev` (`n` / `p`) | Keyset page forward or back |
| `refresh` (`r`) | Read the current page again, without changing stored data |
| `help` / `quit` (`q`) | Show commands or exit |

An open task shows its complete current report: project/task IDs, batch, summary, reported state, revision and responsible binding. Event rows show reporter, summary and reference count; opening the event displays full reference metadata, including target, commit, recorded result, timestamp/hash and temporary status. References are **text only**: they are never opened, imported, executed or treated as current evidence for another commit. Lists shorten long labels; open details for full text. Control and bidirectional formatting characters in stored text are escaped to avoid terminal injection.

Participant rows show roles, pane references and whether a session was recorded. Opening a binding displays the existing observation cache separately from the Agent report. The view shows recorded state, timestamp, age, freshness and uncertainty. Expired or clock-inconsistent data becomes unknown using the existing observation freshness model; missing, invalid or unavailable caches show explicit unknown/error messages. A changed binding cannot inherit another context's cache. The browser does not check observer processes or live service state: even a fresh cache is only a historical observation, never proof of current liveness or task success. `observe --show` additionally checks a cached watch process locally; this TUI deliberately does not probe it. Nothing in browsing invokes `observe --once`, `--watch`, notification delivery or recovery. Freshness advances when the user refreshes/navigates; there is no periodic timer.

Every frame reopens the existing store in read-only mode and uses the same `Store` query functions as the JSON CLI; cross-project browsing applies optional project selection to the same task query. Record details share one SQLite read transaction. The separate observation cache is not atomically sampled with records; its binding identity and timestamps remain visible. There is no second data model or index. Pagination uses increasing IDs and a back-cursor history; concurrent additions may appear on later pages or refresh. A full final page can lead to an empty next page, matching the CLI contract. Changing a filter returns to the first page.

Input and data errors are printed within the browser; a subsequent command/refresh may retry a read. No repair is attempted. Input is bounded to 4096 bytes per line; terminal drivers may impose a smaller canonical input limit. Terminal stdin and stdout are required; redirected use receives a usage error, and existing JSON record commands remain the scripting interface. A successful explicit quit or EOF exits 0; an unrecoverable input/output error uses the CLI error envelope on stderr. Process signals retain their normal operating-system behavior.

The browser never changes termios, enters raw mode, hides the cursor or enters an alternate screen. Quit, EOF, interrupt and termination therefore leave the original terminal modes intact; no background cleanup service or crash recovery hook is needed. Output remains in terminal scrollback. The TUI has no workspace write, dispatch, merge, cleanup, reference-opening or recovery commands. SQLite is opened using the established read-only storage/cache APIs (including their WAL coordination); authoritative records and notification state are never mutated.

Validation uses a real PTY to exercise navigation, project/batch filters, pagination, complete refs, missing storage/cache, error recovery, escape-containing text and quit/EOF/Ctrl-C/SIGTERM. It compares CLI records and database logical dumps before/after, checks termios equality, and traces the browser for process/network/legacy-path access. Rendering tests alone are not the acceptance evidence. No shared service, skill or installed tool is changed; #173 installation convergence and #176 recovery remain outside this feature.
