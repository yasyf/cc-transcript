mod frame;
mod log;
mod owner;
mod pyjson;
mod review_events;
mod scope;
mod service;

use std::io::{self, Write};
use std::process::ExitCode;

use cc_transcript_core::snapshot::NativeStore;
use cc_transcript_core::snapshot_input::{native_value, normalize_numbers};
use sonic_rs::{json, JsonValueTrait, Value};

use crate::frame::{read_frame, ProtocolError};
use crate::pyjson::WIRE;

const CONFIG_SCHEMA: &str =
    include_str!("../../../../cc_transcript/snapshot_schema/config.schema.json");

fn invalid(reason: impl Into<String>) -> ProtocolError {
    ProtocolError::Invalid(reason.into())
}

fn build() -> Result<String, ProtocolError> {
    let mut arguments = std::env::args().skip(1);
    match (
        arguments.next().as_deref(),
        arguments.next(),
        arguments.next(),
    ) {
        (Some("--build"), Some(build), None) => Ok(build),
        _ => Err(invalid(
            "usage: cc-transcript-owner --build <capt-hook version>",
        )),
    }
}

fn config(value: &Value) -> Result<Value, ProtocolError> {
    let schema: serde_json::Value =
        serde_json::from_str(CONFIG_SCHEMA).expect("config schema JSON");
    let mut config: serde_json::Value = serde_json::from_str(&value.to_string())
        .map_err(|_| invalid("malformed snapshot config"))?;
    normalize_numbers(&mut config).map_err(|error| invalid(error.reason))?;
    jsonschema::validator_for(&schema)
        .expect("embedded config schema is valid")
        .validate(&config)
        .map_err(|error| invalid(format!("snapshot config does not match schema: {error}")))?;
    Ok(native_value(&config))
}

fn handshake(
    build: &str,
    input: &mut impl io::Read,
    output: &mut impl Write,
) -> Result<Option<Value>, ProtocolError> {
    let Some((hello, _)) = read_frame(input)? else {
        return Ok(None);
    };
    if frame::keys(&hello) != ["build", "op", "protocol", "snapshot_config"]
        || frame::integer(&hello["protocol"]) != Some(1)
        || hello["op"].as_str() != Some("hello")
        || hello["build"].as_str() != Some(build)
    {
        return Err(invalid("snapshot owner requires exact build handshake"));
    }
    let config = config(&hello["snapshot_config"])?;
    let reply = pyjson::dumps(&json!({"protocol": 1, "op": "hello", "build": build}), WIRE)
        .map_err(invalid)?;
    output.write_all(&frame::encode(&reply).expect("hello frame"))?;
    output.flush()?;
    Ok(Some(config))
}

fn serve() -> Result<(), ProtocolError> {
    let build = build()?;
    let mut input = io::stdin().lock();
    let mut output = io::stdout();
    let Some(config) = handshake(&build, &mut input, &mut output)? else {
        return Ok(());
    };
    if std::env::var_os("CC_TRANSCRIPT_PARSE_THREADS").is_none() {
        std::env::set_var("CC_TRANSCRIPT_PARSE_THREADS", "4");
    }
    let store = NativeStore::new_agent(&config).map_err(|error| invalid(error.reason))?;
    let owner = owner::Owner::new(store).map_err(|error| invalid(error.reason))?;
    service::Service::new(owner, output).run(&mut input)
}

fn main() -> ExitCode {
    std::panic::set_hook(Box::new(|panic| {
        eprintln!("snapshot owner panicked: {panic}");
        std::process::exit(1);
    }));
    match serve() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("snapshot owner failed: {error}");
            ExitCode::FAILURE
        }
    }
}
