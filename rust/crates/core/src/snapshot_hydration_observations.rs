#[cfg(test)]
#[derive(Clone, Debug)]
enum HydrationObservation {
    Facts { stamp: SourceStamp, key: PreparedDiskKey, need: FactsNeed, disposition: &'static str },
    Read { stamp: SourceStamp, bytes: u64 },
    Codex { stamp: SourceStamp, previous: bool, raw: bool, index: bool, append: bool, fallback: Option<&'static str>, suffix_bytes: usize },
    RawVictim { identity: SourceIdentity, before_bytes: usize, after_bytes: usize, latest_before: bool, spine_before: bool, latest_after: bool, spine_after: bool },
}

impl NativeStore {
    #[cfg(not(test))]
    fn recent_codex_raw_bound(&self) -> usize {
        128 * 1024 * 1024
    }

    #[cfg(test)]
    fn recent_codex_raw_bound(&self) -> usize {
        self.recent_codex_raw_limit.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn observe_hydration(&self, observation: HydrationObservation) {
        if let Some(observations) = self.hydration_observations.lock().expect("hydration observations").as_mut() {
            assert!(observations.len() < 4096, "bounded hydration observation capacity");
            observations.push(observation);
        }
    }
}
