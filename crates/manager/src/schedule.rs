//! The scheduler, as in IDM: start the main download queue at a set time,
//! once or on chosen days of the week, stop it at another time, and do
//! something when it is done.

use chrono::{Datelike, NaiveDate, NaiveDateTime, NaiveTime};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Schedule {
    /// Start the queue at `start_at`.
    pub start: bool,
    /// "HH:MM", local time.
    pub start_at: String,
    /// Days of the week to start on, Monday first. If none is ticked the
    /// queue starts once, on `date`.
    pub days: [bool; 7],
    /// "YYYY-MM-DD", for a start that happens once.
    pub date: String,
    /// Stop the queue at `stop_at`.
    pub stop: bool,
    pub stop_at: String,
    pub when_done: WhenDone,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            start: false,
            start_at: "02:00".into(),
            days: [false; 7],
            date: String::new(),
            stop: false,
            stop_at: "07:00".into(),
            when_done: WhenDone::Nothing,
        }
    }
}

impl Schedule {
    /// Starts on chosen days rather than once.
    pub fn daily(&self) -> bool {
        self.days.iter().any(|d| *d)
    }
}

/// What to do once the queue has finished.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WhenDone {
    #[default]
    Nothing,
    /// Quit Slipstream.
    Exit,
    /// Put the computer to sleep.
    Sleep,
    /// Turn the computer off.
    Shutdown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Due {
    Start,
    Stop,
}

/// What the schedule asks for between two checks: after `from`, up to and
/// including `to`. Times missed while the app was not running are not
/// made up for.
pub(crate) fn due(schedule: &Schedule, from: NaiveDateTime, to: NaiveDateTime) -> Vec<Due> {
    let mut out = Vec::new();
    if schedule.stop
        && let Some(at) = parse_time(&schedule.stop_at)
        && passed(at, from, to, |_| true)
    {
        out.push(Due::Stop);
    }
    if schedule.start
        && let Some(at) = parse_time(&schedule.start_at)
    {
        let once = NaiveDate::parse_from_str(schedule.date.trim(), "%Y-%m-%d").ok();
        let on = |day: NaiveDate| {
            if schedule.daily() {
                schedule.days[day.weekday().num_days_from_monday() as usize]
            } else {
                Some(day) == once
            }
        };
        if passed(at, from, to, on) {
            out.push(Due::Start);
        }
    }
    out
}

/// Whether `at` on some day allowed by `on` lies after `from`, up to `to`.
fn passed(
    at: NaiveTime,
    from: NaiveDateTime,
    to: NaiveDateTime,
    on: impl Fn(NaiveDate) -> bool,
) -> bool {
    let mut day = from.date();
    while day <= to.date() {
        let moment = day.and_time(at);
        if moment > from && moment <= to && on(day) {
            return true;
        }
        let Some(next) = day.succ_opt() else { break };
        day = next;
    }
    false
}

fn parse_time(text: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(text.trim(), "%H:%M").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn schedule() -> Schedule {
        Schedule {
            start: true,
            start_at: "02:30".into(),
            date: "2026-10-24".into(),
            ..Schedule::default()
        }
    }

    #[test]
    fn starts_once_on_the_chosen_day() {
        let s = schedule();
        assert_eq!(
            due(&s, at("2026-10-24 02:29:59"), at("2026-10-24 02:30:00")),
            [Due::Start]
        );
        assert!(due(&s, at("2026-10-24 02:30:00"), at("2026-10-24 02:30:01")).is_empty());
        assert!(due(&s, at("2026-10-25 02:29:59"), at("2026-10-25 02:30:00")).is_empty());
    }

    #[test]
    fn starts_on_ticked_weekdays() {
        let mut s = schedule();
        // 2026-10-24 is a Saturday, 2026-10-26 a Monday.
        s.days = [true, false, false, false, false, false, false];
        assert!(due(&s, at("2026-10-24 02:29:00"), at("2026-10-24 02:31:00")).is_empty());
        assert_eq!(
            due(&s, at("2026-10-26 02:29:00"), at("2026-10-26 02:31:00")),
            [Due::Start]
        );
    }

    #[test]
    fn notices_a_time_passed_across_midnight() {
        let mut s = schedule();
        s.start_at = "00:00".into();
        s.date = "2026-10-25".into();
        assert_eq!(
            due(&s, at("2026-10-24 23:59:59"), at("2026-10-25 00:00:01")),
            [Due::Start]
        );
    }

    #[test]
    fn stops_at_the_stop_time() {
        let mut s = schedule();
        s.start = false;
        s.stop = true;
        s.stop_at = "07:15".into();
        assert_eq!(
            due(&s, at("2026-10-24 07:14:59"), at("2026-10-24 07:15:00")),
            [Due::Stop]
        );
        assert!(due(&s, at("2026-10-24 07:15:00"), at("2026-10-24 07:16:00")).is_empty());
    }

    #[test]
    fn ignores_bad_times() {
        let mut s = schedule();
        s.start_at = "25:61".into();
        assert!(due(&s, at("2026-10-24 00:00:00"), at("2026-10-24 23:59:59")).is_empty());
    }
}
