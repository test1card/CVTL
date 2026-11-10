use cvtl_safety::machine::{MachineState, OffEvidence, OutputState, SafetyMachine};
use cvtl_safety::{Branch, FaultReason};

fn verify_off(machine: &mut SafetyMachine) {
    let token = machine.request_off().unwrap();
    machine
        .confirm_off(token, Branch::A, OffEvidence::ReportedOff)
        .unwrap();
    machine
        .confirm_off(token, Branch::B, OffEvidence::MeasuredNearZero)
        .unwrap();
}

#[test]
fn both_off_then_two_operator_actions_and_both_on() {
    let mut machine = SafetyMachine::new();
    assert_eq!(machine.state(), MachineState::Disconnected);
    machine.connect().unwrap();
    assert_eq!(machine.output_state(), OutputState::Unknown);
    let off = machine.request_off().unwrap();
    machine
        .confirm_off(off, Branch::A, OffEvidence::ReportedOff)
        .unwrap();
    assert_eq!(machine.output_state(), OutputState::Unknown);
    assert!(machine.arm().is_err());
    machine
        .confirm_off(off, Branch::B, OffEvidence::MeasuredNearZero)
        .unwrap();
    assert_eq!(machine.state(), MachineState::OutputOffVerified);
    machine.arm().unwrap();
    assert_eq!(machine.state(), MachineState::Armed);
    let on = machine.start().unwrap();
    machine.confirm_on(on, Branch::A).unwrap();
    assert_ne!(machine.state(), MachineState::Running);
    machine.confirm_on(on, Branch::B).unwrap();
    assert_eq!(machine.state(), MachineState::Running);
    let off = machine.stop().unwrap();
    assert_eq!(machine.state(), MachineState::Stopping);
    assert_eq!(machine.output_state(), OutputState::Unknown);
    machine
        .confirm_off(off, Branch::A, OffEvidence::ReportedOff)
        .unwrap();
    machine
        .confirm_off(off, Branch::B, OffEvidence::ReportedOff)
        .unwrap();
    assert_eq!(machine.state(), MachineState::OutputOffVerified);
}

#[test]
fn stale_acknowledgements_cannot_restore_permission() {
    let mut machine = SafetyMachine::new();
    machine.connect().unwrap();
    let old = machine.request_off().unwrap();
    machine.disconnect();
    machine.connect().unwrap();
    let current = machine.request_off().unwrap();
    assert!(machine
        .confirm_off(old, Branch::A, OffEvidence::ReportedOff)
        .is_err());
    machine
        .confirm_off(current, Branch::A, OffEvidence::ReportedOff)
        .unwrap();
    machine
        .confirm_off(current, Branch::B, OffEvidence::ReportedOff)
        .unwrap();
    machine.arm().unwrap();
    let on = machine.start().unwrap();
    let off = machine.stop().unwrap();
    assert!(machine.confirm_on(on, Branch::A).is_err());
    machine
        .confirm_off(off, Branch::A, OffEvidence::ReportedOff)
        .unwrap();
    machine
        .confirm_off(off, Branch::B, OffEvidence::ReportedOff)
        .unwrap();
    assert_eq!(machine.state(), MachineState::OutputOffVerified);
    assert!(machine.start().is_err());
}

#[test]
fn reconnect_requires_fresh_off_evidence_and_new_arm() {
    let mut machine = SafetyMachine::new();
    machine.connect().unwrap();
    verify_off(&mut machine);
    machine.arm().unwrap();
    machine.disconnect();
    machine.connect().unwrap();
    assert_eq!(machine.state(), MachineState::Observing);
    assert_eq!(machine.output_state(), OutputState::Unknown);
    assert!(machine.start().is_err());
    assert!(machine.arm().is_err());
}

#[test]
fn fault_latches_first_cause_and_reset_never_starts() {
    let mut machine = SafetyMachine::new();
    machine.connect().unwrap();
    verify_off(&mut machine);
    machine.arm().unwrap();
    let on = machine.start().unwrap();
    let off = machine.latch_fault(FaultReason::JournalFailure);
    assert_eq!(machine.state(), MachineState::FaultLatched);
    assert_eq!(machine.output_state(), OutputState::Unknown);
    assert!(machine.confirm_on(on, Branch::A).is_err());
    assert!(machine.reset().is_err());
    machine.proof_unavailable(off).unwrap();
    assert!(machine.fallback_requested());
    assert_eq!(machine.fault_reason(), Some(FaultReason::JournalFailure));
    let fresh = machine.request_off().unwrap();
    machine
        .confirm_off(fresh, Branch::A, OffEvidence::ReportedOff)
        .unwrap();
    machine
        .confirm_off(fresh, Branch::B, OffEvidence::ReportedOff)
        .unwrap();
    assert_eq!(machine.state(), MachineState::FaultLatched);
    machine.reset().unwrap();
    assert_eq!(machine.state(), MachineState::OutputOffVerified);
    assert!(machine.start().is_err());
}

