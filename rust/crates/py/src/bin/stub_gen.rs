//! Generates `cc_transcript/_native.pyi` from the gen_stub inventory.
//!
//! The extension is one flat namespace whose classes claim facade module
//! strings (`cc_transcript.models`, …) for reprs, pickling, and docs
//! resolution; pyo3-stub-gen would split stubs along those strings and shim
//! over the hand-written facades, so this bin merges every group into one
//! module, de-qualifies references to merged classes, and writes the single
//! stub file. See cc-notes doc "v14 _native.pyi generation" for rationale.
//!
//! Run: `cargo run -p cc-transcript-py --bin stub_gen`

use std::fmt::Write as _;

use pyo3_stub_gen::TypeInfo;

fn type_info(name: &str, imports: &[&str]) -> TypeInfo {
    TypeInfo {
        name: name.to_string(),
        source_module: None,
        import: imports.iter().map(|&i| i.into()).collect(),
        type_refs: Default::default(),
    }
}

// The __match_args__ classattrs come from Rust fns returning Py<PyTuple>,
// which stubs as bare `tuple`; type them as the ClassVar they are.
fn type_match_args(merged: &mut pyo3_stub_gen::generate::Module) {
    for class in merged.class.values_mut() {
        for attr in &mut class.attrs {
            if attr.name == "__match_args__" {
                attr.r#type = type_info("typing.ClassVar[tuple[str, ...]]", &["typing"]);
            }
        }
    }
}

// EventList's runtime is a Sequence over TranscriptEvent, and __getitem__
// narrows by index shape; the derive macros cannot express bases or overloads,
// so enrich the merged ClassDef in place.
fn enrich_event_list(merged: &mut pyo3_stub_gen::generate::Module) {
    const EVENT: &str = "cc_transcript.models.TranscriptEvent";
    let event_list = merged
        .class
        .values_mut()
        .find(|class| class.name == "EventList")
        .expect("EventList in the gen_stub inventory");
    event_list.bases.push(type_info(
        &format!("collections.abc.Sequence[{EVENT}]"),
        &["collections.abc", "cc_transcript.models"],
    ));
    let getitem = event_list
        .methods
        .get_mut("__getitem__")
        .expect("EventList.__getitem__ in the gen_stub inventory");
    let proto = getitem[0].clone();
    *getitem = [("int", EVENT), ("slice", &format!("list[{EVENT}]"))]
        .into_iter()
        .map(|(index, ret)| {
            let mut def = proto.clone();
            def.is_overload = true;
            def.parameters.positional_or_keyword[0].type_info = type_info(index, &[]);
            def.r#return = type_info(ret, &["cc_transcript.models"]);
            def
        })
        .collect();
}

fn allow_other_attachment_subclasses(merged: &mut pyo3_stub_gen::generate::Module) {
    let other = merged
        .class
        .values_mut()
        .find(|class| class.name == "OtherAttachment")
        .expect("OtherAttachment in the gen_stub inventory");
    other.subclass = true;
}

fn render() -> pyo3_stub_gen::Result<(std::path::PathBuf, String)> {
    let stub = _native::stub_info()?;
    let group_names: Vec<String> = stub.modules.keys().cloned().collect();

    let mut merged = pyo3_stub_gen::generate::Module {
        name: "cc_transcript._native".to_string(),
        default_module_name: "cc_transcript._native".to_string(),
        ..Default::default()
    };
    let mut merged_names: Vec<(String, String)> = Vec::new();
    for (group, module) in stub.modules {
        merged_names.extend(
            module
                .class
                .values()
                .map(|class| (group.clone(), class.name.to_string()))
                .chain(
                    module
                        .enum_
                        .values()
                        .map(|enum_| (group.clone(), enum_.name.to_string())),
                ),
        );
        merged.class.extend(module.class);
        merged.enum_.extend(module.enum_);
        merged.function.extend(module.function);
        merged.variables.extend(module.variables);
        merged.type_aliases.extend(module.type_aliases);
        merged
            .verbatim_all_entries
            .extend(module.verbatim_all_entries);
    }

    type_match_args(&mut merged);
    enrich_event_list(&mut merged);
    allow_other_attachment_subclasses(&mut merged);

    let mut rendered = String::new();
    write!(rendered, "{merged}")?;

    // Override type_reprs are verbatim fully-qualified names; the renderer's
    // header imports internal modules as `from cc_transcript import <mod>`.
    let long_form = regex::Regex::new(r"\bcc_transcript\.([a-z_]+)\.")?;
    rendered = long_form.replace_all(&rendered, "$1.").to_string();

    // `#[getter(r#type)]` exposes `type`, but pyo3-stub-gen renders the Rust
    // raw identifier verbatim; strip the prefix pyo3 itself strips.
    let raw_ident = regex::Regex::new(r"(?m)^(\s*def )r#")?;
    rendered = raw_ident.replace_all(&rendered, "$1").to_string();

    // Merged classes live in this one file: de-qualify references to them,
    // then drop each group import that no longer has a qualified use.
    for (group, name) in &merged_names {
        let short = group.rsplit('.').next().expect("dotted module group");
        let qualified = regex::Regex::new(&format!(r"\b{short}\.{name}\b"))?;
        rendered = qualified.replace_all(&rendered, name.as_str()).to_string();
    }
    // The repo commit hooks strip trailing whitespace per line and normalize
    // the file to one terminating newline; emit exactly that shape so the
    // committed stub byte-matches this render.
    let out = rendered
        .lines()
        .filter(|line| {
            group_names.iter().all(|group| {
                let short = group.rsplit('.').next().expect("dotted module group");
                line.trim() != format!("from cc_transcript import {short}")
                    || rendered.contains(&format!("{short}."))
            })
        })
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end_matches('\n')
        .to_string()
        + "\n";

    Ok((
        stub.python_root.join("cc_transcript").join("_native.pyi"),
        out,
    ))
}

fn main() -> pyo3_stub_gen::Result<()> {
    let (dest, out) = render()?;
    std::fs::write(&dest, out)?;
    println!("wrote {}", dest.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::render;

    // Freshness half of the stub drift gate; tests/test_native_stub.py holds
    // the completeness half.
    #[test]
    fn committed_stub_matches_generator_output() {
        let (dest, out) = render().expect("stub renders");
        let committed = std::fs::read_to_string(&dest).expect("committed _native.pyi");
        assert_eq!(
            committed,
            out,
            "{} is stale — regenerate with `cargo run -p cc-transcript-py --bin stub_gen`",
            dest.display()
        );
    }
}
