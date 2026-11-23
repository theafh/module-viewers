use chrono::{Datelike, Days, NaiveDate};
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;

use crate::dates::{PLAUSIBLE_FLOOR, SERIES_FLOOR};
use crate::harvest::TaskKind;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    /// The integer id of the repository the row belongs to, so a drill-down
    /// link can name it the way every other route does.
    pub repository_id: i64,
    pub project: Arc<str>,
    /// The stored path of the task file, relative to its repository. It names
    /// the row in a warning and is otherwise not charted.
    pub path: String,
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
    /// The page's project filter, as the query spells it: `all` or a repository
    /// id. A drill-down link carries it so the list opens on the same project.
    pub project: String,
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
    /// The repository a project-mode spoke names, so its drill-down link can
    /// open that repository's tasks. A scope-mode spoke carries `None` and
    /// relies on the page's project filter instead.
    pub repository_id: Option<i64>,
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

    // One warning per after-today event, emitted from the single site that
    // decides the fold, so a ship folded into both the weekly and the
    // cumulative series is named once instead of once per series. The event is
    // still counted in the last charted point, so a mistyped future year stays
    // visible instead of vanishing.
    for task in tasks {
        if task.created > today {
            tracing::warn!(
                repository = %task.project,
                path = %task.path,
                date = %crate::dates::format_date(task.created),
                "task created after today; counted in the last charted point"
            );
        }
        if task.kind == TaskKind::Finished
            && let Some(day) = task.ship_date
            && day > today
        {
            tracing::warn!(
                repository = %task.project,
                path = %task.path,
                date = %crate::dates::format_date(day),
                "task shipped after today; counted in the last charted point"
            );
        }
    }

    // One warning per row dated before the plausible floor, emitted from the
    // same single site as the after-today warnings. The row keeps its real
    // values and folds into the first charted point, so a mistyped year stays
    // visible while the series stay bounded.
    for task in tasks {
        let ship = task.ship_date.filter(|_| task.kind == TaskKind::Finished);
        let earliest = [Some(task.created), ship]
            .into_iter()
            .flatten()
            .filter(|day| *day < PLAUSIBLE_FLOOR)
            .min();
        if let Some(day) = earliest {
            tracing::warn!(
                repository = %task.project,
                path = %task.path,
                date = %crate::dates::format_date(day),
                "task dated before the plausible floor; counted in the first charted point"
            );
        }
    }

    let weekly = weekly_points(&created_c, &shipped_c, today);
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
        project: "all".to_string(),
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

fn weekly_points(
    created: &BTreeMap<NaiveDate, i64>,
    shipped: &BTreeMap<NaiveDate, i64>,
    today: NaiveDate,
) -> Vec<WeekPoint> {
    let end = monday(today);
    // Fold every event whose Monday is after today's Monday into today's week,
    // so a future-dated event still reaches the bars. An event after today but
    // inside today's week already lands in that week's row, so it stays where
    // it is instead of counting twice.
    let mut created_weeks: BTreeMap<NaiveDate, i64> = BTreeMap::new();
    for (&day, &count) in created {
        *created_weeks.entry(monday(day).min(end)).or_insert(0) += count;
    }
    let mut shipped_weeks: BTreeMap<NaiveDate, i64> = BTreeMap::new();
    for (&day, &count) in shipped {
        *shipped_weeks.entry(monday(day).min(end)).or_insert(0) += count;
    }
    let Some(first) = created_weeks
        .keys()
        .chain(shipped_weeks.keys())
        .min()
        .copied()
    else {
        return Vec::new();
    };
    // The start caps at today's Monday too, so a series whose only event lies
    // after today still yields one row.
    let start = first.max(monday(series_floor())).min(end);
    let mut points = Vec::new();
    // Weeks before the first charted week fold into that week's row, the way
    // `cumulative_points` seeds its running total, so the weekly sums still
    // reach `counts`.
    let mut carry_created = created_weeks
        .range(..start)
        .map(|(_, count)| *count)
        .sum::<i64>();
    let mut carry_shipped = shipped_weeks
        .range(..start)
        .map(|(_, count)| *count)
        .sum::<i64>();
    let mut cur = start;
    while cur <= end {
        points.push(WeekPoint {
            week: crate::dates::format_date(cur),
            created: std::mem::take(&mut carry_created)
                + created_weeks.get(&cur).copied().unwrap_or(0),
            shipped: std::mem::take(&mut carry_shipped)
                + shipped_weeks.get(&cur).copied().unwrap_or(0),
        });
        match cur.checked_add_days(Days::new(7)) {
            Some(next) => cur = next,
            None => break,
        }
    }
    points
}

