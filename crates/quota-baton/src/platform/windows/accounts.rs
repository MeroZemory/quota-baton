use super::cutover;
use crate::accounts_cli::*;
use clap::{Args, Subcommand};
use qb_core::{accounts::AccountStore, config::Config};
use qb_proto::{CpaImportRequest, ImportMode};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Subcommand)]
pub enum AccountsPlatformCommand {
    #[command(about = "다른 앱에서 계정의 토큰 관리 권한을 가져옵니다")]
    #[command(display_order = 1)]
    Takeover {
        #[arg(value_enum)]
        provider: LoginProvider,
        path: PathBuf,
        #[arg(long)]
        replace: bool,
        #[arg(long)]
        keep_source: bool,
        #[arg(long, help = "경고와 계정 확인 후 가져오기를 승인합니다")]
        yes: bool,
    },
    #[command(about = "계정을 비활성화합니다")]
    #[command(display_order = 5)]
    Disable { id: String },
    #[command(about = "계정을 활성화합니다")]
    #[command(display_order = 6)]
    Enable { id: String },
    #[command(about = "CPA 연결 계정을 직접 갱신하도록 전환합니다")]
    #[command(display_order = 9)]
    Adopt {
        #[arg(required = true, num_args = 1..)]
        ids: Vec<String>,
        #[arg(
            long,
            default_value = "cpa-file",
            help = "store, cpa-file 또는 백업 파일/폴더 경로"
        )]
        from: String,
    },
    #[command(about = "계정 토큰을 지금 갱신합니다")]
    #[command(display_order = 10)]
    Refresh {
        id: String,
        #[arg(long, help = "이전 갱신 결과가 불확실해도 한 번 시도합니다")]
        force: bool,
    },
    #[command(about = "CPA 계정을 가져옵니다")]
    #[command(display_order = 11)]
    Import {
        #[command(subcommand)]
        source: ImportSource,
    },
}
#[derive(Subcommand)]
pub enum ImportSource {
    Cpa(CpaArgs),
}
#[derive(Args)]
pub struct CpaArgs {
    #[arg(long, help = "CPA 인증 폴더를 지정합니다")]
    dir: Option<PathBuf>,
    #[arg(long, num_args = 1.., help = "provider:email 형식으로 활성화할 계정을 지정합니다")]
    activate: Vec<String>,
    #[arg(long, help = "저장하지 않고 결과를 확인합니다")]
    dry_run: bool,
    #[arg(long, help = "이 옵션 대신 accounts adopt 명령을 사용해 주세요")]
    adopt: bool,
}
#[derive(Args)]
pub struct LoginOptions {
    #[arg(long, help = "CPA 연결을 해제하고 직접 로그인 계정으로 전환합니다")]
    replace_linked: bool,
}
impl LoginOptions {
    pub(crate) fn replace_linked(&self) -> bool {
        self.replace_linked
    }
}
pub(crate) async fn run_accounts(
    command: AccountsPlatformCommand,
    home: &Path,
    config: &Config,
) -> anyhow::Result<()> {
    match command {
        AccountsPlatformCommand::Takeover {
            provider,
            path,
            replace,
            keep_source,
            yes,
        } => {
            takeover(home, config, provider, path, replace, keep_source, yes).await?;
        }
        AccountsPlatformCommand::Adopt { ids, from } => {
            adopt_accounts(home, config, ids, from).await?
        }
        AccountsPlatformCommand::Refresh { id, force } => {
            refresh_tokens(home, config, id, force).await?
        }
        AccountsPlatformCommand::Disable { id } => action(home, config, &id, "disable").await?,
        AccountsPlatformCommand::Enable { id } => action(home, config, &id, "enable").await?,
        AccountsPlatformCommand::Import {
            source: ImportSource::Cpa(args),
        } => {
            if args.adopt {
                return Err(account_error(
                    "소유권 이전은 accounts adopt 명령을 사용해 주세요.",
                ));
            }
            let dir = match args.dir {
                Some(dir) => dir,
                None => PathBuf::from(
                    std::env::var_os("USERPROFILE")
                        .ok_or_else(|| anyhow::anyhow!("USERPROFILE is missing"))?,
                )
                .join(".cli-proxy-api/auths"),
            };
            let request = CpaImportRequest {
                source: "cpa".into(),
                mode: ImportMode::Linked,
                dir: Some(dir.clone()),
                activate: args.activate.clone(),
                dry_run: args.dry_run,
            };
            let client = reqwest::Client::builder()
                .no_proxy()
                .user_agent(concat!("quota-baton/", env!("CARGO_PKG_VERSION")))
                .timeout(Duration::from_secs(5))
                .build()?;
            let base = format!("http://127.0.0.1:{}/admin/v1", config.server.admin_port);
            let token = std::fs::read_to_string(home.join("admin.token")).ok();
            let mut probe = client.get(format!("{base}/health"));
            if let Some(token) = &token {
                probe = probe.bearer_auth(token.trim());
            }
            let rows = match probe.send().await {
                Ok(health) => {
                    admin_route_response(health, &client, &base, token.as_deref().unwrap_or(""))
                        .await?
                        .error_for_status()?;
                    let response = client
                        .post(format!("{base}/accounts/import"))
                        .timeout(Duration::from_secs(60))
                        .bearer_auth(token.as_deref().unwrap_or("").trim())
                        .json(&request)
                        .send()
                        .await?;
                    let bytes = admin_route_response(
                        response,
                        &client,
                        &base,
                        token.as_deref().unwrap_or(""),
                    )
                    .await?
                    .error_for_status()?
                    .bytes()
                    .await?;
                    decode_account_subset(&bytes, AccountSubset::Import)?
                }
                Err(error) if error.is_connect() => {
                    if args.dry_run {
                        serde_json::to_value(qb_core::accounts::import::preview(
                            home,
                            &dir,
                            &args.activate,
                        )?)?
                    } else {
                        let store = AccountStore::acquire(home)?;
                        serde_json::to_value(qb_core::accounts::import::run(
                            &store,
                            &dir,
                            &args.activate,
                            false,
                        )?)?
                    }
                }
                Err(error) => return Err(error.into()),
            };
            println_utf8!("계정 ID\t제공자\t이메일\t요금제\t만료 시각\t사용 상태\t결과\t이유");
            for row in rows
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("Invalid import response"))?
            {
                println_utf8!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    cell(&row["id"]),
                    cell(&row["provider"]),
                    cell(&row["email"]),
                    cell(&row["plan"]),
                    cell(&row["expires_at"]),
                    translated(&row["user_state"]),
                    translated(&row["action"]),
                    if row["reason"] == "older_token" {
                        format!(
                            "이전 토큰 (저장 시각 {} > 파일 시각 {})",
                            cell(&row["stored_timestamp"]),
                            cell(&row["file_timestamp"])
                        )
                    } else {
                        translated(&row["reason"])
                    }
                );
            }
        }
    }
    Ok(())
}
fn takeover_inspect_lines(value: &serde_json::Value, keep_source: bool) -> Vec<String> {
    let mut lines = vec![format!(
        "가져오면 quota-baton이 이 계정의 토큰을 관리합니다. 원래 앱({})은 로그아웃되며, 다시 쓰려면 그 앱에서 새로 로그인해야 합니다. 원래 앱이 켜져 있으면 먼저 종료하세요.",
        cell(&value["source_path"])
    )];
    if keep_source {
        lines.push("강한 경고: --keep-source는 원본 토큰 파일을 남깁니다. 원래 앱이 이 토큰을 다시 갱신하면 전체 토큰 계열이 폐기되어 양쪽 모두 다시 로그인해야 할 수 있습니다. 원래 앱을 실행하지 마세요.".into());
    }
    for (key, label) in [
        ("email", "이메일"),
        ("plan", "요금제"),
        ("provider_account_id", "제공자 계정 ID"),
        ("source_path", "원본 경로"),
        ("source_last_refresh", "원본 최근 갱신"),
        ("source_mtime", "원본 수정 시각"),
        ("owner_alive", "원래 앱 실행 중"),
        ("existing_id", "기존 계정 ID"),
    ] {
        let display = if key == "owner_alive" {
            match value[key].as_bool() {
                Some(true) => "감지됨 · 먼저 종료하세요".into(),
                Some(false) => "감지되지 않음 (최선의 확인)".into(),
                None => "알 수 없음".into(),
            }
        } else {
            cell(&value[key])
        };
        lines.push(format!("{label}: {display}"));
    }
    lines
}

