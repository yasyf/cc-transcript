
impl NativeStore {
    fn classify_view_projection(
        &self, snapshot: Arc<TranscriptSnapshot>, request: &Value, context: &Value,
        cancel: &Cancellation, bounds: WorkLimits, next: usize,
    ) -> Result<Projection, SnapshotError> {
        plain_interaction_view(&request["view"])?;
        let classifier = request.get("classifier").ok_or_else(|| invalid("missing classifier"))?;
        let handle = &request["view"]["handle"];
        let current_path = std::fs::canonicalize(&snapshot.canonical_path).map_err(io_error)?;
        self.authority(context, Some(&current_path))?;
        cancel.check(bounds.deadline_unix_ms)?;
        let progress = self.classify(snapshot, classifier, context, cancel, &bounds, &mut [0;20])?;
        let data = if let Some(snapshot) = progress.snapshot {
            let current_path = std::fs::canonicalize(&snapshot.canonical_path).map_err(io_error)?;
            self.authority(context, Some(&current_path))?;
            cancel.check(bounds.deadline_unix_ms)?;
            let mut state = self.lock_state();
            let deadline = self.lease(&state, handle, context)?.absolute_deadline.min(bounds.deadline_unix_ms);
            let token = self.token("classified-view");
            let data = self.issue(&mut state, snapshot, classifier.clone(), context, deadline, token.clone(), 0)?;
            if let Err(error) = cancel.check(bounds.deadline_unix_ms) {
                state.leases.remove(&token);
                return Err(error);
            }
            data
        } else {
            Value::new()
        };
        let complete = !data.is_null();
        Ok(Projection {
            read_bytes: progress.read_bytes, events: progress.events, items: usize::from(complete), data,
            complete, next: (!complete).then_some(next+1),
            reason: (!complete).then(|| "classifier work incomplete".to_owned()),
        })
    }
}

