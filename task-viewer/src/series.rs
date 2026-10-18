use chrono::{Datelike, Days, NaiveDate};
use serde::Serialize;
use std::collections::BTreeMap;

use crate::harvest::TaskKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    pub project: String,
    pub scope: String,
    pub kind: TaskKind,
    pub created: NaiveDate,
    pub ship_date: Option<NaiveDate>,
    pub leave_date: Option<NaiveDate>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RadarMode {
    Project,
    Scope,
}

#[derive(Debug, Serialize)]
pub struct Dashboard {
    pub as_of: String,
    pub radar_mode: String,
    pub project_label: String,
    pub counts: Counts,
    pub weekly: Vec<WeekPoint>,
    pub cumulative: Vec<CumPoint>,
    pub shipped_calendar: Vec<DayCount>,
    pub created_calendar: Vec<DayCount>,
    pub live_backlog: Vec<LivePoint>,
    pub mean_age: Vec<AgePoint>,
    pub radar_shipped: Vec<LabeledCount>,
    pub radar_live: Vec<LabeledCount>,
    pub calendar_peak_shipped: i64,
    pub calendar_peak_created: i64,
}

#[derive(Debug, Serialize)]
pub struct Counts {
    pub tasks: usize,
    pub finished: usize,
    pub deferred: usize,
    pub live: usize,
    pub created_events: usize,
    pub shipped_events: usize,
}

#[derive(Debug, Serialize)]
pub struct WeekPoint {
    pub week: String,
    pub created: i64,
    pub shipped: i64,
}

#[derive(Debug, Serialize)]
pub struct CumPoint {
    pub date: String,
    pub shipped: i64,
    pub cumulative: i64,
    pub trend: f64,
}

#[derive(Debug, Serialize)]
pub struct DayCount {
    pub date: String,
    pub count: i64,
}

#[derive(Debug, Serialize)]
pub struct LivePoint {
    pub date: String,
    pub live: i64,
    pub trend: f64,
}

#[derive(Debug, Serialize)]
pub struct AgePoint {
    pub date: String,
    pub avg_age: f64,
    pub trend: f64,
    pub n: usize,
}

#[derive(Debug, Serialize)]
pub struct LabeledCount {
    pub label: String,
    pub count: i64,
}

pub fn dashboard(
    tasks: &[TaskRecord],
    today: NaiveDate,
    mode: RadarMode,
    project_label: &str,
) -> Dashboard {
    let mut created_c: BTreeMap<NaiveDate, i64> = BTreeMap::new();
    let mut shipped_c: BTreeMap<NaiveDate, i64> = BTreeMap::new();
    let mut finished = 0_usize;
    let mut deferred = 0_usize;
    let mut live = 0_usize;
    for task in tasks {
        *created_c.entry(task.created).or_insert(0) += 1;
        match task.kind {
            TaskKind::Finished => {
                finished += 1;
                if let Some(day) = task.ship_date {
                    *shipped_c.entry(day).or_insert(0) += 1;
                }
            }
            TaskKind::Deferred => deferred += 1,
            TaskKind::Live => live += 1,
        }
    }

    let weekly = weekly_points(tasks, today);
    let cumulative = cumulative_points(&shipped_c, today);
    let (shipped_calendar, calendar_peak_shipped) = calendar(&shipped_c, today);
    let (created_calendar, calendar_peak_created) = calendar(&created_c, today);
    let live_backlog = live_points(tasks, today);
    let mean_age = age_points(tasks, today);
    let (radar_shipped, radar_live) = radar(tasks, mode);

    Dashboard {
        as_of: crate::dates::format_date(today),
        radar_mode: match mode {
            RadarMode::Project => "project",
            RadarMode::Scope => "scope",
        }
        .to_string(),
        project_label: project_label.to_string(),
        counts: Counts {
            tasks: tasks.len(),
            finished,
            deferred,
            live,
            created_events: created_c
                .values()
                .copied()
                .map(|value| value as usize)
                .sum(),
            shipped_events: shipped_c
                .values()
                .copied()
                .map(|value| value as usize)
                .sum(),
        },
        weekly,
        cumulative,
        shipped_calendar,
        created_calendar,
        live_backlog,
        mean_age,
        radar_shipped,
        radar_live,
        calendar_peak_shipped,
        calendar_peak_created,
    }
}

