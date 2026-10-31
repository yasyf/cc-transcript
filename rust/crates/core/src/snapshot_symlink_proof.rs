struct CatalogProofRequest<'a> {
    store: &'a NativeStore,
    context: &'a Value,
    cancel: &'a Cancellation,
    deadline_unix_ms: u64,
}

#[derive(Clone,Copy)]
struct CatalogNameProofInput<'a> {
    reader: &'a OpenedDirectory,
    name: &'a OsStr,
    physical_parent: &'a Path,
    expected_parent: SourceStamp,
    logical_target_hint: Option<&'a Path>,
}

struct CatalogLogicalRoute<'a> {
    scope: [u8;64],
    listing_generation: u64,
    raw_parent: &'a Path,
    order: &'a [usize],
}

struct CatalogPhysicalNameObject {
    parent_stamp: SourceStamp,
    metadata: CatalogChildMetadata,
    payload: Option<OsString>,
}

#[derive(Clone,Copy,PartialEq,Eq)]
struct CatalogEdgeRouteKey([u8;32]);

enum CatalogTargetObservation {
    Metadata { canonical: PathBuf, metadata: CatalogChildMetadata },
    ResolutionError { canonical: Option<PathBuf>, error: SnapshotError },
}

struct CatalogConditionalTargetEdge {
    route: CatalogEdgeRouteKey,
    object_stamp: SourceStamp,
    target: CatalogTargetObservation,
}

enum CatalogProofStage { PhysicalParent, LogicalParent, LogicalTarget }

enum CatalogUnprovenReason {
    ParentIdentityChanged,
    ParentNamespaceChanged,
    LogicalParentChanged,
    ObjectChanged,
    RouteChanged,
    IncompletePayload,
}

enum CatalogNameObjectProof {
    Observed(CatalogPhysicalNameObject),
    CurrentRefusal { stage: CatalogProofStage, error: SnapshotError },
    ObjectError(SnapshotError),
    Unproven(CatalogUnprovenReason),
}

enum CatalogSymlinkEdgeProof {
    CurrentView { object: CatalogPhysicalNameObject, edge: CatalogConditionalTargetEdge },
    NotSymlink(CatalogPhysicalNameObject),
    CurrentRefusal { stage: CatalogProofStage, error: SnapshotError },
    ObjectError(SnapshotError),
    Unproven(CatalogUnprovenReason),
}

enum CatalogProofDirectory {
    Opened(OpenDirectory),
    CurrentRefusal(SnapshotError),
    Unproven(CatalogUnprovenReason),
}

impl CatalogProofRequest<'_> {
    fn check(&self) -> Result<(),SnapshotError> {
        self.cancel.check(self.deadline_unix_ms)
    }
}

fn catalog_open_proof_directory(request: &CatalogProofRequest<'_>, physical_parent: &Path,
    expected_parent: SourceStamp, budget: &mut CatalogBudget<'_, '_>) -> Result<CatalogProofDirectory,SnapshotError> {
    request.check()?;
    if let Err(error) = request.store.authority(request.context,Some(physical_parent)) {
        return Ok(CatalogProofDirectory::CurrentRefusal(error));
    }
    budget.scratch(READ_DIR_HANDLE_BYTES + physical_parent.as_os_str().len() + FILESYSTEM_PATH_BYTES)?;
    let canonical = CatalogBinding::canonical_path(request.store,physical_parent)?;
    if let Err(error) = request.store.authority(request.context,Some(&canonical)) {
        return Ok(CatalogProofDirectory::CurrentRefusal(error));
    }
    if canonical != physical_parent {
        return Ok(CatalogProofDirectory::Unproven(CatalogUnprovenReason::ParentNamespaceChanged));
    }
    let directory = OpenDirectory::open(physical_parent)?;
    request.check()?;
    if directory.stamp() != expected_parent {
        return Ok(CatalogProofDirectory::Unproven(CatalogUnprovenReason::ParentIdentityChanged));
    }
    Ok(CatalogProofDirectory::Opened(directory))
}