fn cumulative_points(shipped: &BTreeMap<NaiveDate, i64>, today: NaiveDate) -> Vec<CumPoint> {
    if shipped.is_empty() {
        return Vec::new();
    }
    // Fold every ship dated after today into today's row, because this series
    // plots the shipped count and has one row per day, so it carries no future
    // day. The line then ends at `counts.shipped_events`.
    let mut folded: BTreeMap<NaiveDate, i64> = BTreeMap::new();
    for (&day, &count) in shipped {
        *folded.entry(day.min(today)).or_insert(0) += count;
    }
    let Some((&first, _)) = folded.iter().next() else {
        return Vec::new();
    };
    // The start caps at today, so a series whose only ship lies after today
    // still yields one row dated today.
    let start = first.max(series_floor()).min(today);
    let mut rows = Vec::new();
    // Ships before the first charted day fold into that day's level, so the
    // line still ends at `counts.shipped_events`.
    let mut total = folded.range(..start).map(|(_, count)| *count).sum::<i64>();
    let mut cur = start;
    while cur <= today {
        let day = folded.get(&cur).copied().unwrap_or(0);
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
    // The start caps at today, so a task created after today stays out of the
    // rows through today while a series whose every task lies after today
    // still yields today's row. Its created day is never reached, so the walk
    // never adds it.
    let start = start.max(series_floor()).min(today);
    let mut rows = Vec::new();
    // Tasks created before the first charted day and still open on it are
    // already in the backlog, so they seed the first row instead of a walk
    // from their own day.
    let mut stock = 0_i64;
    for task in tasks {
        if task.created < start
            && task
                .leave_date
                .is_none_or(|leave| leave.max(task.created) >= start)
        {
            stock += 1;
        }
    }
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
///
/// Only a day that holds at least one unfinished task emits a point, so an
/// empty backlog leaves the series short instead of reporting an average of 0,
/// and the trend is fitted to exactly the days the payload carries.
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
    // The start caps at today, so a task created after today is never added on
    // its future created day: it stays out of today's row and today's average,
    // and the running total never subtracts a future day. A series whose every
    // task lies after today walks only today, where the zero-count rule below
    // emits no point.
    let start = start.max(series_floor()).min(today);
    let mut rows = Vec::new();
    let mut steps = 0_usize;
    // Tasks created before the first charted day and still open on it seed the
    // running count and created-day sum, so the first row applies only that
    // day's own adds and removes while an old task still counts its real age.
    let mut count = 0_i64;
    let mut sum_created = 0_i64;
    for task in tasks {
        if task.created < start
            && task
                .leave_date
                .is_none_or(|leave| leave.max(task.created) >= start)
        {
            count += 1;
            sum_created += i64::from(task.created.num_days_from_ce());
        }
    }
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
        // A day whose backlog is empty carries no average, so it emits no point
        // and the trend is fitted only to the days that do. The running count
        // and created-day sum still advance on every day, empty ones included,
        // so the linear work bound is unchanged.
        if count > 0 {
            let total = count * i64::from(cur.num_days_from_ce()) - sum_created;
            let avg = total as f64 / count as f64;
            rows.push((cur, avg, count as usize));
        }
        match cur.checked_add_days(Days::new(1)) {
            Some(next) => cur = next,
            None => break,
        }
    }
    let values: Vec<f64> = rows.iter().map(|(_, avg, _)| *avg).collect();
    // The rows skip the days whose backlog is empty, so an index step is not a
    // day. Fit against each point's day offset from the first emitted point, so
    // the dashed line stays the least-squares fit in the date space it is drawn
    // in and a gap does not kink its slope.
    let offsets: Vec<f64> = match rows.first() {
        Some((first, _, _)) => rows
            .iter()
            .map(|(date, _, _)| f64::from(date.num_days_from_ce() - first.num_days_from_ce()))
            .collect(),
        None => Vec::new(),
    };
    let trend = linreg_at(&offsets, &values);
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
        // The project radar keys on the shared handle and the repository id, so
        // counting a task costs a refcount bump instead of a `String`
        // allocation per task while a spoke still names its repository.
        RadarMode::Project => {
            let mut shipped: BTreeMap<(Arc<str>, i64), i64> = BTreeMap::new();
            let mut live: BTreeMap<(Arc<str>, i64), i64> = BTreeMap::new();
            for task in tasks {
                let counts = match task.kind {
                    TaskKind::Finished => &mut shipped,
                    TaskKind::Live => &mut live,
                    TaskKind::Deferred => continue,
                };
                *counts
                    .entry((Arc::clone(&task.project), task.repository_id))
                    .or_insert(0) += 1;
            }
            radar_rows(
                &shipped,
                &live,
                |project| project.0.to_string(),
                |project| Some(project.1),
            )
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
            radar_rows(&shipped, &live, |scope| (*scope).to_string(), |_| None)
        }
    }
}

