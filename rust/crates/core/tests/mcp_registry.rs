use std::collections::HashSet;
use std::sync::Arc;

use cc_transcript_core::toolcall::{
    expand_tool_names, mcp_access, mcp_parts, mcp_tool_alias, parse_tool_call, register_mcp_tool,
    tool_name_matches, unregister_mcp_tool, with_registry, McpToolSpec, SpanEditMap, ToolCall,
    ToolRegistrySnapshot,
};
use sonic_rs::Value;

fn obj(json: &str) -> Value {
    sonic_rs::from_str(json).unwrap()
}

fn snapshot_spec(behaves_like: &str, span_edit: bool) -> McpToolSpec {
    McpToolSpec {
        behaves_like: behaves_like.to_string(),
        span_edit: span_edit.then(|| SpanEditMap {
            path: "path".to_string(),
            content: "content".to_string(),
            delete: Some("delete".to_string()),
        }),
    }
}

#[test]
fn name_helpers() {
    assert_eq!(mcp_parts("mcp__semble__search"), Some(("semble", "search")));
    assert!(tool_name_matches("Execute", "Bash|Grep"));
    assert_eq!(mcp_access("ccx_read"), "read");
    assert_eq!(mcp_access("deploy"), "write");

    register_mcp_tool(
        "syn_helper_edit".to_string(),
        McpToolSpec {
            behaves_like: "Edit".to_string(),
            span_edit: None,
        },
    );
    assert_eq!(mcp_tool_alias("syn_helper_edit"), Some("Edit".to_string()));
    assert!(tool_name_matches(
        "mcp__cc-context__syn_helper_edit",
        "Edit"
    ));
    assert!(unregister_mcp_tool("syn_helper_edit"));
    assert_eq!(mcp_tool_alias("syn_helper_edit"), None);
    assert!(!tool_name_matches(
        "mcp__cc-context__syn_helper_edit",
        "Edit"
    ));
}

#[test]
fn registered_span_edit_lowers_from_mapped_keys() {
    register_mcp_tool(
        "syn_lower_edit".to_string(),
        McpToolSpec {
            behaves_like: "Edit".to_string(),
            span_edit: Some(SpanEditMap {
                path: "path".to_string(),
                content: "content".to_string(),
                delete: Some("delete".to_string()),
            }),
        },
    );
    match parse_tool_call(
        "mcp__cc-context__syn_lower_edit",
        &obj(r#"{"path":"/x.py","content":"body"}"#),
    ) {
        ToolCall::SpanEdit(c) => {
            assert_eq!(c.file_path, "/x.py");
            assert_eq!(c.new, Some("body".to_string()));
        }
        other => panic!("{other:?}"),
    }
    match parse_tool_call(
        "mcp__cc-context__syn_lower_edit",
        &obj(r#"{"path":"/x.py","content":"body","delete":true}"#),
    ) {
        ToolCall::SpanEdit(c) => assert_eq!(c.new, None),
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        parse_tool_call(
            "mcp__cc-context__syn_lower_edit",
            &obj(r#"{"content":"body"}"#)
        ),
        ToolCall::Other(_)
    ));
    assert!(matches!(
        parse_tool_call(
            "mcp__cc-context__syn_lower_edit",
            &obj(r#"{"path":"/x.py"}"#)
        ),
        ToolCall::Other(_)
    ));
    assert!(unregister_mcp_tool("syn_lower_edit"));
}

#[test]
fn registered_behaves_like_gates_but_lowers_to_other() {
    register_mcp_tool(
        "syn_gate_only".to_string(),
        McpToolSpec {
            behaves_like: "Write".to_string(),
            span_edit: None,
        },
    );
    assert!(expand_tool_names("Write").contains("syn_gate_only"));
    assert!(matches!(
        parse_tool_call("mcp__cc-context__syn_gate_only", &obj(r#"{"path":"/x"}"#)),
        ToolCall::Other(_)
    ));
    assert!(unregister_mcp_tool("syn_gate_only"));
    assert!(!expand_tool_names("Write").contains("syn_gate_only"));
}

#[test]
fn registry_snapshot_is_immutable_across_default_mutations() {
    let tool = "syn_registry_snapshot";
    register_mcp_tool(tool.to_string(), snapshot_spec("Edit", true));
    let pinned = ToolRegistrySnapshot::capture_current();
    with_registry(pinned.clone(), || {
        let (sender, receiver) = std::sync::mpsc::channel();
        let writer = std::thread::spawn(move || {
            register_mcp_tool(tool.to_string(), snapshot_spec("Write", false));
            sender.send(()).unwrap();
        });
        receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a pinned registry scope must not block default updates");
        writer.join().unwrap();
        assert!(Arc::ptr_eq(
            &pinned,
            &ToolRegistrySnapshot::capture_current()
        ));
        assert_eq!(mcp_tool_alias(tool).as_deref(), Some("Edit"));
        assert!(tool_name_matches(
            "mcp__fixture__syn_registry_snapshot",
            "Edit"
        ));
        assert!(!tool_name_matches(
            "mcp__fixture__syn_registry_snapshot",
            "Write"
        ));
        assert!(cc_transcript_core::types::matches_names(
            "mcp__fixture__syn_registry_snapshot",
            &HashSet::from(["Edit".to_string()]),
        ));
        match parse_tool_call(
            "mcp__fixture__syn_registry_snapshot",
            &obj(r#"{"path":"a.py","content":"pinned"}"#),
        ) {
            ToolCall::SpanEdit(call) => assert_eq!(call.new.as_deref(), Some("pinned")),
            other => panic!("{other:?}"),
        }
        assert!(unregister_mcp_tool(tool));
        assert_eq!(mcp_tool_alias(tool).as_deref(), Some("Edit"));
    });
    assert_eq!(mcp_tool_alias(tool), None);
}