fn weekly_points(tasks: &[TaskRecord], today: NaiveDate) -> Vec<WeekPoint> {
    let mut created: BTreeMap<NaiveDate, i64> = BTreeMap::new();
    let mut shipped: BTreeMap<NaiveDate, i64> = BTreeMap::new();
    for task in tasks {
        *created.entry(monday(task.created)).or_insert(0) += 1;
        if task.kind == TaskKind::Finished
            && let Some(day) = task.ship_date
        {
            *shipped.entry(monday(day)).or_insert(0) += 1;
        }
    }
    let Some((&start, _)) = created.iter().next() else {
        return Vec::new();
    };
    let end = monday(today);
    let mut points = Vec::new();
    let mut cur = start;
    while cur <= end {
        points.push(WeekPoint {
            week: crate::dates::format_date(cur),
            created: created.get(&cur).copied().unwrap_or(0),
            shipped: shipped.get(&cur).copied().unwrap_or(0),
        });
        match cur.checked_add_days(Days::new(7)) {
            Some(next) => cur = next,
            None => break,
        }
    }
    points
}

fn cumulative_points(shipped: &BTreeMap<NaiveDate, i64>, today: NaiveDate) -> Vec<CumPoint> {
    let Some((&start, _)) = shipped.iter().next() else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    let mut total = 0_i64;
    let mut cur = start;
    while cur <= today {
        let day = shipped.get(&cur).copied().unwrap_or(0);
        total += day;
        rows.push((cur, day, total));
        match cur.checked_add_days(Days::new(1)) {
            Some(next) => cur = next,
            None => break,
        }
    }
    let values: Vec<f64> = rows.iter().map(|(_, _, total)| *total as f64).collect();
    let trend = linreg(&values);
    rows.into_iter()
        .zip(trend)
        .map(|((date, shipped, cumulative), trend)| CumPoint {
            date: crate::dates::format_date(date),
            shipped,
            cumulative,
            trend: round_to(trend, 2),
        })
        .collect()
}

fn calendar(counter: &BTreeMap<NaiveDate, i64>, today: NaiveDate) -> (Vec<DayCount>, i64) {
    let start = today.checked_sub_days(Days::new(364)).unwrap_or(today);
    let mut rows = Vec::new();
    let mut peak = 0_i64;
    let mut cur = start;
    while cur <= today {
        let count = counter.get(&cur).copied().unwrap_or(0);
        peak = peak.max(count);
        rows.push(DayCount {
            date: crate::dates::format_date(cur),
            count,
        });
        match cur.checked_add_days(Days::new(1)) {
            Some(next) => cur = next,
            None => break,
        }
    }
    (rows, peak)
}

fn live_points(tasks: &[TaskRecord], today: NaiveDate) -> Vec<LivePoint> {
    if tasks.is_empty() {
        return Vec::new();
    }
    let mut delta: BTreeMap<NaiveDate, i64> = BTreeMap::new();
    for task in tasks {
        *delta.entry(task.created).or_insert(0) += 1;
        if let Some(leave) = task.leave_date {
            *delta.entry(leave).or_insert(0) -= 1;
        }
    }
    let start = tasks.iter().map(|task| task.created).min().unwrap_or(today);
    let mut rows = Vec::new();
    let mut stock = 0_i64;
    let mut cur = start;
    while cur <= today {
        stock += delta.get(&cur).copied().unwrap_or(0);
        rows.push((cur, stock));
        match cur.checked_add_days(Days::new(1)) {
            Some(next) => cur = next,
            None => break,
        }
    }
    let values: Vec<f64> = rows.iter().map(|(_, stock)| *stock as f64).collect();
    let trend = linreg(&values);
    rows.into_iter()
        .zip(trend)
        .map(|((date, live), trend)| LivePoint {
            date: crate::dates::format_date(date),
            live,
            trend: round_to(trend, 2),
        })
        .collect()
}

