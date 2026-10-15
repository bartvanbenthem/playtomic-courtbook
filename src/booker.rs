use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use serde_json::Value;
use tracing::{debug, error, info, warn};

use crate::api::{self, Credentials, PaymentIntent, PaymentMethod, Playtomic, ResourceAvailability};
use crate::config::Config;
use crate::state::{Booking, BookingStatus, State};

#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub resource_id: String,
    pub court_name: String,
    /// Start time as the API reported it; sent back unchanged when booking.
    pub start_api: NaiveDateTime,
    /// Start time in the club's timezone, used for matching and logs.
    pub start_local: NaiveDateTime,
    pub duration: u32,
    pub listed_price: Option<f64>,
    target: usize,
}

impl Candidate {
    fn paid_key(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.resource_id,
            self.start_local.weekday(),
            self.start_local.format("%H:%M"),
            self.duration
        )
    }
}

/// Slots on `date` that match a target, best first: listed as free, then
/// earlier target, then earlier start, then preferred court.
pub fn candidates_for_date(
    cfg: &Config,
    date: NaiveDate,
    now_local: NaiveDateTime,
    availability: &[ResourceAvailability],
    court_names: &HashMap<String, String>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for res in availability {
        let court_name = court_names.get(&res.resource_id).unwrap_or(&res.resource_id);
        if !cfg.court_allowed(&res.resource_id, court_name) {
            continue;
        }
        for slot in &res.slots {
            let start_api = res.start_date.and_time(slot.start_time);
            let start_local = if cfg.slot_times_utc {
                Utc.from_utc_datetime(&start_api).with_timezone(&cfg.timezone).naive_local()
            } else {
                start_api
            };
            if start_local.date() != date || start_local <= now_local {
                continue;
            }
            let Some(target) = cfg.targets.iter().position(|t| t.matches(start_local, slot.duration)) else {
                continue;
            };
            out.push(Candidate {
                resource_id: res.resource_id.clone(),
                court_name: court_name.clone(),
                start_api,
                start_local,
                duration: slot.duration,
                listed_price: slot.price.as_deref().and_then(api::parse_price_str),
                target,
            });
        }
    }

    let listed_free = |c: &Candidate| c.listed_price.is_some_and(|p| p <= cfg.max_price);
    let rank = |c: &Candidate| cfg.court_rank(&c.resource_id, &c.court_name).unwrap_or(usize::MAX);
    out.sort_by(|a, b| {
        listed_free(b)
            .cmp(&listed_free(a))
            .then(a.target.cmp(&b.target))
            .then(a.start_local.cmp(&b.start_local))
            .then(rank(a).cmp(&rank(b)))
    });
    out
}

fn pick_method<'a>(methods: &'a [PaymentMethod], preference: &[String]) -> Option<&'a PaymentMethod> {
    preference
        .iter()
        .find_map(|p| methods.iter().find(|m| m.method_type.eq_ignore_ascii_case(p)))
        .or(methods.first())
}

/// What the caller should do after a poll.
#[derive(Debug, PartialEq)]
pub enum Next {
    Poll,
    /// The open booking limit is reached; polling again cannot book anything.
    Stop,
}

pub struct Booker {
    cfg: Config,
    creds: Credentials,
    api: Playtomic,
    notifier: reqwest::Client,
    court_names: HashMap<String, String>,
}

impl Booker {
    pub async fn new(cfg: Config, creds: Credentials) -> Result<Self> {
        let mut api = Playtomic::new()?;
        api.ensure_login(&creds).await?;
        let tenant = api.tenant(&cfg.tenant_id).await.context("loading club")?;
        info!(club = %tenant.tenant_name, courts = tenant.resources.len(), "club loaded");
        let court_names = tenant.resources.into_iter().map(|r| (r.resource_id, r.name)).collect();
        if cfg.dry_run {
            warn!("dry run: free slots are quoted but never booked");
        }
        Ok(Self { cfg, creds, api, notifier: reqwest::Client::new(), court_names })
    }

    /// One poll. Any API error (quote failed, conflict, rate limited, ...) is
    /// returned so the caller stops instead of retrying it every poll.
    pub async fn run_once(&mut self) -> Result<Next> {
        let mut state = State::load(&self.cfg.state_file)?;
        let now = Utc::now();
        state.prune(now);
        let now_local = now.with_timezone(&self.cfg.timezone).naive_local();
        state.open_bookings = state.count_open(now_local);
        // Checked before logging in, so a full quota costs no requests at all.
        if state.open_bookings >= self.cfg.max_open_bookings {
            info!(open = state.open_bookings, max = self.cfg.max_open_bookings, "open booking limit reached, stopping");
            state.save(&self.cfg.state_file)?;
            return Ok(Next::Stop);
        }
        let result = self.poll(&mut state, now, now_local).await;
        state.save(&self.cfg.state_file)?;
        result
    }

