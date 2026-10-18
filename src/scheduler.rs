use crate::model::*;

/// Days since epoch for a civil date (Howard Hinnant algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((m + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}
/// weekday Mon=0..Sun=6 for days-since-epoch (1970-01-01 = Thursday)
pub fn weekday(days: i64) -> u32 {
    (((days + 3) % 7 + 7) % 7) as u32
}

pub fn day_of_yyyymmdd(v: u32) -> i64 {
    days_from_civil((v / 10000) as i64, (v / 100) % 100, v % 100)
}

/// Window for treating a late start as on-time rather than missed: mirrors
/// latency of two app polls (poll cadence is 500ms busy / 5s idle, see
/// app.rs poll tail). App launches later than `start_secs+GRACE_SECS` only
/// catch up when `run_missed` is set.
pub const GRACE_SECS: u32 = 60;

fn recurrence_matches(s: &Schedule, day: i64) -> bool {
    match s.recurrence {
        Recurrence::Daily => {
            if s.repeat_days > 1 && s.once_date != 0 {
                let anchor = day_of_yyyymmdd(s.once_date);
                (day - anchor).rem_euclid(s.repeat_days as i64) == 0
            } else {
                true
            }
        }
        Recurrence::Weekdays => weekday(day) <= 4,
        Recurrence::Weekly => {
            let bit = 1u8 << weekday(day);
            s.weekdays_mask & bit != 0
        }
        Recurrence::Once => s.once_date != 0 && day_of_yyyymmdd(s.once_date) == day,
    }
}

/// Scheduled time has passed today; the queue has not started today.
/// Caller restarts the queue (caller owns setting `last_started_day`).
pub fn missed_run(now: (i64, u32), s: &Schedule, last_triggered_date: i64) -> bool {
    let (day, secs_of_day) = now;
    if !s.enabled || !s.run_missed {
        return false;
    }
    if last_triggered_date == day {
        return false;
    }
    // Include the exact start second: late-but-within-grace always fires.
    if secs_of_day <= s.start_secs.saturating_add(GRACE_SECS) {
        return false;
    }
    recurrence_matches(s, day)
}

