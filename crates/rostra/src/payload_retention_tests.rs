use rostra_core::id::RostraIdSecretKey;
use serde_json::json;

use crate::payload_retention::parse;

#[test]
fn startup_json_requires_explicit_validated_mode_policy_and_budgets() {
    let id = RostraIdSecretKey::generate().id().to_string();
    let enforce = json!([{
        "id": id,
        "retention": {
            "mode": "enforce",
            "policy": { "size_floor": 1, "tau_seconds": 1, "alpha_q16": 0,
                "beta_q16": 0, "max_bonus": 1, "grace_seconds": 0 },
            "admission": { "database_bytes": 1000, "author_bytes": 100,
                "overrides": {}, "in_flight_count": 8, "in_flight_bytes": 1000 },
            "worker": { "operations": 32, "bytes": 1000, "gc_bytes": 1000, "time_ms": 20 }
        }
    }]);
    let account = parse(&serde_json::to_vec(&enforce).unwrap())
        .unwrap()
        .remove(0);
    let held = account.reserve_payload_allocation(1000).unwrap().unwrap();
    assert!(account.reserve_payload_allocation(1).is_err());
    drop(held);
    for field in ["mode", "policy", "admission", "worker"] {
        let mut missing = enforce.clone();
        missing[0]["retention"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert!(parse(&serde_json::to_vec(&missing).unwrap()).is_err());
    }
    let mut unknown = enforce.clone();
    unknown[0]["retention"]["worker"]["typo"] = json!(1);
    assert!(parse(&serde_json::to_vec(&unknown).unwrap()).is_err());
    let duplicate = json!([enforce[0], enforce[0]]);
    assert!(parse(&serde_json::to_vec(&duplicate).unwrap()).is_err());
    let mut invalid = enforce.clone();
    invalid[0]["retention"]["worker"]["time_ms"] = json!(0);
    assert!(parse(&serde_json::to_vec(&invalid).unwrap()).is_err());
    let mut dry = enforce;
    let mode = dry[0]["retention"].as_object_mut().unwrap();
    mode.insert("mode".into(), json!("dry-run"));
    mode.remove("worker");
    mode.insert(
        "snapshot".into(),
        json!({
            "events": 100, "authors": 10, "logical_bytes": 10000, "time_ms": 20
        }),
    );
    let account = parse(&serde_json::to_vec(&dry).unwrap()).unwrap().remove(0);
    assert!(
        account
            .reserve_payload_allocation(u64::MAX)
            .unwrap()
            .is_none()
    );
    assert!(parse(b"[]").unwrap().is_empty());
}