fn age_points(tasks: &[TaskRecord], today: NaiveDate) -> Vec<AgePoint> {
    let eligible: Vec<&TaskRecord> = tasks
        .iter()
        .filter(|task| task.kind != TaskKind::Deferred)
        .collect();
    if eligible.is_empty() {
        return Vec::new();
    }
    let start = eligible
        .iter()
        .map(|task| task.created)
        .min()
        .unwrap_or(today);
    let mut rows = Vec::new();
    let mut cur = start;
    while cur <= today {
        let mut total = 0_i64;
        let mut count = 0_usize;
        for task in &eligible {
            if task.created > cur {
                continue;
            }
            if task.kind == TaskKind::Finished
                && let Some(leave) = task.leave_date
                && leave <= cur
            {
                continue;
            }
            total += (cur - task.created).num_days();
            count += 1;
        }
        let avg = if count == 0 {
            0.0
        } else {
            total as f64 / count as f64
        };
        rows.push((cur, avg, count));
        match cur.checked_add_days(Days::new(1)) {
            Some(next) => cur = next,
            None => break,
        }
    }
    let values: Vec<f64> = rows.iter().map(|(_, avg, _)| *avg).collect();
    let trend = linreg(&values);
    rows.into_iter()
        .zip(trend)
        .map(|((date, avg, n), trend)| AgePoint {
            date: crate::dates::format_date(date),
            avg_age: round_to(avg, 2),
            trend: round_to(trend, 2),
            n,
        })
        .collect()
}