fn takeover_result_line(value: &serde_json::Value) -> String {
    format!(
        "계정 {}: 가져오기 완료 · 원본 이름 변경 {} · 갱신 {} · 만료 {}",
        cell(&value["id"]),
        cell(&value["renamed_to"]),
        match value["refresh"]["ok"].as_bool() {
            Some(true) => "성공",
            Some(false) | None => "확인 필요",
        },
        cell(&value["refresh"]["token_expires_at"])
    )
}

async fn takeover_response<T: serde::de::DeserializeOwned + serde::Serialize>(
    response: reqwest::Response,
) -> anyhow::Result<serde_json::Value> {
    if !response.status().is_success() {
        return account_response(response).await;
    }
    let bytes = response.bytes().await?;
    if bytes.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return Err(account_error(
            "계정 가져오기 응답 형식이 올바르지 않습니다.",
        ));
    }
    let wire: T = serde_json::from_slice(&bytes)
        .map_err(|_| account_error("계정 가져오기 응답 형식이 올바르지 않습니다."))?;
    Ok(serde_json::to_value(wire)?)
}

async fn takeover(
    home: &Path,
    config: &Config,
    provider: LoginProvider,
    path: PathBuf,
    replace: bool,
    keep_source: bool,
    yes: bool,
) -> anyhow::Result<()> {
    let (client, base, token) = running_admin(home, config)
        .await?
        .ok_or_else(|| account_error("계정을 가져오려면 인증된 코어를 먼저 실행해 주세요."))?;
    // Resolve relative paths for the core without reading any source file locally.
    let path = if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut request = serde_json::json!({
        "provider": qb_core::accounts::Provider::from(provider), "path": path,
        "phase": "inspect", "replace": replace, "keep_source": keep_source,
    });
    let inspect = takeover_response::<qb_proto::admin_takeover::TakeoverInspect>(
        admin_route_response(
            client
                .post(format!("{base}/accounts/takeover"))
                .bearer_auth(token.trim())
                .json(&request)
                .send()
                .await?,
            &client,
            &base,
            &token,
        )
        .await?,
    )
    .await?;
    for line in takeover_inspect_lines(&inspect, keep_source) {
        println_utf8!("{line}");
    }
    if !yes {
        return Err(TakeoverInspectOnly.into());
    }
    let digest = inspect["inspect_digest"]
        .as_str()
        .filter(|digest| !digest.is_empty())
        .ok_or_else(|| account_error("확인 다이제스트가 없습니다. 코어 응답을 확인해 주세요."))?;
    request["phase"] = serde_json::json!("commit");
    request["inspect_digest"] = serde_json::json!(digest);
    let result = takeover_response::<qb_proto::admin_takeover::TakeoverCommit>(
        admin_route_response(
            client
                .post(format!("{base}/accounts/takeover"))
                .timeout(Duration::from_secs(40))
                .bearer_auth(token.trim())
                .json(&request)
                .send()
                .await?,
            &client,
            &base,
            &token,
        )
        .await?,
    )
    .await?;
    println_utf8!("{}", takeover_result_line(&result));
    if result["refresh"]["ok"].as_bool() != Some(true) {
        return Err(account_error(
            "계정은 가져왔지만 갱신을 확인하지 못했습니다. 계정 상태를 확인해 주세요.",
        ));
    }
    Ok(())
}

