use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use cc_transcript_core::snapshot::Cancellation;
use crossbeam_channel::{unbounded, Sender};
use sonic_rs::{json, JsonValueTrait, Value};

use crate::frame::{self, ProtocolError};
use crate::owner::{failure, Owner, HOST_SCHEMA};
use crate::pyjson::{self, check_ingress, WIRE};

const LANES: [(&str, usize, usize); 4] = [
    ("hook", 8, 64),
    ("review", 2, 16),
    ("release", 4, 32),
    ("warm", 1, 4),
];

struct Job {
    id: u64,
    request: Value,
    context: Value,
    cancel: Cancellation,
    tool_registry: Value,
    queued: Instant,
}

struct Lane {
    sender: Mutex<Option<Sender<Job>>>,
    used: AtomicUsize,
    slots: usize,
}

enum WriteError {
    OutputLimit,
    Fatal(String),
}

impl WriteError {
    fn reason(self) -> String {
        match self {
            Self::OutputLimit => "snapshot frame exceeds encoded byte bound".to_owned(),
            Self::Fatal(reason) => reason,
        }
    }
}

pub struct Service<W: Write + Send + 'static> {
    owner: Owner,
    output: Mutex<W>,
    pending: Mutex<HashMap<u64, Cancellation>>,
    failed: Mutex<Option<String>>,
}

