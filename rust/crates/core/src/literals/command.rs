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
