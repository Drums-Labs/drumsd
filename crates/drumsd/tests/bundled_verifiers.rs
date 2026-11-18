//! The lab verifiers the release ships (A-13) hash to what install/lock.json approves.

use std::path::Path;

#[test]
fn every_bundled_verifier_matches_the_lock() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let lock: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join("install/lock.json")).unwrap()).unwrap();
    let vs = lock["verifiers"].as_object().expect("install/lock.json has a verifiers section");
    let mut n = 0;
    for (name, v) in vs.iter().filter(|(_, v)| v.is_object()) {
        let path = root.join(v["path"].as_str().unwrap());
        let src = std::fs::read(&path).unwrap_or_else(|e| panic!("{name}: {}: {e}", path.display()));
        assert_eq!(drumsd_core::verifiers::sha256_hex(&src), v["sha256"].as_str().unwrap(), "{name}: {} differs from the lock", path.display());
        drumsd_core::verifiers::check_source(&src, drumsd_core::verifiers::DEFAULT_MAX_BYTES).unwrap();
        n += 1;
    }
    assert!(vs.contains_key("drums_t6") && n >= 1);
}
