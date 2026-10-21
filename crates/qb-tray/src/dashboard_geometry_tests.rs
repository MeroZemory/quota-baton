use super::*;
use crate::dashboard_fixtures as fixtures;
fn view(state: &mut DashboardState) -> m::DashboardView {
    m::project(&fixtures::snapshot(), state, &fixtures::clock())
}

#[test]
fn r10_retained_banner_and_polling_footer_use_the_last_data_time() {
    for connected in [false, true] {
        let mut data = fixtures::snapshot();
        let mut clock = fixtures::clock();
        data.data_at = Some(clock.utc());
        data.connection.core_ok = connected;
        data.connection.stream_ok = false;
        clock.now += chrono::Duration::seconds(30);
        let mut state = DashboardState::default();
        let v = m::project(&data, &mut state, &clock);
        let scene = Scene::build(&v, &state, [1320., 960.], &Metrics::default());
        let text: Vec<_> = scene
            .draw
            .iter()
            .filter_map(|d| match d {
                Draw::Text { measure, .. } => Some(measure.text.as_str()),
                _ => None,
            })
            .collect();
        assert!(
            text.iter()
                .any(|t| t.starts_with("15:42:08 기준 · 다시 연결 중"))
        );
        if !connected {
            assert!(
                text.iter()
                    .any(|t| t.contains("아래는 15:42:08의 마지막 값입니다"))
            );
        }
    }
}

#[test]
fn r10_linked_relogin_headers_show_the_reason_instead_of_retained_meters() {
    let mut data = fixtures::snapshot();
    data.accounts = v2::project(
        &serde_json::json!({"accounts":[{
            "id":"cl-a","provider":"claude","email":"linked@example.invalid",
            "user_state":"active","source":{"kind":"cpa","mode":"linked"},
            "health":{"kind":"needs_relogin"},"windows":[{"name":"5h","used_pct":10}]
        }]})
        .to_string(),
        fixtures::clock().utc().into(),
    )
    .unwrap();
    for row in &mut data.sessions.sessions {
        row.account = "cl-a".into();
        row.provider = "claude".into();
    }
    for offset in [0., 96.] {
        let mut state = DashboardState {
            table_offset: offset,
            ..Default::default()
        };
        let v = m::project(&data, &mut state, &fixtures::clock());
        let group = &v.groups[0];
        assert!(!group.relogin, "linked accounts have no own-login action");
        assert!(
            !group.windows.is_empty(),
            "regression requires retained stale observations"
        );
        let scene = Scene::build(&v, &state, [1024., 640.], &Metrics::default());
        let text: Vec<_> = scene
            .draw
            .iter()
            .filter_map(|d| match d {
                Draw::Text { measure, .. } => Some(measure.text.as_str()),
                _ => None,
            })
            .collect();
        assert!(text.contains(&group.relogin_reason.as_str()));
        assert!(!text.contains(&"90% 남음"));
        let group_node = scene
            .accessibility
            .iter()
            .find(|n| n.key == "group:cl-a")
            .unwrap();
        assert!(group_node.name.contains(&group.relogin_reason));
        assert!(!group_node.name.contains("90% 남음"));
        if offset > 0. {
            assert!(
                text.iter().any(|t| t.starts_with("Claude · ")),
                "must exercise the pinned header"
            );
        }
        assert!(
            !scene
                .targets
                .iter()
                .any(|t| matches!(t.target, Target::OpenTrayAccount(_)))
        );
    }
}