fn adoption_error(error: qb_core::accounts::adopt::AdoptError) -> anyhow::Error {
    use qb_core::accounts::adopt::AdoptError;
    account_error(match error {
        AdoptError::NotFound => "계정을 찾을 수 없습니다.",
        AdoptError::NotLinked => "CPA 연결 계정만 소유권을 이전할 수 있습니다.",
        AdoptError::Generation { .. } => "계정이 변경됐습니다. 상태를 다시 읽고 시도해 주세요.",
        AdoptError::IdentityMismatch | AdoptError::AmbiguousIdentity => {
            "계정의 사용자를 확인할 수 없거나 사용자 파일이 겹칩니다."
        }
        AdoptError::CpaRunning => "CPA가 아직 이 계정을 갱신할 수 있습니다.",
        AdoptError::ProbeUnavailable => "CPA 실행 상태를 확인할 수 없습니다.",
        AdoptError::SourceIo | AdoptError::Store(_) => "계정 자료를 읽거나 저장할 수 없습니다.",
    })
}

async fn adoption_response(
    response: reqwest::Response,
) -> anyhow::Result<qb_core::accounts::adopt::AdoptResult> {
    if !response.status().is_success() {
        account_response(response).await?;
        return Err(account_error(
            "계정 작업을 마치지 못했습니다. 코어 상태를 확인해 주세요.",
        ));
    }
    let bytes = response.bytes().await?;
    if bytes.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return Err(account_error("소유권 이전 응답 형식이 올바르지 않습니다."));
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| account_error("소유권 이전 응답 형식이 올바르지 않습니다."))
}