fn catalog_prove_name_object(request: &CatalogProofRequest<'_>, input: CatalogNameProofInput<'_>,
    budget: &mut CatalogBudget<'_, '_>) -> Result<CatalogNameObjectProof,SnapshotError> {
    request.check()?;
    budget.scratch(7 * FILESYSTEM_PATH_BYTES)?;
    if let Err(error) = request.store.authority(request.context,Some(input.physical_parent)) {
        return Ok(CatalogNameObjectProof::CurrentRefusal { stage:CatalogProofStage::PhysicalParent,error });
    }
    if let Some(target) = input.logical_target_hint {
        if let Err(error) = request.store.authority(request.context,Some(target)) {
            return Ok(CatalogNameObjectProof::CurrentRefusal { stage:CatalogProofStage::LogicalTarget,error });
        }
    }
    let canonical = match CatalogBinding::canonical_path(request.store,input.physical_parent) {
        Ok(path) => path,
        Err(error) => return Ok(CatalogNameObjectProof::ObjectError(error)),
    };
    if let Err(error) = request.store.authority(request.context,Some(&canonical)) {
        return Ok(CatalogNameObjectProof::CurrentRefusal { stage:CatalogProofStage::PhysicalParent,error });
    }
    if canonical != input.physical_parent {
        return Ok(CatalogNameObjectProof::Unproven(CatalogUnprovenReason::ParentNamespaceChanged));
    }
    let parent = match std::fs::metadata(&canonical) {
        Ok(metadata) => SourceStamp::of(&metadata),
        Err(error) => return Ok(CatalogNameObjectProof::ObjectError(io_error(error))),
    };
    // Cached path text may now authorize a replacement B while the borrowed descriptor still
    // names forbidden A. Correlate the CURRENT authorized path with cached generation data
    // before attempting even parent metadata through that descriptor.
    if parent != input.expected_parent || input.reader.stamp != input.expected_parent {
        return Ok(CatalogNameObjectProof::Unproven(CatalogUnprovenReason::ParentNamespaceChanged));
    }
    request.check()?;
    let parent_stamp = match input.reader.current_stamp() {
        Ok(stamp) => stamp,
        Err(error) => return Ok(CatalogNameObjectProof::ObjectError(io_error(error))),
    };
    if parent_stamp != parent {
        return Ok(CatalogNameObjectProof::Unproven(CatalogUnprovenReason::ParentIdentityChanged));
    }
    let metadata = match input.reader.object_metadata(input.name,request.store) {
        Ok(metadata) => metadata,
        Err(error) => return Ok(CatalogNameObjectProof::ObjectError(io_error(error))),
    };
    request.check()?;
    if input.reader.current_stamp().map_err(io_error)? != parent_stamp {
        return Ok(CatalogNameObjectProof::Unproven(CatalogUnprovenReason::ParentNamespaceChanged));
    }
    Ok(CatalogNameObjectProof::Observed(CatalogPhysicalNameObject { parent_stamp,metadata,payload:None }))
}

