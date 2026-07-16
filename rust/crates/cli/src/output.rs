//! Exit-code plumbing and the SIGPIPE-safe stdout writer (cli.py emit / click.echo).
//!
//! Python's `emit()` converts `BrokenPipeError` to `SystemExit(0)`; every stdout write
//! here routes through [`Out`], which maps `EPIPE` to a clean exit-0 the same way.
//! App-level usage errors reproduce click's `UsageError` rendering byte-for-byte —
//! the golden matrix pins that shape (scratchpad_invalid).

use std::io::{self, ErrorKind, Stderr, Stdout, Write};

/// Terminate the command with this exit code (cli.py's SystemExit).
#[derive(Debug)]
pub struct CliExit(pub i32);

pub type CmdResult = Result<(), CliExit>;

fn pipe_or_die(result: io::Result<()>) -> CmdResult {
    match result {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::BrokenPipe => Err(CliExit(0)),
        Err(_) => Err(CliExit(1)),
    }
}

/// Buffered stdout with Python `emit()` SIGPIPE semantics: a broken pipe exits 0.
pub struct Out {
    w: io::BufWriter<Stdout>,
}

impl Out {
    pub fn new() -> Self {
        Out {
            w: io::BufWriter::new(io::stdout()),
        }
    }

    pub fn line(&mut self, line: &str) -> CmdResult {
        pipe_or_die(
            self.w
                .write_all(line.as_bytes())
                .and_then(|()| self.w.write_all(b"\n")),
        )
    }

    pub fn lines<I: IntoIterator<Item = String>>(&mut self, lines: I) -> CmdResult {
        for line in lines {
            self.line(&line)?;
        }
        Ok(())
    }

    pub fn finish(&mut self) -> CmdResult {
        pipe_or_die(self.w.flush())
    }
}

impl Default for Out {
    fn default() -> Self {
        Self::new()
    }
}

/// Buffered stderr; broken pipes on stderr are swallowed (nothing useful remains to say).
pub struct Err_ {
    w: io::BufWriter<Stderr>,
}

impl Err_ {
    pub fn new() -> Self {
        Err_ {
            w: io::BufWriter::new(io::stderr()),
        }
    }

    pub fn line(&mut self, line: &str) {
        let _ = self.w.write_all(line.as_bytes());
        let _ = self.w.write_all(b"\n");
    }

    pub fn finish(&mut self) {
        let _ = self.w.flush();
    }
}

impl Default for Err_ {
    fn default() -> Self {
        Self::new()
    }
}

pub fn eline(line: &str) {
    let mut err = Err_::new();
    err.line(line);
    err.finish();
}

/// click UsageError rendering: usage line, help hint, blank, `Error: <msg>`; exit 2.
pub fn usage_error(usage: &str, help_path: &str, message: &str) -> CliExit {
    eline(&format!(
        "Usage: {usage}\nTry '{help_path} --help' for help.\n\nError: {message}"
    ));
    CliExit(2)
}

/// click ClickException rendering: `Error: <msg>`; exit 1.
pub fn click_error(message: &str) -> CliExit {
    eline(&format!("Error: {message}"));
    CliExit(1)
}

/// Python `repr()` of a str, for click's `{value!r}` interpolations.
pub fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || (c as u32) == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}