async fn adopt_accounts(
    home: &Path,
    config: &Config,
    ids: Vec<String>,
    from: String,
) -> anyhow::Result<()> {
    use qb_core::accounts::adopt::{AdoptFrom, AdoptRequest, AdoptSource};
    for id in &ids {
        valid_account_id(id)?;
    }
    let from = match from.as_str() {
        "store" => AdoptFrom::Source(AdoptSource::Store),
        "cpa-file" | "cpa_file" => AdoptFrom::Source(AdoptSource::CpaFile),
        _ => {
            let path = PathBuf::from(from);
            AdoptFrom::Path {
                path: if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir()?.join(path)
                },
            }
        }
    };
    let admin = running_admin(home, config).await?;
    // A responding core always owns the lease, including on auth/health failure.
    let local = if admin.is_none() {
        Some(std::sync::Arc::new(AccountStore::acquire(home)?))
    } else {
        None
    };
    println_utf8!("계정 ID\t결과\t토큰 출처\t만료 시각");
    for id in ids {
        let result: qb_core::accounts::adopt::AdoptResult = if let Some((client, base, token)) =
            &admin
        {
            let state = account_response(
                admin_route_response(
                    client
                        .get(format!("{base}/state"))
                        .bearer_auth(token.trim())
                        .send()
                        .await?,
                    client,
                    base,
                    token,
                )
                .await?,
            )
            .await?;
            let generation = state["accounts"]
                .as_array()
                .and_then(|accounts| accounts.iter().find(|account| account["id"] == id))
                .and_then(|account| account["generation"].as_u64())
                .ok_or_else(|| {
                    account_error("계정을 찾을 수 없거나 코어가 소유권 이전을 지원하지 않습니다.")
                })?;
            adoption_response(
                admin_route_response(
                    client
                        .post(format!("{base}/accounts/{id}/adopt"))
                        .timeout(Duration::from_secs(35))
                        .bearer_auth(token.trim())
                        .json(&AdoptRequest {
                            from: from.clone(),
                            expected_generation: generation,
                        })
                        .send()
                        .await?,
                    client,
                    base,
                    token,
                )
                .await?,
            )
            .await?
        } else {
            let store = local.as_ref().expect("local lease").clone();
            let from = from.clone();
            let account_id = id.clone();
            tokio::task::spawn_blocking(move || {
                let account = store
                    .load(&account_id)
                    .map_err(qb_core::accounts::adopt::AdoptError::from)?;
                qb_core::accounts::adopt::run(
                    &store,
                    &account_id,
                    AdoptRequest {
                        from,
                        expected_generation: account.generation,
                    },
                )
            })
            .await?
            .map_err(adoption_error)?
        };
        let origin = match result.from.as_str() {
            "cpa_file" => "CPA 파일",
            "backup" => "백업",
            _ => "저장된 토큰",
        };
        println_utf8!(
            "{}\t직접 갱신으로 전환\t{}\t{}",
            id,
            origin,
            cell(&serde_json::to_value(result.token_expires_at)?)
        );
        for warning in result.warnings {
            use qb_proto::admin_adopt::AdoptWarning;
            let message = match warning {
                AdoptWarning::CpaFileMissing => "CPA 파일이 없어 다른 자료를 사용했습니다.",
                AdoptWarning::CpaFileUnreadable => {
                    "읽을 수 없는 CPA 파일이 있어 다른 자료를 사용했습니다."
                }
                AdoptWarning::BackupMissing | AdoptWarning::BackupUnreadable => {
                    "백업을 읽을 수 없어 저장된 자료를 사용했습니다."
                }
                AdoptWarning::OlderOrUnorderedCandidateIgnored => {
                    "오래되었거나 시각을 비교할 수 없는 토큰은 사용하지 않았습니다."
                }
                AdoptWarning::EmailUpdated => "같은 계정의 이메일 표시를 갱신했습니다.",
                AdoptWarning::StoredTokensMayNeedRelogin => {
                    "저장된 토큰의 만료 또는 불확실 상태를 확인해 주세요."
                }
            };
            println_utf8!("계정 {}: {}", id, message);
        }
    }
    Ok(())
}