fn catalog_observe_symlink_edge(request: &CatalogProofRequest<'_>, input: CatalogNameProofInput<'_>,
    route: CatalogLogicalRoute<'_>, budget: &mut CatalogBudget<'_, '_>) -> Result<CatalogSymlinkEdgeProof,SnapshotError> {
    request.check()?;
    budget.scratch(7 * FILESYSTEM_PATH_BYTES)?;
    let raw = catalog_child_path(route.raw_parent,input.name);
    match CatalogBinding::canonical_path(request.store,&raw) {
        Ok(canonical) => {
            if let Err(error) = request.store.authority(request.context,Some(&canonical)) {
                return Ok(CatalogSymlinkEdgeProof::CurrentRefusal { stage:CatalogProofStage::LogicalTarget,error });
            }
        }
        Err(error) if error.status == Status::Missing => {}
        Err(error) => return Ok(CatalogSymlinkEdgeProof::CurrentRefusal { stage:CatalogProofStage::LogicalTarget,error }),
    }
    let object = match catalog_prove_name_object(request,input,budget)? {
        CatalogNameObjectProof::Observed(object) => object,
        CatalogNameObjectProof::CurrentRefusal { stage,error } => return Ok(CatalogSymlinkEdgeProof::CurrentRefusal { stage,error }),
        CatalogNameObjectProof::ObjectError(error) => return Ok(CatalogSymlinkEdgeProof::ObjectError(error)),
        CatalogNameObjectProof::Unproven(reason) => return Ok(CatalogSymlinkEdgeProof::Unproven(reason)),
    };
    if !object.metadata.symlink { return Ok(CatalogSymlinkEdgeProof::NotSymlink(object)); }
    let parent_before = match catalog_proof_logical_parent(request,route.raw_parent) {
        Ok(parent) => parent,
        Err(error) => return Ok(CatalogSymlinkEdgeProof::CurrentRefusal { stage:CatalogProofStage::LogicalParent,error }),
    };
    if parent_before.0 != input.physical_parent || parent_before.1 != object.parent_stamp {
        return Ok(CatalogSymlinkEdgeProof::Unproven(CatalogUnprovenReason::LogicalParentChanged));
    }
    let before = match std::fs::symlink_metadata(&raw) {
        Ok(metadata) => CatalogChildMetadata::of(&metadata),
        Err(error) => return Ok(CatalogSymlinkEdgeProof::ObjectError(io_error(error))),
    };
    if !before.symlink || before.stamp != object.metadata.stamp {
        return Ok(CatalogSymlinkEdgeProof::Unproven(CatalogUnprovenReason::ObjectChanged));
    }
    request.check()?;
    let target = match CatalogBinding::canonical_path(request.store,&raw) {
        Ok(canonical) => {
            if let Err(error) = request.store.authority(request.context,Some(&canonical)) {
                return Ok(CatalogSymlinkEdgeProof::CurrentRefusal { stage:CatalogProofStage::LogicalTarget,error });
            }
            match std::fs::metadata(&canonical) {
                Ok(metadata) => CatalogTargetObservation::Metadata { canonical,metadata:CatalogChildMetadata::of(&metadata) },
                Err(error) => CatalogTargetObservation::ResolutionError { canonical:Some(canonical),error:io_error(error) },
            }
        }
        Err(error) => CatalogTargetObservation::ResolutionError { canonical:None,error },
    };
    request.check()?;
    let parent_after = match catalog_proof_logical_parent(request,route.raw_parent) {
        Ok(parent) => parent,
        Err(error) => return Ok(CatalogSymlinkEdgeProof::CurrentRefusal { stage:CatalogProofStage::LogicalParent,error }),
    };
    if parent_after != parent_before || input.reader.current_stamp().map_err(io_error)? != object.parent_stamp {
        return Ok(CatalogSymlinkEdgeProof::Unproven(CatalogUnprovenReason::LogicalParentChanged));
    }
    let after = match std::fs::symlink_metadata(&raw) {
        Ok(metadata) => CatalogChildMetadata::of(&metadata),
        Err(error) => return Ok(CatalogSymlinkEdgeProof::ObjectError(io_error(error))),
    };
    if !after.symlink || after.stamp != before.stamp {
        return Ok(CatalogSymlinkEdgeProof::Unproven(CatalogUnprovenReason::ObjectChanged));
    }
    let edge = CatalogConditionalTargetEdge { route:catalog_edge_route_key(&input,&route),
        object_stamp:object.metadata.stamp,target };
    Ok(CatalogSymlinkEdgeProof::CurrentView { object,edge })
}

fn catalog_proof_logical_parent(request: &CatalogProofRequest<'_>, raw: &Path) -> Result<(PathBuf,SourceStamp),SnapshotError> {
    request.check()?;
    let canonical = CatalogBinding::canonical_path(request.store,raw)?;
    request.store.authority(request.context,Some(&canonical))?;
    let metadata = std::fs::metadata(&canonical).map_err(io_error)?;
    Ok((canonical,SourceStamp::of(&metadata)))
}

fn catalog_edge_route_key(input: &CatalogNameProofInput<'_>, route: &CatalogLogicalRoute<'_>) -> CatalogEdgeRouteKey {
    let mut digest = Sha256::new();
    digest.update(b"cc-transcript/catalog-conditional-target/v1\0");
    digest.update(route.scope);
    digest.update(route.listing_generation.to_le_bytes());
    for path in [input.physical_parent.as_os_str(),route.raw_parent.as_os_str(),input.name] {
        let bytes = path.as_encoded_bytes();
        digest.update((bytes.len() as u64).to_le_bytes());
        digest.update(bytes);
    }
    digest.update(input.expected_parent.identity.device.to_le_bytes());
    digest.update(input.expected_parent.identity.inode.to_le_bytes());
    digest.update(input.expected_parent.ctime_ns.to_le_bytes());
    digest.update((route.order.len() as u64).to_le_bytes());
    for part in route.order { digest.update((*part as u64).to_le_bytes()); }
    CatalogEdgeRouteKey(digest.finalize().into())
}
