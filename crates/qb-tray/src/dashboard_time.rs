//! Pure visible-time scheduling for the dashboard_window path module.
//! Caller owns monotonic mapping, clock resync and frame invalidation.
//! Inputs must come from one completed frame. No projection or text parsing.
//! Approximation: row visibility drives its activity cell; detail uses existing
//! semantic fact keys. Popup menus do not suppress visible background ticks.
//! Midnight is conservative because day-scoped filters/counts are visible.
//! Integration limits: Host must rebase at day_end/clock jumps and round
//! future subsecond deadlines to its effective clock quantum without busy loops.
//! No exact occlusion subtraction or current-filter matching for future events;
//! those conservative cases can wake early, but never impose periodic 500ms work.
use chrono::{DateTime, Duration, Utc};
use qb_tray::{
    Rect,
    dashboard_model::{Clock, DashboardSnapshot, Session},
    geometry::{Scene, Target},
    state::{DashboardState, Range, ScrollRegion, SortKey, Tab},
};

fn intersects(a: Rect, b: Rect) -> bool {
    [
        a.left, a.top, a.right, a.bottom, b.left, b.top, b.right, b.bottom,
    ]
    .iter()
    .all(|v| v.is_finite())
        && a.left.max(b.left) < a.right.min(b.right)
        && a.top.max(b.top) < a.bottom.min(b.bottom)
}

fn region_visible(scene: &Scene, rect: Rect, region: ScrollRegion) -> bool {
    intersects(rect, scene.rect)
        && scene
            .content
            .iter()
            .any(|r| r.region == region && intersects(rect, r.viewport))
}

fn node_visible(scene: &Scene, key: &str) -> bool {
    scene
        .accessibility
        .iter()
        .any(|n| n.key == key && !n.hidden && intersects(n.rect, scene.rect))
}

fn flying(s: &Session) -> bool {
    s.inflight.requests.unwrap_or(0) > 0 || s.inflight.ws_response_in_flight
}

// Integer display buckets are anchored at the source timestamp, including its
// fractional part; never align elapsed minutes to UTC wall-clock minutes.
fn next_bucket(now: DateTime<Utc>, at: DateTime<Utc>, seconds: i64) -> Option<DateTime<Utc>> {
    let elapsed = now.signed_duration_since(at).num_seconds().max(0);
    let bucket = elapsed.checked_div(seconds)?.checked_add(1)?;
    at.checked_add_signed(Duration::seconds(bucket.checked_mul(seconds)?))
}

fn next_age(now: DateTime<Utc>, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
    match now.signed_duration_since(at).num_seconds().max(0) {
        0..=3599 => next_bucket(now, at, 60),
        3600..=86399 => next_bucket(now, at, 3600),
        // Clock::time date labels are covered by the local-day boundary below.
        _ => None,
    }
}

struct Deadlines {
    now: DateTime<Utc>,
    next: Option<DateTime<Utc>>,
}
impl Deadlines {
    fn add(&mut self, at: Option<DateTime<Utc>>) {
        if let Some(at) = at.filter(|at| *at > self.now) {
            self.next = Some(self.next.map_or(at, |old| old.min(at)));
        }
    }
    fn bucket(&mut self, at: Option<DateTime<Utc>>, seconds: i64) {
        self.add(at.and_then(|at| next_bucket(self.now, at, seconds)));
    }
    fn age(&mut self, at: Option<DateTime<Utc>>) {
        self.add(at.and_then(|at| next_age(self.now, at)));
    }
}