pub fn should_start(s: &Schedule, day: i64, secs_of_day: u32) -> bool {
    if !s.enabled {
        return false;
    }
    if secs_of_day < s.start_secs {
        return false;
    }
    if s.last_started_day == day {
        return false;
    }
    // On-time starts (including grace window) fire regardless of run_missed;
    // starts later than that are missed runs, gated on run_missed.
    if secs_of_day > s.start_secs.saturating_add(GRACE_SECS) && !s.run_missed {
        return false;
    }
    recurrence_matches(s, day)
}
pub fn should_stop(s: &Schedule, day: i64, secs_of_day: u32) -> bool {
    if !s.enabled {
        return false;
    }
    if s.last_stopped_day == day {
        return false;
    } // latched: manual Resume after stop wins until tomorrow
    match s.stop_secs {
        Some(stop) => secs_of_day >= stop && secs_of_day >= s.start_secs,
        None => false,
    }
}
/// Seconds until next start (approx, day granularity). None if Once in past.
pub fn next_run_in(s: &Schedule, day: i64, secs_of_day: u32) -> Option<i64> {
    if !s.enabled {
        return None;
    }
    for offset in 0..370i64 {
        let d = day + offset;
        let fake = Schedule {
            last_started_day: 0,
            ..s.clone()
        };
        // check date matches recurrence ignoring last_started
        let date_ok = match s.recurrence {
            Recurrence::Daily => {
                if s.repeat_days > 1 && s.once_date != 0 {
                    (d - day_of_yyyymmdd(s.once_date)).rem_euclid(s.repeat_days as i64) == 0
                } else {
                    true
                }
            }
            Recurrence::Weekdays => weekday(d) <= 4,
            Recurrence::Weekly => s.weekdays_mask & (1u8 << weekday(d)) != 0,
            Recurrence::Once => s.once_date != 0 && day_of_yyyymmdd(s.once_date) == d,
        };
        let _ = fake;
        if !date_ok {
            continue;
        }
        let delta = offset * 86400 + s.start_secs as i64 - secs_of_day as i64;
        if delta > 0 {
            return Some(delta);
        }
        if offset == 0 && should_start(s, day, secs_of_day) {
            return Some(0);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn stop_latches_after_apply() {
        let base = Schedule {
            id: "1".into(),
            queue_id: "main".into(),
            enabled: true,
            recurrence: Recurrence::Daily,
            start_secs: 3600,
            stop_secs: Some(7200),
            ..Default::default()
        };
        assert!(should_stop(&base, 10, 7200));
        let s2 = Schedule {
            last_stopped_day: 10,
            ..base.clone()
        };
        assert!(!should_stop(&s2, 10, 8000));
        assert!(should_stop(&s2, 11, 7200));
    }
    #[test]
    fn stop_latch_unset_fires_all_day() {
        let s = Schedule {
            id: "1".into(),
            queue_id: "main".into(),
            enabled: true,
            recurrence: Recurrence::Daily,
            start_secs: 3600,
            stop_secs: Some(7200),
            ..Default::default()
        };
        assert!(should_stop(&s, 10, 86399));
        assert!(!should_stop(&s, 10, 7199));
    }
    #[test]
    fn stop_latch_suppresses_rest_of_day() {
        let s = Schedule {
            last_stopped_day: 10,
            id: "1".into(),
            queue_id: "main".into(),
            enabled: true,
            recurrence: Recurrence::Daily,
            start_secs: 3600,
            stop_secs: Some(7200),
            ..Default::default()
        };
        assert!(!should_stop(&s, 10, 43200));
        assert!(!should_stop(&s, 10, 86399));
    }
    #[test]
    fn weekdays_logic() {
        let mut s = Schedule {
            id: "1".into(),
            queue_id: "main".into(),
            enabled: true,
            recurrence: Recurrence::Weekdays,
            start_secs: 3600,
            ..Default::default()
        };
        let mon = days_from_civil(2026, 10, 19); // Monday
        assert_eq!(weekday(mon), 0);
        assert!(should_start(&s, mon, 3600));
        assert!(!should_start(&s, mon, 3599));
        let sat = days_from_civil(2026, 10, 17);
        assert_eq!(weekday(sat), 5);
        assert!(!should_start(&s, sat, 7200));
        s.last_started_day = mon;
        assert!(!should_start(&s, mon, 7200));
    }

    #[test]
    fn missed_run_cases() {
        // Start 01:00, already 02:00 Monday, enabled, queued, run_missed on.
        let mut s = Schedule {
            id: "1".into(),
            queue_id: "main".into(),
            enabled: true,
            recurrence: Recurrence::Daily,
            start_secs: 3600,
            run_missed: true,
            ..Default::default()
        };
        let mon = days_from_civil(2026, 10, 19);
        // missed today -> true (once; start time already passed)
        assert!(missed_run((mon, 7200), &s, 0));
        // already triggered today -> false (once per day)
        assert!(!missed_run((mon, 7200), &s, mon));
        // `Schedule.last_started_day` mirrors the same once-per-day guard.
        s.last_started_day = mon;
        assert!(!should_start(&s, mon, 7200));
        s.last_started_day = 0;
        assert!(should_start(&s, mon, 7200));
        // future time -> false (not yet due)
        assert!(!missed_run((mon, 3599), &s, 0));
        // run_missed off -> false
        s.run_missed = false;
        assert!(!missed_run((mon, 7200), &s, 0));
        assert!(!should_start(&s, mon, 7200));
        // disabled -> false
        s.run_missed = true;
        s.enabled = false;
        assert!(!missed_run((mon, 7200), &s, 0));
        s.enabled = true;
        // grace window: on-time start fires even with run_missed off
        let s2 = Schedule {
            id: "2".into(),
            queue_id: "main".into(),
            enabled: true,
            recurrence: Recurrence::Daily,
            start_secs: 3600,
            ..Default::default()
        };
        assert!(should_start(&s2, mon, 3600));
        assert!(should_start(&s2, mon, 3600 + GRACE_SECS));
        assert!(!missed_run((mon, 3600 + GRACE_SECS), &s2, 0));
    }
    #[test]
    fn once_and_next() {
        let s = Schedule {
            id: "1".into(),
            queue_id: "main".into(),
            enabled: true,
            recurrence: Recurrence::Once,
            once_date: 20261020,
            start_secs: 0,
            ..Default::default()
        };
        let d = days_from_civil(2026, 10, 19);
        assert_eq!(next_run_in(&s, d, 0), Some(86400));
    }
}
