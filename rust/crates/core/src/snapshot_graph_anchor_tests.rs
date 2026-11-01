#[derive(Clone, Copy, Debug, Default)]
struct PreparedGraphAnchorCost {
    calls: usize,
    fields: usize,
    state_held: usize,
}

struct PreparedGraphAnchorTrace {
    store: Arc<NativeStore>,
    cost: PreparedGraphAnchorCost,
}

std::thread_local! {
    static PREPARED_GRAPH_ANCHOR_TRACE: std::cell::RefCell<Option<PreparedGraphAnchorTrace>> = const { std::cell::RefCell::new(None) };
}

struct PreparedGraphAnchorRecording;

impl PreparedGraphAnchorRecording {
    fn start(store: Arc<NativeStore>) -> Self {
        PREPARED_GRAPH_ANCHOR_TRACE.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(PreparedGraphAnchorTrace {
                store,
                cost: PreparedGraphAnchorCost::default(),
            });
        });
        Self
    }

    fn finish(self) -> PreparedGraphAnchorCost {
        PREPARED_GRAPH_ANCHOR_TRACE.with(|slot| slot.borrow_mut().take().unwrap().cost)
    }
}

impl Drop for PreparedGraphAnchorRecording {
    fn drop(&mut self) {
        PREPARED_GRAPH_ANCHOR_TRACE.with(|slot| {
            slot.borrow_mut().take();
        });
    }
}

pub(super) fn record_prepared_graph_input_anchors(sources: usize, directories: usize) {
    PREPARED_GRAPH_ANCHOR_TRACE.with(|slot| {
        if let Some(trace) = slot.borrow_mut().as_mut() {
            trace.cost.calls += 1;
            trace.cost.fields += sources + directories;
            trace.cost.state_held += usize::from(trace.store.state.try_lock().is_err());
        }
    });
}

fn prepared_graph_anchor_cost_case(count: usize) -> (PreparedGraphAnchorCost, PreparedGraphAnchorCost) {
    let source = Source::new(&format!("{}\n", user("anchor-root")));
    let paths: Vec<_> = (0..count)
        .map(|index| {
            let path = source.directory.join(format!("member-{index:04}.jsonl"));
            std::fs::write(&path, format!("{}\n", user(&format!("anchor-{index}")))).unwrap();
            path.to_string_lossy().into_owned()
        })
        .collect();
    let store = Arc::new(store());
    let owner = context("anchor-cost");
    let root = finish(
        &store,
        store.request(&acquire(&source.path), &owner, &Cancellation::default()),
        &owner,
    );
    let mut request = prepare_request(&root);
    request["direct_paths"] = json!(paths);
    request["limits"]["max_sources"] = json!(count + 1);
    let recording = PreparedGraphAnchorRecording::start(Arc::clone(&store));
    let prepared = finish_prepared(
        &store,
        store.request(&request, &owner, &Cancellation::default()),
        &owner,
    );
    let publication = recording.finish();
    assert_eq!(prepared["status"].as_str(), Some("ok"), "{prepared:?}");
    {
        let state = store.lock_state();
        let graph = state.prepared_graphs[prepared["data"]["handle"]["graph_id"].as_str().unwrap()]
            .lock()
            .unwrap();
        assert_eq!(graph.sources.len(), count);
        assert_eq!(graph.sidechain_dirs.len(), count + 1);
    }
    let recording = PreparedGraphAnchorRecording::start(Arc::clone(&store));
    release_graph(&store, &prepared, &owner);
    let release = recording.finish();
    store.assert_conserved();
    eprintln!(
        "PREPARED_GRAPH_ANCHOR_COST sources={count} directories={} publication_calls={} publication_fields={} publication_state_held={} release_calls={} release_fields={} release_state_held={}",
        count + 1,
        publication.calls,
        publication.fields,
        publication.state_held,
        release.calls,
        release.fields,
        release.state_held,
    );
    (publication, release)
}