/// Use the actual rendered view clock, not the last mailbox clock sample or the
/// current system time. Recompute after a completed frame or relevant input.
///
/// Normally returns a strictly future deadline. A clock outside its supplied day
/// bounds, or a visible chart from a different clock, returns `clock.utc()`:
/// reconcile the clock/frame before scheduling again; do not spin on that value.
pub(super) fn next_visible_time_change(
    snapshot: &DashboardSnapshot,
    state: &DashboardState,
    scene: &Scene,
    clock: &Clock,
    dpi: u32,
) -> Option<DateTime<Utc>> {
    if scene.accessibility.is_empty() && scene.draw.is_empty() {
        return None;
    }
    let now = clock.utc();
    if now < clock.day_start || now >= clock.day_end {
        return Some(now);
    }
    let mut due = Deadlines { now, next: None };
    due.add(Some(clock.day_end));

    if state.tab == Tab::Overview {
        let memo_visible = scene.accessibility.iter().any(|n| {
            n.target == Some(Target::Sort(SortKey::Memo))
                && !n.hidden
                && intersects(n.rect, scene.rect)
        });
        for s in &snapshot.sessions.sessions {
            let visible = scene
                .session_rows
                .iter()
                .any(|row| row.id == s.id && region_visible(scene, row.rect, ScrollRegion::Table));
            if visible {
                if s.inflight_known && flying(s) {
                    due.bucket(s.inflight.oldest_started_at, 1);
                } else {
                    due.age(s.last_used_at);
                }
                if memo_visible
                    && let Some(p) = &s.pacing_wait
                    && p.waiting.unwrap_or(0) > 0
                {
                    due.bucket(p.since, 1);
                }
            }
            // Range changes can change counts/rows even if this row is offscreen.
            // Match in_range's inclusive last_used_at+1h boundary exactly.
            if state.range == Range::Hour && !flying(s) {
                due.add(s.last_used_at);
                due.add(
                    s.last_used_at
                        .and_then(|at| at.checked_add_signed(Duration::hours(1)))
                        .and_then(|at| at.checked_add_signed(Duration::nanoseconds(1))),
                );
            }
        }

        // Empty-last text has no stable semantic key today. Approximate only when
        // no rows exist AND the empty-table help is actually drawn/visible.
        if scene.session_rows.is_empty() && node_visible(scene, "empty:sessions:help") {
            due.age(
                snapshot
                    .sessions
                    .sessions
                    .iter()
                    .filter_map(|s| s.last_used_at)
                    .max(),
            );
        }

        if let Some(d) = state
            .detail
            .as_ref()
            .filter(|d| state.selected.as_deref() == Some(d.session.id.as_str()))
        {
            let s = &d.session;
            let fact =
                |suffix: &str| node_visible(scene, &format!("detail:{}:fact:{suffix}", s.id));
            // Literal UIA identifiers emitted by geometry; no display-name/value
            // parsing. If localization changes keys, replace with stable IDs.
            if fact("시작") {
                due.bucket(s.first_seen_at, 60);
            }
            if fact("마지막 활동") {
                due.age(s.last_used_at);
            }
            if fact("속도 조절 대기")
                && let Some(p) = &s.pacing_wait
                && p.waiting.unwrap_or(0) > 0
            {
                due.bucket(p.since, 1);
            }
            for r in &d.recent_requests {
                due.add(r.ts);
            }
        }
    }

    // Future events become eligible on Overview's recent panel / Events tab.
    // Conservative across filters: bounded source scan, never layout or parsing.
    if matches!(state.tab, Tab::Overview | Tab::Events) {
        for e in &snapshot.activity {
            due.add(e.ts);
        }
    }

    if state.tab == Tab::Usage {
        for chart in &scene.charts {
            if !region_visible(scene, chart.rect, ScrollRegion::Usage) {
                continue;
            }
            if chart.now != now {
                return Some(now);
            }
            // Future reset captions disappear/change at their timestamp even
            // when there is no AccountHistory row for this rendered account.
            for series in &chart.series {
                for at in &series.future_resets {
                    due.add(Some(*at));
                }
            }
            let pixels = f64::from(chart.rect.right - chart.rect.left) * f64::from(dpi) / 96.;
            if pixels.is_finite() && pixels > 0. && now >= chart.from && now < chart.to {
                // Grid anchored at plot.left, one physical pixel maximum drift.
                // Use ms because chart geometry uses num_milliseconds too.
                let span = chart
                    .to
                    .signed_duration_since(chart.from)
                    .num_milliseconds();
                if span > 0 {
                    let elapsed = now.signed_duration_since(chart.from).num_milliseconds() as f64;
                    let column = (elapsed * pixels / span as f64).floor() + 1.;
                    let next_ms = (column * span as f64 / pixels).ceil();
                    if next_ms.is_finite() && next_ms < i64::MAX as f64 {
                        due.add(
                            chart
                                .from
                                .checked_add_signed(Duration::milliseconds(next_ms as i64)),
                        );
                    }
                }
            } else if now < chart.from {
                due.add(Some(chart.from));
            }
            if let Some(h) = snapshot.usage.accounts.iter().find(|h| h.id == chart.id) {
                for w in &h.windows {
                    if w.name.starts_with("7d_") || w.name.contains("model:") {
                        continue;
                    }
                    for (at, _) in &w.points {
                        due.add(Some(*at));
                    }
                    for at in &w.resets {
                        due.add(Some(*at));
                    }
                }
                for (start, end) in &h.credits_spans {
                    due.add(Some(*start));
                    due.add(*end);
                }
            }
        }
    }
    due.next
}