/// Materialize the shared label set once per distinct key, sorted by shipped
/// plus live and then by label, and turn each distinct key into a label string
/// once per chart rather than once per task. `repository` names the repository
/// a project-mode key stands for; a scope-mode key names none.
fn radar_rows<K: Ord>(
    shipped: &BTreeMap<K, i64>,
    live: &BTreeMap<K, i64>,
    label: impl Fn(&K) -> String,
    repository: impl Fn(&K) -> Option<i64>,
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
    let repository = &repository;
    let shipped_rows = labels
        .iter()
        .map(|key| LabeledCount {
            label: label(key),
            count: shipped.get(*key).copied().unwrap_or(0),
            repository_id: repository(key),
        })
        .collect();
    let live_rows = labels
        .iter()
        .map(|key| LabeledCount {
            label: label(key),
            count: live.get(*key).copied().unwrap_or(0),
            repository_id: repository(key),
        })
        .collect();
    (shipped_rows, live_rows)
}

fn monday(date: NaiveDate) -> NaiveDate {
    let offset = u64::from(date.weekday().num_days_from_monday());
    date.checked_sub_days(Days::new(offset)).unwrap_or(date)
}

/// The day every series starts no earlier than. A git-backed task file cannot
/// predate git's first commit, so `PLAUSIBLE_FLOOR` bounds the walk while
/// `SERIES_FLOOR` stays the absolute lower bound it never falls below.
fn series_floor() -> NaiveDate {
    PLAUSIBLE_FLOOR.max(SERIES_FLOOR)
}

/// The least-squares fit against the array index, returned at each index. The
/// cumulative and backlog charts use it because their rows are one per calendar
/// day, so one index step is one day.
fn linreg(values: &[f64]) -> Vec<f64> {
    let xs: Vec<f64> = (0..values.len()).map(|index| index as f64).collect();
    linreg_at(&xs, values)
}

/// The least-squares fit of `values` against their own x positions, returned at
/// each position. The mean-age chart skips the days whose backlog is empty, so
/// it fits against each point's day offset from the first charted day and the
/// slope stays a per-day rate in the date space the chart draws.
fn linreg_at(xs: &[f64], values: &[f64]) -> Vec<f64> {
    let count = values.len();
    if count == 0 {
        return Vec::new();
    }
    if count == 1 {
        return vec![values[0]];
    }
    let length = count as f64;
    let mean_x = xs.iter().sum::<f64>() / length;
    let mean_y = values.iter().sum::<f64>() / length;
    let mut numerator = 0.0;
    let mut denominator = 0.0;
    for (x, value) in xs.iter().zip(values) {
        let offset = x - mean_x;
        numerator += offset * (value - mean_y);
        denominator += offset * offset;
    }
    if denominator.abs() < f64::EPSILON {
        denominator = 1.0;
    }
    let slope = numerator / denominator;
    let intercept = mean_y - slope * mean_x;
    xs.iter().map(|x| intercept + slope * x).collect()
}

fn round_to(value: f64, places: i32) -> f64 {
    let factor = 10_f64.powi(places);
    (value * factor).round() / factor
}

