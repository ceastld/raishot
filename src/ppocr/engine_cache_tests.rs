use super::*;
use crate::recognition::task::{is_cancelled, CancelOwner};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

struct OwnedEngine {
    identity: &'static str,
    drops: Arc<AtomicUsize>,
}

impl Drop for OwnedEngine {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

fn candidate(identity: &'static str, drops: &Arc<AtomicUsize>) -> OwnedEngine {
    OwnedEngine {
        identity,
        drops: drops.clone(),
    }
}

#[test]
fn new_cache_has_no_implicitly_loaded_profile() {
    let cache: EngineCache<OwnedEngine> = EngineCache::default();
    assert!(cache.entry(OcrProfile::Tiny).is_none());
    assert!(cache.entry(OcrProfile::Small).is_none());
}

#[test]
fn publishing_tiny_does_not_load_small_or_alias_its_slot() {
    let mut cache = EngineCache::default();
    let owner = CancelOwner::new();
    let drops = Arc::new(AtomicUsize::new(0));
    cache
        .publish_cancellable(
            OcrProfile::Tiny,
            candidate("tiny-4", &drops),
            &owner.token(),
        )
        .unwrap();
    assert!(cache.entry(OcrProfile::Small).is_none());
    assert_eq!(
        cache.entry(OcrProfile::Tiny).as_ref().unwrap().identity,
        "tiny-4"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 0);
}

#[test]
fn cancelled_small_candidate_releases_itself_and_preserves_tiny() {
    let mut cache = EngineCache::default();
    let owner = CancelOwner::new();
    let cancelled = CancelOwner::new();
    let tiny_drops = Arc::new(AtomicUsize::new(0));
    let small_drops = Arc::new(AtomicUsize::new(0));
    cache
        .publish_cancellable(
            OcrProfile::Tiny,
            candidate("tiny-4", &tiny_drops),
            &owner.token(),
        )
        .unwrap();
    cancelled.cancel().unwrap();
    let error = cache
        .publish_cancellable(
            OcrProfile::Small,
            candidate("small-4", &small_drops),
            &cancelled.token(),
        )
        .unwrap_err();
    assert!(is_cancelled(error.as_ref()));
    assert_eq!(small_drops.load(Ordering::SeqCst), 1);
    assert_eq!(tiny_drops.load(Ordering::SeqCst), 0);
    assert!(cache.entry(OcrProfile::Small).is_none());
    assert_eq!(
        cache.entry(OcrProfile::Tiny).as_ref().unwrap().identity,
        "tiny-4"
    );
}

#[test]
fn selected_profile_replacement_releases_only_its_previous_engine() {
    let mut cache = EngineCache::default();
    let owner = CancelOwner::new();
    let tiny_drops = Arc::new(AtomicUsize::new(0));
    let old_small_drops = Arc::new(AtomicUsize::new(0));
    let new_small_drops = Arc::new(AtomicUsize::new(0));
    cache
        .publish_cancellable(
            OcrProfile::Tiny,
            candidate("tiny-4", &tiny_drops),
            &owner.token(),
        )
        .unwrap();
    cache
        .publish_cancellable(
            OcrProfile::Small,
            candidate("small-4", &old_small_drops),
            &owner.token(),
        )
        .unwrap();
    cache
        .publish_cancellable(
            OcrProfile::Small,
            candidate("small-2", &new_small_drops),
            &owner.token(),
        )
        .unwrap();
    assert_eq!(old_small_drops.load(Ordering::SeqCst), 1);
    assert_eq!(tiny_drops.load(Ordering::SeqCst), 0);
    assert_eq!(new_small_drops.load(Ordering::SeqCst), 0);
    assert_eq!(
        cache.entry(OcrProfile::Tiny).as_ref().unwrap().identity,
        "tiny-4"
    );
    assert_eq!(
        cache.entry(OcrProfile::Small).as_ref().unwrap().identity,
        "small-2"
    );
}

#[test]
fn cancelled_replacement_preserves_both_previously_successful_engines() {
    let mut cache = EngineCache::default();
    let owner = CancelOwner::new();
    let cancelled = CancelOwner::new();
    let drops = Arc::new(AtomicUsize::new(0));
    cache
        .publish_cancellable(OcrProfile::Tiny, candidate("tiny", &drops), &owner.token())
        .unwrap();
    cache
        .publish_cancellable(
            OcrProfile::Small,
            candidate("small", &drops),
            &owner.token(),
        )
        .unwrap();
    cancelled.cancel().unwrap();
    assert!(cache
        .publish_cancellable(
            OcrProfile::Small,
            candidate("replacement", &drops),
            &cancelled.token()
        )
        .is_err());
    assert_eq!(
        cache.entry(OcrProfile::Tiny).as_ref().unwrap().identity,
        "tiny"
    );
    assert_eq!(
        cache.entry(OcrProfile::Small).as_ref().unwrap().identity,
        "small"
    );
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}