#[test]
fn loss_of_connection_discards_proof_and_preserves_fault() {
    let mut machine = SafetyMachine::new();
    machine.connect().unwrap();
    let old = machine.latch_fault(FaultReason::SourceLinkLost);
    machine
        .confirm_off(old, Branch::A, OffEvidence::ReportedOff)
        .unwrap();
    machine.disconnect();
    machine.connect().unwrap();
    assert_eq!(machine.state(), MachineState::FaultLatched);
    assert_eq!(machine.fault_reason(), Some(FaultReason::SourceLinkLost));
    assert_eq!(machine.output_state(), OutputState::Unknown);
    assert!(machine
        .confirm_off(old, Branch::B, OffEvidence::ReportedOff)
        .is_err());
    assert!(machine.reset().is_err());
}

#[test]
fn old_machine_tokens_cannot_confirm_new_machine_requests() {
    let mut old = SafetyMachine::new();
    old.connect().unwrap();
    let old_token = old.request_off().unwrap();
    let mut restarted = SafetyMachine::new();
    restarted.connect().unwrap();
    let current = restarted.request_off().unwrap();
    assert!(restarted
        .confirm_off(old_token, Branch::A, OffEvidence::ReportedOff)
        .is_err());
    restarted
        .confirm_off(current, Branch::A, OffEvidence::ReportedOff)
        .unwrap();
    assert_eq!(restarted.output_state(), OutputState::Unknown);
}

#[test]
fn link_loss_during_start_latches_fault_and_requests_fallback() {
    let mut machine = SafetyMachine::new();
    machine.connect().unwrap();
    verify_off(&mut machine);
    machine.arm().unwrap();
    let old = machine.start().unwrap();
    machine.disconnect();
    assert_eq!(machine.state(), MachineState::FaultLatched);
    assert_eq!(machine.fault_reason(), Some(FaultReason::SourceLinkLost));
    assert!(machine.fallback_requested());
    machine.connect().unwrap();
    assert!(machine.confirm_on(old, Branch::A).is_err());
    assert!(machine.reset().is_err());
}

#[test]
fn fault_link_loss_before_off_proof_requests_fallback_and_keeps_cause() {
    let mut machine = SafetyMachine::new();
    machine.connect().unwrap();
    let token = machine.latch_fault(FaultReason::JournalFailure);
    machine
        .confirm_off(token, Branch::A, OffEvidence::ReportedOff)
        .unwrap();
    machine.disconnect();
    assert!(machine.fallback_requested());
    assert_eq!(machine.fault_reason(), Some(FaultReason::JournalFailure));
    assert_eq!(machine.output_state(), OutputState::Unknown);
    machine.connect().unwrap();
    assert!(machine
        .confirm_off(token, Branch::B, OffEvidence::ReportedOff)
        .is_err());
}

#[test]
fn enable_permission_is_scoped_to_current_on_request() {
    let mut machine = SafetyMachine::new();
    machine.connect().unwrap();
    verify_off(&mut machine);
    machine.arm().unwrap();
    let first = machine.start().unwrap();
    assert!(machine.can_enable(first));
    let off = machine.stop().unwrap();
    assert!(!machine.can_enable(first));
    machine
        .confirm_off(off, Branch::A, OffEvidence::ReportedOff)
        .unwrap();
    machine
        .confirm_off(off, Branch::B, OffEvidence::ReportedOff)
        .unwrap();
    machine.arm().unwrap();
    let second = machine.start().unwrap();
    assert!(!machine.can_enable(first));
    assert!(machine.can_enable(second));
    machine.latch_fault(FaultReason::JournalFailure);
    assert!(!machine.can_enable(second));
    machine.disconnect();
    machine.connect().unwrap();
    assert!(!machine.can_enable(second));
}

#[test]
fn renewed_off_request_withdraws_verified_state_until_both_confirm() {
    let mut machine = SafetyMachine::new();
    machine.connect().unwrap();
    verify_off(&mut machine);
    let token = machine.request_off().unwrap();
    assert_eq!(machine.state(), MachineState::Stopping);
    assert_eq!(machine.output_state(), OutputState::Unknown);
    assert!(machine.arm().is_err());
    machine
        .confirm_off(token, Branch::A, OffEvidence::ReportedOff)
        .unwrap();
    assert_eq!(machine.state(), MachineState::Stopping);
    machine
        .confirm_off(token, Branch::B, OffEvidence::ReportedOff)
        .unwrap();
    assert_eq!(machine.state(), MachineState::OutputOffVerified);
}
