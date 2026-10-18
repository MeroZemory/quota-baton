//! Bounded accessible projection of observed history; full chart series stay intact.
use crate::dashboard_model::{Series, UsageTile};
use chrono::{DateTime, Utc};
use std::collections::{BTreeMap, BTreeSet};

pub const SCENE_NODE_BUDGET: usize = 12_000;
pub const CHART_ROW_BUDGET: usize = 512;
pub const SCENE_ROW_BUDGET: usize = 4096;

#[derive(Debug)]
pub struct Row {
    pub at: DateTime<Utc>,
    pub values: Vec<Option<f64>>,
}
#[derive(Debug)]
pub struct Table {
    pub rows: Vec<Row>,
    pub total_rows: usize,
    /// False also covers lost source history, not just representative selection.
    pub complete: bool,
    pub point_comparisons: usize,
}

/// Includes chart, table, exact columns and an optional partial-notice node.
pub fn row_limit(series: usize, charts: usize, available_nodes: usize) -> usize {
    if charts == 0 {
        return 0;
    }
    let fixed = series + 4;
    CHART_ROW_BUDGET
        .min(SCENE_ROW_BUDGET / charts)
        .min((available_nodes / charts).saturating_sub(fixed) / (series + 2))
}

fn spread(indices: &[usize], count: usize, selected: &mut BTreeSet<usize>) {
    let count = count.min(indices.len());
    for i in 0..count {
        let at = if count == 1 {
            indices.len() / 2
        } else {
            i * (indices.len() - 1) / (count - 1)
        };
        selected.insert(indices[at]);
    }
}

pub fn value_at(
    series: &Series,
    at: DateTime<Utc>,
    unconfirmed: bool,
    comparisons: &mut usize,
) -> Option<f64> {
    if unconfirmed && series.points.last().is_some_and(|(last, _)| at > *last) {
        return None;
    }
    let end = series.points.partition_point(|(ts, _)| {
        *comparisons += 1;
        *ts <= at
    });
    end.checked_sub(1).and_then(|i| series.points[i].1)
}

pub fn project(tile: &UsageTile, limit: usize) -> Table {
    // Timestamp flags: explicit unknown boundary, reset boundary. No point prefix rescans.
    let mut times = BTreeMap::<DateTime<Utc>, (bool, bool)>::new();
    for series in &tile.series {
        for (at, value) in &series.points {
            times.entry(*at).or_default().0 |= value.is_none();
        }
        for at in &series.resets {
            times.entry(*at).or_default().1 = true;
        }
    }
    let times: Vec<_> = times.into_iter().collect();
    let total_rows = times.len();
    let limit = limit.min(CHART_ROW_BUDGET).min(total_rows);
    let mut selected = BTreeSet::new();
    if limit > 0 {
        selected.insert(0);
    }
    if limit > 1 {
        selected.insert(total_rows - 1);
    }
    if limit > 2 {
        let nulls: Vec<_> = times
            .iter()
            .enumerate()
            .filter_map(|(i, (_, flags))| flags.0.then_some(i))
            .collect();
        let resets: Vec<_> = times
            .iter()
            .enumerate()
            .filter_map(|(i, (_, flags))| flags.1.then_some(i))
            .collect();
        let remaining = limit - selected.len();
        spread(&nulls, remaining.div_ceil(2), &mut selected);
        spread(&resets, limit - selected.len(), &mut selected);
        let all: Vec<_> = (0..total_rows).filter(|i| !selected.contains(i)).collect();
        spread(&all, limit - selected.len(), &mut selected);
    }
    let mut point_comparisons = 0;
    let rows = selected
        .into_iter()
        .map(|index| {
            let at = times[index].0;
            let values = tile
                .series
                .iter()
                .map(|series| value_at(series, at, tile.unconfirmed, &mut point_comparisons))
                .collect();
            Row { at, values }
        })
        .collect();
    Table {
        rows,
        total_rows,
        complete: limit == total_rows && !tile.truncated,
        point_comparisons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{dashboard_fixtures, dashboard_model::Series};
    #[test]
    fn r7_maximum_history_projection_preserves_boundaries_with_binary_work_budget() {
        let mut tile = crate::dashboard_model::project(
            &dashboard_fixtures::snapshot(),
            &mut Default::default(),
            &dashboard_fixtures::clock(),
        )
        .usage
        .remove(0);
        let start = dashboard_fixtures::clock().day_start;
        tile.series = (0..5)
            .map(|window| Series {
                name: format!("{window}h"),
                points: (0..10_000)
                    .map(|i| {
                        (
                            start + chrono::Duration::milliseconds(i * 5 + window),
                            if i == 1234 {
                                None
                            } else {
                                Some((i % 100) as f64)
                            },
                        )
                    })
                    .collect(),
                resets: vec![start + chrono::Duration::milliseconds(30_001 + window)],
                future_resets: vec![],
                remaining: None,
            })
            .collect();
        let table = project(&tile, 512);
        assert_eq!(table.rows.len(), 512);
        assert!(!table.complete);
        assert_eq!(table.total_rows, 50_000);
        assert_eq!(table.rows.first().unwrap().at, start);
        assert_eq!(
            table.rows.last().unwrap().at,
            start + chrono::Duration::milliseconds(49_999)
        );
        let nulls: BTreeSet<_> = tile
            .series
            .iter()
            .flat_map(|series| {
                series
                    .points
                    .iter()
                    .filter_map(|(at, value)| value.is_none().then_some(*at))
            })
            .collect();
        assert!(table.rows.iter().any(|row| nulls.contains(&row.at)));
        assert!(table.rows.iter().any(|row| {
            tile.series
                .iter()
                .any(|series| series.resets.contains(&row.at))
        }));
        assert!(table.point_comparisons <= 512 * 5 * 15);
        let smallest = project(&tile, 4);
        assert_eq!(smallest.rows.len(), 4);
        assert_eq!(smallest.rows.first().unwrap().at, start);
        assert_eq!(
            smallest.rows.last().unwrap().at,
            start + chrono::Duration::milliseconds(49_999)
        );
        assert!(smallest.rows.iter().any(|row| nulls.contains(&row.at)));
        assert!(smallest.rows.iter().any(|row| {
            tile.series
                .iter()
                .any(|series| series.resets.contains(&row.at))
        }));
        assert!(smallest.point_comparisons <= 4 * 5 * 15);
        assert_eq!(
            tile.series.iter().map(|s| s.points.len()).sum::<usize>(),
            50_000
        );
    }
    #[test]
    fn r7_accessible_projection_known_unknown_and_source_loss_are_distinct() {
        let mut tile = crate::dashboard_model::project(
            &dashboard_fixtures::snapshot(),
            &mut Default::default(),
            &dashboard_fixtures::clock(),
        )
        .usage
        .remove(0);
        let start = dashboard_fixtures::clock().day_start;
        tile.series = vec![Series {
            name: "5h".into(),
            points: vec![
                (start, Some(0.)),
                (start + chrono::Duration::seconds(1), None),
            ],
            resets: vec![],
            future_resets: vec![],
            remaining: None,
        }];
        let table = project(&tile, 512);
        assert!(table.complete);
        assert_eq!(table.rows[0].values, vec![Some(0.)]);
        assert_eq!(table.rows[1].values, vec![None]);
        tile.truncated = true;
        assert!(!project(&tile, 512).complete);
    }
}
