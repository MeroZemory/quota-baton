use clap::Subcommand;
use std::{path::Path, time::Duration};

const ERROR_BODY_LIMIT: usize = 8192;
const ERROR_MESSAGE_LIMIT: usize = 512;
const REQUEST_BUDGET: Duration = Duration::from_secs(10);
const HANDOFF_DELAYS: [Duration; 4] = [
    Duration::from_millis(100),
    Duration::from_millis(250),
    Duration::from_millis(500),
    Duration::from_secs(1),
];

#[derive(Debug)]
pub(crate) struct RoutingCliError {
    status: u16,
    reason: String,
}

impl std::fmt::Display for RoutingCliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "계정 전환 정책 요청을 완료하지 못했습니다 (마지막 HTTP {}): {}",
            self.status, self.reason
        )
    }
}

impl std::error::Error for RoutingCliError {}

#[derive(Subcommand)]
pub enum RoutingCommand {
    #[command(about = "프로젝트와 루트 세션의 계정 전환 정책을 확인합니다")]
    List,
    #[command(
        about = "프로젝트 기본 정책을 등록하거나 수정합니다",
        long_about = "새로 연결할 세션의 프로젝트 기본 정책을 등록하거나 수정합니다. 이미 연결된 세션은 저장된 기존 기본값을 유지합니다."
    )]
    Project {
        name: String,
        #[arg(long)]
        id: Option<String>,
        #[arg(long,value_parser=["strict","recover","rebalance"])]
        mode: String,
        #[arg(long, default_value_t = 60)]
        reset_wait_secs: u64,
        #[arg(long, value_parser = ["wait", "move"])]
        on_5h_limit: Option<String>,
    },
    #[command(about = "루트 세션의 정책을 지정합니다. 자식은 루트를 따릅니다")]
    Session {
        id: String,
        #[arg(long,value_parser=["strict","recover","rebalance"],conflicts_with="inherit")]
        mode: Option<String>,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, requires = "mode")]
        reset_wait_secs: Option<u64>,
        #[arg(long, requires = "mode", conflicts_with = "inherit", value_parser = ["wait", "move"])]
        on_5h_limit: Option<String>,
        #[arg(long, conflicts_with = "mode")]
        inherit: bool,
    },
}
pub async fn run(
    command: RoutingCommand,
    home: &Path,
    config: &qb_core::config::Config,
) -> anyhow::Result<()> {
    let (client, base, admin_token, _) =
        crate::admin_transport::connection(home, config, Duration::from_secs(10), true).await?;
    let url = format!("{base}/routing");
    let inherited_wait = if let RoutingCommand::Session {
        id,
        mode: Some(_),
        reset_wait_secs: None,
        ..
    } = &command
    {
        let response = client.get(&url).bearer_auth(&admin_token).send().await?;
        if !response.status().is_success() {
            return Err(response_error(response, &admin_token).await.into());
        }
        let value = routing_value(&response.bytes().await?)?;
        Some(
            current_wait(&value, id).ok_or(crate::accounts_cli::AccountCliError(
                "현재 세션 대기 시간을 확인하지 못했습니다.",
            ))?,
        )
    } else {
        None
    };
    let body = match command {
        RoutingCommand::List => None,
        RoutingCommand::Project {
            name,
            id,
            mode,
            reset_wait_secs,
            on_5h_limit,
        } => Some(
            serde_json::json!({"kind":"project","id":id,"name":name,"settings":settings_body(mode,reset_wait_secs,on_5h_limit)}),
        ),
        RoutingCommand::Session {
            id,
            mode,
            project,
            reset_wait_secs,
            on_5h_limit,
            inherit,
        } => {
            if mode.is_none() && project.is_none() && !inherit {
                return Err(crate::accounts_cli::AccountCliError(
                    "--mode, --project 또는 --inherit를 지정해 주세요.",
                )
                .into());
            }
            let settings = mode.map(|mode| {
                settings_body(
                    mode,
                    reset_wait_secs
                        .or(inherited_wait)
                        .unwrap_or(config.routing.reset_hold_secs),
                    on_5h_limit,
                )
            });
            Some(
                serde_json::json!({"kind":"session","id":id,"settings":settings,"project":project,"inherit":inherit}),
            )
        }
    };
    send_request(&client, &url, &admin_token, body).await
}

// Omitted urgency must stay absent so the policy writer preserves its latest value.
fn settings_body(
    mode: String,
    reset_wait_secs: u64,
    on_5h_limit: Option<String>,
) -> serde_json::Value {
    let mut settings = serde_json::json!({"mode":mode,"reset_wait_secs":reset_wait_secs});
    if let Some(urgency) = on_5h_limit {
        settings["on_5h_limit"] = urgency.into();
    }
    settings
}

pub async fn urgent(
    id: String,
    enabled: bool,
    home: &Path,
    config: &qb_core::config::Config,
) -> anyhow::Result<()> {
    let (client, base, admin_token, _) =
        crate::admin_transport::connection(home, config, Duration::from_secs(10), true).await?;
    let url = format!("{base}/routing");
    send_request(
        &client,
        &url,
        &admin_token,
        Some(serde_json::json!({
            "kind":"urgent", "id":id, "on_5h_limit":if enabled { "move" } else { "wait" }
        })),
    )
    .await
}