async fn manual_refresh_response(
    response: reqwest::Response,
) -> anyhow::Result<qb_proto::admin_refresh::ManualRefreshReply> {
    if !response.status().is_success() {
        account_response(response).await?;
        return Err(account_error(
            "계정 작업을 마치지 못했습니다. 코어 상태를 확인해 주세요.",
        ));
    }
    let bytes = response.bytes().await?;
    if bytes.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        return Err(account_error("갱신 응답 형식이 올바르지 않습니다."));
    }
    serde_json::from_slice(&bytes).map_err(|_| account_error("갱신 응답 형식이 올바르지 않습니다."))
}

async fn refresh_tokens(
    home: &Path,
    config: &Config,
    id: String,
    force: bool,
) -> anyhow::Result<()> {
    valid_account_id(&id)?;
    let summary = if let Some((client, base, token)) = running_admin(home, config).await? {
        manual_refresh_response(
            admin_route_response(
                client
                    .post(format!("{base}/accounts/{id}/refresh"))
                    .timeout(Duration::from_secs(40))
                    .bearer_auth(token.trim())
                    .json(&serde_json::json!({"force":force}))
                    .send()
                    .await?,
                &client,
                &base,
                &token,
            )
            .await?,
        )
        .await?
    } else {
        let store = std::sync::Arc::new(AccountStore::acquire(home)?);
        let obs = std::sync::Arc::new(qb_core::observability::Observability::new(home, None)?);
        let relay = qb_core::relay::RelayState::new(
            store,
            config,
            obs.clone(),
            std::sync::Arc::new(qb_core::debug_pin::DebugPins::default()),
        )?;
        #[cfg(debug_assertions)]
        let relay = {
            let mut relay = relay;
            for (provider, endpoint) in [
                (
                    qb_core::accounts::Provider::Claude,
                    &config.upstream.claude_token_endpoint,
                ),
                (
                    qb_core::accounts::Provider::Codex,
                    &config.upstream.codex_token_endpoint,
                ),
            ] {
                if let Some(endpoint) = endpoint {
                    relay = relay.with_token_endpoint(provider, endpoint.clone());
                }
            }
            relay
        };
        let result = relay.refresh_account_typed(id.clone(), force).await;
        let mut warned = false;
        // Never drop a lease while detached consumption or an unsaved rotation survives.
        while relay.shutdown_refreshes().await.is_err() {
            if !warned {
                println_utf8!(
                    "갱신 결과를 저장하는 동안 계정 잠금을 유지합니다. 저장 공간과 권한을 확인해 주세요."
                );
                warned = true;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        obs.flush()?;
        result?
    };
    if summary.outcome == qb_proto::admin_refresh::RefreshOutcome::Success {
        println_utf8!(
            "계정 {}: 갱신 완료 · 만료 {} · 최근 갱신 {}",
            id,
            cell(&serde_json::to_value(summary.token_expires_at)?),
            cell(&serde_json::to_value(summary.last_refresh_at)?)
        );
        Ok(())
    } else {
        Err(account_error(
            "갱신 자격 증명이 무효화되거나 만료됐습니다. 다시 로그인해 주세요.",
        ))
    }
}
pub(crate) fn open_browser(url: &str) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "shell32")]
    unsafe extern "system" {
        fn ShellExecuteW(
            window: *mut std::ffi::c_void,
            operation: *const u16,
            file: *const u16,
            parameters: *const u16,
            directory: *const u16,
            show: i32,
        ) -> *mut std::ffi::c_void;
    }
    let operation: Vec<u16> = "open\0".encode_utf16().collect();
    let file: Vec<u16> = std::ffi::OsStr::new(url)
        .encode_wide()
        .chain(Some(0))
        .collect();
    // ShellExecute receives the validated URL as data, without a command shell.
    let result = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            1,
        )
    };
    if result as isize <= 32 {
        return Err(std::io::Error::other("브라우저를 열지 못했습니다."));
    }
    Ok(())
}
pub(crate) fn cpa_dependency_note(home: &Path) -> anyhow::Result<Option<String>> {
    cutover::cpa_dependency_note(home)
}
#[cfg(test)]
mod r1_cpa_status_tests {
    use super::*;
    use crate::cutover::cpa_dependency_tests::{account, scratch};
    use serde_json::json;

