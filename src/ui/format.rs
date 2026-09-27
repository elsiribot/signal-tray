//! Compact human-readable timestamps.

use gtk::glib;

use crate::model::Timestamp;

fn local(ts: Timestamp) -> Option<glib::DateTime> {
    glib::DateTime::from_unix_local(ts.0.div_euclid(1000)).ok()
}

fn fmt(dt: &glib::DateTime, pattern: &str) -> String {
    dt.format(pattern)
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn same_day(a: &glib::DateTime, b: &glib::DateTime) -> bool {
    a.year() == b.year() && a.day_of_year() == b.day_of_year()
}

/// For the chat list: time today, weekday within a week, date otherwise.
pub fn list_time(ts: Timestamp) -> String {
    let (Some(dt), Ok(now)) = (local(ts), glib::DateTime::now_local()) else {
        return String::new();
    };
    let age_days = now.difference(&dt).as_days();
    if same_day(&dt, &now) {
        fmt(&dt, "%H:%M")
    } else if age_days < 6 {
        fmt(&dt, "%a")
    } else if dt.year() == now.year() {
        fmt(&dt, "%d %b")
    } else {
        fmt(&dt, "%d.%m.%y")
    }
}

/// For message bubbles; the day is shown by separators.
pub fn message_time(ts: Timestamp) -> String {
    local(ts).map(|dt| fmt(&dt, "%H:%M")).unwrap_or_default()
}

/// Separator label for the day of a message.
pub fn day_label(ts: Timestamp) -> String {
    let (Some(dt), Ok(now)) = (local(ts), glib::DateTime::now_local()) else {
        return String::new();
    };
    if same_day(&dt, &now) {
        "Today".to_owned()
    } else if now
        .add_days(-1)
        .is_ok_and(|yesterday| same_day(&dt, &yesterday))
    {
        "Yesterday".to_owned()
    } else if dt.year() == now.year() {
        fmt(&dt, "%a, %d %b")
    } else {
        fmt(&dt, "%a, %d %b %Y")
    }
}

/// Key identifying the local calendar day, for inserting day separators.
pub fn day_key(ts: Timestamp) -> Option<(i32, i32)> {
    local(ts).map(|dt| (dt.year(), dt.day_of_year()))
}