fn radar(tasks: &[TaskRecord], mode: RadarMode) -> (Vec<LabeledCount>, Vec<LabeledCount>) {
    let mut shipped: BTreeMap<String, i64> = BTreeMap::new();
    let mut live: BTreeMap<String, i64> = BTreeMap::new();
    for task in tasks {
        let label = match mode {
            RadarMode::Project => task.project.clone(),
            RadarMode::Scope => task.scope.clone(),
        };
        match task.kind {
            TaskKind::Finished => *shipped.entry(label).or_insert(0) += 1,
            TaskKind::Live => *live.entry(label).or_insert(0) += 1,
            TaskKind::Deferred => {}
        }
    }
    let mut labels: Vec<String> = shipped
        .keys()
        .chain(live.keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    labels.sort_by(|left, right| {
        let left_total =
            shipped.get(left).copied().unwrap_or(0) + live.get(left).copied().unwrap_or(0);
        let right_total =
            shipped.get(right).copied().unwrap_or(0) + live.get(right).copied().unwrap_or(0);
        right_total.cmp(&left_total).then_with(|| left.cmp(right))
    });
    let shipped_rows = labels
        .iter()
        .map(|label| LabeledCount {
            label: label.clone(),
            count: shipped.get(label).copied().unwrap_or(0),
        })
        .collect();
    let live_rows = labels
        .iter()
        .map(|label| LabeledCount {
            label: label.clone(),
            count: live.get(label).copied().unwrap_or(0),
        })
        .collect();
    (shipped_rows, live_rows)
}

fn monday(date: NaiveDate) -> NaiveDate {
    let offset = u64::from(date.weekday().num_days_from_monday());
    date.checked_sub_days(Days::new(offset)).unwrap_or(date)
}

fn linreg(values: &[f64]) -> Vec<f64> {
    let count = values.len();
    if count == 0 {
        return Vec::new();
    }
    if count == 1 {
        return vec![values[0]];
    }
    let length = count as f64;
    let mean_x = (length - 1.0) / 2.0;
    let mean_y = values.iter().sum::<f64>() / length;
    let mut numerator = 0.0;
    let mut denominator = 0.0;
    for (index, value) in values.iter().enumerate() {
        let x = index as f64 - mean_x;
        numerator += x * (value - mean_y);
        denominator += x * x;
    }
    if denominator.abs() < f64::EPSILON {
        denominator = 1.0;
    }
    let slope = numerator / denominator;
    let intercept = mean_y - slope * mean_x;
    (0..count)
        .map(|index| intercept + slope * index as f64)
        .collect()
}

fn round_to(value: f64, places: i32) -> f64 {
    let factor = 10_f64.powi(places);
    (value * factor).round() / factor
}

#[cfg(test)]
mod tests {
    use super::{RadarMode, TaskRecord, dashboard, monday};
    use crate::harvest::TaskKind;
    use chrono::NaiveDate;

    fn day(year: i32, month: u32, date: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, date).expect("valid date")
    }

    fn task(
        project: &str,
        scope: &str,
        kind: TaskKind,
        created: NaiveDate,
        ship: Option<NaiveDate>,
        leave: Option<NaiveDate>,
    ) -> TaskRecord {
        TaskRecord {
            project: project.to_string(),
            scope: scope.to_string(),
            kind,
            created,
            ship_date: ship,
            leave_date: leave,
        }
    }

    #[test]
    fn series_counts_same_week_create_and_ship_and_drops_deferred_from_age() {
        let created = day(2026, 6, 1);
        let today = day(2026, 6, 8);
        let tasks = vec![
            task(
                "alpha",
                "scope",
                TaskKind::Finished,
                created,
                Some(created),
                Some(created),
            ),
            task(
                "alpha",
                "scope",
                TaskKind::Deferred,
                created,
                None,
                Some(day(2026, 6, 3)),
            ),
            task(
                "alpha",
                "other",
                TaskKind::Live,
                day(2026, 6, 2),
                None,
                None,
            ),
        ];
        let board = dashboard(&tasks, today, RadarMode::Scope, "alpha");
        let week = board
            .weekly
            .iter()
            .find(|point| point.week == "2026-06-01")
            .expect("week");
        assert_eq!(week.created, 3);
        assert_eq!(week.shipped, 1);
        assert_eq!(board.counts.shipped_events, 1);
        assert_eq!(board.counts.deferred, 1);
        let age_on_create = board
            .mean_age
            .iter()
            .find(|point| point.date == "2026-06-01")
            .expect("age");
        assert_eq!(age_on_create.n, 0);
        let age_next = board
            .mean_age
            .iter()
            .find(|point| point.date == "2026-06-02")
            .expect("age");
        assert_eq!(age_next.n, 1);
        assert!(monday(day(2026, 6, 7)) == day(2026, 6, 1));
    }

    #[test]
    fn series_all_projects_radar_uses_project_counts() {
        let today = day(2026, 6, 2);
        let tasks = vec![
            task(
                "alpha",
                "one",
                TaskKind::Finished,
                day(2026, 6, 1),
                Some(day(2026, 6, 2)),
                Some(day(2026, 6, 2)),
            ),
            task("alpha", "two", TaskKind::Live, day(2026, 6, 1), None, None),
            task("beta", "one", TaskKind::Live, day(2026, 6, 1), None, None),
            task("beta", "two", TaskKind::Live, day(2026, 6, 2), None, None),
        ];
        let all = dashboard(&tasks, today, RadarMode::Project, "All projects");
        assert_eq!(all.radar_mode, "project");
        assert_eq!(all.radar_live.len(), 2);
        let beta = all
            .radar_live
            .iter()
            .find(|row| row.label == "beta")
            .expect("beta");
        assert_eq!(beta.count, 2);
        let alpha_ship = all
            .radar_shipped
            .iter()
            .find(|row| row.label == "alpha")
            .expect("alpha");
        assert_eq!(alpha_ship.count, 1);
        let one = dashboard(
            &tasks
                .into_iter()
                .filter(|task| task.project == "alpha")
                .collect::<Vec<_>>(),
            today,
            RadarMode::Scope,
            "alpha",
        );
        assert_eq!(one.radar_mode, "scope");
        assert!(
            one.radar_live
                .iter()
                .any(|row| row.label == "two" && row.count == 1)
        );
        assert!(one.radar_shipped.iter().all(|row| row.label != "alpha"));
    }
}