async fn send_request(
    client: &reqwest::Client,
    url: &str,
    admin_token: &str,
    body: Option<serde_json::Value>,
) -> anyhow::Result<()> {
    let value = request_value(client, url, admin_token, body.as_ref(), &HANDOFF_DELAYS).await?;
    crate::print_stdout(format_args!("{}", serde_json::to_string_pretty(&value)?))?;
    Ok(())
}

async fn request_value(
    client: &reqwest::Client,
    url: &str,
    admin_token: &str,
    body: Option<&serde_json::Value>,
    retry_delays: &[Duration],
) -> anyhow::Result<serde_json::Value> {
    let is_read = body.is_none();
    // Urgency sets one absolute value. Other mutations may create a project or
    // carry an inherited setting, so they must not be replayed automatically.
    let retry_urgent = body.is_some_and(|body| {
        body["kind"] == "urgent"
            && body["id"].as_str().is_some_and(|id| !id.is_empty())
            && matches!(body["on_5h_limit"].as_str(), Some("wait" | "move"))
    });
    let deadline = tokio::time::Instant::now() + REQUEST_BUDGET;
    let mut retries = retry_delays.iter();
    let mut last_conflict: Option<RoutingCliError> = None;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(match last_conflict {
                Some(error) => error.into(),
                None => crate::accounts_cli::AccountCliError(
                    "정책 요청 대기 시간이 지났습니다. 코어 상태를 확인해 주세요.",
                )
                .into(),
            });
        }
        let request = match body {
            Some(body) => client.post(url).json(body),
            None => client.get(url),
        };
        // A retry must connect to the successor, not keep using the incumbent's
        // draining keep-alive connection. Never retry an ambiguous send error.
        let response = request
            .header(reqwest::header::CONNECTION, "close")
            .bearer_auth(admin_token)
            .timeout(deadline.saturating_duration_since(tokio::time::Instant::now()))
            .send()
            .await;
        let response = match response {
            Ok(response) => response,
            Err(error) if retry_urgent && error.is_connect() => {
                // After a confirmed handoff refusal the old listener may close
                // before the successor binds. A connect failure sent no HTTP
                // bytes, so the same absolute edit can safely wait for it.
                if let Some(conflict) = last_conflict.take() {
                    if let Some(delay) = retries.next()
                        && *delay < deadline.saturating_duration_since(tokio::time::Instant::now())
                    {
                        last_conflict = Some(conflict);
                        tokio::time::sleep(*delay).await;
                        continue;
                    }
                    return Err(conflict.into());
                }
                return Err(error.into());
            }
            Err(error) => return Err(error.into()),
        };
        if response.status().is_success() {
            let bytes = response.bytes().await?;
            return if is_read {
                routing_value(&bytes)
            } else {
                Ok(serde_json::from_slice(&bytes)?)
            };
        }
        let conflict = response.status() == reqwest::StatusCode::CONFLICT;
        let error = response_error(response, admin_token).await;
        if retry_urgent
            && conflict
            && let Some(delay) = retries.next()
            && *delay < deadline.saturating_duration_since(tokio::time::Instant::now())
        {
            last_conflict = Some(error);
            tokio::time::sleep(*delay).await;
            continue;
        }
        return Err(error.into());
    }
}

async fn response_error(mut response: reqwest::Response, admin_token: &str) -> RoutingCliError {
    let status = response.status().as_u16();
    let mut bytes = Vec::new();
    let readable = loop {
        match response.chunk().await {
            Ok(Some(chunk)) if chunk.len() <= ERROR_BODY_LIMIT - bytes.len() => {
                bytes.extend_from_slice(&chunk);
            }
            Ok(None) => break true,
            _ => break false,
        }
    };
    let message = readable
        .then(|| {
            serde_json::from_slice::<qb_proto::admin_error::AdminErrorReply>(&bytes)
                .map(|reply| reply.error.message)
                .or_else(|_| {
                    serde_json::from_slice::<qb_proto::admin_error::HttpErrorReply>(&bytes)
                        .map(|reply| reply.error.message)
                })
                .ok()
        })
        .flatten();
    let message = message
        .map(|message| {
            let redacted = if admin_token.is_empty() {
                message
            } else {
                message.replace(admin_token, "<redacted>")
            };
            let bounded: String = redacted
                .chars()
                .take(ERROR_MESSAGE_LIMIT)
                .map(|c| if c.is_control() { ' ' } else { c })
                .collect();
            bounded.split_whitespace().collect::<Vec<_>>().join(" ")
        })
        .filter(|message| !message.is_empty());
    RoutingCliError {
        status,
        reason: message.unwrap_or_else(|| "코어 오류 응답을 확인하지 못했습니다.".into()),
    }
}