#[cfg(test)]
mod tests {
    use super::*;
    use qb_tray::{dashboard_model as m, geometry as g, v2};

    fn clock() -> Clock {
        let now = DateTime::parse_from_rfc3339("2026-10-21T12:00:00+09:00").unwrap();
        Clock {
            now,
            day_start: now.with_timezone(&Utc) - Duration::hours(12),
            day_end: now.with_timezone(&Utc) + Duration::hours(12),
        }
    }
    fn empty() -> (DashboardSnapshot, DashboardState, Scene, Clock) {
        let c = clock();
        let snapshot = DashboardSnapshot::new(
            v2::project(r#"{"accounts":[],"pools":{}}"#, c.utc().into()).unwrap(),
        );
        let scene = Scene {
            modal: false,
            rect: g::rect(0., 0., 800., 600.),
            content: vec![g::Region {
                region: ScrollRegion::Table,
                viewport: g::rect(0., 100., 800., 300.),
                content_height: 1000.,
                offset: 0.,
                max_offset: 700.,
            }],
            accessibility: vec![node("root", g::rect(0., 0., 800., 600.))],
            draw: vec![],
            targets: vec![],
            measurements: vec![],
            charts: vec![],
            tooltips: vec![],
            session_rows: vec![],
        };
        let mut state = DashboardState::default();
        state.range = Range::All;
        (snapshot, state, scene, c)
    }
    fn node(key: &str, rect: Rect) -> g::AccessibleNode {
        g::AccessibleNode {
            key: key.into(),
            parent: None,
            role: g::Role::Text,
            name: String::new(),
            rect,
            target: None,
            enabled: true,
            selected: false,
            expanded: None,
            value: None,
            hidden: false,
        }
    }
    fn row(snapshot: &mut DashboardSnapshot, scene: &mut Scene, c: &Clock) {
        snapshot.sessions.sessions.push(Session {
            id: "000000000001".into(),
            last_used_at: Some(c.utc() - Duration::seconds(20)),
            ..Default::default()
        });
        scene.session_rows.push(g::SessionGeometry {
            id: "000000000001".into(),
            rect: g::rect(0., 110., 800., 35.),
        });
    }
    #[test]
    fn age_boundaries_and_future_fractional_timestamp() {
        let at = clock().utc() + Duration::milliseconds(123);
        for (elapsed, next) in [
            (0, 60),
            (59, 60),
            (60, 120),
            (3599, 3600),
            (3600, 7200),
            (86399, 86400),
        ] {
            assert_eq!(
                next_age(at + Duration::seconds(elapsed), at),
                Some(at + Duration::seconds(next))
            );
            let mut c = clock();
            c.now = (at + Duration::seconds(elapsed)).with_timezone(c.now.offset());
            let initial = c.age(Some(at));
            c.now = (at + Duration::seconds(next) - Duration::nanoseconds(1))
                .with_timezone(c.now.offset());
            assert_eq!(c.age(Some(at)), initial);
            c.now += Duration::nanoseconds(1);
            assert_ne!(c.age(Some(at)), initial);
        }
        assert_eq!(
            next_age(at - Duration::hours(2), at),
            Some(at + Duration::seconds(60))
        );
        assert_eq!(next_age(at + Duration::days(1), at), None);
        assert_eq!(
            next_age(at + Duration::milliseconds(59999), at),
            Some(at + Duration::seconds(60))
        );
    }
    #[test]
    fn visible_clipped_and_unknown_rows() {
        let (mut snap, state, mut scene, c) = empty();
        row(&mut snap, &mut scene, &c);
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.utc() + Duration::seconds(40))
        );
        scene.session_rows[0].rect.top = 401.;
        scene.session_rows[0].rect.bottom = 435.;
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.day_end)
        );
        scene.session_rows[0].rect = g::rect(0., 390., 800., 35.); // partial row
        snap.sessions.sessions[0].inflight.requests = Some(1);
        snap.sessions.sessions[0].inflight.oldest_started_at =
            Some(c.utc() - Duration::milliseconds(1250));
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.utc() + Duration::milliseconds(750))
        );
        snap.sessions.sessions[0].inflight_known = false;
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.utc() + Duration::seconds(40))
        );
        scene.modal = true;
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.utc() + Duration::seconds(40)),
            "small popup menus do not hide the whole background"
        );
    }
    #[test]
    fn pacing_requires_visible_memo_column() {
        let (mut snap, state, mut scene, c) = empty();
        row(&mut snap, &mut scene, &c);
        snap.sessions.sessions[0].pacing_wait = Some(m::PacingWait {
            waiting: Some(1),
            since: Some(c.utc() - Duration::milliseconds(1500)),
        });
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.utc() + Duration::seconds(40))
        );
        let mut n = node("memo", g::rect(500., 80., 100., 20.));
        n.target = Some(Target::Sort(SortKey::Memo));
        scene.accessibility.push(n);
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.utc() + Duration::milliseconds(500))
        );
    }
    #[test]
    fn detail_minutes_seconds_and_clipped_facts() {
        let (snap, mut state, mut scene, c) = empty();
        let id = "000000000002";
        let detail: m::SessionDetail = serde_json::from_value(serde_json::json!({
            "id":id,"first_seen_at":c.utc()-Duration::seconds(125),
            "pacing_wait":{"waiting":1,"since":c.utc()-Duration::milliseconds(1250)}
        }))
        .unwrap();
        state.selected = Some(id.into());
        state.detail = Some(detail);
        scene.accessibility.push(node(
            &format!("detail:{id}:fact:시작"),
            g::rect(500., 100., 200., 24.),
        ));
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.utc() + Duration::seconds(55))
        );
        scene.accessibility.push(node(
            &format!("detail:{id}:fact:속도 조절 대기"),
            g::rect(500., 130., 200., 24.),
        ));
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.utc() + Duration::milliseconds(750))
        );
        scene.accessibility.last_mut().unwrap().rect = g::rect(0., 0., 0., 0.);
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.utc() + Duration::seconds(55))
        );
        state.selected = None;
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.day_end)
        );
    }
    #[test]
    fn midnight_supplied_day_end_and_hour_inclusive_range() {
        let (mut snap, mut state, mut scene, mut c) = empty();
        row(&mut snap, &mut scene, &c);
        c.day_end = c.utc() + Duration::seconds(10); // producer's boundary wins
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.day_end)
        );
        state.range = Range::Hour;
        snap.sessions.sessions[0].last_used_at = Some(c.utc() - Duration::hours(1));
        scene.session_rows.clear();
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.utc() + Duration::nanoseconds(1))
        );
        c.now += Duration::nanoseconds(1);
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.day_end)
        );
    }
    #[test]
    fn chart_pixel_budget_and_hidden_plot() {
        let (snap, mut state, mut scene, c) = empty();
        state.tab = Tab::Usage;
        scene.content[0].region = ScrollRegion::Usage;
        scene.charts.push(g::ChartData {
            id: "fixture".into(),
            rect: g::rect(0., 100., 480., 200.),
            from: c.day_start,
            to: c.day_end,
            now: c.utc(),
            series: vec![],
            unconfirmed: false,
            offset: *c.now.offset(),
        });
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.utc() + Duration::seconds(180))
        );
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 192),
            Some(c.utc() + Duration::seconds(90))
        );
        scene.charts[0].series.push(m::Series {
            name: "fixture".into(),
            points: vec![],
            resets: vec![],
            future_resets: vec![c.utc() + Duration::seconds(5)],
            remaining: None,
        });
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.utc() + Duration::seconds(5)),
            "rendered resets also matter without raw history"
        );
        scene.charts[0].now -= Duration::milliseconds(1);
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.utc()),
            "mixed frame/clock inputs require reconciliation"
        );
        scene.charts[0].rect = g::rect(0., 700., 480., 200.);
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.day_end)
        );
    }

    #[test]
    fn rendered_clock_and_rollover_are_explicit() {
        let (mut snap, state, mut scene, mut c) = empty();
        row(&mut snap, &mut scene, &c);
        let expected = c.utc() + Duration::seconds(40);
        c.now += Duration::seconds(7);
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(expected)
        );
        c.now = c.day_end.with_timezone(c.now.offset());
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.utc())
        );
        c.now = (c.day_start - Duration::seconds(1)).with_timezone(c.now.offset());
        assert_eq!(
            next_visible_time_change(&snap, &state, &scene, &c, 96),
            Some(c.utc())
        );
    }

    #[test]
    fn rollover_reconciles_to_authoritative_short_or_long_day() {
        let (snap, state, scene, _) = empty();
        for (start, end, hours) in [
            ("2026-03-08T00:00:00-05:00", "2026-03-09T00:00:00-04:00", 23),
            ("2026-11-01T00:00:00-04:00", "2026-11-02T00:00:00-05:00", 25),
        ] {
            let start = DateTime::parse_from_rfc3339(start).unwrap();
            let end = DateTime::parse_from_rfc3339(end)
                .unwrap()
                .with_timezone(&Utc);
            let mut stale = Clock {
                now: start,
                day_start: start.with_timezone(&Utc) - Duration::days(1),
                day_end: start.with_timezone(&Utc),
            };
            // The pure helper requests reconciliation on each stale input; it
            // cannot coalesce Host requests or invent tomorrow's timezone rules.
            for ms in [0, 250, 1000, 14000] {
                stale.now = start + Duration::milliseconds(ms);
                assert_eq!(
                    next_visible_time_change(&snap, &state, &scene, &stale, 96),
                    Some(stale.utc())
                );
            }
            let fresh = Clock {
                now: stale.now,
                day_start: start.with_timezone(&Utc),
                day_end: end,
            };
            assert_eq!(fresh.day_end - fresh.day_start, Duration::hours(hours));
            assert_eq!(
                next_visible_time_change(&snap, &state, &scene, &fresh, 96),
                Some(end)
            );
            assert!(end > fresh.utc());
            assert_ne!(end, fresh.day_start + Duration::days(1));
        }
    }

    #[test]
    fn supplied_short_and_long_day_chart_spans_and_invalid_geometry() {
        for hours in [23, 25] {
            let (snap, mut state, mut scene, mut c) = empty();
            c.day_end = c.day_start + Duration::hours(hours);
            state.tab = Tab::Usage;
            scene.content[0].region = ScrollRegion::Usage;
            // At 12h after day_start, this width places now exactly at pixel 120.
            scene.charts.push(g::ChartData {
                id: "fixture".into(),
                rect: g::rect(0., 100., hours as f32 * 10., 200.),
                from: c.day_start,
                to: c.day_end,
                now: c.utc(),
                series: vec![],
                unconfirmed: false,
                offset: *c.now.offset(),
            });
            assert_eq!(
                next_visible_time_change(&snap, &state, &scene, &c, 96),
                Some(c.utc() + Duration::minutes(6))
            );
            scene.charts[0].rect.right = f32::NAN;
            assert_eq!(
                next_visible_time_change(&snap, &state, &scene, &c, 96),
                Some(c.day_end)
            );
        }
    }
}
