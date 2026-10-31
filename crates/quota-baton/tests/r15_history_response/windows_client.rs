use super::*;

pub(super) async fn same_client_response(h: &Harness) {
    let client = qb_tray::client::AdminClient::new(h.servers.admin_addr.port(), ADMIN).unwrap();
    let activity = client.dashboard_activity_raw(7).await.unwrap();
    let original = h.captured.lock().unwrap().last().unwrap().clone();
    assert_eq!(serde_json::to_vec(&activity).unwrap(), original);
    let display = client.dashboard_activity(7).await.unwrap();
    let body = h.captured.lock().unwrap().last().unwrap().clone();
    let previous: qb_tray::dashboard_model::ActivityResponse =
        serde_json::from_slice(&body).unwrap();
    assert_eq!(
        serde_json::to_value(display).unwrap(),
        serde_json::to_value(previous).unwrap()
    );
    let usage = client.dashboard_usage_history_raw().await.unwrap();
    let original = h.captured.lock().unwrap().last().unwrap().clone();
    assert_eq!(serde_json::to_vec(&usage).unwrap(), original);
    let display = client.dashboard_usage_history().await.unwrap();
    let body = h.captured.lock().unwrap().last().unwrap().clone();
    let previous: qb_tray::dashboard_model::UsageHistory = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        serde_json::to_value(display).unwrap(),
        serde_json::to_value(previous).unwrap()
    );
}