impl<W: Write + Send + 'static> Service<W> {
    pub fn new(owner: Owner, output: W) -> Arc<Self> {
        Arc::new(Self {
            owner,
            output: Mutex::new(output),
            pending: Mutex::new(HashMap::new()),
            failed: Mutex::new(None),
        })
    }

    fn write(&self, message: &Value) -> Result<(), WriteError> {
        let body = pyjson::dumps(message, WIRE).map_err(WriteError::Fatal)?;
        let encoded = frame::encode(&body).ok_or(WriteError::OutputLimit)?;
        let mut output = self.output.lock().expect("owner output");
        output
            .write_all(&encoded)
            .and_then(|()| output.flush())
            .map_err(|error| {
                WriteError::Fatal(format!("snapshot output closed during frame: {error}"))
            })?;
        self.owner.store.record_transport(encoded.len());
        Ok(())
    }

    fn result(&self, id: u64, response: Value, context: &Value) -> Result<(), String> {
        let wrap = |response: &Value| {
            json!({
                "protocol": 1,
                "op": "snapshot_result",
                "id": id,
                "snapshot": {"schema": HOST_SCHEMA, "response": response},
            })
        };
        match self.write(&wrap(&response)) {
            Ok(()) => Ok(()),
            Err(WriteError::OutputLimit) => {
                let _ = self.owner.discard(&response, context);
                let fallback = failure(
                    response["id"].as_str().unwrap_or_default(),
                    "output_limit",
                    "whole snapshot response exceeds frame bound",
                    response.get("usage"),
                );
                self.write(&wrap(&fallback)).map_err(WriteError::reason)
            }
            Err(error) => {
                let _ = self.owner.discard(&response, context);
                Err(error.reason())
            }
        }
    }

    fn done(&self, job: Job, response: Value, lane: &Lane) {
        if let Err(reason) = self.result(job.id, response, &job.context) {
            eprintln!("snapshot owner result failed: {reason}");
            let error = json!({
                "protocol": 1,
                "op": "error",
                "id": job.id,
                "error": reason.chars().take(4096).collect::<String>(),
            });
            if let Err(error) = self.write(&error) {
                *self.failed.lock().expect("owner failure") = Some(error.reason());
            }
        }
        self.pending.lock().expect("owner pending").remove(&job.id);
        lane.used.fetch_sub(1, Ordering::AcqRel);
    }

    fn lane_of(request: &Value, context: &Value) -> Result<&'static str, ProtocolError> {
        let invalid = |reason: &str| ProtocolError::Invalid(reason.to_owned());
        Ok(
            match request["operation"]
                .as_str()
                .ok_or_else(|| invalid("snapshot request has no operation"))?
            {
                "release" => "release",
                "warm_registered" | "warm_root" => "warm",
                "prepare_review" | "prepare_corrections" => "review",
                _ => match context["admission"].as_str() {
                    Some("hook") => "hook",
                    Some("review") => "review",
                    _ => return Err(invalid("snapshot context has no admission lane")),
                },
            },
        )
    }

    pub fn run(self: &Arc<Self>, input: &mut impl Read) -> Result<(), ProtocolError> {
        let mut workers = Vec::new();
        let lanes: HashMap<&'static str, Arc<Lane>> = LANES
            .iter()
            .map(|&(name, count, slots)| {
                let (sender, receiver) = unbounded::<Job>();
                let lane = Arc::new(Lane {
                    sender: Mutex::new(Some(sender)),
                    used: AtomicUsize::new(0),
                    slots,
                });
                for index in 0..count {
                    let receiver = receiver.clone();
                    let service = Arc::clone(self);
                    let lane = Arc::clone(&lane);
                    workers.push(
                        std::thread::Builder::new()
                            .name(format!("capt-evidence-{name}-{index}"))
                            .spawn(move || {
                                for job in receiver {
                                    let response = service.owner.call(
                                        &job.request,
                                        job.context.clone(),
                                        &job.cancel,
                                        &job.tool_registry,
                                        job.queued.elapsed(),
                                    );
                                    service.done(job, response, &lane);
                                }
                            })
                            .expect("lane worker"),
                    );
                }
                (name, lane)
            })
            .collect();
        let outcome = self.serve(input, &lanes);
        for cancel in self.pending.lock().expect("owner pending").values() {
            cancel.cancel();
        }
        for lane in lanes.values() {
            lane.sender.lock().expect("lane sender").take();
        }
        for worker in workers {
            let _ = worker.join();
        }
        outcome
    }

    fn serve(
        &self,
        input: &mut impl Read,
        lanes: &HashMap<&'static str, Arc<Lane>>,
    ) -> Result<(), ProtocolError> {
        while let Some((frame, size)) = frame::read_frame(input)? {
            self.owner.store.record_transport(size);
            if let Some(failed) = self.failed.lock().expect("owner failure").take() {
                return Err(ProtocolError::Invalid(failed));
            }
            self.submit(frame, lanes)?;
        }
        Ok(())
    }

    fn submit(
        &self,
        frame: Value,
        lanes: &HashMap<&'static str, Arc<Lane>>,
    ) -> Result<(), ProtocolError> {
        let invalid = |reason: &str| ProtocolError::Invalid(reason.to_owned());
        let id = frame::protocol_id(&frame)
            .ok_or_else(|| invalid("invalid snapshot owner frame identity"))?;
        let keys = frame::keys(&frame);
        if frame["op"].as_str() == Some("snapshot_cancel") {
            if keys != ["id", "op", "protocol"] {
                return Err(invalid("invalid snapshot cancellation fields"));
            }
            if let Some(cancel) = self.pending.lock().expect("owner pending").get(&id) {
                cancel.cancel();
            }
            return Ok(());
        }
        if frame["op"].as_str() != Some("snapshot_request")
            || keys != ["id", "op", "protocol", "snapshot", "snapshot_context"]
        {
            return Err(invalid("invalid snapshot request frame fields"));
        }
        check_ingress(&frame["snapshot"]).map_err(ProtocolError::Invalid)?;
        check_ingress(&frame["snapshot_context"]).map_err(ProtocolError::Invalid)?;
        let request = frame["snapshot"]
            .get("request")
            .cloned()
            .ok_or_else(|| invalid("snapshot request frame has no request"))?;
        let tool_registry = frame["snapshot"]
            .get("tool_registry")
            .cloned()
            .ok_or_else(|| invalid("snapshot request frame has no tool registry"))?;
        let context = frame["snapshot_context"].clone();
        let lane = &lanes[Self::lane_of(&request, &context)?];
        let mut pending = self.pending.lock().expect("owner pending");
        if pending.contains_key(&id) {
            return Err(invalid("duplicate live snapshot frame identity"));
        }
        if lane
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < lane.slots).then_some(used + 1)
            })
            .is_err()
        {
            drop(pending);
            let refused = failure(
                request["id"].as_str().unwrap_or_default(),
                "retained_limit",
                "snapshot owner admission slots are occupied",
                None,
            );
            return self
                .result(id, refused, &context)
                .map_err(ProtocolError::Invalid);
        }
        let cancel = Cancellation::default();
        pending.insert(id, cancel.clone());
        drop(pending);
        lane.sender
            .lock()
            .expect("lane sender")
            .as_ref()
            .expect("live lane")
            .send(Job {
                id,
                request,
                context,
                cancel,
                tool_registry,
                queued: Instant::now(),
            })
            .expect("lane worker alive");
        Ok(())
    }
}
