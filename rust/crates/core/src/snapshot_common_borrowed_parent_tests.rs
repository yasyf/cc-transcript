#[test]
fn registered_catalog_borrowed_parent_proof_rejects_stable_namespace_replacement_before_old_fd_metadata() {
    for observe_edge in [false,true] {
        let source = Source::new("");
        let namespace = source.directory.join("P");
        let replacement = source.directory.join("stagedP");
        let saved = source.directory.join("savedP");
        let parent = namespace.join("A");
        for root in [&namespace,&replacement] {
            let directory = root.join("A");
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(directory.join("target"),"target").unwrap();
            std::os::unix::fs::symlink("target",directory.join("link")).unwrap();
        }
        let parent = std::fs::canonicalize(&parent).unwrap();
        let reader = OpenDirectory::open(&parent).unwrap();
        let expected = reader.stamp();
        let store = store();
        let owner = unknown_dtype_restricted("borrowed-parent-proof",&parent);
        let cancel = Cancellation::default();
        let request = CatalogProofRequest { store:&store,context:&owner,cancel:&cancel,deadline_unix_ms:now_ms()+60_000 };
        let mut reservation = store.reserve_projection(&owner,4096).unwrap();
        let mut budget = CatalogBudget { store:&store,context:&owner,reservation:&mut reservation,attempt:1,scratch_bytes:0,observed_issue:false };
        let input = CatalogNameProofInput { reader:&reader.entries,name:OsStr::new("link"),physical_parent:&parent,
            expected_parent:expected,logical_target_hint:None };
        if observe_edge {
            let result = catalog_observe_symlink_edge(&request,input,
                CatalogLogicalRoute { scope:[0;64],listing_generation:1,raw_parent:&parent,order:&[0,0] },&mut budget).unwrap();
            assert!(matches!(result,CatalogSymlinkEdgeProof::CurrentView { .. }));
            assert_eq!(reader.entries.parent_metadata_attempts.load(Ordering::Relaxed),3);
        } else {
            assert!(matches!(catalog_prove_name_object(&request,input,&mut budget).unwrap(),CatalogNameObjectProof::Observed(_)));
            assert_eq!(reader.entries.parent_metadata_attempts.load(Ordering::Relaxed),2);
        }
        assert_eq!(store.directory_descriptor_metadata.load(Ordering::Relaxed),1,"authorized positive control must read actual NOFOLLOW object metadata");
        reader.entries.parent_metadata_attempts.store(0,Ordering::Relaxed);
        store.directory_descriptor_metadata.store(0,Ordering::Relaxed);
        std::fs::rename(&namespace,&saved).unwrap();
        std::fs::rename(&replacement,&namespace).unwrap();
        let saved_parent = saved.join("A");
        assert!(SourceStamp::of(&std::fs::metadata(&saved_parent).unwrap()) == expected,"ancestor rename preserves A listing generation");
        let current = SourceStamp::of(&std::fs::metadata(&parent).unwrap());
        assert!(current.identity != expected.identity,"current pathname must now denote distinct B");
        assert!(store.authority(&owner,Some(&parent)).is_ok());
        assert!(store.authority(&owner,Some(&saved_parent)).is_err(),"old A is now outside CURRENT restricted roots");
        if observe_edge {
            let result = catalog_observe_symlink_edge(&request,input,
                CatalogLogicalRoute { scope:[0;64],listing_generation:1,raw_parent:&parent,order:&[0,0] },&mut budget).unwrap();
            assert!(matches!(result,CatalogSymlinkEdgeProof::Unproven(_)));
        } else {
            assert!(matches!(catalog_prove_name_object(&request,input,&mut budget).unwrap(),CatalogNameObjectProof::Unproven(_)));
        }
        assert_eq!(reader.entries.parent_metadata_attempts.load(Ordering::Relaxed),0,"mismatch must reject BEFORE any old A fstat attempt");
        assert_eq!(store.directory_descriptor_metadata.load(Ordering::Relaxed),0,"mismatch must reject BEFORE any old A child fstatat attempt");
        assert!(matches!(catalog_open_proof_directory(&request,&parent,expected,&mut budget).unwrap(),CatalogProofDirectory::Unproven(_)),"fresh current B handle must also reject old expected A generation");
        assert_eq!(reader.entries.parent_metadata_attempts.load(Ordering::Relaxed),0);
        drop(budget);
        drop(reservation);
        store.assert_conserved();
    }
}
