//! Bash-command wrapper / multi-level tool tables and the assignment pattern.
//! Hand-owned; mirrored to Python via `_native.embedded_literals()`.

pub const WRAPPER_COMMANDS: &[&str] = &[
    "command", "doas", "env", "exec", "nice", "nohup", "sudo", "time", "timeout", "xargs",
];
pub const MULTI_LEVEL_TOOLS: &[&str] = &[
    "aws",
    "brew",
    "cargo",
    "docker",
    "gcloud",
    "gh",
    "git",
    "go",
    "jj",
    "kubectl",
    "npm",
    "npx",
    "pip",
    "pnpm",
    "terraform",
    "uv",
    "uvx",
    "yarn",
];
pub const COMPOUND_OPS: &[&str] = &["&", "&&", ";", "|", "||"];
pub const ASSIGNMENT_PATTERN: &str = "^\\w+=";
pub const SHELL_COMMANDS: &[&str] = &[
    "ash", "bash", "csh", "dash", "fish", "ksh", "sh", "tcsh", "zsh",
];
pub const POSIX_QUOTING_SHELLS: &[&str] = &["ash", "bash", "dash", "ksh", "sh", "zsh"];
pub const PAYLOAD_DEPTH_LIMIT: u8 = 3;

// Per-wrapper flags that consume the next token as a value. Mandatory space-separable arguments
// only (man-page verified); boolean and optional-argument flags are excluded so unwrap never
// swallows the wrapped command.
pub const WRAPPER_VALUE_FLAGS: &[(&str, &[&str])] = &[
    // env -S/--split-string is omitted on purpose: its argument is a shell command env re-splits
    // and runs, so consuming it would hide the wrapped command; a bare flag keeps it visible.
    ("env", &["-u", "-C", "--unset", "--chdir"]),
    (
        "sudo",
        &[
            "-u",
            "-g",
            "-p",
            "-U",
            "-R",
            "-D",
            "-C",
            "-T",
            "-r",
            "-t",
            "-c",
            "-a",
            "--user",
            "--group",
            "--prompt",
            "--other-user",
            "--chroot",
            "--chdir",
            "--close-from",
            "--command-timeout",
            "--role",
            "--type",
            "--login-class",
            "--auth-type",
        ],
    ),
    ("doas", &["-u", "-C"]),
    ("timeout", &["-k", "-s", "--kill-after", "--signal"]),
    ("nice", &["-n", "--adjustment"]),
    (
        "xargs",
        &[
            "-I",
            "-n",
            "-P",
            "-s",
            "-d",
            "-a",
            "-E",
            "-L",
            "--max-args",
            "--max-procs",
            "--max-chars",
            "--delimiter",
            "--arg-file",
        ],
    ),
    ("exec", &["-a"]),
    ("time", &["-f", "-o", "--format", "--output"]),
];

// Leading positional operands a wrapper consumes after its flags — timeout's DURATION.
pub const WRAPPER_OPERAND_SKIP: &[(&str, usize)] = &[("timeout", 1)];
