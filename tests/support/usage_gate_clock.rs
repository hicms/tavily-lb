use super::*;

#[test]
fn a_forward_wall_clock_jump_does_not_bypass_monotonic_deadlines() {
    let dir = tempfile::tempdir().unwrap();
    let gate = UsageGate::open(&dir.path().join("state.json")).unwrap();
    drop(gate.reserve("key-a", "ip-a").unwrap());
    // Simulate wall timestamps becoming past due, without changing the machine clock.
    gate.state
        .lock()
        .unwrap()
        .next
        .values_mut()
        .for_each(|until| *until = 0);
    drop(gate.reserve("key-b", "ip-b").unwrap());
    assert!(matches!(
        gate.reserve("key-a", "ip-c"),
        Err(GateError::Limited(_))
    ));
    assert!(matches!(
        gate.reserve("key-c", "ip-a"),
        Err(GateError::Limited(_))
    ));
}

#[test]
fn expired_deadlines_still_block_in_flight_requests_then_release() {
    let dir = tempfile::tempdir().unwrap();
    let gate = UsageGate::open(&dir.path().join("state.json")).unwrap();
    let permit = gate.reserve("key", "ip").unwrap();
    {
        let mut state = gate.state.lock().unwrap();
        state.next.values_mut().for_each(|until| *until = 0);
        state
            .monotonic
            .values_mut()
            .for_each(|until| *until = Instant::now() - Duration::from_secs(1));
    }
    assert!(matches!(
        gate.reserve("key", "ip"),
        Err(GateError::Limited(_))
    ));
    drop(permit);
    assert!(gate.reserve("key", "ip").is_ok());
}
