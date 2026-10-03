//! Manual smoke test against REAL subagent data on this machine — same
//! `#[ignore]` convention as `external_import_real_smoke.rs`.

#[test]
#[ignore = "depends on real subagent directories on this machine"]
fn scans_a_real_session_with_known_subagents() {
    let path = std::path::Path::new(
        "/Users/mikey/.claude/projects/-Users-mikey-Projects-cofactr-workspace/13371e6c-e8b4-4c61-a772-da80b5909352.jsonl",
    );
    let result = zeron_engine::subagent_scan::scan_subagents(path).expect("scan");
    eprintln!("found {} subagents", result.len());
    for s in result.iter().take(5) {
        eprintln!("{s:?}");
    }
    assert!(
        !result.is_empty(),
        "this session has real subagent directories on disk"
    );
}
