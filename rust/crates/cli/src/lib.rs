//! The cc-transcript CLI: the clap twin of the retired click tree (cli.py), sharing one
//! command surface between the `[[bin]]` and the `_native.cli_main` pyfunction.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use clap::error::ErrorKind as ClapErrorKind;
use clap::{Args, Parser, Subcommand};

pub mod commands;
pub mod output;
pub mod target;

use output::CliExit;

/// Whether a `watch` loop is active: SIGINT then means "stop tailing, exit 0"
/// (cli.py watch_ catches KeyboardInterrupt); outside watch it means exit 130.
pub static WATCH_ACTIVE: AtomicBool = AtomicBool::new(false);
pub static WATCH_INTERRUPTED: AtomicBool = AtomicBool::new(false);

/// The real SIGINT handler spike C mandates: with the GIL detached, Python-side
/// KeyboardInterrupt delivery never fires, so the Rust side owns the signal.
pub fn install_sigint_handler() {
    let _ = ctrlc::set_handler(|| {
        if WATCH_ACTIVE.load(Ordering::SeqCst) {
            WATCH_INTERRUPTED.store(true, Ordering::SeqCst);
        } else {
            std::process::exit(130);
        }
    });
}

fn pkg_version() -> String {
    let pyproject = include_str!("../../../../pyproject.toml");
    pyproject
        .lines()
        .find_map(|line| line.strip_prefix("version = \""))
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(env!("CARGO_PKG_VERSION"))
        .to_string()
}

const KIND_CHOICES: [&str; 6] = ["user", "assistant", "system", "mode", "other", "attachment"];
const WHERE_CHOICES: [&str; 3] = ["text", "thinking", "tools"];
const LIST_PROVIDER_CHOICES: [&str; 3] = ["claude", "codex", "all"];

#[derive(Parser)]
#[command(
    name = "cc-transcript",
    version = pkg_version(),
    about = "Investigate Claude Code transcripts: list, show, grep, and stats.",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Args)]
pub struct DiscoveryOpts {
    /// Projects directory to search [default: ~/.claude/projects].
    #[arg(long)]
    pub root: Option<PathBuf>,
    /// Substring filter over project directory names.
    #[arg(long)]
    pub project: Option<String>,
    /// Substring filter over transcript file names.
    #[arg(long)]
    pub contains: Option<String>,
    /// Keep only the newest N transcripts.
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
    /// Ignore --limit.
    #[arg(long)]
    pub all: bool,
}

impl DiscoveryOpts {
    pub fn root(&self) -> PathBuf {
        self.root
            .clone()
            .unwrap_or_else(target::claude_projects_dir)
    }

    pub fn validated_root(&self, usage: &str, help_path: &str) -> Result<PathBuf, CliExit> {
        let root = self.root();
        target::require_dir(&root, "'--root'", usage, help_path)?;
        Ok(root)
    }

    pub fn effective_limit(&self) -> Option<usize> {
        if self.all {
            None
        } else {
            Some(self.limit)
        }
    }
}

