use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

use cc_transcript_core::command::{Command, CommandLine, Redirect};

pub use cc_transcript_core::command::prefixes;

fn redirect_to_py<'py>(py: Python<'py>, redirect: &Redirect) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("op", &redirect.op)?;
    dict.set_item("target", &redirect.target)?;
    dict.set_item("fd", redirect.fd)?;
    Ok(dict)
}

fn command_to_py<'py>(py: Python<'py>, cmd: &Command) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("raw", &cmd.raw)?;
    dict.set_item("executable", &cmd.executable)?;
    dict.set_item("args", &cmd.args)?;
    let env = PyList::empty(py);
    for (name, value) in &cmd.env {
        env.append(PyList::new(py, [name.as_str(), value.as_str()])?)?;
    }
    dict.set_item("env", env)?;
    let redirects = PyList::empty(py);
    for redirect in &cmd.redirects {
        redirects.append(redirect_to_py(py, redirect)?)?;
    }
    dict.set_item("redirects", redirects)?;
    dict.set_item("program", cmd.program())?;
    dict.set_item("unwrapped_argv", cmd.unwrapped().argv())?;
    dict.set_item("prefix", cmd.prefix())?;
    Ok(dict)
}

pub fn line_to_py<'py>(py: Python<'py>, line: &CommandLine) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    dict.set_item("raw", &line.raw)?;
    let parts = PyList::empty(py);
    for (cmd, op) in &line.parts {
        let part = PyDict::new(py);
        part.set_item("op", op.as_deref())?;
        part.set_item("command", command_to_py(py, cmd)?)?;
        parts.append(part)?;
    }
    dict.set_item("parts", parts)?;
    dict.set_item("prefixes", line.prefixes())?;
    Ok(dict)
}

#[cfg(test)]
mod tests {
    use super::prefixes;

    const PIN_DELIM: char = '|';
    const PINS_TSV: &str = include_str!("../data/command_prefix_pins.tsv");

    fn decode_pin(field: &str) -> String {
        let mut out = String::with_capacity(field.len());
        let mut chars = field.chars();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                out.push(ch);
                continue;
            }
            match chars.next() {
                Some('n') => out.push('\n'),
                Some(other) => out.push(other),
                None => out.push('\\'),
            }
        }
        out
    }

    fn pins() -> Vec<(String, Vec<String>)> {
        PINS_TSV
            .lines()
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(|line| {
                let mut fields = line.split('\t');
                let _id = fields.next().unwrap();
                let command = decode_pin(fields.next().unwrap());
                let expected = match fields.next().unwrap() {
                    "" => Vec::new(),
                    field => field.split(PIN_DELIM).map(str::to_string).collect(),
                };
                (command, expected)
            })
            .collect()
    }

    #[test]
    fn prefix_battery_matches_python_pins() {
        for (command, expected) in pins() {
            assert_eq!(
                prefixes(&command),
                expected,
                "prefixes mismatch for {command:?}"
            );
        }
    }
}