fn assert_prepared_graph_anchor_cost(count: usize) {
    let (publication, release) = prepared_graph_anchor_cost_case(count);
    assert_eq!(
        publication.calls + release.calls,
        1,
        "prepared graph repeats immutable roster byte accounting"
    );
    assert_eq!(publication.fields, count + count + 1);
    assert_eq!(release.fields, 0);
}

#[test]
fn prepared_graph_anchor_cost_zero() {
    assert_prepared_graph_anchor_cost(0);
}

#[test]
fn prepared_graph_anchor_cost_small() {
    assert_prepared_graph_anchor_cost(32);
}

#[test]
fn prepared_graph_anchor_cost_923() {
    assert_prepared_graph_anchor_cost(923);
}

#[test]
fn prepared_graph_anchor_walks_do_not_hold_shared_state() {
    let (publication, release) = prepared_graph_anchor_cost_case(923);
    assert_eq!(
        publication.state_held + release.state_held,
        0,
        "prepared graph counts roster bytes under shared state"
    );
}

#[test]
fn prepared_graph_anchor_owners_preserve_slice_aliases() {
    let source = Source::new(&format!("{}\n", user("anchor-alias-root")));
    let store = store();
    let owner = context("anchor-alias");
    let root = finish(
        &store,
        store.request(&acquire(&source.path), &owner, &Cancellation::default()),
        &owner,
    );
    let prepared = finish_prepared(
        &store,
        store.request(&prepare_request(&root), &owner, &Cancellation::default()),
        &owner,
    );
    assert_eq!(prepared["status"].as_str(), Some("ok"), "{prepared:?}");
    let graph_id = prepared["data"]["handle"]["graph_id"].as_str().unwrap().to_owned();
    let graph = store.lock_state().remove_prepared_graph(&graph_id).unwrap();
    let Ok(graph) = Arc::try_unwrap(graph) else {
        panic!("prepared graph has another owner");
    };
    let Ok(mut graph) = graph.into_inner() else {
        panic!("prepared graph is poisoned");
    };
    for name in ["alias-one", "alias-two"] {
        let key = name.to_owned();
        graph.root_slices.reserve_for(&key);
        graph.root_slices.insert(key, Arc::clone(&graph.root_facts));
    }
    let root_identity = facts_key(&graph.root_facts);
    let root_bytes = graph.root_facts.accounted_bytes();
    let root_weak = Arc::downgrade(&graph.root_facts);
    assert_eq!(graph.anchors().filter(|anchor| anchor.id == root_identity).count(), 3);
    let revision = graph.revision.clone();
    let bytes = {
        let state = store.lock_state();
        charged_bytes(&graph_id, &graph)
            + state.ledger.shared.unowned_bytes(graph.anchors())
            + state.ledger.shared.growth(graph.anchors())
            + state.prepared_graphs.growth_for(&graph_id)
    };
    let mut reservation = store.reserve_projection(&owner, bytes).unwrap();
    let (data, cursor, load) = store
        .publish_prepared_graph(graph_id, graph, revision, &owner, &mut reservation)
        .unwrap();
    assert!(cursor.is_none() && load.is_none());
    drop(reservation);
    store.assert_conserved();
    release_graph(&store, &json!({"data": data}), &owner);
    {
        let mut state = store.lock_state();
        let before = state.ledger.shared.facts();
        let keys: Vec<_> = state
            .prepared_facts
            .iter()
            .filter(|(_, cached)| facts_key(&cached.facts) == root_identity)
            .map(|(key, _)| *key)
            .collect();
        assert!(!keys.is_empty());
        for key in keys {
            drop(state.remove_prepared_facts(&key).unwrap());
        }
        assert_eq!(state.ledger.shared.facts(), before - root_bytes);
    }
    assert!(root_weak.upgrade().is_none());
    store.assert_conserved();
}
