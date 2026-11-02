#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisteredReviewChange {
    Unchanged,
    Append { offset: u64 },
    Invalidated,
}

pub struct RegisteredReviewCheckpoint {
    pub stamp: SourceStamp,
    pub committed: u64,
    pub events: usize,
    pub boundary: Vec<u8>,
    pub epoch: [u8; 32],
}

impl RegisteredReviewCheckpoint {
    pub fn change(&self, current: SourceStamp, epoch: &[u8; 32]) -> RegisteredReviewChange {
        if self.stamp.identity.window_base != 0
            || current.identity.window_base != 0
            || self.epoch != *epoch
            || self.stamp.identity != current.identity
        {
            return RegisteredReviewChange::Invalidated;
        }
        if self.stamp == current {
            return RegisteredReviewChange::Unchanged;
        }
        if current.size <= self.stamp.size {
            return RegisteredReviewChange::Invalidated;
        }
        RegisteredReviewChange::Append {
            offset: self.committed,
        }
    }

    pub fn boundary_matches(&self, observed: &[u8]) -> bool {
        observed == self.boundary.as_slice()
    }

    pub fn change_with_boundary(
        &self,
        current: SourceStamp,
        epoch: &[u8; 32],
        observed: &[u8],
    ) -> RegisteredReviewChange {
        let change = self.change(current, epoch);
        if matches!(change, RegisteredReviewChange::Append { .. })
            && !self.boundary_matches(observed)
        {
            RegisteredReviewChange::Invalidated
        } else {
            change
        }
    }

    pub fn accounted_bytes(&self) -> usize {
        size_of::<Self>() + self.boundary.capacity()
    }
}

impl NativeStore {
    fn registered_review_epoch(
        &self,
        canonical: &Path,
        context: &Value,
        registered_policy: &Value,
    ) -> Result<[u8; 32], SnapshotError> {
        self.authority(context, Some(canonical))?;
        self.registry(context)?;
        let binding = json!({
            "schema":"registered-review-append/1",
            "engine":crate::BUILD_IDENTITY,
            "path":canonical.as_os_str().as_encoded_bytes(),
            "registry_generation":str_field(context, "registry_generation")?,
            "admission":str_field(context, "admission")?,
            "authority":&context["authority"],
            "registered_policy":registered_policy,
        });
        let canonical = crate::ids::canonical_json(&binding)
            .map_err(|error| invalid(format!("registered review binding: {error}")))?;
        if std::env::var_os("CC_TRANSCRIPT_REVIEW_BINDING_DIAGNOSTICS").is_some() {
            eprintln!("registered_review_binding={canonical}");
        }
        Ok(Sha256::digest(canonical.as_bytes()).into())
    }
}

#[cfg(test)]
mod registered_review_source_tests {
    use super::*;

    fn checkpoint() -> RegisteredReviewCheckpoint {
        RegisteredReviewCheckpoint {
            stamp: SourceStamp {
                identity: SourceIdentity {
                    device: 1,
                    inode: 2,
                    window_base: 0,
                },
                size: 100,
                mtime_ns: 10,
                ctime_ns: 20,
            },
            committed: 100,
            events: 1,
            boundary: vec![7; 64],
            epoch: [1; 32],
        }
    }

    #[test]
    fn registered_review_append_classification_keeps_explicit_invalidation_boundaries() {
        let prior = checkpoint();
        assert_eq!(
            prior.change(prior.stamp, &[1; 32]),
            RegisteredReviewChange::Unchanged
        );
        assert_eq!(
            prior.change(
                SourceStamp {
                    size: 122,
                    mtime_ns: 11,
                    ctime_ns: 21,
                    ..prior.stamp
                },
                &[1; 32]
            ),
            RegisteredReviewChange::Append { offset: 100 }
        );
        for changed in [
            SourceStamp {
                size: 99,
                ..prior.stamp
            },
            SourceStamp {
                mtime_ns: 11,
                ..prior.stamp
            },
            SourceStamp {
                ctime_ns: 21,
                ..prior.stamp
            },
            SourceStamp {
                identity: SourceIdentity {
                    inode: 3,
                    ..prior.stamp.identity
                },
                ..prior.stamp
            },
            SourceStamp {
                identity: SourceIdentity {
                    window_base: 1,
                    ..prior.stamp.identity
                },
                ..prior.stamp
            },
        ] {
            assert_eq!(
                prior.change(changed, &[1; 32]),
                RegisteredReviewChange::Invalidated
            );
        }
        assert_eq!(
            prior.change(prior.stamp, &[2; 32]),
            RegisteredReviewChange::Invalidated
        );
        assert!(prior.boundary_matches(&[7; 64]));
        assert!(!prior.boundary_matches(&[8; 64]));
        let appended = SourceStamp {
            size: 122,
            mtime_ns: 11,
            ctime_ns: 21,
            ..prior.stamp
        };
        assert_eq!(
            prior.change_with_boundary(appended, &[1; 32], &[7; 64]),
            RegisteredReviewChange::Append { offset: 100 }
        );
        assert_eq!(
            prior.change_with_boundary(appended, &[1; 32], &[8; 64]),
            RegisteredReviewChange::Invalidated
        );
    }
}
