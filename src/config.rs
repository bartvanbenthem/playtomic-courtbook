use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use chrono::{Datelike, NaiveDateTime, NaiveTime, Weekday};
use chrono_tz::Tz;
use serde::{Deserialize, Deserializer};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Playtomic club id (`padel-autobook search <name>` finds it).
    pub tenant_id: String,
    #[serde(default = "default_timezone")]
    pub timezone: Tz,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_minutes: u64,
    /// How many days ahead (including today) to look for slots.
    #[serde(default = "default_days_ahead")]
    pub days_ahead: u32,
    /// Quote free slots but never confirm a booking.
    #[serde(default = "default_true")]
    pub dry_run: bool,
    /// Highest price (your member price, as Playtomic quotes it) that may be booked.
    #[serde(default)]
    pub max_price: f64,
    /// Cap on payment-intent quotes per poll, to stay polite.
    #[serde(default = "default_max_quotes")]
    pub max_quotes_per_run: usize,
    /// Stop booking while this many booked slots are still ahead.
    #[serde(default = "default_max_open_bookings")]
    pub max_open_bookings: usize,
    /// Allowed courts by resource id or name, in order of preference. Empty = all.
    #[serde(default)]
    pub courts: Vec<String>,
    #[serde(default = "default_state_file")]
    pub state_file: PathBuf,
    /// Whether availability slot times are UTC (true) or already club-local time.
    #[serde(default = "default_true")]
    pub slot_times_utc: bool,
    #[serde(default = "default_payment_methods")]
    pub payment_method_preference: Vec<String>,
    /// Optional URL that gets a POST with a plain-text message per booking (e.g. ntfy.sh).
    #[serde(default)]
    pub notify_url: Option<String>,
    pub targets: Vec<Target>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub weekdays: Vec<Weekday>,
    #[serde(deserialize_with = "hh_mm")]
    pub earliest_start: NaiveTime,
    #[serde(deserialize_with = "hh_mm")]
    pub latest_start: NaiveTime,
    /// Accepted slot lengths in minutes.
    pub durations: Vec<u32>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: Self = toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&self) -> Result<()> {
        ensure!(!self.tenant_id.trim().is_empty(), "tenant_id is empty");
        ensure!(self.poll_interval_minutes >= 5, "poll_interval_minutes must be at least 5");
        ensure!(self.max_price >= 0.0, "max_price must not be negative");
        ensure!(self.max_quotes_per_run > 0, "max_quotes_per_run must be at least 1");
        ensure!(self.max_open_bookings > 0, "max_open_bookings must be at least 1");
        ensure!(!self.targets.is_empty(), "at least one [[targets]] entry is required");
        for (i, t) in self.targets.iter().enumerate() {
            ensure!(!t.weekdays.is_empty(), "targets[{i}]: weekdays is empty");
            ensure!(!t.durations.is_empty(), "targets[{i}]: durations is empty");
            ensure!(
                t.earliest_start <= t.latest_start,
                "targets[{i}]: earliest_start is after latest_start"
            );
        }
        Ok(())
    }

    pub fn court_allowed(&self, resource_id: &str, name: &str) -> bool {
        self.courts.is_empty() || self.court_rank(resource_id, name).is_some()
    }

    /// Position of the court in the preference list, if listed.
    pub fn court_rank(&self, resource_id: &str, name: &str) -> Option<usize> {
        self.courts
            .iter()
            .position(|c| c == resource_id || c.eq_ignore_ascii_case(name))
    }
}

impl Target {
    pub fn matches(&self, start_local: NaiveDateTime, duration: u32) -> bool {
        let time = start_local.time();
        self.weekdays.contains(&start_local.weekday())
            && self.durations.contains(&duration)
            && time >= self.earliest_start
            && time <= self.latest_start
    }
}

fn hh_mm<'de, D: Deserializer<'de>>(d: D) -> Result<NaiveTime, D::Error> {
    let s = String::deserialize(d)?;
    NaiveTime::parse_from_str(&s, "%H:%M").map_err(serde::de::Error::custom)
}

fn default_timezone() -> Tz {
    chrono_tz::Europe::Amsterdam
}
fn default_poll_interval() -> u64 {
    15
}
fn default_days_ahead() -> u32 {
    14
}
fn default_true() -> bool {
    true
}
fn default_max_quotes() -> usize {
    6
}
fn default_max_open_bookings() -> usize {
    3
}
fn default_state_file() -> PathBuf {
    PathBuf::from("state.json")
}
fn default_payment_methods() -> Vec<String> {
    ["OFFER", "MERCHANT_WALLET", "CREDIT_CARD"].map(String::from).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_config_parses() {
        let cfg: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
        cfg.validate().unwrap();
        assert!(cfg.dry_run);
        assert_eq!(cfg.max_price, 0.0);
        assert_eq!(cfg.targets[0].earliest_start, NaiveTime::from_hms_opt(19, 0, 0).unwrap());
    }

    #[test]
    fn target_matches_window_inclusive() {
        let t = Target {
            weekdays: vec![Weekday::Tue],
            earliest_start: NaiveTime::from_hms_opt(19, 0, 0).unwrap(),
            latest_start: NaiveTime::from_hms_opt(20, 30, 0).unwrap(),
            durations: vec![90],
        };
        let at = |s: &str| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap();
        // 2026-10-13 is a Tuesday.
        assert!(t.matches(at("2026-10-13 19:00"), 90));
        assert!(t.matches(at("2026-10-13 20:30"), 90));
        assert!(!t.matches(at("2026-10-13 20:31"), 90));
        assert!(!t.matches(at("2026-10-13 19:00"), 60));
        assert!(!t.matches(at("2026-10-14 19:00"), 90));
    }
}