#[cfg(test)]
mod tests {
    use super::{
        AgePoint, RadarMode, TaskRecord, age_points, age_points_counted, cumulative_points,
        dashboard, monday, weekly_points,
    };
    use crate::dates::{PLAUSIBLE_FLOOR, SERIES_FLOOR};
    use crate::harvest::TaskKind;
    use chrono::{Datelike, Days, NaiveDate};
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use tracing_subscriber::layer::SubscriberExt;

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
            repository_id: 0,
            project: Arc::from(project),
            path: format!("tasks/{project}_{scope}.md"),
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
            // The sweep emits no point for an empty backlog, so the reference
            // loop skips that day too.
            if count > 0 {
                rows.push((cur, total as f64 / count as f64, count));
            }
            match cur.checked_add_days(Days::new(1)) {
                Some(next) => cur = next,
                None => break,
            }
        }
        let values: Vec<f64> = rows.iter().map(|(_, avg, _)| *avg).collect();
        // The sweep fits the trend against each point's day offset from the
        // first emitted point, so the reference loop does the same and the two
        // still compare field by field.
        let offsets: Vec<f64> = match rows.first() {
            Some((first, _, _)) => rows
                .iter()
                .map(|(date, _, _)| f64::from(date.num_days_from_ce() - first.num_days_from_ce()))
                .collect(),
            None => Vec::new(),
        };
        let trend = super::linreg_at(&offsets, &values);
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

    /// Covers: the sweep matches the pre-change loop on `avg_age`, `n`, and
    /// `trend`, including a record whose leave day precedes `created`, one
    /// created and left on the same day, and an empty-backlog day the sweep
    /// omits.
    #[test]
    fn series_age_sweep_matches_the_day_times_task_loop() {
        let today = day(2026, 6, 10);
        let tasks = vec![
            // Leaves on the day both remaining tasks leave, so the next day
            // holds no unfinished task.
            task(
                "alpha",
                "scope",
                TaskKind::Deferred,
                day(2026, 6, 1),
                None,
                Some(day(2026, 6, 8)),
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
            // Created after the empty day, so 2026-06-09 carries a point again.
            task(
                "alpha",
                "scope",
                TaskKind::Live,
                day(2026, 6, 9),
                None,
                None,
            ),
        ];
        let sweep = age_points(&tasks, today);
        let legacy = age_points_by_day_and_task(&tasks, today);
        assert_same_age_points(&sweep, &legacy);
        assert!(
            sweep.iter().all(|point| point.n > 0),
            "no emitted point carries an empty backlog"
        );
        assert!(
            !sweep.iter().any(|point| point.date == "2026-06-08"),
            "the empty-backlog day emits no point"
        );
        let out_of_order = sweep
            .iter()
            .find(|point| point.date == "2026-06-06")
            .expect("the out-of-order record's created day");
        assert_eq!(
            out_of_order.n, 2,
            "the leave-before-created record must not inflate the count"
        );
    }

    /// Covers: the fixed 500-task set starting at the chart floor stays
    /// within the linear work-step bound for `today` 2026-11-06. The pre-change
    /// day-times-task loop would report millions of steps here.
    #[test]
    fn series_age_sweep_stays_linear_for_an_old_task_set() {
        let today = day(2026, 11, 6);
        let start = PLAUSIBLE_FLOOR;
        let span = (today - start).num_days();
        let tasks = (0..500_u64)
            .map(|index| {
                let created = start + Days::new(index * 15);
                let leave = created + Days::new(15);
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
        let days = span + 1;
        assert_eq!(days, 7_884, "the fixture spans the declared day count");
        let bound = days as usize + 2 * tasks.len();
        assert_eq!(bound, 8_884, "the bound is days + 2 * tasks");
        assert!(
            steps <= bound,
            "the sweep reported {steps} work steps for {days} days and {} tasks, above the linear bound of {bound}",
            tasks.len()
        );
        // Every window holds 15 unfinished days and the windows start 15 days
        // apart, so each task contributes its own 15 points and no day is
        // shared.
        assert_eq!(
            points.len(),
            tasks.len() * 15,
            "the sweep emits one point per unfinished day"
        );
        assert!(
            points.iter().all(|point| point.n > 0),
            "no emitted point carries an empty backlog"
        );
    }

    /// Covers: one mistyped year cannot stretch any series, because every
    /// walk starts at the chart floor.
    #[test]
    fn series_stays_bounded_when_a_task_year_is_mistyped() {
        let today = day(2026, 11, 6);
        let tasks = vec![
            task("alpha", "scope", TaskKind::Live, day(26, 3, 1), None, None),
            task(
                "alpha",
                "scope",
                TaskKind::Finished,
                day(2026, 1, 5),
                Some(day(2026, 2, 1)),
                Some(day(2026, 2, 1)),
            ),
        ];
        let board = dashboard(&tasks, today, RadarMode::Scope, "alpha");
        let span = (today - PLAUSIBLE_FLOOR).num_days();
        for (name, len) in [
            ("cumulative", board.cumulative.len()),
            ("live_backlog", board.live_backlog.len()),
            ("mean_age", board.mean_age.len()),
        ] {
            assert!(
                len as i64 <= span + 1,
                "{name} has {len} points, above the bounded {span} days plus one"
            );
        }
        assert!(
            board.weekly.len() as i64 <= span / 7 + 2,
            "the weekly series has {} points, above the bounded {} weeks plus two",
            board.weekly.len(),
            span / 7
        );
    }

    /// Covers: a task created before the floor folds into the first charted
    /// day exactly once, and the radar and the counts still see it.
    #[test]
    fn series_backlog_folds_a_pre_floor_created_day_into_the_floor() {
        let today = day(2026, 11, 6);
        let tasks = vec![
            task(
                "alpha",
                "ancient",
                TaskKind::Live,
                day(26, 3, 1),
                None,
                None,
            ),
            task(
                "alpha",
                "modern",
                TaskKind::Live,
                PLAUSIBLE_FLOOR,
                None,
                None,
            ),
        ];
        let board = dashboard(&tasks, today, RadarMode::Scope, "alpha");
        let first = board.live_backlog.first().expect("first point");
        assert_eq!(first.date, "2005-04-07");
        assert_eq!(first.live, 2, "one fold, not a double count");
        let ancient = board
            .radar_live
            .iter()
            .find(|row| row.label == "ancient")
            .expect("the pre-floor scope");
        assert_eq!(ancient.count, 1);
        assert_eq!(board.counts.created_events, 2);
    }

    /// Covers: ships before the floor fold into the first charted day and
    /// the line still ends at the shipped event count.
    #[test]
    fn series_cumulative_folds_a_pre_floor_ship_day_into_the_floor() {
        let today = day(2026, 11, 6);
        let tasks = vec![
            task(
                "alpha",
                "scope",
                TaskKind::Finished,
                day(26, 3, 1),
                Some(day(26, 3, 1)),
                Some(day(26, 3, 1)),
            ),
            task(
                "alpha",
                "scope",
                TaskKind::Finished,
                PLAUSIBLE_FLOOR,
                Some(PLAUSIBLE_FLOOR),
                Some(PLAUSIBLE_FLOOR),
            ),
        ];
        let board = dashboard(&tasks, today, RadarMode::Scope, "alpha");
        let first = board.cumulative.first().expect("first point");
        assert_eq!(first.date, "2005-04-07");
        assert_eq!(first.shipped, 1);
        assert_eq!(first.cumulative, 2, "one fold, not a double count");
        let span = (today - PLAUSIBLE_FLOOR).num_days();
        assert!(board.cumulative.len() as i64 <= span + 1);
        assert_eq!(
            board.cumulative.last().expect("last point").cumulative,
            board.counts.shipped_events as i64
        );
    }

    /// Covers: a task created before the floor counts its real age on the
    /// first charted day and leaves the average on its ship day.
    #[test]
    fn series_mean_age_folds_a_pre_floor_created_day_into_the_floor() {
        let today = day(2026, 11, 6);
        let created = day(26, 3, 1);
        let shipped = day(2026, 6, 1);
        let tasks = vec![task(
            "alpha",
            "scope",
            TaskKind::Finished,
            created,
            Some(shipped),
            Some(shipped),
        )];
        let board = dashboard(&tasks, today, RadarMode::Scope, "alpha");
        let first = board.mean_age.first().expect("first point");
        assert_eq!(first.date, "2005-04-07");
        assert_eq!(first.n, 1);
        let expected = (PLAUSIBLE_FLOOR - created).num_days() as f64;
        assert_eq!(
            first.avg_age.to_bits(),
            expected.to_bits(),
            "the first row keeps the task's real age"
        );
        assert!(
            !board
                .mean_age
                .iter()
                .any(|point| point.date == "2026-06-01"),
            "the ship day leaves the backlog, so it carries no point"
        );
        assert_eq!(
            board.mean_age.last().expect("last point").date,
            "2026-05-31",
            "the series ends on the last day that holds the task"
        );
    }

    /// Covers: a day with an empty backlog emits no point, so the series runs
    /// from the created day of the only task through its leave day and stops
    /// there instead of reporting an average of 0 for the rest of the range.
    #[test]
    fn series_mean_age_omits_a_day_with_an_empty_backlog() {
        let today = day(2026, 6, 10);
        let tasks = vec![task(
            "alpha",
            "scope",
            TaskKind::Finished,
            day(2026, 6, 1),
            Some(day(2026, 6, 2)),
            Some(day(2026, 6, 2)),
        )];
        let board = dashboard(&tasks, today, RadarMode::Scope, "alpha");
        assert_eq!(
            board.mean_age.len(),
            1,
            "only the one unfinished day emits a point"
        );
        let only = board.mean_age.first().expect("the only point");
        assert_eq!(only.date, "2026-06-01");
        assert_eq!(only.n, 1);
    }

    /// Covers: the gap between one task's leave day and a task created two days
    /// later holds no mean-age point, while a point sits on each side of it.
    #[test]
    fn series_mean_age_leaves_the_gap_between_a_leave_day_and_a_later_created_day_empty() {
        let today = day(2026, 6, 8);
        let tasks = vec![
            task(
                "alpha",
                "scope",
                TaskKind::Finished,
                day(2026, 6, 1),
                Some(day(2026, 6, 3)),
                Some(day(2026, 6, 3)),
            ),
            task(
                "alpha",
                "scope",
                TaskKind::Live,
                day(2026, 6, 5),
                None,
                None,
            ),
        ];
        let board = dashboard(&tasks, today, RadarMode::Scope, "alpha");
        assert!(
            !board
                .mean_age
                .iter()
                .any(|point| point.date == "2026-06-03" || point.date == "2026-06-04"),
            "neither gap day carries a point"
        );
        assert!(
            board.mean_age.iter().all(|point| point.n >= 1),
            "every emitted point holds at least one unfinished task"
        );
        assert!(
            board
                .mean_age
                .iter()
                .any(|point| point.date == "2026-06-02"),
            "the day before the gap carries a point"
        );
        assert!(
            board
                .mean_age
                .iter()
                .any(|point| point.date == "2026-06-05"),
            "the day after the gap carries a point"
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

    /// Covers: a task created and shipped before the chart floor folds into
    /// the first charted week, so the weekly sums still reach `counts`.
    #[test]
    fn series_weekly_folds_a_pre_floor_week_into_the_floor() {
        let today = day(2026, 11, 6);
        let tasks = vec![
            task(
                "alpha",
                "ancient",
                TaskKind::Finished,
                day(26, 3, 1),
                Some(day(26, 3, 1)),
                Some(day(26, 3, 1)),
            ),
            task(
                "alpha",
                "modern",
                TaskKind::Live,
                day(2026, 6, 1),
                None,
                None,
            ),
        ];
        let board = dashboard(&tasks, today, RadarMode::Scope, "alpha");
        let first = board.weekly.first().expect("first week");
        assert_eq!(
            first.week,
            crate::dates::format_date(monday(PLAUSIBLE_FLOOR))
        );
        assert_eq!(
            first.created, 1,
            "the pre-floor created week folds into the first row"
        );
        assert_eq!(
            first.shipped, 1,
            "the pre-floor ship week folds into the first row"
        );
        assert_eq!(
            board.weekly.iter().map(|point| point.created).sum::<i64>(),
            board.counts.created_events as i64
        );
        assert_eq!(
            board.weekly.iter().map(|point| point.shipped).sum::<i64>(),
            board.counts.shipped_events as i64
        );
    }

    /// The fields of one captured warning, flattened so a test can hold the
    /// warning to the values it must name.
    #[derive(Default)]
    struct WarningFields {
        message: Option<String>,
        repository: Option<String>,
        path: Option<String>,
        date: Option<String>,
    }

    impl WarningFields {
        /// One line carrying every field the warning named.
        fn rendered(&self) -> String {
            let mut line = self.message.clone().unwrap_or_default();
            for (name, value) in [
                ("repository", &self.repository),
                ("path", &self.path),
                ("date", &self.date),
            ] {
                if let Some(value) = value {
                    line.push(' ');
                    line.push_str(name);
                    line.push('=');
                    line.push_str(value);
                }
            }
            line
        }

        fn set(&mut self, name: &str, value: &str) {
            let slot = match name {
                "message" => &mut self.message,
                "repository" => &mut self.repository,
                "path" => &mut self.path,
                "date" => &mut self.date,
                _ => return,
            };
            *slot = Some(value.to_string());
        }
    }

    impl tracing::field::Visit for WarningFields {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.set(field.name(), value);
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.set(field.name(), &format!("{value:?}"));
        }
    }

    /// A thread-scoped layer that captures the warnings `series` logs, so a
    /// test reads them without installing a process-wide subscriber. Each test
    /// runs on its own thread, so the capture stays parallel-safe.
    #[derive(Clone, Default)]
    struct CapturedWarnings(std::sync::Arc<std::sync::Mutex<Vec<String>>>);

    impl CapturedWarnings {
        fn take(&self) -> Vec<String> {
            self.0
                .lock()
                .map(|mut lines| std::mem::take(&mut *lines))
                .unwrap_or_default()
        }
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CapturedWarnings {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _context: tracing_subscriber::layer::Context<'_, S>,
        ) {
            if *event.metadata().level() != tracing::Level::WARN
                || event.metadata().target() != "task_viewer::series"
            {
                return;
            }
            let mut fields = WarningFields::default();
            event.record(&mut fields);
            if let Ok(mut lines) = self.0.lock() {
                lines.push(fields.rendered());
            }
        }
    }

    /// Covers: one created date and one ship date after today still reach the
    /// last charted row, so the cumulative line ends at `counts.shipped_events`
    /// and the weekly bars sum to both event counts.
    #[test]
    fn series_folds_after_today_events_into_the_last_charted_point() {
        let today = day(2026, 6, 10);
        let tasks = vec![
            task("alpha", "old", TaskKind::Live, day(2026, 6, 1), None, None),
            task(
                "alpha",
                "shipped",
                TaskKind::Finished,
                day(2026, 6, 2),
                Some(day(2026, 6, 20)),
                Some(day(2026, 6, 20)),
            ),
            task(
                "alpha",
                "future",
                TaskKind::Live,
                day(2026, 6, 15),
                None,
                None,
            ),
        ];
        let board = dashboard(&tasks, today, RadarMode::Scope, "alpha");
        let last = board.cumulative.last().expect("the last cumulative point");
        assert_eq!(last.date, "2026-06-10");
        assert_eq!(
            last.cumulative, board.counts.shipped_events as i64,
            "the folded ship still reaches the line's end"
        );
        assert_eq!(
            board.weekly.iter().map(|point| point.created).sum::<i64>(),
            board.counts.created_events as i64,
            "the weekly created bars reach the created count"
        );
        assert_eq!(
            board.weekly.iter().map(|point| point.shipped).sum::<i64>(),
            board.counts.shipped_events as i64,
            "the weekly shipped bars reach the shipped count"
        );
        let last_week = board.weekly.last().expect("the last charted week");
        assert_eq!(last_week.week, crate::dates::format_date(monday(today)));
        assert_eq!(
            last_week.created, 1,
            "the after-today created event lands in today's week"
        );
        assert_eq!(
            last_week.shipped, 1,
            "the after-today ship lands in today's week"
        );
    }

    /// Covers: each after-today event is warned about once, naming the
    /// repository, path, and date, and a ship folded into both the weekly and
    /// cumulative series is still warned about once.
    #[test]
    fn series_warns_once_per_after_today_event() {
        let today = day(2026, 6, 10);
        let tasks = vec![
            task(
                "alpha",
                "future",
                TaskKind::Live,
                day(2027, 1, 5),
                None,
                None,
            ),
            task(
                "alpha",
                "late",
                TaskKind::Finished,
                day(2026, 6, 1),
                Some(day(2027, 1, 6)),
                Some(day(2027, 1, 6)),
            ),
        ];
        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        let _board = dashboard(&tasks, today, RadarMode::Scope, "alpha");
        drop(guard);
        let warnings = captured.take();
        assert_eq!(
            warnings.len(),
            2,
            "one warning per after-today event, not one per series: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("repository=alpha")
                    && warning.contains("path=tasks/alpha_future.md")
                    && warning.contains("date=2027-01-05")),
            "the created event names its repository, path, and date: {warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|warning| warning.contains("repository=alpha")
                    && warning.contains("path=tasks/alpha_late.md")
                    && warning.contains("date=2027-01-06")),
            "the ship event names its repository, path, and date: {warnings:?}"
        );
    }

    /// Covers: a row dated before `PLAUSIBLE_FLOOR` folds into the first
    /// charted point, keeps every series inside the plausible span, still
    /// reaches the summary counts, and logs one warning naming its path and
    /// date. `SERIES_FLOOR` stays the absolute bound at or below the plausible
    /// one.
    #[test]
    fn series_warns_and_bounds_a_row_before_the_plausible_floor() {
        assert!(
            SERIES_FLOOR <= PLAUSIBLE_FLOOR,
            "the absolute floor stays at or below the plausible floor"
        );
        let today = day(2026, 11, 6);
        let tasks = vec![
            task(
                "alpha",
                "ancient",
                TaskKind::Finished,
                day(26, 3, 1),
                Some(day(26, 3, 2)),
                Some(day(26, 3, 2)),
            ),
            task(
                "alpha",
                "modern",
                TaskKind::Live,
                day(2026, 6, 1),
                None,
                None,
            ),
        ];
        let captured = CapturedWarnings::default();
        let subscriber = tracing_subscriber::registry().with(captured.clone());
        let guard = tracing::subscriber::set_default(subscriber);
        let board = dashboard(&tasks, today, RadarMode::Scope, "alpha");
        drop(guard);
        let warnings = captured.take();
        assert_eq!(
            warnings.len(),
            1,
            "one warning for the one pre-floor row: {warnings:?}"
        );
        assert!(
            warnings[0].contains("repository=alpha")
                && warnings[0].contains("path=tasks/alpha_ancient.md")
                && warnings[0].contains("date=0026-03-01"),
            "the warning names the row's repository, path, and date: {warnings:?}"
        );

        let span = (today - PLAUSIBLE_FLOOR).num_days();
        for (name, len) in [
            ("cumulative", board.cumulative.len()),
            ("live_backlog", board.live_backlog.len()),
            ("mean_age", board.mean_age.len()),
        ] {
            assert!(
                len as i64 <= span + 1,
                "{name} has {len} points, above the bounded {span} days plus one"
            );
        }
        assert!(
            board.weekly.len() as i64 <= span / 7 + 2,
            "the weekly series has {} points, above the bounded {} weeks plus two",
            board.weekly.len(),
            span / 7
        );
        assert_eq!(
            board.cumulative.first().expect("first point").date,
            "2005-04-07",
            "the pre-floor ship folds into the plausible floor"
        );
        assert_eq!(
            board.cumulative.last().expect("last point").cumulative,
            board.counts.shipped_events as i64,
            "the folded ship still reaches the line's end"
        );
        assert_eq!(
            board.weekly.iter().map(|point| point.created).sum::<i64>(),
            board.counts.created_events as i64,
            "the weekly created bars reach the created count"
        );
        assert_eq!(
            board.weekly.iter().map(|point| point.shipped).sum::<i64>(),
            board.counts.shipped_events as i64,
            "the weekly shipped bars reach the shipped count"
        );
    }

    /// Covers: a series whose only event is a ship date after today still
    /// yields one row, dated today for the cumulative walk and today's Monday
    /// for the weekly walk.
    #[test]
    fn series_after_today_only_ship_yields_one_row() {
        let today = day(2026, 6, 10);
        let mut shipped: BTreeMap<NaiveDate, i64> = BTreeMap::new();
        shipped.insert(day(2026, 7, 1), 1);
        let cumulative = cumulative_points(&shipped, today);
        assert_eq!(cumulative.len(), 1, "the cumulative walk yields one row");
        assert_eq!(cumulative[0].date, "2026-06-10");
        assert_eq!(cumulative[0].cumulative, 1);
        let created: BTreeMap<NaiveDate, i64> = BTreeMap::new();
        let weekly = weekly_points(&created, &shipped, today);
        assert_eq!(weekly.len(), 1, "the weekly walk yields one row");
        assert_eq!(weekly[0].week, "2026-06-08", "today's Monday");
        assert_eq!(weekly[0].shipped, 1);
        assert_eq!(weekly[0].created, 0);
    }

    /// Covers: a task created after today stays out of today's backlog row and
    /// today's mean age, the backlog series ends at today, and the mean-age
    /// series carries no point after today and no negative average.
    #[test]
    fn series_keeps_after_today_created_out_of_the_backlog_and_mean_age() {
        let today = day(2026, 6, 10);
        let tasks = vec![
            task("alpha", "old", TaskKind::Live, day(2026, 6, 1), None, None),
            task(
                "alpha",
                "future",
                TaskKind::Live,
                day(2026, 6, 20),
                None,
                None,
            ),
        ];
        let board = dashboard(&tasks, today, RadarMode::Scope, "alpha");
        let last = board.live_backlog.last().expect("the last backlog row");
        assert_eq!(last.date, "2026-06-10", "the backlog ends at today");
        assert_eq!(
            last.live, 1,
            "the after-today task stays out of today's row"
        );
        assert!(
            board
                .mean_age
                .iter()
                .all(|point| point.date.as_str() <= "2026-06-10"),
            "no mean-age point is dated after today"
        );
        assert!(
            board.mean_age.iter().all(|point| point.avg_age >= 0.0),
            "no mean-age average is negative"
        );
        let today_age = board
            .mean_age
            .iter()
            .find(|point| point.date == "2026-06-10")
            .expect("today's mean-age point");
        assert_eq!(
            today_age.n, 1,
            "the after-today task stays out of today's average"
        );

        let only_future = dashboard(&tasks[1..], today, RadarMode::Scope, "alpha");
        assert_eq!(
            only_future.live_backlog.len(),
            1,
            "a series whose every task lies after today still yields today's row"
        );
        assert_eq!(only_future.live_backlog[0].date, "2026-06-10");
        assert_eq!(only_future.live_backlog[0].live, 0);
        assert!(
            only_future
                .mean_age
                .iter()
                .all(|point| point.date.as_str() <= "2026-06-10" && point.n > 0),
            "the future-only fixture carries no point after today"
        );
    }

    /// Covers: a leave day after today keeps its task in today's backlog row
    /// and in today's mean age, so the task drops out on its own leave day.
    #[test]
    fn series_keeps_a_task_leaving_after_today_in_todays_backlog_and_mean_age() {
        let today = day(2026, 6, 10);
        let tasks = vec![task(
            "alpha",
            "scope",
            TaskKind::Finished,
            day(2026, 6, 1),
            Some(day(2026, 6, 20)),
            Some(day(2026, 6, 20)),
        )];
        let board = dashboard(&tasks, today, RadarMode::Scope, "alpha");
        let last = board.live_backlog.last().expect("the last backlog row");
        assert_eq!(last.date, "2026-06-10");
        assert_eq!(last.live, 1, "the task stays in the backlog through today");
        let today_age = board
            .mean_age
            .iter()
            .find(|point| point.date == "2026-06-10")
            .expect("today's mean-age point");
        assert_eq!(today_age.n, 1);
        assert_eq!(
            today_age.avg_age.to_bits(),
            9.0_f64.to_bits(),
            "the task is nine days old on today"
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

    #[test]
    fn series_project_radar_spokes_name_their_repository() {
        let today = day(2026, 6, 2);
        let mut alpha = task("alpha", "one", TaskKind::Live, day(2026, 6, 1), None, None);
        alpha.repository_id = 7;
        let mut beta = task("beta", "one", TaskKind::Live, day(2026, 6, 1), None, None);
        beta.repository_id = 9;
        let board = dashboard(&[alpha, beta], today, RadarMode::Project, "All projects");
        let ids: BTreeMap<&str, Option<i64>> = board
            .radar_live
            .iter()
            .map(|row| (row.label.as_str(), row.repository_id))
            .collect();
        assert_eq!(ids.get("alpha").copied().flatten(), Some(7));
        assert_eq!(ids.get("beta").copied().flatten(), Some(9));

        let one = dashboard(
            &[task(
                "alpha",
                "one",
                TaskKind::Live,
                day(2026, 6, 1),
                None,
                None,
            )],
            today,
            RadarMode::Scope,
            "alpha",
        );
        assert!(
            one.radar_live.iter().all(|row| row.repository_id.is_none()),
            "a scope-mode spoke names no repository"
        );
    }
}
