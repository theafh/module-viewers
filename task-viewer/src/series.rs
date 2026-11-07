use chrono::{Datelike, Days, NaiveDate};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::harvest::TaskKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    pub project: Arc<str>,
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
    let Some(start) = created.keys().chain(shipped.keys()).min().copied() else {
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
            // The backlog cannot lose a task before it exists; a leave day
            // before `created` counts from `created`. The stored row keeps its
            // real ship day for the weekly and cumulative walks.
            *delta.entry(leave.max(task.created)).or_insert(0) -= 1;
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
    age_points_counted(tasks, today).0
}

/// The mean-age sweep. One work step is one day row plus one task add or one
/// task remove, so the work grows with `days + 2 * tasks` rather than
/// `days * tasks`. The returned count lets a test hold the sweep to that bound.
///
/// A task is active from its `created` day until its leave day, and it drops out
/// on that leave day. A leave day before `created` counts from `created`, the
/// way `live_points` clamps it, so a record whose dates are out of order is
/// added and removed on the same day and counts on no day. The running total is
/// `count * day - sum_created`, which equals the sum of `day - created` over the
/// active tasks without touching each task on each day.
fn age_points_counted(tasks: &[TaskRecord], today: NaiveDate) -> (Vec<AgePoint>, usize) {
    if tasks.is_empty() {
        return (Vec::new(), 0);
    }
    // Per-day (task count, sum of created days) for the adds and the removes,
    // both keyed by day. A created day is stored as its day number so the
    // running total stays one integer subtraction.
    let mut adds: BTreeMap<NaiveDate, (i64, i64)> = BTreeMap::new();
    let mut removes: BTreeMap<NaiveDate, (i64, i64)> = BTreeMap::new();
    for task in tasks {
        let created = i64::from(task.created.num_days_from_ce());
        let add = adds.entry(task.created).or_insert((0, 0));
        add.0 += 1;
        add.1 += created;
        if let Some(leave) = task.leave_date {
            let remove = removes.entry(leave.max(task.created)).or_insert((0, 0));
            remove.0 += 1;
            remove.1 += created;
        }
    }
    let start = tasks.iter().map(|task| task.created).min().unwrap_or(today);
    let mut rows = Vec::new();
    let mut steps = 0_usize;
    let mut count = 0_i64;
    let mut sum_created = 0_i64;
    let mut cur = start;
    while cur <= today {
        steps += 1;
        if let Some((added, added_sum)) = adds.get(&cur) {
            count += added;
            sum_created += added_sum;
            steps += *added as usize;
        }
        if let Some((removed, removed_sum)) = removes.get(&cur) {
            count -= removed;
            sum_created -= removed_sum;
            steps += *removed as usize;
        }
        let avg = if count == 0 {
            0.0
        } else {
            let total = count * i64::from(cur.num_days_from_ce()) - sum_created;
            total as f64 / count as f64
        };
        rows.push((cur, avg, count as usize));
        match cur.checked_add_days(Days::new(1)) {
            Some(next) => cur = next,
            None => break,
        }
    }
    let values: Vec<f64> = rows.iter().map(|(_, avg, _)| *avg).collect();
    let trend = linreg(&values);
    let points = rows
        .into_iter()
        .zip(trend)
        .map(|((date, avg, n), trend)| AgePoint {
            date: crate::dates::format_date(date),
            avg_age: round_to(avg, 2),
            trend: round_to(trend, 2),
            n,
        })
        .collect();
    (points, steps)
}

fn radar(tasks: &[TaskRecord], mode: RadarMode) -> (Vec<LabeledCount>, Vec<LabeledCount>) {
    match mode {
        // The project radar keys on the shared handle, so counting a task costs a
        // refcount bump instead of a `String` allocation per task.
        RadarMode::Project => {
            let mut shipped: BTreeMap<Arc<str>, i64> = BTreeMap::new();
            let mut live: BTreeMap<Arc<str>, i64> = BTreeMap::new();
            for task in tasks {
                let counts = match task.kind {
                    TaskKind::Finished => &mut shipped,
                    TaskKind::Live => &mut live,
                    TaskKind::Deferred => continue,
                };
                *counts.entry(Arc::clone(&task.project)).or_insert(0) += 1;
            }
            radar_rows(&shipped, &live, |project| project.to_string())
        }
        RadarMode::Scope => {
            let mut shipped: BTreeMap<&str, i64> = BTreeMap::new();
            let mut live: BTreeMap<&str, i64> = BTreeMap::new();
            for task in tasks {
                let counts = match task.kind {
                    TaskKind::Finished => &mut shipped,
                    TaskKind::Live => &mut live,
                    TaskKind::Deferred => continue,
                };
                *counts.entry(task.scope.as_str()).or_insert(0) += 1;
            }
            radar_rows(&shipped, &live, |scope| (*scope).to_string())
        }
    }
}

