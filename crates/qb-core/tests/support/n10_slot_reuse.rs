use super::*;

#[tokio::test]
async fn n10_returned_own_slot_is_reused_before_spilling_to_a_free_account() {
    let h = Harness::new_with_options(2, false, false, None, Some((30, 2))).await;
    h.fake.state.keep_old_connections();
    let source = h.ids[0].clone();
    let session = "synthetic-n10-own-slot";
    let mut occupied = learn_one_socket_cap(&h, &source, session).await;
    let (waiting, release) = h.state.pause_next_ws_own_wait();
    let request = session_request(&h, session);
    let pending = tokio::spawn(async move { connect_async(request).await });
    tokio::time::timeout(Duration::from_secs(1), waiting)
        .await
        .expect("the actual upgrade must enter its own-slot wait")
        .unwrap();
    occupied.close(None).await.unwrap();
    drop(occupied);
    capacity_count(&h, &source, 0).await;
    release.send(()).unwrap();
    let (mut resumed, response) = tokio::time::timeout(Duration::from_secs(3), pending)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(response.headers()["x-quota-baton-account"], source.as_str());
    assert_eq!(h.state.ws_reserved_for_test(&h.ids[1]), 0);
    assert_eq!(
        h.state.bindings.current(Some(&format!("codex:{session}"))),
        Some(source)
    );
    resumed.close(None).await.unwrap();
}
