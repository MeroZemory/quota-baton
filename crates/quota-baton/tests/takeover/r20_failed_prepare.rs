//! Real failed quota persistence followed by a fresh native takeover recipient.

use super::*;

#[tokio::test]
async fn r20_failed_quota_prepare_allows_fresh_successor_retry() {
    tokio::time::timeout(HARD, async {
        let _serial = SERIAL
            .get_or_init(|| TokioMutex::const_new(()))
            .lock()
            .await;
        let upstream = CatalogUpstream::start().await;
        let home = Home::new(&upstream.url).await;
        let account = accounts(&home, 1).remove(0);
        let client = client();
        let mut command = home.command();
        command.arg("serve").env("QB_LOG", "info");
        let owner = Core::from_command(command, home.path.join("failed-prepare-owner.stderr"));
        let token = token(&home).await;
        ready(&client, &home, &token, owner.pid).await;

        // Wait for startup's genuine quota observation to finish persisting.
        // Obstruct only this disposable home's exact destination, before the
        // next event, so a periodic flush cannot clear its dirty state first.
        let quota = home.path.join("state/quota.json");
        until(|| {
            fs::read(&quota)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .is_some_and(|value| value["accounts"].get(&account).is_some())
        })
        .await;
        fs::remove_file(&quota).unwrap();
        fs::create_dir(&quota).unwrap();

        let mut response = ScriptedResponse::json(StatusCode::OK, r#"{"fake":true}"#);
        for (name, value) in [
            ("x-codex-primary-used-percent", "37"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-after-seconds", "3600"),
        ] {
            response.headers.insert(name, value.parse().unwrap());
        }
        upstream.state.push(response);
        assert_eq!(
            data_account(&client, &home, "r20-dirty-quota-before-drain").await,
            account
        );
        // This warning requires a real dirty quota write attempt, rather than
        // merely a directory at an otherwise clean persistence destination.
        until(|| {
            owner
                .stderr()
                .contains("Quota state write failed; next batch will retry")
        })
        .await;

        let mut command = home.command();
        command.args(["serve", "--takeover"]).env("QB_LOG", "info");
        let first = Core::from_command(command, home.path.join("failed-prepare-first.stderr"));
        let first_status = first.wait(Duration::from_secs(15)).await;
        assert!(
            !first_status.success(),
            "first successor must fail the obstructed drain"
        );
        assert_eq!(first_status.code(), Some(1));
        let first_stderr = first.stderr();
        // request_takeover's internal 503 diagnostic is deliberately replaced
        // by friendly_error; stderr does not expose the HTTP refusal text.
        assert!(
            first_stderr.contains(
                "실패했습니다: 요청을 처리하지 못했습니다. 데이터 폴더와 설정을 확인해 주세요."
            ),
            "first successor must report the sanitized refusal: {first_stderr}"
        );
        assert!(quota.is_dir());
        assert!(
            owner.status().is_none(),
            "failed drain must keep owner alive"
        );
        lease_busy(&home);
        ready(&client, &home, &token, owner.pid).await;
        let state = client
            .get(home.admin_url("/admin/v1/state"))
            .bearer_auth(&token)
            .send()
            .await
            .unwrap();
        assert_eq!(state.status(), StatusCode::OK);
        assert_eq!(
            state.json::<serde_json::Value>().await.unwrap()["draining"],
            true
        );
        // No successor is running here. The incumbent still accepts data
        // connections and returns its explicit retryable drain response.
        let response = client
            .get(format!("http://127.0.0.1:{}/r20-listener-probe", home.data))
            .bearer_auth(home.key())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["retry-after"], "1");
        response.bytes().await.unwrap();
        assert!(!phase(&home, owner.pid, "lease_released"));
        assert!(!phase(&home, first.pid, "lease_acquired"));

        // The first process has actually exited. Only remove our empty exact
        // obstruction; preserve all other state and retry through the real CLI.
        fs::remove_dir(&quota).unwrap();
        let mut command = home.command();
        command.args(["serve", "--takeover"]).env("QB_LOG", "info");
        let second = Core::from_command(command, home.path.join("failed-prepare-second.stderr"));
        assert_ne!(first.pid, second.pid);
        ready(&client, &home, &token, second.pid).await;
        assert!(owner.wait(Duration::from_secs(10)).await.success());
        assert!(second.status().is_none());
        lease_busy(&home);
        until(|| phase(&home, owner.pid, "lease_released")).await;
        until(|| phase(&home, second.pid, "bindings_imported")).await;
        until(|| {
            fs::read(&quota)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
                .is_some_and(|value| value["accounts"].get(&account).is_some())
        })
        .await;
        upstream
            .state
            .push(ScriptedResponse::json(StatusCode::OK, r#"{"fake":true}"#));
        assert_eq!(
            data_account(&client, &home, "r20-after-failed-prepare-retry").await,
            account
        );
    })
    .await
    .expect("failed-preparation takeover retry exceeded hard timeout");
}