/// Materialize the shared label set once per distinct key, sorted by shipped
/// plus live and then by label, and turn each distinct key into a label string
/// once per chart rather than once per task.
fn radar_rows<K: Ord>(
    shipped: &BTreeMap<K, i64>,
    live: &BTreeMap<K, i64>,
    label: impl Fn(&K) -> String,
) -> (Vec<LabeledCount>, Vec<LabeledCount>) {
    let mut labels: Vec<&K> = shipped
        .keys()
        .chain(live.keys())
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
    let label = &label;
    let shipped_rows = labels
        .iter()
        .map(|key| LabeledCount {
            label: label(key),
            count: shipped.get(*key).copied().unwrap_or(0),
        })
        .collect();
    let live_rows = labels
        .iter()
        .map(|key| LabeledCount {
            label: label(key),
            count: live.get(*key).copied().unwrap_or(0),
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
    use super::{
        AgePoint, RadarMode, TaskRecord, age_points, age_points_counted, dashboard, monday,
    };
    use crate::harvest::TaskKind;
    use chrono::{Days, NaiveDate};
    use std::sync::Arc;

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
            project: Arc::from(project),
            scope: scope.to_string(),
            kind,
            created,
            ship_date: ship,
            leave_date: leave,
        }
    }

    /// The pre-change `age_points`: a day loop with an inner task loop, kept
    /// beside the sweep so the equivalence test compares against the exact
    /// shape the sweep replaced. It is test-only and never runs in production.
    fn age_points_by_day_and_task(tasks: &[TaskRecord], today: NaiveDate) -> Vec<AgePoint> {
        if tasks.is_empty() {
            return Vec::new();
        }
        let start = tasks.iter().map(|task| task.created).min().unwrap_or(today);
        let mut rows = Vec::new();
        let mut cur = start;
        while cur <= today {
            let mut total = 0_i64;
            let mut count = 0_usize;
            for task in tasks {
                if task.created > cur {
                    continue;
                }
                if let Some(leave) = task.leave_date
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
        let trend = super::linreg(&values);
        rows.into_iter()
            .zip(trend)
            .map(|((date, avg, n), trend)| AgePoint {
                date: crate::dates::format_date(date),
                avg_age: super::round_to(avg, 2),
                trend: super::round_to(trend, 2),
                n,
            })
            .collect()
    }

    /// Compare the sweep against the pre-change loop field by field. Both
    /// produce the same integer total and the same rounded output, so the
    /// floats compare bit for bit and no float equality lint applies.
    fn assert_same_age_points(actual: &[AgePoint], expected: &[AgePoint]) {
        assert_eq!(actual.len(), expected.len(), "the day range differs");
        for (actual, expected) in actual.iter().zip(expected) {
            assert_eq!(actual.date, expected.date);
            assert_eq!(actual.n, expected.n, "n differs at {}", actual.date);
            assert_eq!(
                actual.avg_age.to_bits(),
                expected.avg_age.to_bits(),
                "avg_age differs at {}",
                actual.date
            );
            assert_eq!(
                actual.trend.to_bits(),
                expected.trend.to_bits(),
                "trend differs at {}",
                actual.date
            );
        }
    }

    /// Acceptance: the sweep matches the pre-change loop on `avg_age`, `n`, and
    /// `trend`, including a record whose leave day precedes `created` and one
    /// created and left on the same day.
    #[test]
    fn series_age_sweep_matches_the_day_times_task_loop() {
        let today = day(2026, 6, 10);
        let tasks = vec![
            task(
                "alpha",
                "scope",
                TaskKind::Live,
                day(2026, 6, 1),
                None,
                None,
            ),
            task(
                "alpha",
                "scope",
                TaskKind::Finished,
                day(2026, 6, 2),
                Some(day(2026, 6, 5)),
                Some(day(2026, 6, 5)),
            ),
            task(
                "alpha",
                "other",
                TaskKind::Deferred,
                day(2026, 6, 3),
                None,
                Some(day(2026, 6, 8)),
            ),
            // A leave day before `created`: the record counts on no day.
            task(
                "alpha",
                "other",
                TaskKind::Finished,
                day(2026, 6, 6),
                Some(day(2026, 6, 1)),
                Some(day(2026, 6, 1)),
            ),
            // Created and left on the same day: also counts on no day.
            task(
                "alpha",
                "scope",
                TaskKind::Finished,
                day(2026, 6, 4),
                Some(day(2026, 6, 4)),
                Some(day(2026, 6, 4)),
            ),
        ];
        let sweep = age_points(&tasks, today);
        let legacy = age_points_by_day_and_task(&tasks, today);
        assert_same_age_points(&sweep, &legacy);
        let out_of_order = sweep
            .iter()
            .find(|point| point.date == "2026-06-06")
            .expect("the out-of-order record's created day");
        assert_eq!(
            out_of_order.n, 2,
            "the leave-before-created record must not inflate the count"
        );
    }

    /// Acceptance: the fixed 500-task set starting 1900-01-01 stays within the
    /// linear work-step bound for `today` 2026-11-06. The pre-change
    /// day-times-task loop would report about 23,000,000 steps here.
    #[test]
    fn series_age_sweep_stays_linear_for_an_old_task_set() {
        let today = day(2026, 11, 6);
        let start = day(1900, 1, 1);
        let tasks = (0..500_u64)
            .map(|index| {
                let created = start + Days::new(index * 90);
                let leave = created + Days::new(30);
                task(
                    "alpha",
                    "scope",
                    TaskKind::Finished,
                    created,
                    Some(leave),
                    Some(leave),
                )
            })
            .collect::<Vec<_>>();
        let (points, steps) = age_points_counted(&tasks, today);
        let days = (today - start).num_days() + 1;
        assert_eq!(days, 46_331, "the fixture spans the declared day count");
        let bound = days as usize + 2 * tasks.len();
        assert_eq!(bound, 47_331, "the bound is days + 2 * tasks");
        assert!(
            steps <= bound,
            "the sweep reported {steps} work steps for {days} days and {} tasks, above the linear bound of {bound}",
            tasks.len()
        );
        assert_eq!(
            points.len() as i64,
            days,
            "the sweep still emits one point per day"
        );
    }

    #[test]
    fn series_counts_same_week_create_and_ship_and_ages_deferred_until_leave() {
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
        // Finished left the same day; deferred is still open (age 0).
        assert_eq!(age_on_create.n, 1);
        let age_next = board
            .mean_age
            .iter()
            .find(|point| point.date == "2026-06-02")
            .expect("age");
        // Deferred still open plus the live task created that day.
        assert_eq!(age_next.n, 2);
        let age_after_defer = board
            .mean_age
            .iter()
            .find(|point| point.date == "2026-06-03")
            .expect("age");
        // Deferred left; only the live task remains unfinished.
        assert_eq!(age_after_defer.n, 1);
        assert!(monday(day(2026, 6, 7)) == day(2026, 6, 1));
    }

    #[test]
    fn series_live_backlog_clamps_a_leave_day_before_created() {
        let created = day(2026, 6, 10);
        let today = day(2026, 6, 15);
        let tasks = vec![task(
            "alpha",
            "scope",
            TaskKind::Finished,
            created,
            Some(day(2026, 6, 1)),
            Some(day(2026, 6, 1)),
        )];
        let board = dashboard(&tasks, today, RadarMode::Scope, "alpha");
        assert_eq!(board.live_backlog.first().expect("first").live, 0);
        assert_eq!(board.live_backlog.last().expect("last").live, 0);
        assert!(board.live_backlog.iter().all(|point| point.live == 0));
    }

    #[test]
    fn series_cumulative_includes_a_ship_day_before_the_first_created_day() {
        let today = day(2026, 6, 15);
        let tasks = vec![
            task(
                "alpha",
                "scope",
                TaskKind::Live,
                day(2026, 6, 10),
                None,
                None,
            ),
            task(
                "alpha",
                "scope",
                TaskKind::Finished,
                day(2026, 6, 10),
                Some(day(2026, 6, 1)),
                Some(day(2026, 6, 1)),
            ),
        ];
        let board = dashboard(&tasks, today, RadarMode::Scope, "alpha");
        let early = board
            .cumulative
            .iter()
            .find(|point| point.date == "2026-06-01")
            .expect("early ship day");
        assert_eq!(early.shipped, 1);
        let last = board.cumulative.last().expect("cumulative");
        assert_eq!(last.cumulative, board.counts.shipped_events as i64);
    }

    #[test]
    fn series_weekly_includes_a_ship_week_before_the_first_created_week() {
        let today = day(2026, 6, 15);
        let ship_day = day(2026, 6, 1);
        let tasks = vec![
            task(
                "alpha",
                "scope",
                TaskKind::Live,
                day(2026, 6, 10),
                None,
                None,
            ),
            task(
                "alpha",
                "scope",
                TaskKind::Finished,
                day(2026, 6, 10),
                Some(ship_day),
                Some(ship_day),
            ),
        ];
        let board = dashboard(&tasks, today, RadarMode::Scope, "alpha");
        let week = board
            .weekly
            .iter()
            .find(|point| point.week == crate::dates::format_date(monday(ship_day)))
            .expect("early ship week");
        assert_eq!(week.shipped, 1);
        assert_eq!(
            board.weekly.iter().map(|point| point.shipped).sum::<i64>(),
            board.counts.shipped_events as i64
        );
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
                .filter(|task| &*task.project == "alpha")
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