    async fn poll(&mut self, state: &mut State, now: DateTime<Utc>, now_local: NaiveDateTime) -> Result<Next> {
        self.api.ensure_login(&self.creds).await?;
        let mut quotes_left = self.cfg.max_quotes_per_run;

        'days: for offset in 0..=i64::from(self.cfg.days_ahead) {
            let date = now_local.date() + Duration::days(offset);
            if state.has_booking_on(date) {
                debug!(%date, "already booked");
                continue;
            }
            if !self.cfg.targets.iter().any(|t| t.weekdays.contains(&date.weekday())) {
                continue;
            }
            let availability = self
                .api
                .availability(&self.cfg.tenant_id, date)
                .await
                .with_context(|| format!("availability for {date} failed"))?;
            let candidates = candidates_for_date(&self.cfg, date, now_local, &availability, &self.court_names);
            debug!(%date, matching = candidates.len(), "checked availability");

            for c in candidates {
                let key = c.paid_key();
                if state.is_known_paid(&key, now) {
                    continue;
                }
                if quotes_left == 0 {
                    info!("quote budget for this poll used up");
                    break 'days;
                }
                quotes_left -= 1;

                // A refusal here (409 conflict, club booking limit, 429, ...) would
                // repeat on every poll, so it ends the run.
                let intent = self
                    .api
                    .create_payment_intent(&self.cfg.tenant_id, &c.resource_id, c.start_api, c.duration)
                    .await
                    .with_context(|| format!("quote failed for {} at {}", c.court_name, c.start_local))?;
                match intent.price {
                    None => bail!(
                        "could not read price from payment intent for {} at {}: {}",
                        c.court_name, c.start_local, intent.raw
                    ),
                    Some(price) if price > self.cfg.max_price => {
                        info!(court = %c.court_name, start = %c.start_local, price, "not free, skipping");
                        state.mark_paid(key, now);
                    }
                    Some(price) if self.cfg.dry_run => {
                        info!(
                            court = %c.court_name, start = %c.start_local, duration = c.duration, price,
                            "DRY RUN: would book"
                        );
                        continue 'days;
                    }
                    Some(price) => {
                        info!(court = %c.court_name, start = %c.start_local, price, "booking");
                        let booking = self.book(&c, &intent).await;
                        state.bookings.push(booking.clone());
                        state.open_bookings += 1;
                        state.save(&self.cfg.state_file)?;
                        info!("{}", booking.summary());
                        self.notify(&booking.summary()).await;
                        if booking.status != BookingStatus::Confirmed {
                            bail!("booking not confirmed, stopping");
                        }
                        if state.open_bookings >= self.cfg.max_open_bookings {
                            info!(open = state.open_bookings, "open booking limit reached, stopping");
                            return Ok(Next::Stop);
                        }
                        continue 'days;
                    }
                }
            }
        }
        Ok(Next::Poll)
    }

    /// Never fails: anything short of a clean success is recorded as
    /// `Uncertain`, which blocks the day rather than risk a double booking.
    async fn book(&self, c: &Candidate, intent: &PaymentIntent) -> Booking {
        let status = match self.confirm(intent).await {
            Ok(s) if s == "SUCCEEDED" => BookingStatus::Confirmed,
            Ok(s) => {
                warn!(status = %s, "unexpected confirmation status");
                BookingStatus::Uncertain
            }
            Err(e) => {
                error!("booking failed: {e:#}");
                BookingStatus::Uncertain
            }
        };
        Booking {
            date: c.start_local.date(),
            court: c.court_name.clone(),
            resource_id: c.resource_id.clone(),
            start_local: c.start_local,
            duration: c.duration,
            payment_intent_id: intent.id.clone(),
            status,
            created_at: Utc::now(),
        }
    }

    async fn confirm(&self, intent: &PaymentIntent) -> Result<String> {
        if let Some(m) = pick_method(&intent.methods, &self.cfg.payment_method_preference) {
            debug!(method = %m.method_type, "selecting payment method");
            self.api.select_payment_method(&intent.id, &m.payment_method_id).await?;
        }
        let resp = self.api.confirm_payment_intent(&intent.id).await?;
        Ok(resp.get("status").and_then(Value::as_str).unwrap_or("UNKNOWN").to_owned())
    }

    async fn notify(&self, msg: &str) {
        let Some(url) = &self.cfg.notify_url else { return };
        let res = self.notifier.post(url).body(msg.to_owned()).send().await;
        if let Err(e) = res.and_then(|r| r.error_for_status()) {
            warn!("notification failed: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Slot;
    use chrono::NaiveTime;

    fn config(extra: &str) -> Config {
        toml::from_str(&format!(
            r#"
            tenant_id = "t"
            timezone = "Europe/Amsterdam"
            {extra}
            [[targets]]
            weekdays = ["Thu"]
            earliest_start = "19:00"
            latest_start = "20:30"
            durations = [90]
            "#
        ))
        .unwrap()
    }

    fn slot(time: &str, duration: u32, price: &str) -> Slot {
        Slot {
            start_time: NaiveTime::parse_from_str(time, "%H:%M").unwrap(),
            duration,
            price: Some(price.into()),
        }
    }

    // 2026-10-15 is a Thursday; Amsterdam is UTC+2 then.
    fn thursday() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 10, 15).unwrap()
    }

    fn earlier() -> NaiveDateTime {
        thursday().and_hms_opt(8, 0, 0).unwrap()
    }

    #[test]
    fn converts_utc_slots_and_filters_window() {
        let cfg = config("");
        let avail = vec![ResourceAvailability {
            resource_id: "r1".into(),
            start_date: thursday(),
            slots: vec![
                slot("17:00", 90, "24 EUR"), // 19:00 local: match
                slot("18:30", 90, "24 EUR"), // 20:30 local: match (inclusive)
                slot("19:00", 90, "24 EUR"), // 21:00 local: too late
                slot("17:00", 60, "16 EUR"), // wrong duration
            ],
        }];
        let names = HashMap::from([("r1".to_string(), "Court 1".to_string())]);
        let c = candidates_for_date(&cfg, thursday(), earlier(), &avail, &names);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].court_name, "Court 1");
        assert_eq!(c[0].start_local, thursday().and_hms_opt(19, 0, 0).unwrap());
        assert_eq!(c[0].start_api, thursday().and_hms_opt(17, 0, 0).unwrap());
    }

    #[test]
    fn local_slot_times_are_used_as_is() {
        let cfg = config("slot_times_utc = false");
        let avail = vec![ResourceAvailability {
            resource_id: "r1".into(),
            start_date: thursday(),
            slots: vec![slot("19:00", 90, "24 EUR"), slot("17:00", 90, "24 EUR")],
        }];
        let c = candidates_for_date(&cfg, thursday(), earlier(), &avail, &HashMap::new());
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].start_api, c[0].start_local);
    }

    #[test]
    fn skips_past_slots_and_other_courts() {
        let cfg = config(r#"courts = ["Court 2"]"#);
        let avail = vec![
            ResourceAvailability { resource_id: "r1".into(), start_date: thursday(), slots: vec![slot("17:00", 90, "0 EUR")] },
            ResourceAvailability { resource_id: "r2".into(), start_date: thursday(), slots: vec![slot("17:00", 90, "0 EUR"), slot("18:00", 90, "0 EUR")] },
        ];
        let names = HashMap::from([("r1".into(), "Court 1".into()), ("r2".into(), "court 2".into())]);
        let now = thursday().and_hms_opt(19, 30, 0).unwrap();
        let c = candidates_for_date(&cfg, thursday(), now, &avail, &names);
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].resource_id, "r2");
        assert_eq!(c[0].start_local.time(), NaiveTime::from_hms_opt(20, 0, 0).unwrap());
    }

    #[test]
    fn listed_free_slots_come_first() {
        let cfg = config("");
        let avail = vec![
            ResourceAvailability { resource_id: "paid".into(), start_date: thursday(), slots: vec![slot("17:00", 90, "24 EUR")] },
            ResourceAvailability { resource_id: "free".into(), start_date: thursday(), slots: vec![slot("18:00", 90, "0 EUR")] },
        ];
        let c = candidates_for_date(&cfg, thursday(), earlier(), &avail, &HashMap::new());
        assert_eq!(c[0].resource_id, "free");
        assert_eq!(c[1].resource_id, "paid");
    }

    #[test]
    fn picks_preferred_payment_method() {
        let m = |t: &str| PaymentMethod { payment_method_id: t.to_lowercase(), method_type: t.into() };
        let methods = vec![m("CREDIT_CARD"), m("MERCHANT_WALLET")];
        let pref = ["OFFER", "MERCHANT_WALLET"].map(String::from);
        assert_eq!(pick_method(&methods, &pref).unwrap().method_type, "MERCHANT_WALLET");
        assert_eq!(pick_method(&methods, &[]).unwrap().method_type, "CREDIT_CARD");
        assert!(pick_method(&[], &pref).is_none());
    }
}
