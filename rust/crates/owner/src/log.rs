use std::io::Write;

use cc_transcript_core::render::{py_float_repr, py_string_repr};
use sha2::{Digest, Sha256};
use sonic_rs::{json, JsonContainerTrait, JsonType, JsonValueTrait, Value};

use crate::pyjson::{self, SORTED};

const SOURCE_FIELDS: [&str; 5] = ["path", "direct_paths", "roots", "session_ids", "thread_ids"];
const HANDLE_FIELDS: [&str; 5] = ["handles", "handle", "view", "cursor", "token"];

pub enum Field {
    Json(Value),
    Float(f64),
    Strings(Vec<String>),
}

impl Field {
    fn repr(&self) -> String {
        match self {
            Self::Json(value) => repr(value),
            Self::Float(value) => py_float_repr(*value, true),
            Self::Strings(values) => match values.as_slice() {
                [only] => format!("({},)", py_string_repr(only)),
                values => format!(
                    "({})",
                    values
                        .iter()
                        .map(|value| py_string_repr(value))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
        }
    }
}

fn repr(value: &Value) -> String {
    match value.get_type() {
        JsonType::Null => "None".to_owned(),
        JsonType::Boolean => if value.as_bool().expect("boolean") {
            "True"
        } else {
            "False"
        }
        .to_owned(),
        JsonType::String => py_string_repr(value.as_str().expect("string")),
        _ => pyjson::dumps(value, SORTED).unwrap_or_default(),
    }
}

pub fn truthy(value: Option<&Value>) -> bool {
    value.is_some_and(|value| match value.get_type() {
        JsonType::Null => false,
        JsonType::Boolean => value.as_bool().expect("boolean"),
        JsonType::String => !value.as_str().expect("string").is_empty(),
        JsonType::Array => !value.as_array().expect("array").is_empty(),
        JsonType::Object => !value.as_object().expect("object").is_empty(),
        JsonType::Number => value.as_f64().is_some_and(|number| number != 0.0),
    })
}

pub fn metadata(request: &Value) -> Vec<(&'static str, Field)> {
    let sources: Vec<_> = SOURCE_FIELDS
        .iter()
        .filter(|key| truthy(request.get(**key)))
        .map(|key| (*key, request.get(*key).expect("source field")))
        .collect();
    let resource = if sources.is_empty() {
        vec![
            (
                "resource_kind",
                Field::Json(
                    HANDLE_FIELDS
                        .iter()
                        .find(|key| truthy(request.get(**key)))
                        .map_or(json!(null), |key| json!(key)),
                ),
            ),
            ("resource_count", Field::Json(json!(null))),
            ("resource_sha256", Field::Json(json!(null))),
        ]
    } else {
        let mut object = sonic_rs::Object::new();
        for (key, value) in &sources {
            object.insert(key, (*value).clone());
        }
        let digest = Sha256::digest(
            pyjson::dumps(&object.into_value(), SORTED)
                .unwrap_or_default()
                .as_bytes(),
        );
        vec![
            (
                "resource_kind",
                Field::Json(json!(sources
                    .iter()
                    .map(|(key, _)| *key)
                    .collect::<Vec<_>>()
                    .join("+"))),
            ),
            (
                "resource_count",
                Field::Json(json!(sources
                    .iter()
                    .map(|(_, value)| value.as_array().map_or(1, |values| values.len()))
                    .sum::<usize>())),
            ),
            (
                "resource_sha256",
                Field::Json(json!(format!("{digest:x}")[..16].to_owned())),
            ),
        ]
    };
    let mut fields = vec![
        ("operation", Field::Json(lookup(request, "operation"))),
        ("request_id", Field::Json(lookup(request, "id"))),
        (
            "deadline_unix_ms",
            Field::Json(lookup(request, "deadline_unix_ms")),
        ),
    ];
    fields.extend(resource);
    fields
}

pub fn lookup(value: &Value, key: &str) -> Value {
    value.get(key).cloned().unwrap_or_else(|| json!(null))
}

pub fn cursor_digest(cursor: Option<&Value>) -> Field {
    Field::Json(
        cursor
            .and_then(|cursor| cursor.as_str())
            .map_or(json!(null), |cursor| {
                json!(format!("{:x}", Sha256::digest(cursor.as_bytes())))
            }),
    )
}

pub fn info(message: &str, fields: &[(&'static str, Field)]) {
    write("INFO", message, fields);
}

pub fn warn(message: &str, fields: &[(&'static str, Field)]) {
    write("WARNING", message, fields);
}

fn write(level: &str, message: &str, fields: &[(&'static str, Field)]) {
    let rendered = fields
        .iter()
        .map(|(key, value)| format!("{key}={}", value.repr()))
        .collect::<Vec<_>>()
        .join(" ");
    let line = format!(
        "{} | {level:<8} | cc_transcript_owner - {message}: {rendered}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f")
    );
    let _ = std::io::stderr().lock().write_all(line.as_bytes());
}

#[cfg(target_os = "macos")]
pub fn heap_in_use_bytes() -> Option<usize> {
    #[repr(C)]
    #[derive(Default)]
    struct MallocStatistics {
        blocks_in_use: u32,
        size_in_use: usize,
        max_size_in_use: usize,
        size_allocated: usize,
    }
    extern "C" {
        fn malloc_zone_statistics(zone: *mut std::ffi::c_void, stats: *mut MallocStatistics);
    }
    let mut stats = MallocStatistics::default();
    // SAFETY: a null zone asks libmalloc to total every zone into the caller's struct.
    unsafe { malloc_zone_statistics(std::ptr::null_mut(), &mut stats) };
    Some(stats.size_in_use)
}

#[cfg(not(target_os = "macos"))]
pub fn heap_in_use_bytes() -> Option<usize> {
    None
}