#[derive(Subcommand)]
pub enum Cmd {
    /// List discovered transcripts, newest first.
    List {
        #[command(flatten)]
        discovery: DiscoveryOpts,
        #[arg(
            long,
            value_parser = LIST_PROVIDER_CHOICES,
            default_value = "claude",
            help = "Transcript provider to list"
        )]
        provider: String,
        #[arg(
            long,
            help = "Codex sessions directory to search [default: ~/.codex/sessions]"
        )]
        codex_root: Option<PathBuf>,
        /// Emit one JSON object per transcript.
        #[arg(long)]
        json: bool,
    },
    /// Show a transcript's events, one compact line per event.
    Show {
        path: PathBuf,
        /// Show only the first N matching events.
        #[arg(long)]
        head: Option<usize>,
        /// Show only the last N matching events.
        #[arg(long)]
        tail: Option<usize>,
        /// Show raw-index range A:B (half-open; A: and :B work).
        #[arg(long)]
        range: Option<String>,
        /// Disable the default 200-event cap.
        #[arg(long)]
        all: bool,
        /// Keep only these event kinds.
        #[arg(long = "kind", value_parser = KIND_CHOICES)]
        kinds: Vec<String>,
        /// Keep only substantive user/assistant turns.
        #[arg(long)]
        signal: bool,
        /// Drop structural junk events.
        #[arg(long = "no-junk")]
        no_junk: bool,
        /// Render thinking text inline.
        #[arg(long)]
        thinking: bool,
        /// Truncation width per chunk (0 = no cut).
        #[arg(long, default_value_t = 100)]
        width: usize,
        /// Append each event's uuid.
        #[arg(long)]
        uuids: bool,
        /// Emit one JSON object per event.
        #[arg(long)]
        json: bool,
    },
    /// Search transcript events for a regex pattern.
    Grep {
        pattern: String,
        paths: Vec<PathBuf>,
        #[command(flatten)]
        discovery: DiscoveryOpts,
        /// Keep only these event kinds.
        #[arg(long = "kind", value_parser = KIND_CHOICES)]
        kinds: Vec<String>,
        /// Keep only events using this tool, or carrying its results.
        #[arg(long)]
        tool: Option<String>,
        /// Case-insensitive matching.
        #[arg(short = 'i', long = "ignore-case")]
        ignore_case: bool,
        /// Search only these areas [default: all].
        #[arg(long = "where", value_parser = WHERE_CHOICES)]
        wheres: Vec<String>,
        /// Events of context around each hit.
        #[arg(short = 'C', long, default_value_t = 0)]
        context: usize,
        /// Stop after this many matches.
        #[arg(long = "max-matches", default_value_t = 20)]
        max_matches: usize,
        /// Truncation width per chunk (0 = no cut).
        #[arg(long, default_value_t = 100)]
        width: usize,
        /// Append each event's uuid.
        #[arg(long)]
        uuids: bool,
        /// Annotate tool-use hits with their result's outcome.
        #[arg(long = "with-result")]
        with_result: bool,
        /// Emit one JSON object per match.
        #[arg(long)]
        json: bool,
    },
    /// Summarize event, model, and tool statistics.
    Stats {
        paths: Vec<PathBuf>,
        #[command(flatten)]
        discovery: DiscoveryOpts,
        /// One stats block per transcript.
        #[arg(long = "per-file")]
        per_file: bool,
        /// Emit stats as JSON.
        #[arg(long)]
        json: bool,
    },
    /// List every tool call across the matched transcripts, one compact line each.
    Tools {
        paths: Vec<PathBuf>,
        #[command(flatten)]
        discovery: DiscoveryOpts,
        /// Keep only calls to this tool.
        #[arg(long)]
        tool: Option<String>,
        /// Emit one JSON object per tool call.
        #[arg(long)]
        json: bool,
    },
    /// Tally Bash command prefixes across the matched transcripts, most frequent first.
    Commands {
        paths: Vec<PathBuf>,
        #[command(flatten)]
        discovery: DiscoveryOpts,
        /// Emit one JSON object per prefix.
        #[arg(long)]
        json: bool,
    },
    /// List tool uses the user denied, with the instruction they gave instead.
    Permissions {
        paths: Vec<PathBuf>,
        #[command(flatten)]
        discovery: DiscoveryOpts,
        /// Emit one JSON object per denial.
        #[arg(long)]
        json: bool,
    },
    /// Summarize MCP server and tool usage across the matched transcripts.
    Mcp {
        paths: Vec<PathBuf>,
        #[command(flatten)]
        discovery: DiscoveryOpts,
        /// Emit one JSON object per server.
        #[arg(long)]
        json: bool,
    },
    /// Emit a session window's tool calls, one cc-transcript.slice/1 JSON line each.
    Slice {
        /// Claude session UUID.
        #[arg(long, required = true)]
        session: String,
        /// Window start, RFC 3339 (inclusive).
        #[arg(long, required = true)]
        since: String,
        /// Window end, RFC 3339 (exclusive).
        #[arg(long, required = true)]
        until: String,
        /// Projects directory to search [default: ~/.claude/projects].
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Print the scratchpad directory for a Claude Code session.
    Scratchpad {
        /// Claude session UUID.
        #[arg(long, env = "CLAUDE_CODE_SESSION_ID", required = true)]
        session: String,
    },
    /// Generate the tool-digest fixture corpus from stdin, or verify one with --check.
    Digest {
        /// Verify an existing fixture file instead of generating.
        #[arg(long)]
        check: Option<PathBuf>,
    },
    /// Tail transcripts live, one line per newly appended event, until interrupted.
    Watch {
        /// Projects directory to tail; repeatable [default: ~/.claude/projects].
        #[arg(long = "root")]
        roots: Vec<PathBuf>,
        /// Seconds between filesystem polls.
        #[arg(long, default_value_t = 1.0, value_parser = parse_poll, allow_hyphen_values = true)]
        poll: f64,
        /// Replay preexisting transcript content instead of tailing from EOF.
        #[arg(long = "from-start")]
        from_start: bool,
        /// Emit one NDJSON object per event.
        #[arg(long)]
        json: bool,
    },
    /// Read and write the shared code-correction ledger.
    Corrections {
        #[command(subcommand)]
        cmd: CorrectionsCmd,
    },
}

/// `--poll` clap-side validation: negatives rejected with a clean error (the P4
/// review's binding follow-up; click accepted them and slept on a negative).
fn parse_poll(value: &str) -> Result<f64, String> {
    let poll: f64 = value
        .parse()
        .map_err(|_| format!("'{value}' is not a number"))?;
    if poll < 0.0 || !poll.is_finite() {
        return Err(format!("'{value}' is not a non-negative number of seconds"));
    }
    Ok(poll)
}

