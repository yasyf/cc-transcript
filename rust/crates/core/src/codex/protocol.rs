use sonic_rs::{JsonValueTrait, Value};

use crate::pystr;
use crate::value::field;

pub const INJECTION_WRAPPERS: &[&str] = &[
    "environment_context",
    "user_instructions",
    "skills_instructions",
    "plugins_instructions",
    "apps_instructions",
    "recommended_plugins",
];

pub fn injection_wrapper(text: &str) -> Option<&'static str> {
    let body = pystr::lstrip(text).strip_prefix('<')?;
    INJECTION_WRAPPERS.iter().copied().find(|w| {
        body.strip_prefix(*w)
            .is_some_and(|rest| rest.starts_with('>'))
    })
}

pub fn mcp_tool_name(name: &str, namespace: Option<&str>) -> Option<String> {
    if name.starts_with("mcp__") {
        return Some(name.to_string());
    }
    match namespace {
        Some(ns) if !ns.is_empty() => Some(format!("mcp__{ns}__{name}")),
        _ => None,
    }
}

pub fn output_exit_code(output: &Value) -> Option<i64> {
    match output.as_str() {
        Some(text) => metadata_exit_code(&sonic_rs::from_str::<Value>(text).ok()?),
        None => metadata_exit_code(output),
    }
}

fn metadata_exit_code(value: &Value) -> Option<i64> {
    field(value, "metadata")
        .and_then(|m| field(m, "exit_code"))
        .and_then(JsonValueTrait::as_i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injection_wrapper_matches_known_leading_tags() {
        assert_eq!(
            injection_wrapper("<environment_context>\n  <cwd>/tmp</cwd>\n</environment_context>"),
            Some("environment_context")
        );
        assert_eq!(
            injection_wrapper("  \n<user_instructions>be nice</user_instructions>"),
            Some("user_instructions")
        );
        assert_eq!(
            injection_wrapper("<plugins_instructions>x</plugins_instructions>"),
            Some("plugins_instructions")
        );
    }

    #[test]
    fn injection_wrapper_rejects_authored_and_unknown_tags() {
        assert_eq!(injection_wrapper("How do I reverse a string?"), None);
        // Start-anchored: a wrapper named mid-text is authored prose.
        assert_eq!(
            injection_wrapper("as shown in the <environment_context> above"),
            None
        );
        assert_eq!(
            injection_wrapper("<unknown_wrapper>x</unknown_wrapper>"),
            None
        );
        // The tag must be closed by '>', not extended into a different name.
        assert_eq!(injection_wrapper("<environment_contextual>x"), None);
        assert_eq!(injection_wrapper(""), None);
    }

    #[test]
    fn mcp_tool_name_synthesizes_from_namespace() {
        assert_eq!(
            mcp_tool_name("list_items", Some("demo_server")).as_deref(),
            Some("mcp__demo_server__list_items")
        );
    }

    #[test]
    fn mcp_tool_name_passes_through_already_synthesized() {
        assert_eq!(
            mcp_tool_name("mcp__demo_server__list_items", None).as_deref(),
            Some("mcp__demo_server__list_items")
        );
    }

    #[test]
    fn mcp_tool_name_none_for_native_tool() {
        assert_eq!(mcp_tool_name("exec_command", None), None);
        assert_eq!(mcp_tool_name("shell", Some("")), None);
    }

    #[test]
    fn exit_code_from_json_string_metadata() {
        let output: Value =
            sonic_rs::from_str(r#""{\"output\": \"ok\", \"metadata\": {\"exit_code\": 2}}""#)
                .unwrap();
        assert_eq!(output_exit_code(&output), Some(2));
    }

    #[test]
    fn exit_code_from_direct_object_metadata() {
        let output: Value = sonic_rs::from_str(r#"{"metadata": {"exit_code": 0}}"#).unwrap();
        assert_eq!(output_exit_code(&output), Some(0));
    }

    #[test]
    fn exit_code_absent_for_plain_and_structured_output() {
        assert_eq!(
            output_exit_code(&sonic_rs::from_str(r#""omed""#).unwrap()),
            None
        );
        let arr: Value =
            sonic_rs::from_str(r#"[{"type": "output_text", "text": "omed"}]"#).unwrap();
        assert_eq!(output_exit_code(&arr), None);
    }
}
