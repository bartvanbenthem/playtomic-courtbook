use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};

/// Price rules rarely change, so a slot seen as paid is not re-quoted for a week.
const PAID_CACHE_TTL: Duration = Duration::days(7);

/// Persisted between polls so a day is never booked twice.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub bookings: Vec<Booking>,
    /// "resource|weekday|HH:MM|duration" -> when it was last quoted above max_price.
    #[serde(default)]
    pub paid_slots: HashMap<String, DateTime<Utc>>,
    /// Bookings that have not started yet, refreshed every poll. Informational only.
    #[serde(default)]
    pub open_bookings: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum BookingStatus {
    Confirmed,
    /// Confirmation errored or returned an unexpected status. Still blocks the
    /// day; check the app and delete the entry if nothing was booked.
    Uncertain,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Booking {
    pub date: NaiveDate,
    pub court: String,
    pub resource_id: String,
    pub start_local: NaiveDateTime,
    pub duration: u32,
    pub payment_intent_id: String,
    pub status: BookingStatus,
    pub created_at: DateTime<Utc>,
}

impl Booking {
    pub fn summary(&self) -> String {
        let what = format!(
            "{} {} ({} min)",
            self.court,
            self.start_local.format("%a %d %b %H:%M"),
            self.duration
        );
        match self.status {
            BookingStatus::Confirmed => format!("Booked {what}"),
            BookingStatus::Uncertain => format!(
                "Booking {what} may have failed. Check Playtomic; if nothing was booked, \
                 remove the entry from the state file to retry that day."
            ),
        }
    }
}

impl State {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    /// Writes via a temp file so a crash never leaves a truncated state file.
    pub fn save(&self, path: &Path) -> Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)?)
            .with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))
    }

    pub fn has_booking_on(&self, date: NaiveDate) -> bool {
        self.bookings.iter().any(|b| b.date == date)
    }

    /// Bookings (confirmed or uncertain) whose start time is still ahead.
    pub fn count_open(&self, now_local: NaiveDateTime) -> usize {
        self.bookings.iter().filter(|b| b.start_local > now_local).count()
    }

    pub fn is_known_paid(&self, key: &str, now: DateTime<Utc>) -> bool {
        self.paid_slots.get(key).is_some_and(|seen| now - *seen < PAID_CACHE_TTL)
    }

    pub fn mark_paid(&mut self, key: String, now: DateTime<Utc>) {
        self.paid_slots.insert(key, now);
    }

    pub fn prune(&mut self, now: DateTime<Utc>) {
        self.paid_slots.retain(|_, seen| now - *seen < PAID_CACHE_TTL);
        let today = now.date_naive();
        self.bookings.retain(|b| b.date + Duration::days(30) >= today);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paid_cache_expires() {
        let mut s = State::default();
        let t0 = Utc::now();
        s.mark_paid("k".into(), t0);
        assert!(s.is_known_paid("k", t0 + Duration::days(6)));
        assert!(!s.is_known_paid("k", t0 + Duration::days(8)));
        s.prune(t0 + Duration::days(8));
        assert!(s.paid_slots.is_empty());
    }

    #[test]
    fn counts_only_future_bookings_as_open() {
        let date = NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        let booking = |hour| Booking {
            date,
            court: "Court 1".into(),
            resource_id: "r1".into(),
            start_local: date.and_hms_opt(hour, 0, 0).unwrap(),
            duration: 90,
            payment_intent_id: "pi".into(),
            status: BookingStatus::Uncertain,
            created_at: Utc::now(),
        };
        let s = State { bookings: vec![booking(18), booking(20)], ..State::default() };
        assert_eq!(s.count_open(date.and_hms_opt(17, 0, 0).unwrap()), 2);
        assert_eq!(s.count_open(date.and_hms_opt(19, 0, 0).unwrap()), 1);
        assert_eq!(s.count_open(date.and_hms_opt(20, 0, 0).unwrap()), 0);
    }

    #[test]
    fn roundtrips_through_disk() {
        let dir = std::env::temp_dir().join(format!("padel-autobook-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("state.json");
        let date = NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        let mut s = State::default();
        s.bookings.push(Booking {
            date,
            court: "Court 1".into(),
            resource_id: "r1".into(),
            start_local: date.and_hms_opt(19, 0, 0).unwrap(),
            duration: 90,
            payment_intent_id: "pi".into(),
            status: BookingStatus::Confirmed,
            created_at: Utc::now(),
        });
        s.save(&path).unwrap();
        let loaded = State::load(&path).unwrap();
        assert!(loaded.has_booking_on(date));
        assert!(State::load(&dir.join("missing.json")).unwrap().bookings.is_empty());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
