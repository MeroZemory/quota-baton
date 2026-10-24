    #[test]
    fn transient_sync_and_quota_errors_retry_in_next_batch_without_sticky_failure() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = directory();
        let mut writer = writer(dir.path());
        let start = writer.last_sync;
        writer.event(Command::Ledger(json!({"req": "synthetic"})));
        writer.event(quota_command("42"));
        let ledger = writer.dirty_files.iter().next().unwrap().clone();
        let lock = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&ledger)
            .unwrap();
        fs::create_dir(dir.path().join("state/quota.json")).unwrap();
        let log = warnings(|| writer.persist(start + SYNC_INTERVAL));
        assert!(log.contains("Quota state write failed; next batch will retry"));
        assert!(log.contains("Observability sync failed; next batch will retry"));
        assert!(writer.quota_dirty);
        assert!(writer.dirty_files.contains(&ledger));
        drop(lock);
        fs::remove_dir(dir.path().join("state/quota.json")).unwrap();
        writer.persist(start + SYNC_INTERVAL * 2);
        assert!(!writer.quota_dirty);
        assert!(writer.dirty_files.is_empty());
        writer.flush().unwrap();
        let state: Value =
            serde_json::from_slice(&fs::read(dir.path().join("state/quota.json")).unwrap())
                .unwrap();
        assert_eq!(
            state["accounts"]["cx-synthetic"]["windows"][0]["used_pct"],
            42.0
        );
        assert_eq!(fs::read_to_string(ledger).unwrap().lines().count(), 1);
    }

    #[test]
    fn locked_expired_ledger_warns_starts_and_retries_next_prune() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = directory();
        fs::create_dir(dir.path().join("ledger")).unwrap();
        let expired = dir.path().join("ledger/2000-01-01.jsonl");
        fs::write(&expired, "synthetic").unwrap();
        let lock = OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&expired)
            .unwrap();
        let log = warnings(|| {
            let obs = Observability::new(dir.path(), None).unwrap();
            obs.record_request(record());
            obs.flush().unwrap();
        });
        assert!(log.contains("Ledger retention failed; next prune will retry"));
        assert!(expired.exists());
        let mut writer = writer(dir.path());
        let start = writer.last_prune;
        let log = warnings(|| writer.persist(start + WARN_INTERVAL));
        assert!(log.contains("Ledger retention failed"));
        drop(lock);
        writer.persist(start + WARN_INTERVAL + StdDuration::from_secs(59));
        assert!(expired.exists());
        writer.persist(start + WARN_INTERVAL * 2);
        assert!(!expired.exists());
        assert!(!writer.error);
    }