#[test]
fn r10_flat_account_chip_fits_its_column_and_leaves_the_session_id_visible() {
    for size in [[1024., 640.], [1180., 960.], [1320., 960.]] {
        let mut state = DashboardState {
            grouped: false,
            ..Default::default()
        };
        let mut v = view(&mut state);
        v.flat.truncate(1);
        v.flat[0].account = "very-long-synthetic-account-name".into();
        v.flat[0].account_provider = "Claude".into();
        v.flat[0].account_chip = Some(v2::Chip {
            text: "한도 도달".into(),
            tone: v2::Tone::Limit,
        });
        let scene = Scene::build(&v, &state, size, &Metrics::default());
        let accessible = scene
            .accessibility
            .iter()
            .find(|node| node.target == Some(Target::Session(v.flat[0].id.clone())))
            .unwrap();
        assert!(accessible.name.contains("Claude · 한도 도달"));
        let row = scene.session_rows[0].rect;
        let labels: Vec<_> = scene
            .draw
            .iter()
            .filter_map(|draw| match draw {
                Draw::Text { rect, measure, .. }
                    if rect.top >= row.top && rect.bottom <= row.bottom =>
                {
                    Some((*rect, measure.text.as_str()))
                }
                _ => None,
            })
            .collect();
        let (name, _) = labels
            .iter()
            .find(|(_, text)| *text == v.flat[0].account)
            .unwrap();
        let (provider, _) = labels
            .iter()
            .find(|(_, text)| *text == " · Claude")
            .unwrap();
        let (status, _) = labels
            .iter()
            .find(|(_, text)| *text == "한도 도달")
            .unwrap();
        assert!(name.right <= provider.left);
        assert!(provider.right + 6. <= status.left + 0.01);
        assert_eq!(height(*status), 16.);
        let suffix = format!(" · {}", v.flat[0].short_id);
        let (id, _) = labels.iter().find(|(_, text)| *text == suffix).unwrap();
        assert!(status.right <= id.left && id.right <= row.right);
    }
}
#[test]
fn r7_popup_targets_are_measured_modal_and_keyboard_order_excludes_underlay() {
    let mut state = DashboardState::default();
    state.activate(&Target::RangeMenu);
    let v = view(&mut state);
    let scene = Scene::build(&v, &state, [1024., 640.], &Metrics::default());
    assert!(scene.modal);
    assert_eq!(scene.focus_order().len(), 4);
    assert!(
        scene
            .focus_order()
            .iter()
            .all(|target| matches!(target, Target::Range(_)))
    );
    assert_eq!(scene.hit_test(1., 1.), Some(&Target::DismissPopup));
    let target = scene
        .targets
        .iter()
        .find(|h| h.target == Target::Range(Range::All))
        .unwrap();
    assert!(target.enabled);
    state.activate(&target.target);
    assert_eq!(state.range, Range::All);
    assert_eq!(state.popup, None);
}
#[test]
fn r7_semantic_keys_are_unique_and_parent_chart_nodes_exist() {
    let mut state = DashboardState {
        tab: Tab::Events,
        event_filter: EventFilter::All,
        ..Default::default()
    };
    for tab in [Tab::Overview, Tab::Events, Tab::Usage] {
        state.tab = tab;
        let v = view(&mut state);
        let scene = Scene::build(&v, &state, [1320., 960.], &Metrics::default());
        let mut keys = std::collections::BTreeSet::new();
        for node in &scene.accessibility {
            assert!(
                keys.insert(node.key.clone()),
                "duplicate UIA key {}",
                node.key
            );
        }
        for node in &scene.accessibility {
            if let Some(parent) = &node.parent {
                assert!(keys.contains(parent));
            }
        }
    }
}
#[test]
fn r7_keyboard_reveal_uses_full_row_geometry_and_rebuilds_visible_semantics() {
    let mut state = DashboardState::default();
    let v = view(&mut state);
    let scene = Scene::build(&v, &state, [1024., 640.], &Metrics::default());
    let region = scene
        .content
        .iter()
        .find(|r| r.region == ScrollRegion::Table)
        .unwrap();
    let row = scene
        .session_rows
        .iter()
        .find(|r| r.rect.top >= region.viewport.bottom)
        .unwrap();
    let id = row.id.clone();
    let node = scene
        .accessibility
        .iter()
        .find(|n| n.key == format!("session:{id}"))
        .unwrap();
    assert_eq!(height(node.rect), 0.);
    let offset = scene.reveal_session(&id).unwrap();
    assert!(offset > 0. && offset <= region.max_offset);
    state.table_offset = offset;
    let scene = Scene::build(&v, &state, [1024., 640.], &Metrics::default());
    let node = scene
        .accessibility
        .iter()
        .find(|n| n.key == format!("session:{id}"))
        .unwrap();
    assert_eq!(height(node.rect), 35.);
    assert_eq!(scene.reveal_session(&id), Some(offset));
    assert_eq!(scene.reveal_session("missing"), None);
    assert!(
        scene
            .targets
            .iter()
            .any(|hit| hit.target == Target::Session(id.clone()) && hit.rect == node.rect)
    );
}
#[test]
fn r7_default_and_narrow_scenes_keep_all_hits_inside_client_and_same_semantic_rects() {
    for size in [[1320., 960.], [1024., 640.], [1179., 800.], [1180., 800.]] {
        for tab in [Tab::Overview, Tab::Events, Tab::Usage] {
            let mut state = DashboardState {
                tab,
                ..Default::default()
            };
            let v = view(&mut state);
            let scene = Scene::build(&v, &state, size, &Metrics::default());
            for hit in &scene.targets {
                assert!(hit.rect.left >= 0. && hit.rect.top >= 0.);
                assert!(hit.rect.right <= size[0] && hit.rect.bottom <= size[1]);
                assert!(width(hit.rect) > 0. && height(hit.rect) > 0.);
                let name = scene
                    .accessibility
                    .iter()
                    .find(|n| n.target.as_ref() == Some(&hit.target) && n.rect == hit.rect);
                assert!(
                    name.is_some(),
                    "missing semantic bounds for {:?}",
                    hit.target
                );
                assert_eq!(
                    scene.hit_test(
                        (hit.rect.left + hit.rect.right) / 2.,
                        (hit.rect.top + hit.rect.bottom) / 2.
                    ),
                    Some(&hit.target)
                );
            }
            let mut depth = 0;
            for draw in &scene.draw {
                match draw {
                    Draw::Clip(_) => depth += 1,
                    Draw::Unclip => {
                        assert!(depth > 0);
                        depth -= 1;
                    }
                    _ => {}
                }
            }
            assert_eq!(depth, 0);
        }
    }
}
#[test]
fn r10_narrow_recent_strip_preserves_list_height_and_wide_side_scroll() {
    for (size, stacked) in [([1320., 960.], false), ([1024., 640.], true)] {
        let mut state = DashboardState {
            table_offset: 1e6,
            side_offset: 1e6,
            ..Default::default()
        };
        let v = view(&mut state);
        let scene = Scene::build(&v, &state, size, &Metrics::default());
        let table = scene
            .content
            .iter()
            .find(|r| r.region == ScrollRegion::Table)
            .unwrap();
        if stacked {
            assert!(!scene.content.iter().any(|r| r.region == ScrollRegion::Side));
            let link = scene
                .targets
                .iter()
                .find(|h| h.target == Target::AllEvents)
                .unwrap();
            assert!(link.rect.top > table.viewport.bottom);
            assert!(
                height(table.viewport) / 35. >= 10.,
                "the 640 DIP window must retain ten full session rows"
            );
            assert_eq!(height(link.rect), 28.);
        } else {
            let side = scene
                .content
                .iter()
                .find(|r| r.region == ScrollRegion::Side)
                .unwrap();
            assert!(side.viewport.left > table.viewport.right);
        }
        for r in &scene.content {
            assert!(r.offset >= 0. && r.offset <= r.max_offset);
            assert!((r.max_offset - (r.content_height - height(r.viewport)).max(0.)).abs() < 0.1);
        }
    }
}
#[test]
fn r7_columns_drop_memo_then_start_then_model_never_scroll_horizontally() {
    let wide = columns(1100., true);
    assert!(wide.iter().any(|c| c.0 == SortKey::Memo));
    let medium = columns(900., true);
    assert!(!medium.iter().any(|c| c.0 == SortKey::Memo));
    assert!(medium.iter().any(|c| c.0 == SortKey::Start));
    let narrow = columns(800., true);
    assert!(!narrow.iter().any(|c| c.0 == SortKey::Start));
    assert!(narrow.iter().any(|c| c.0 == SortKey::Model));
    let tight = columns(640., true);
    assert!(!tight.iter().any(|c| c.0 == SortKey::Model));
    assert!(tight.iter().map(|c| c.2).sum::<f32>() <= 640.);
}
#[test]
fn r7_whole_row_hits_selection_group_disclosure_and_sort_semantics() {
    let mut state = DashboardState::default();
    let v = view(&mut state);
    let scene = Scene::build(&v, &state, [1320., 960.], &Metrics::default());
    let row = scene
        .targets
        .iter()
        .find(|h| matches!(h.target, Target::Session(_)))
        .unwrap();
    for x in [row.rect.left + 1., row.rect.right - 1.] {
        assert_eq!(
            scene.hit_test(x, row.rect.top + height(row.rect) / 2.),
            Some(&row.target)
        );
    }
    assert!(scene.accessibility.iter().any(|n| n.expanded == Some(true)));
    state.grouped = false;
    let v = view(&mut state);
    let scene = Scene::build(&v, &state, [1320., 960.], &Metrics::default());
    assert!(
        scene
            .accessibility
            .iter()
            .any(|n| n.role == Role::ColumnHeader)
    );
}
#[test]
fn r7_event_toolbar_measures_full_counts_and_wraps_without_overlapping_controls() {
    let mut state = DashboardState {
        tab: Tab::Events,
        ..Default::default()
    };
    let mut v = view(&mut state);
    for (_, count) in &mut v.event_counts {
        *count = 2000;
    }
    let probe = Scene::build(&v, &state, [1320., 960.], &Metrics::default());
    let mut metrics = Metrics::default();
    for measure in probe
        .measurements
        .iter()
        .filter(|measure| measure.width == 4096.)
    {
        metrics.widths.insert(
            measure.key(),
            if measure.text.starts_with("한도·계정 전환") || measure.text.starts_with("속도 조절")
            {
                110.
            } else {
                90.
            },
        );
    }
    for (size, toolbar_height) in [
        ([1320., 960.], 52.),
        ([1304., 889.], 52.),
        ([1024., 640.], 88.),
    ] {
        let scene = Scene::build(&v, &state, size, &metrics);
        let controls: Vec<_> = scene
            .targets
            .iter()
            .filter(|hit| {
                matches!(
                    hit.target,
                    Target::EventFilter(_) | Target::EventDaysMenu | Target::EventSearch
                )
            })
            .collect();
        assert_eq!(controls.len(), 9);
        for (i, control) in controls.iter().enumerate() {
            assert!(control.rect.left >= 20. && control.rect.right <= size[0] - 20.);
            for other in controls.iter().skip(i + 1) {
                assert!(intersection(control.rect, other.rect).is_none());
            }
            if let Target::EventFilter(filter) = control.target {
                let expected = if matches!(filter, EventFilter::Moves | EventFilter::Pacing) {
                    110.
                } else {
                    90.
                };
                assert!(width(control.rect) - 16. >= expected);
            }
        }
        let region = scene
            .content
            .iter()
            .find(|r| r.region == ScrollRegion::Events)
            .unwrap();
        assert_eq!(region.viewport.top, 64. + toolbar_height);
    }
}
#[test]
fn r7_future_reset_now_and_high_remaining_annotations_do_not_overlap() {
    let mut state = DashboardState {
        tab: Tab::Usage,
        ..Default::default()
    };
    let mut v = view(&mut state);
    let mut tile = v.usage.remove(0);
    let now = v.clock.utc();
    tile.series = [("5시간", 88.), ("주간", 69.)]
        .into_iter()
        .map(|(name, value)| m::Series {
            name: name.into(),
            points: vec![(now - chrono::Duration::minutes(5), Some(value))],
            resets: vec![],
            future_resets: vec![now + chrono::Duration::minutes(90)],
            remaining: Some(value as u8),
        })
        .collect();
    v.usage = vec![tile];
    for size in [[1320., 960.], [1024., 640.]] {
        let scene = Scene::build(&v, &state, size, &Metrics::default());
        let plot = scene.charts[0].rect;
        let labels: Vec<_> = scene
            .draw
            .iter()
            .filter_map(|draw| {
                if let Draw::Text { rect, measure, .. } = draw {
                    Some((*rect, measure.text.as_str()))
                } else {
                    None
                }
            })
            .collect();
        let now_rect = labels.iter().find(|(_, text)| *text == "지금").unwrap().0;
        let reset_rect = labels
            .iter()
            .find(|(_, text)| text.ends_with(" 초기화"))
            .unwrap()
            .0;
        assert!(reset_rect.bottom <= now_rect.top);
        assert!(now_rect.bottom <= plot.top);
        let end: Vec<_> = labels
            .iter()
            .filter(|(_, text)| matches!(*text, "88%" | "69%"))
            .map(|(r, _)| *r)
            .collect();
        assert_eq!(end.len(), 2);
        assert!(intersection(end[0], end[1]).is_none());
        for r in end {
            assert!(r.top >= plot.top && r.bottom <= plot.bottom);
            assert!(intersection(r, reset_rect).is_none());
            assert!(intersection(r, now_rect).is_none());
        }
    }
}
#[test]
fn r7_usage_maximum_accounts_windows_and_points_stay_within_global_node_budget() {
    let mut state = DashboardState {
        tab: Tab::Usage,
        ..Default::default()
    };
    let mut v = view(&mut state);
    let template = v.usage[0].clone();
    let start = v.clock.day_start;
    v.usage = (0..128)
        .map(|account| {
            let mut tile = template.clone();
            tile.id = format!("synthetic-{account}");
            tile.series = (0..16)
                .map(|window| {
                    let index = account * 16 + window;
                    let count = 24 + usize::from(index < 848);
                    m::Series {
                        name: format!("window-{window}"),
                        points: (0..count)
                            .map(|point| {
                                (
                                    start
                                        + chrono::Duration::milliseconds(
                                            (index * 25 + point) as i64,
                                        ),
                                    if point == 7 { None } else { Some(50.) },
                                )
                            })
                            .collect(),
                        resets: vec![
                            start + chrono::Duration::milliseconds((index * 25 + 11) as i64),
                        ],
                        future_resets: vec![],
                        remaining: Some(50),
                    }
                })
                .collect();
            tile
        })
        .collect();
    for size in [[1320., 960.], [1024., 640.]] {
        let scene = Scene::build(&v, &state, size, &Metrics::default());
        assert!(scene.accessibility.len() <= history::SCENE_NODE_BUDGET);
        assert!(scene.accessibility.len() < 16_384);
        assert_eq!(scene.charts.len(), 128);
        assert_eq!(
            scene
                .charts
                .iter()
                .flat_map(|chart| &chart.series)
                .map(|s| s.points.len())
                .sum::<usize>(),
            50_000
        );
        let tables: Vec<_> = scene
            .accessibility
            .iter()
            .filter(|node| node.role == Role::Table)
            .collect();
        assert_eq!(tables.len(), 128);
        let rows = scene
            .accessibility
            .iter()
            .filter(|node| node.role == Role::Row)
            .count();
        assert!(rows <= history::SCENE_ROW_BUDGET);
        for table in tables {
            assert_eq!(table.value.as_deref(), Some("incomplete"));
            assert!(table.name.contains("대표"));
            assert_eq!(
                scene
                    .accessibility
                    .iter()
                    .filter(|node| node.parent.as_deref() == Some(&table.key)
                        && node.role == Role::ColumnHeader)
                    .count(),
                17
            );
            assert!(
                scene
                    .accessibility
                    .iter()
                    .filter(
                        |node| node.parent.as_deref() == Some(&table.key) && node.role == Role::Row
                    )
                    .count()
                    <= history::CHART_ROW_BUDGET
            );
        }
        assert!(scene.draw.iter().any(
            |draw| matches!(draw,Draw::Text{measure,..}if measure.text.contains("접근성 표: 대표"))
        ));
    }
}
#[test]
fn r7_usage_charts_have_table_children_whole_plot_hit_and_remaining_values() {
    let mut state = DashboardState {
        tab: Tab::Usage,
        ..Default::default()
    };
    let v = view(&mut state);
    let scene = Scene::build(&v, &state, [1320., 960.], &Metrics::default());
    let chart = scene.charts.iter().find(|c| c.id == "cx-a").unwrap();
    assert_eq!(
        scene.hit_test(chart.rect.left + 1., chart.rect.top + 1.),
        Some(&Target::Chart("cx-a".into()))
    );
    let chartkey = "chart:cx-a";
    let table = scene
        .accessibility
        .iter()
        .find(|n| n.parent.as_deref() == Some(chartkey) && n.role == Role::Table)
        .unwrap();
    assert!(table.hidden);
    assert!(
        scene
            .accessibility
            .iter()
            .any(|n| n.parent.as_deref() == Some(&table.key) && n.name.contains("90%"))
    );
    let tip = scene
        .chart_tooltip("cx-a", chart.rect.left + width(chart.rect) * 0.5)
        .unwrap();
    assert!(tip.text.contains("남음"));
    assert!(!tip.text.contains("사용"));
    assert!(scene.draw.iter().any(|d| matches!(
        d,
        Draw::Line {
            dash: Some([5., 3.]),
            ..
        }
    )));
}
#[test]
fn r7_measured_long_events_expand_content_and_all_text_uses_clip() {
    let mut data = fixtures::snapshot();
    data.activity[0].data.labels = vec!["긴이름".repeat(64)];
    data.activity[0].kind = "move".into();
    data.activity[0].data.reason = Some("overload".into());
    let mut state = DashboardState {
        tab: Tab::Events,
        event_filter: EventFilter::All,
        ..Default::default()
    };
    let v = m::project(&data, &mut state, &fixtures::clock());
    let first = Scene::build(&v, &state, [1024., 640.], &Metrics::default());
    let mut metrics = Metrics::default();
    for measure in &first.measurements {
        if measure.wrap {
            metrics.heights.insert(measure.key(), 400.);
        }
    }
    let measured = Scene::build(&v, &state, [1024., 640.], &metrics);
    let before = first
        .content
        .iter()
        .find(|r| r.region == ScrollRegion::Events)
        .unwrap();
    let after = measured
        .content
        .iter()
        .find(|r| r.region == ScrollRegion::Events)
        .unwrap();
    assert!(after.content_height > before.content_height);
    assert!(after.max_offset > 0.);
}
#[test]
fn r7_no_sessions_block_is_centered_with_measured_text_and_no_column_header() {
    let mut data = fixtures::snapshot();
    data.sessions.sessions.clear();
    let mut state = DashboardState::default();
    let v = m::project(&data, &mut state, &fixtures::clock());
    for size in [[1320., 960.], [1024., 640.]] {
        let scene = Scene::build(&v, &state, size, &Metrics::default());
        let viewport = scene
            .content
            .iter()
            .find(|r| r.region == ScrollRegion::Table)
            .unwrap()
            .viewport;
        let cx = (viewport.left + viewport.right) / 2.;
        let button = scene
            .targets
            .iter()
            .find(|h| h.target == Target::Today)
            .unwrap();
        assert!(((button.rect.left + button.rect.right) / 2. - cx).abs() < 0.1);
        assert!(button.rect.top >= viewport.top && button.rect.bottom <= viewport.bottom);
        let icon = scene
            .draw
            .iter()
            .find_map(|d| match d {
                Draw::Fill { rect, radius, .. } if *radius == 24. && width(*rect) == 48. => {
                    Some(*rect)
                }
                _ => None,
            })
            .unwrap();
        assert!(
            ((icon.top + button.rect.bottom) / 2. - (viewport.top + viewport.bottom) / 2.).abs()
                < 0.1
        );
        let help = scene
            .accessibility
            .iter()
            .find(|n| n.key == "empty:sessions:help")
            .unwrap();
        assert!(width(help.rect) <= 620.);
        assert!(((help.rect.left + help.rect.right) / 2. - cx).abs() < 0.1);
        assert!(
            !scene
                .draw
                .iter()
                .any(|d| matches!(d,Draw::Text{measure,..}if measure.text=="모델"))
        );
        let centered = scene
            .draw
            .iter()
            .filter_map(|d| match d {
                Draw::Text { measure, .. } if matches!(measure.align, TextAlign::Center) => {
                    Some(measure)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(centered.iter().any(|m| m.text.starts_with("최근 1시간")));
        assert!(centered.iter().any(|m| m.text.starts_with("codex-qb나")));
        let mut leading = (*centered[0]).clone();
        leading.align = TextAlign::Leading;
        assert_ne!(leading.key(), centered[0].key());
    }
}
#[test]
fn r7_empty_accounts_search_and_disconnected_stale_are_semantic() {
    let mut state = DashboardState::default();
    let accounts = crate::v2::project("{\"accounts\":[]}", fixtures::clock().utc().into()).unwrap();
    let data = m::DashboardSnapshot::new(accounts);
    let v = m::project(&data, &mut state, &fixtures::clock());
    let scene = Scene::build(&v, &state, [1024., 640.], &Metrics::default());
    for (key, text) in [
        ("empty:accounts:title", "아직 계정이 없습니다"),
        ("empty:accounts:help", "Claude나 Codex 계정을 추가하세요."),
    ] {
        let node = scene
            .accessibility
            .iter()
            .find(|node| node.key == key)
            .unwrap();
        assert_eq!(node.role, Role::Text);
        assert!(node.name.starts_with(text));
        assert!(scene.draw.iter().any(|draw|matches!(draw,Draw::Text{rect,measure,..}if *rect==node.rect && measure.text==node.name)));
    }
    assert!(
        scene
            .targets
            .iter()
            .any(|h| h.target == Target::OpenTrayAdd)
    );
    let mut data = fixtures::snapshot();
    data.connection.core_ok = false;
    data.connection.stream_ok = false;
    let mut state = DashboardState {
        search: "no match".into(),
        ..Default::default()
    };
    let v = m::project(&data, &mut state, &fixtures::clock());
    let scene = Scene::build(&v, &state, [1024., 640.], &Metrics::default());
    assert!(
        scene
            .draw
            .iter()
            .any(|d| matches!(d,Draw::Alpha(n) if *n==0.5))
    );
    assert!(scene.draw.iter().any(
        |d| matches!(d,Draw::Text{measure,..} if measure.text.contains("맞는 세션이 없습니다"))
    ));
}
#[test]
fn r7_detail_prefix_stays_only_in_selected_panel_and_close_is_targeted() {
    let data = fixtures::snapshot();
    let mut state = DashboardState::default();
    let ticket = state
        .select(Some(data.sessions.sessions[0].id.clone()))
        .unwrap();
    state.publish_detail(
        &ticket,
        m::SessionDetail {
            session: data.sessions.sessions[0].clone(),
            original_short_id: Some("abcdefgh-private-rest".into()),
            ..Default::default()
        },
    );
    let v = m::project(&data, &mut state, &fixtures::clock());
    let scene = Scene::build(&v, &state, [1320., 960.], &Metrics::default());
    assert!(
        scene
            .targets
            .iter()
            .any(|h| h.target == Target::CloseDetail)
    );
    let text = scene
        .draw
        .iter()
        .filter_map(|d| {
            if let Draw::Text { measure, .. } = d {
                Some(measure.text.as_str())
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("abcdefgh"));
    assert!(!text.contains("private-rest"));
}

#[test]
fn r10_account_boxes_gap_group_children_and_header_height_are_shared_geometry() {
    let mut state = DashboardState::default();
    let v = view(&mut state);
    let scene = Scene::build(&v, &state, [1320., 960.], &Metrics::default());
    for group in &v.groups {
        let key = format!("group:{}", group.id);
        let node = scene.accessibility.iter().find(|n| n.key == key).unwrap();
        assert_eq!(node.role, Role::Group);
        assert!(node.name.contains(&group.name));
        for row in &group.rows {
            let child = scene
                .accessibility
                .iter()
                .find(|n| n.key == format!("session:{}", row.id))
                .unwrap();
            assert_eq!(child.parent.as_deref(), Some(key.as_str()));
        }
    }
    let boxes: Vec<_> = scene
        .draw
        .iter()
        .filter_map(|draw| match draw {
            Draw::Stroke {
                rect,
                color: Color::GroupBorder,
                radius,
                ..
            } if *radius == 6. => Some(*rect),
            _ => None,
        })
        .collect();
    assert_eq!(boxes.len(), v.groups.len());
    for (index, pair) in boxes.windows(2).enumerate() {
        let gap = if v.groups[index].provider == v.groups[index + 1].provider {
            10.
        } else {
            38.
        };
        assert_eq!(pair[1].top - pair[0].bottom, gap);
    }
    for group in &v.groups {
        assert_eq!(group_header_height(group, 900.), 60.);
    }
}
#[test]
fn r10_zero_window_dash_has_neutral_limit_color_unless_paused() {
    let mut state = DashboardState::default();
    let mut v = view(&mut state);
    v.groups.truncate(1);
    v.groups[0].windows[0].remaining_pct = 0;
    for paused in [false, true] {
        v.groups[0].paused = paused;
        let scene = Scene::build(&v, &state, [1320., 960.], &Metrics::default());
        let limit = scene
            .draw
            .iter()
            .filter(|d| {
                matches!(
                    d,
                    Draw::Line {
                        color: Color::ChipBg(v2::Tone::Limit),
                        dash: Some([4., 3.]),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(limit, usize::from(!paused));
    }
}
#[test]
fn r10_client_number_column_is_fixed_and_does_not_share_session_label_box() {
    for flat in [false, true] {
        for width in [640., 714., 900., 1100.] {
            let cols = columns(width, flat);
            assert_eq!(
                cols.iter().find(|c| c.0 == SortKey::Client).unwrap().2,
                172.
            );
            assert!(cols.iter().map(|c| c.2).sum::<f32>() <= width);
        }
    }
    let mut state = DashboardState::default();
    let v = view(&mut state);
    let scene = Scene::build(&v, &state, [1320., 960.], &Metrics::default());
    for row in &v.flat {
        let suffix = format!(" · {}", row.short_id);
        let labels: Vec<_> = scene
            .draw
            .iter()
            .filter_map(|d| {
                if let Draw::Text { rect, measure, .. } = d {
                    Some((*rect, measure.text.as_str()))
                } else {
                    None
                }
            })
            .collect();
        let id = labels.iter().find(|(_, text)| *text == suffix).unwrap().0;
        assert!(width(id) > 0.);
        assert!(
            labels
                .iter()
                .any(|(r, text)| *text == row.label && r.right <= id.left)
        );
    }
}
#[test]
fn r10_offline_keeps_data_text_full_contrast_and_disables_every_control() {
    let mut state = DashboardState::default();
    let mut v = view(&mut state);
    v.connection.core_ok = false;
    let scene = Scene::build(&v, &state, [1320., 960.], &Metrics::default());
    assert!(scene.targets.iter().all(|h| !h.enabled));
    assert!(
        scene
            .accessibility
            .iter()
            .filter(|n| n.target.is_some())
            .all(|n| !n.enabled)
    );
    let mut alpha = 1.;
    let names: std::collections::BTreeSet<_> = v.groups.iter().map(|g| g.name.as_str()).collect();
    let mut checked = 0;
    for draw in &scene.draw {
        match draw {
            Draw::Alpha(value) => alpha = *value,
            Draw::Text {
                measure,
                color: Color::Text,
                ..
            } if names.contains(measure.text.as_str()) => {
                assert_eq!(alpha, 1.);
                checked += 1;
            }
            _ => {}
        }
    }
    assert_eq!(checked, v.groups.len());
}