#[derive(serde::Serialize)]
#[serde(untagged)]
enum InteractionWire<'a> {
    Key { key: &'a str },
    Winner { key: &'a str, present: bool, first: Option<usize>, name: Option<&'a str> },
    Call(crate::snapshot_codec::ToolUseWire<'a>),
}

fn plain_interaction_view(view: &Value) -> Result<(), SnapshotError> {
    for field in ["selectors", "attachments"] {
        if !view.get(field).and_then(Value::as_array).ok_or_else(|| invalid("missing interaction view scope"))?.is_empty() {
            return Err(invalid("interaction projections require an unselected local generation"));
        }
    }
    Ok(())
}

impl NativeStore {
    fn interaction_projection(
        &self, snapshot: Arc<TranscriptSnapshot>, request: &Value, context: &Value,
        cancel: &Cancellation, bounds: WorkLimits, next: usize,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Result<Projection, SnapshotError> {
        plain_interaction_view(&request["view"])?;
        let query = &request["query"];
        let kind = str_field(query, "kind")?;
        let name = if kind == "interaction_keys" { Some(str_field(query, "name")?) } else { None };
        let keys = if name.is_none() {
            let keys = query.get("keys").and_then(Value::as_array).ok_or_else(|| invalid("missing interaction keys"))?;
            if keys.len() > 256 { return Err(invalid("interaction keys exceed 256")); }
            Some(keys)
        } else { None };
        let scope = self.indexed_interactions(snapshot, context, cancel, bounds)?;
        let named = name.and_then(|name| scope.index.names.get(name));
        let total = keys.map_or_else(|| named.map_or(0, Vec::len), |keys| keys.len());
        let schema = match kind {
            "interaction_keys" => "cc-transcript.interaction-key/1",
            "interaction_winners" => "cc-transcript.interaction-winner/1",
            "interaction_records" => "cc-transcript.tool-use/1",
            _ => return Err(invalid("unsupported interaction projection")),
        };
        let empty = json!({"kind":"records","record_schema":schema,"records_json":[]});
        let mut output = encoded_size(&empty, bounds.max_output_bytes)?;
        let mut records = Vec::new();
        let mut read_bytes = scope.read_bytes;
        let mut events = scope.events;
        let mut at = next;
        let mut reason = "item_limit";
        while at < total && records.len() < bounds.max_items && events < bounds.max_events {
            cancel.check(bounds.deadline_unix_ms)?;
            let key = if let Some(keys) = keys {
                keys[at].as_str().ok_or_else(|| invalid("interaction keys must be strings"))?
            } else {
                named.expect("nonempty named index")[at].as_ref()
            };
            let winner = scope.winner(key);
            let name = winner.map(|winner| scope.snapshot.activity.call_name_at(winner.event, winner.ordinal)
                .expect("interaction winner has a call name"));
            let cost = key.len() + if kind == "interaction_records" {
                let winner = winner.ok_or_else(|| invalid("selected interaction key is absent"))?;
                scope.snapshot.activity.projected_call_at_bytes(winner.event, winner.ordinal).expect("indexed call")
            } else { size_of::<InteractionWinner>() + name.map_or(0, str::len) };
            if cost > bounds.max_read_bytes.saturating_sub(read_bytes) {
                reason = "read_limit";
                break;
            }
            read_bytes += cost;
            events += 1;
            let call = if kind == "interaction_records" {
                Some(scope.call(winner.expect("selected winner"), reservation, context)?)
            } else { None };
            let wire = match kind {
                "interaction_keys" => InteractionWire::Key { key },
                "interaction_winners" => InteractionWire::Winner { key, present: winner.is_some(), first: winner.map(|winner| winner.first), name },
                "interaction_records" => InteractionWire::Call(crate::snapshot_codec::ToolUseWire::new(call.as_ref().expect("selected call"), &scope.snapshot.session_id)),
                _ => unreachable!(),
            };
            let record_bytes = crate::snapshot_codec::encoded_size(&wire, bounds.max_output_bytes)?;
            self.extend_projection_reservation(reservation, context, record_bytes.saturating_mul(8) + vec_growth(&records, 1))?;
            let record = crate::snapshot_codec::encode(&wire, bounds.max_output_bytes)?;
            let quoted = encoded_size(&json!(&record), bounds.max_output_bytes)? + 1;
            if quoted > bounds.max_output_bytes.saturating_sub(output) {
                if records.is_empty() { return Err(SnapshotError::new(Status::OutputLimit, "interaction record exceeds page budget")); }
                reason = "output_limit";
                break;
            }
            output += quoted;
            records.push(record);
            at += 1;
        }
        let complete = at == total;
        let items = records.len();
        let data = json!({"kind":"records","record_schema":schema,"records_json":records});
        encoded_size(&data, bounds.max_output_bytes)?;
        Ok(Projection {
            read_bytes, events, items, data, complete,
            next: (!complete && reason != "read_limit").then_some(at), reason: (!complete).then(|| reason.to_owned()),
        })
    }
}

#[derive(serde::Serialize)]
struct MetaMessageWire<'a> {
    kind: &'static str,
    text: &'a str,
    ts: chrono::DateTime<chrono::FixedOffset>,
    is_meta: bool,
    source_tool_use_id: Option<&'a str>,
    source_tool_assistant_uuid: Option<&'a str>,
    origin: Option<&'a str>,
}

impl NativeStore {
    fn meta_message_projection(
        &self, snapshot: Arc<TranscriptSnapshot>, request: &Value, context: &Value,
        cancel: &Cancellation, bounds: WorkLimits, next: usize,
        reservation: &mut ProjectionReservation<'_>,
    ) -> Result<Projection, SnapshotError> {
        plain_interaction_view(&request["view"])?;
        let contains = match request["query"].get("contains") {
            None => None,
            Some(value) if value.is_null() => None,
            Some(value) => Some(value.as_str().ok_or_else(|| invalid("message contains must be a string"))?),
        };
        let scope = self.indexed_interactions(snapshot, context, cancel, bounds)?;
        let schema = "cc-transcript.meta-message/1";
        let mut records = Vec::new();
        let mut output = encoded_size(&json!({"kind":"records","record_schema":schema,"records_json":[]}), bounds.max_output_bytes)?;
        let mut read_bytes = scope.read_bytes;
        let mut events = scope.events;
        let mut at = next;
        let mut reason = "item_limit";
        while at < scope.index.messages.len() && records.len() < bounds.max_items && events < bounds.max_events {
            cancel.check(bounds.deadline_unix_ms)?;
            let entry = scope.snapshot.entry(scope.index.messages[at]);
            let length = match entry {
                Entry::User(user) => user.content.text_len(),
                Entry::Attachment(attachment) => match &attachment.detail {
                    crate::types::AttachmentDetail::QueuedCommand(queued) => queued.prompt.as_ref().map_or(0, String::len),
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            };
            let cost = length + size_of::<MetaMessageWire<'_>>();
            if cost > bounds.max_read_bytes.saturating_sub(read_bytes) { reason = "read_limit"; break; }
            read_bytes += cost;
            events += 1;
            if matches!(entry, Entry::User(user) if matches!(&user.content, crate::types::UserContent::Blocks(_))) && length > 0 {
                self.extend_projection_reservation(reservation, context, length)?;
            }
            let text = match entry {
                Entry::User(user) => match &user.content {
                    crate::types::UserContent::Plain(text) => Cow::Borrowed(text.as_str()),
                    crate::types::UserContent::Blocks(_) => Cow::Owned(user.content.text()),
                },
                Entry::Attachment(attachment) => match &attachment.detail {
                    crate::types::AttachmentDetail::QueuedCommand(queued) => Cow::Borrowed(queued.prompt.as_deref().unwrap_or("")),
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            };
            if contains.is_some_and(|literal| !text.contains(literal)) { at += 1; continue; }
            let wire = match entry {
                Entry::User(user) => MetaMessageWire {
                    kind: "user", text: &text, ts: user.meta.timestamp, is_meta: user.meta.is_meta,
                    source_tool_use_id: user.source_tool_use_id.as_deref(),
                    source_tool_assistant_uuid: user.source_tool_assistant_uuid.as_deref(), origin: None,
                },
                Entry::Attachment(attachment) => match &attachment.detail {
                    crate::types::AttachmentDetail::QueuedCommand(queued) => MetaMessageWire {
                        kind: "queued", text: &text, ts: attachment.meta.timestamp, is_meta: attachment.meta.is_meta,
                        source_tool_use_id: None, source_tool_assistant_uuid: None, origin: queued.origin.as_deref(),
                    },
                    _ => unreachable!(),
                },
                _ => unreachable!(),
            };
            let bytes = crate::snapshot_codec::encoded_size(&wire, bounds.max_output_bytes)?;
            self.extend_projection_reservation(reservation, context, bytes.saturating_mul(8) + vec_growth(&records, 1))?;
            let record = crate::snapshot_codec::encode(&wire, bounds.max_output_bytes)?;
            let quoted = encoded_size(&json!(&record), bounds.max_output_bytes)? + 1;
            if quoted > bounds.max_output_bytes.saturating_sub(output) {
                if records.is_empty() { return Err(SnapshotError::new(Status::OutputLimit, "meta message exceeds page budget")); }
                reason = "output_limit"; break;
            }
            output += quoted;
            records.push(record);
            at += 1;
        }
        let complete = at == scope.index.messages.len();
        let items = records.len();
        let data = json!({"kind":"records","record_schema":schema,"records_json":records});
        encoded_size(&data, bounds.max_output_bytes)?;
        Ok(Projection { read_bytes, events, items, data, complete,
            next: (!complete && reason != "read_limit").then_some(at), reason: (!complete).then(|| reason.to_owned()) })
    }
}

impl NativeStore {
    fn acquire_views(
        &self, request: &Value, context: &Value, cancel: &Cancellation, usage: &mut [u64; 20],
    ) -> Result<(Value, Option<String>, Option<String>), SnapshotError> {
        let bounds = limits(request)?;
        let handle = request.get("handle").ok_or_else(|| invalid("missing source handle"))?;
        let excerpt = request.get("excerpt").filter(|value| !value.is_null());
        let tail = request.get("tail_events").filter(|value| !value.is_null()).map(|value| {
            let count = value.as_u64().and_then(|value| usize::try_from(value).ok()).ok_or_else(|| invalid("tail event count must be a count"))?;
            if count == 0 || count > 256 { return Err(invalid("tail event count must be within 256")); }
            Ok(count)
        }).transpose()?;
        if excerpt.is_none() && tail.is_none() { return Err(invalid("source views require a selection")); }
        let _reservation = self.reserve_projection(context, 2 * (size_of::<(&str, Arc<TranscriptSnapshot>)>() + size_of::<String>() + size_of::<Value>() + 128))?;
        let mut views = Vec::with_capacity(2);
        let raw = self.raw_source_scope(handle, 16*1024*1024, context, cancel, bounds, usage)?;
        if let Some(selection) = excerpt {
            let forms = selection.get("forms").and_then(Value::as_array).ok_or_else(|| invalid("missing raw match forms"))?;
            if forms.len() > 256 { return Err(invalid("raw match forms exceed 256")); }
            let _forms_reservation = if forms.is_empty() { None } else {
                Some(self.reserve_projection(context, forms.len() * size_of::<&[u8]>())?)
            };
            let mut selected = Vec::with_capacity(forms.len());
            for form in forms.iter() { selected.push(form.as_str().ok_or_else(|| invalid("raw match forms must be strings"))?.as_bytes()); }
            let count = number(selection, "count")?;
            if count == 0 || count > 256 { return Err(invalid("raw match count must be within 256")); }
            let snapshot = self.raw_excerpt_from_scope(handle, &raw, &selected, number(selection, "around")?, count, context, cancel, bounds, usage)?;
            views.push(("excerpt", snapshot));
        }
        if let Some(count) = tail {
            views.push(("tail", self.raw_tail_from_scope(handle, &raw, count, context, cancel, bounds, usage)?));
        }
        self.raw_projection_source(handle, &raw, context, cancel, bounds)?;
        let mut state = self.lock_state();
        let source = self.lease(&state, handle, context)?;
        let deadline = source.absolute_deadline.min(bounds.deadline_unix_ms);
        let classifier = json!({"id":"native","version":"1"});
        let mut descriptions = Vec::with_capacity(views.len());
        let mut issued = Vec::with_capacity(views.len());
        for (label, snapshot) in views {
            let token = self.token("source-view");
            let result = cancel.check(bounds.deadline_unix_ms).and_then(|()| {
                self.issue(&mut state, snapshot, classifier.clone(), context, deadline, token.clone(), 0)
            });
            match result {
                Ok(data) => {
                    issued.push(token);
                    descriptions.push(json!({"label":label,"description":data["description"]}));
                }
                Err(error) => {
                    for token in issued { state.leases.remove(&token); }
                    return Err(error);
                }
            }
        }
        if let Err(error) = cancel.check(bounds.deadline_unix_ms) {
            for token in issued { state.leases.remove(&token); }
            return Err(error);
        }
        Ok((json!({"kind":"acquired_views","views":descriptions}), None, None))
    }
}