#[derive(Subcommand)]
pub enum CorrectionsCmd {
    /// Append one correction to the ledger (idempotent).
    Add {
        /// The Claude session UUID the edit fired in.
        #[arg(long, required = true)]
        session: String,
        /// The writing system, e.g. cc-review.
        #[arg(long, required = true)]
        source: String,
        /// The feedback anchor uuid, or review:<reviewID>:<commentID>.
        #[arg(long, required = true)]
        anchor: String,
        /// The file the incorrect edit targeted.
        #[arg(long = "incorrect-file", required = true)]
        incorrect_file: String,
        /// Edit timestamp in ms [default: now].
        #[arg(long = "ts-ms")]
        ts_ms: Option<i64>,
        #[arg(long, value_parser = ["session", "git", "review"])]
        origin: Option<String>,
        /// Content the incorrect edit replaced.
        #[arg(long = "incorrect-old", default_value = "")]
        incorrect_old: String,
        /// Content the incorrect edit wrote.
        #[arg(long = "incorrect-new", default_value = "")]
        incorrect_new: String,
        /// Cross-language tool digest; omit for review rows.
        #[arg(long = "incorrect-digest")]
        incorrect_digest: Option<String>,
        #[arg(long = "correction-file")]
        correction_file: Option<String>,
        #[arg(long = "correction-old")]
        correction_old: Option<String>,
        #[arg(long = "correction-new")]
        correction_new: Option<String>,
        #[arg(long = "correction-commit")]
        correction_commit: Option<String>,
        /// The reviewer's verbatim note, for review rows.
        #[arg(long = "correction-text")]
        correction_text: Option<String>,
        #[arg(long, default_value_t = 0.0)]
        overlap: f64,
        /// Repo key, stamped into detail.repo.
        #[arg(long)]
        repo: Option<String>,
        /// Extra JSON object merged into detail.
        #[arg(long = "detail")]
        detail_json: Option<String>,
    },
    /// Query corrections as one JSON object per line.
    Query {
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        repo: Option<String>,
        /// Requires --session.
        #[arg(long)]
        digest: Option<String>,
        /// Corrections with ts_ms greater than this.
        #[arg(long)]
        since: Option<i64>,
        /// Scopes --since to one producer.
        #[arg(long)]
        source: Option<String>,
    },
    /// Run a raw SQL statement against the ledger — the escape hatch.
    Sql { statement: String },
}

/// Full-argv entry shared by the `[[bin]]` and `_native.cli_main`: parses, dispatches,
/// and returns the exit code verbatim.
pub fn run(argv: Vec<String>) -> i32 {
    let cli = match Cli::try_parse_from(&argv) {
        Ok(cli) => cli,
        Err(err) => {
            let code = match err.kind() {
                ClapErrorKind::DisplayHelp | ClapErrorKind::DisplayVersion => 0,
                _ => 2,
            };
            let _ = err.print();
            return code;
        }
    };
    let Some(cmd) = cli.cmd else {
        // click: a bare group invocation prints help to stderr and exits 2.
        let mut help = <Cli as clap::CommandFactory>::command();
        eprint!("{}", help.render_long_help());
        return 2;
    };
    match dispatch(cmd) {
        Ok(()) => 0,
        Err(CliExit(code)) => code,
    }
}

fn dispatch(cmd: Cmd) -> Result<(), CliExit> {
    match cmd {
        Cmd::List {
            discovery,
            provider,
            codex_root,
            json,
        } => commands::list::run(&discovery, &provider, codex_root.as_deref(), json),
        Cmd::Show {
            path,
            head,
            tail,
            range,
            all,
            kinds,
            signal,
            no_junk,
            thinking,
            width,
            uuids,
            json,
        } => commands::show::run(commands::show::ShowArgs {
            path,
            head,
            tail,
            range,
            all,
            kinds,
            signal,
            no_junk,
            thinking,
            width,
            uuids,
            json,
        }),
        Cmd::Grep {
            pattern,
            paths,
            discovery,
            kinds,
            tool,
            ignore_case,
            wheres,
            context,
            max_matches,
            width,
            uuids,
            with_result,
            json,
        } => commands::grep::run(commands::grep::GrepArgs {
            pattern,
            paths,
            discovery,
            kinds,
            tool,
            ignore_case,
            wheres,
            context,
            max_matches,
            width,
            uuids,
            with_result,
            json,
        }),
        Cmd::Stats {
            paths,
            discovery,
            per_file,
            json,
        } => commands::stats::run(&paths, &discovery, per_file, json),
        Cmd::Tools {
            paths,
            discovery,
            tool,
            json,
        } => commands::facts::tools(&paths, &discovery, tool.as_deref(), json),
        Cmd::Commands {
            paths,
            discovery,
            json,
        } => commands::facts::commands(&paths, &discovery, json),
        Cmd::Permissions {
            paths,
            discovery,
            json,
        } => commands::facts::permissions(&paths, &discovery, json),
        Cmd::Mcp {
            paths,
            discovery,
            json,
        } => commands::facts::mcp(&paths, &discovery, json),
        Cmd::Slice {
            session,
            since,
            until,
            root,
        } => commands::slice::run(&session, &since, &until, root),
        Cmd::Scratchpad { session } => commands::scratchpad::run(&session),
        Cmd::Digest { check } => commands::digest::run(check.as_deref()),
        Cmd::Watch {
            roots,
            poll,
            from_start,
            json,
        } => commands::watch::run(&roots, poll, from_start, json),
        Cmd::Corrections { cmd } => commands::corrections::run(cmd),
    }
}