    #[test]
    fn r162_accounts_list_detail_flag_defaults_off() {
        use clap::Parser;
        #[derive(Parser)]
        struct Args {
            #[command(subcommand)]
            command: AccountsCommand,
        }
        assert!(matches!(
            Args::try_parse_from(["synthetic", "list"]).unwrap().command,
            AccountsCommand::List { detail: false }
        ));
        assert!(matches!(
            Args::try_parse_from(["synthetic", "list", "--detail"])
                .unwrap()
                .command,
            AccountsCommand::List { detail: true }
        ));
    }

    #[test]
    fn r162_accounts_list_summary_shared_and_detail_legacy_preserved() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-10-23T00:00:00Z")
            .unwrap()
            .to_utc();
        let mut samples = qb_core::usage_forecast::Forecasts::default();
        samples.observe(
            "synthetic",
            "5h",
            50.,
            Some(now + chrono::Duration::hours(3)),
            now,
        );
        let forecast = samples.view("synthetic", now, None);
        let account = json!({"usage_forecast":forecast,"credits_in_use":true});
        for english in [false, true] {
            assert_eq!(
                list_forecast_lines(Some(&account), true, false, english),
                vec![qb_core::usage_forecast_display::summary(&forecast, english).text]
            );
        }
        let detail = list_forecast_lines(Some(&account), true, true, false);
        assert_eq!(detail, forecast_lines(Some(&account), true));
        assert_eq!(
            detail,
            qb_core::usage_forecast_display::lines(&forecast, true)
        );
        assert!(detail.iter().any(|line| line.contains("25.0%/시간")));
        assert!(detail.iter().any(|line| line.contains("크레딧")));
        for account in [None, Some(&json!({}))] {
            assert_eq!(
                list_forecast_lines(account, true, false, false),
                vec!["측정 중"]
            );
            assert_eq!(
                list_forecast_lines(account, false, false, true),
                vec!["Measuring"]
            );
        }
        assert!(list_forecast_lines(None, false, true, false)[0].contains("코어가 꺼져"));
    }

    #[test]
    fn r13_forecast_cli_handles_old_core_offline_and_shared_text() {
        assert!(forecast_lines(None, false)[0].contains("코어가 꺼져"));
        assert_eq!(
            forecast_lines(Some(&json!({})), true),
            vec!["소진 예측: 측정 중"]
        );
        let dto = qb_core::usage_forecast::Forecasts::default().view(
            "cx-123abc",
            std::time::SystemTime::now().into(),
            None,
        );
        let account = json!({"usage_forecast":dto,"credits_in_use":true});
        assert_eq!(
            forecast_lines(Some(&account), true),
            qb_core::usage_forecast_display::lines(&dto, true)
        );
    }

    #[test]
    fn r1_cpa_status_heading_shares_wire_note_online_and_offline() {
        let dir = scratch();
        for (label, mode, state) in [
            ("one", "linked", "active"),
            ("two", "linked", "active"),
            ("paused", "linked", "paused"),
            ("disabled", "linked", "disabled"),
            ("owned", "owned", "active"),
            ("adopted", "adopted", "active"),
        ] {
            account(dir.path(), label, mode, state, json!({"kind":"ready"}));
        }
        let _lease = AccountStore::acquire(dir.path()).unwrap();
        for running in [false, true] {
            let lines = status_heading(dir.path(), running).unwrap();
            assert_eq!(lines.len(), 2);
            assert_eq!(
                lines[0],
                if running {
                    "코어: 실행 중"
                } else {
                    "코어: 중지됨 (저장된 상태)"
                }
            );
            assert_eq!(
                lines[1],
                "CPA는 계속 실행되어야 합니다 (연결 계정 2개의 토큰 갱신)"
            );
        }
    }

    #[test]
    fn r1_cpa_status_heading_omits_note_without_active_linked_accounts() {
        let dir = scratch();
        for (label, mode, state) in [
            ("paused", "linked", "paused"),
            ("disabled", "linked", "disabled"),
            ("owned", "owned", "active"),
            ("adopted", "adopted", "active"),
        ] {
            account(dir.path(), label, mode, state, json!({"kind":"ready"}));
        }
        for running in [false, true] {
            assert_eq!(status_heading(dir.path(), running).unwrap().len(), 1);
        }
    }
}

#[cfg(test)]
mod r15_adoption_contract_tests {
    #[tokio::test]
    async fn cli_adoption_reader_preserves_the_entire_shared_reply() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let raw = br#"{"id":"sample-acct","generation":2,"from":"store","token_expires_at":null,"last_refresh_at":null,"warnings":["stored_tokens_may_need_relogin"]}"#;
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let byte = socket.read_u8().await.unwrap();
                request.push(byte);
                assert!(request.len() < 8192);
            }
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                raw.len()
            );
            socket.write_all(header.as_bytes()).await.unwrap();
            socket.write_all(raw).await.unwrap();
        });
        let response = reqwest::Client::builder()
            .no_proxy()
            .build()
            .unwrap()
            .get(format!("http://127.0.0.1:{port}"))
            .send()
            .await
            .unwrap();
        let reply = super::adoption_response(response).await.unwrap();
        server.await.unwrap();
        assert_eq!(serde_json::to_vec(&reply).unwrap(), raw);
    }
}

#[cfg(test)]
#[path = "../../accounts_cli_r6_tests.rs"]
mod r6_tests;