// Validate the complete GET graph before existing Value-based presentation.
// Retain original bytes for the presentation map, including future object keys.
fn routing_value(bytes: &[u8]) -> anyhow::Result<serde_json::Value> {
    let _: qb_proto::admin_routing::RoutingGet = serde_json::from_slice(bytes)
        .map_err(|_| crate::accounts_cli::AccountCliError("Invalid routing response contract"))?;
    Ok(serde_json::from_slice(bytes)?)
}

fn current_wait(value: &serde_json::Value, id: &str) -> Option<u64> {
    value["sessions"][id]["settings"]["reset_wait_secs"]
        .as_u64()
        .or_else(|| {
            value["observed_sessions"]
                .as_array()?
                .iter()
                .find(|s| s["id"] == id)?["policy"]["settings"]["reset_wait_secs"]
                .as_u64()
        })
        .or_else(|| value["defaults"]["reset_wait_secs"].as_u64())
        .filter(|n| *n <= 120)
}
#[cfg(test)]
mod http_tests;
#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Args {
        #[command(subcommand)]
        command: RoutingCommand,
    }

    #[test]
    fn n5c_routing_cli_parses_explicit_urgency_and_rejects_ambiguous_edits() {
        for urgency in ["wait", "move"] {
            let args = Args::try_parse_from([
                "qb",
                "project",
                "synthetic",
                "--mode",
                "strict",
                "--on-5h-limit",
                urgency,
            ])
            .unwrap();
            assert!(
                matches!(args.command, RoutingCommand::Project { on_5h_limit: Some(value), .. }
                if value == urgency)
            );
            let args = Args::try_parse_from([
                "qb",
                "session",
                "synthetic",
                "--mode",
                "strict",
                "--on-5h-limit",
                urgency,
            ])
            .unwrap();
            assert!(
                matches!(args.command, RoutingCommand::Session { on_5h_limit: Some(value), .. }
                if value == urgency)
            );
        }
        for args in [
            vec![
                "qb",
                "project",
                "synthetic",
                "--mode",
                "strict",
                "--on-5h-limit",
                "urgent",
            ],
            vec!["qb", "session", "synthetic", "--on-5h-limit", "move"],
            vec![
                "qb",
                "session",
                "synthetic",
                "--inherit",
                "--on-5h-limit",
                "move",
            ],
        ] {
            assert!(Args::try_parse_from(&args).is_err(), "accepted: {args:?}");
        }
    }

    #[test]
    fn n5c_cli_omits_unspecified_urgency_from_admin_json() {
        let omitted = settings_body("strict".into(), 17, None);
        assert!(omitted.get("on_5h_limit").is_none());
        for urgency in ["wait", "move"] {
            let explicit = settings_body("strict".into(), 17, Some(urgency.into()));
            assert_eq!(explicit["on_5h_limit"], urgency);
            assert_eq!(explicit["mode"], omitted["mode"]);
            assert_eq!(explicit["reset_wait_secs"], omitted["reset_wait_secs"]);
        }
    }
    fn routing_fixture() -> serde_json::Value {
        let settings = |wait| serde_json::json!({"mode":"recover","reset_wait_secs":wait,"on_5h_limit":"wait"});
        let plan = |wait| serde_json::json!({"settings":settings(wait),"anchor":null,"project":null,"source":"session","revision":1});
        serde_json::json!({"revision":1,"defaults":settings(13),"projects":{},"sessions":{"synthetic":plan(7)},"observed_sessions":[{"id":"observed","account":"synthetic-account","policy":plan(0)}],"forecast_shadow":[]})
    }

    #[test]
    fn routing_get_decode_precedes_existing_presentation() {
        let fixture = routing_fixture();
        let value = routing_value(&serde_json::to_vec(&fixture).unwrap()).unwrap();
        assert_eq!(value, fixture);
        assert_eq!(current_wait(&value, "synthetic"), Some(7));
        assert_eq!(current_wait(&value, "observed"), Some(0));
        assert_eq!(current_wait(&value, "new"), Some(13));
    }

    #[test]
    fn routing_get_refuses_sparse_nested_settings_and_raw_duplicates() {
        for pointer in [
            "/defaults",
            "/sessions/synthetic/settings",
            "/observed_sessions/0/policy/settings",
        ] {
            let mut fixture = routing_fixture();
            fixture
                .pointer_mut(pointer)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove("on_5h_limit");
            assert!(routing_value(&serde_json::to_vec(&fixture).unwrap()).is_err());
        }
        let fixture = routing_fixture();
        let raw = serde_json::to_string(&fixture).unwrap();
        let duplicate = format!("{},\"revision\":2}}", &raw[..raw.len() - 1]);
        assert!(routing_value(duplicate.as_bytes()).is_err());
    }

    #[test]
    fn mode_edit_uses_running_root_wait_then_observed_policy_then_running_default() {
        let mut view = routing_fixture();
        assert_eq!(current_wait(&view, "synthetic"), Some(7));
        assert_eq!(current_wait(&view, "observed"), Some(0));
        assert_eq!(current_wait(&view, "new"), Some(13));
        view["defaults"]["reset_wait_secs"] = 900.into();
        assert_eq!(current_wait(&view, "new"), None);
    }
}
