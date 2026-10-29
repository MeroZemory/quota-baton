use super::*;

fn sample_export() -> Export {
    Export {
        recipientpid: std::process::id(),
        nonce: vec![7; 32],
        sockets: [
            SocketInfo {
                role: "data".into(),
                endpoint: "127.0.0.1:10200".parse().unwrap(),
                protocol_info: vec![0; PROTOCOL_SIZE],
            },
            SocketInfo {
                role: "admin".into(),
                endpoint: "127.0.0.1:10201".parse().unwrap(),
                protocol_info: vec![1; PROTOCOL_SIZE],
            },
        ],
    }
}

#[test]
fn rejects_wrong_roles_endpoints_sizes_and_reused_capabilities_before_import() {
    let ports = [10200, 10201];
    let mut export = sample_export();
    assert!(validate_export(&export, ports).is_ok());
    export.sockets.swap(0, 1);
    assert!(validate_export(&export, ports).is_err());
    export.sockets.swap(0, 1);
    export.sockets[0].endpoint = "0.0.0.0:10200".parse().unwrap();
    assert!(validate_export(&export, ports).is_err());
    export.sockets[0].endpoint = "127.0.0.1:10200".parse().unwrap();
    export.sockets[0].protocol_info.pop();
    assert!(validate_export(&export, ports).is_err());
    export.sockets[0].protocol_info = export.sockets[1].protocol_info.clone();
    assert!(validate_export(&export, ports).is_err());
}

#[test]
fn rejects_wrong_recipient_nonce_and_unexportable_ports() {
    let mut export = sample_export();
    assert!(validate_export(&export, [0, 10201]).is_err());
    assert!(validate_export(&export, [10200, 10200]).is_err());
    export.recipientpid = 0;
    assert!(validate_export(&export, [10200, 10201]).is_err());
    export.recipientpid = std::process::id();
    export.nonce.pop();
    assert!(validate_export(&export, [10200, 10201]).is_err());
}

#[test]
fn ready_requires_exact_pid_nonce_unexpired_and_one_use() {
    let now = Instant::now();
    let mut transfer = Transfer {
        pid: 42,
        recipient: open_recipient(std::process::id()).unwrap(),
        nonce: Zeroizing::new(vec![7; 32]),
        deadline: now + TRANSFER_LIMIT,
        exported: true,
        ready: false,
    };
    let mut ready = ReadyRequest {
        recipientpid: 41,
        nonce: vec![7; 32],
    };
    assert!(transfer.accept_ready(&ready, now).is_err());
    ready.recipientpid = 42;
    ready.nonce[0] = 8;
    assert!(transfer.accept_ready(&ready, now).is_err());
    ready.nonce[0] = 7;
    transfer.exported = false;
    assert!(transfer.accept_ready(&ready, now).is_err());
    transfer.exported = true;
    assert!(transfer.accept_ready(&ready, transfer.deadline).is_err());
    assert!(transfer.accept_ready(&ready, now).is_ok());
    assert!(transfer.accept_ready(&ready, now).is_err());
}

#[test]
fn grant_recovery_requires_unready_proven_dead_recipient_before_shutdown() {
    assert!(can_recover_grant(false, false, 42, 43, RecipientLife::Dead));
    for life in [RecipientLife::Alive, RecipientLife::Unknown] {
        assert!(!can_recover_grant(false, false, 42, 43, life));
    }
    assert!(!can_recover_grant(true, false, 42, 43, RecipientLife::Dead));
    assert!(!can_recover_grant(false, true, 42, 43, RecipientLife::Dead));
    assert!(!can_recover_grant(
        false,
        false,
        42,
        42,
        RecipientLife::Dead
    ));
}

#[test]
fn handover_gate_requires_ready_and_closed_and_refuses_failure_before_fence() {
    let now = Instant::now();
    let deadline = now + TRANSFER_LIMIT;
    assert_eq!(
        handover_gate(false, true, false, deadline, now),
        Some(false)
    );
    assert_eq!(handover_gate(true, true, false, deadline, now), Some(true));
    assert_eq!(handover_gate(true, false, false, deadline, now), None);
    assert_eq!(handover_gate(false, false, false, deadline, now), None);
    assert_eq!(
        handover_gate(true, false, false, deadline, deadline),
        Some(false)
    );
    assert_eq!(handover_gate(true, false, true, deadline, now), Some(false));
    assert_eq!(handover_gate(false, true, true, deadline, now), Some(false));
}

#[test]
fn successful_handover_latches_after_deadline_and_original_recipient_exit() {
    let now = Instant::now();
    let deadline = now + TRANSFER_LIMIT;
    let later = deadline + Duration::from_secs(1);
    assert_eq!(
        handover_gate(true, true, false, deadline, later),
        Some(true)
    );
    assert_eq!(handover_gate(true, true, true, deadline, now), Some(true));
    assert_eq!(handover_gate(true, true, true, deadline, later), Some(true));
    assert_eq!(
        handover_gate(false, true, true, deadline, later),
        Some(false)
    );
}

#[test]
fn retained_recipient_handle_reports_actual_current_process_alive() {
    let recipient = open_recipient(std::process::id()).unwrap();
    assert_eq!(recipient_life(&recipient), RecipientLife::Alive);
    // Missing PID never reserves a new grant, and is not treated as proof that
    // an existing retained process exited. No native capability is issued.
    assert!(open_recipient(0).is_err());
}

#[test]
fn protocol_buffer_is_aligned_fully_initialized_and_exact_size() {
    let mut storage = ProtocolStorage::new();
    assert_eq!(
        storage.pointer() as usize % align_of::<WSAPROTOCOL_INFOW>(),
        0
    );
    assert_eq!(storage.bytes().len(), PROTOCOL_SIZE);
    assert!(storage.bytes().iter().all(|&byte| byte == 0));
    let export = sample_export();
    // Malformed protocol fails before invoking native socket creation.
    assert!(import_one(&export.sockets[0]).is_err());
}

#[test]
fn notification_failure_defers_to_mandatory_cleanup() {
    assert!(
        super::super::defer_startup_error(Err(anyhow!("synthetic ready failure")))
            .unwrap()
            .is_some()
    );
    assert!(super::super::defer_startup_error(Ok(())).unwrap().is_none());
}

#[test]
fn cli_health_waits_for_acknowledgement_and_clears_on_failure_or_stop() {
    let readiness = super::super::Readiness::new();
    assert!(readiness.health()); // Direct-library compatibility.
    readiness.initialize();
    assert!(!readiness.health());
    readiness.acknowledge();
    assert!(readiness.health());
    readiness.clear();
    assert!(!readiness.health());
    let library = super::super::Readiness::new();
    library.clear();
    assert!(library.health());
}

#[test]
fn imported_native_listener_survives_source_descriptor_close() {
    use std::net::TcpStream;
    let original = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let endpoint = original.local_addr().unwrap();
    let info = duplicate(&original, "data", std::process::id()).unwrap();
    let imported = import_one(&info).unwrap();
    assert_eq!(imported.local_addr().unwrap(), endpoint);
    imported.set_nonblocking(true).unwrap();
    drop(original);
    let _client = TcpStream::connect_timeout(&endpoint, Duration::from_secs(1)).unwrap();
    let deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match imported.accept() {
            Ok(_) => break,
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(_) => panic!("imported listener did not accept after source close"),
        }
    }
}
