//! Minimal client for Playtomic's undocumented web API.
//!
//! Endpoints and payloads mirror what the Playtomic app sends; they can change
//! without notice.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use reqwest::{RequestBuilder, Response};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};

// The old api.playtomic.io no longer resolves. Every endpoint on this host,
// club search included, requires a bearer token.
const BASE_URL: &str = "https://api.app.playtomic.io";
/// Log in again well before the access token expires.
const SESSION_MAX_AGE: Duration = Duration::from_secs(30 * 60);

pub struct Credentials {
    pub email: String,
    pub password: String,
}

impl Credentials {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            email: std::env::var("PLAYTOMIC_EMAIL").context("PLAYTOMIC_EMAIL is not set")?,
            password: std::env::var("PLAYTOMIC_PASSWORD").context("PLAYTOMIC_PASSWORD is not set")?,
        })
    }
}

struct Session {
    access_token: String,
    user_id: String,
    created: Instant,
}

#[derive(Debug, Deserialize)]
pub struct Tenant {
    pub tenant_id: String,
    pub tenant_name: String,
    #[serde(default)]
    pub address: Option<Address>,
    #[serde(default)]
    pub resources: Vec<Resource>,
}

#[derive(Debug, Deserialize)]
pub struct Address {
    #[serde(default)]
    pub city: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Resource {
    pub resource_id: String,
    pub name: String,
    #[serde(default)]
    pub sport_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ResourceAvailability {
    pub resource_id: String,
    pub start_date: NaiveDate,
    #[serde(default)]
    pub slots: Vec<Slot>,
}

#[derive(Debug, Deserialize)]
pub struct Slot {
    pub start_time: NaiveTime,
    pub duration: u32,
    /// Listed (usually non-member) price, e.g. "24 EUR".
    #[serde(default)]
    pub price: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PaymentMethod {
    pub payment_method_id: String,
    pub method_type: String,
}

#[derive(Debug)]
pub struct PaymentIntent {
    pub id: String,
    pub methods: Vec<PaymentMethod>,
    /// Price quoted for this user; `None` if it could not be read.
    pub price: Option<f64>,
    pub raw: Value,
}

impl PaymentIntent {
    pub fn from_value(raw: Value) -> Result<Self> {
        let id = raw
            .get("payment_intent_id")
            .and_then(Value::as_str)
            .context("payment intent has no payment_intent_id")?
            .to_owned();
        let methods = match raw.get("available_payment_methods") {
            Some(v) => serde_json::from_value(v.clone()).context("parsing payment methods")?,
            None => Vec::new(),
        };
        let price = intent_price(&raw);
        Ok(Self { id, methods, price, raw })
    }
}

/// Places a payment intent may carry its price. The highest one found wins, so a
/// share-vs-total mismatch can only make a slot look more expensive, never free.
const PRICE_POINTERS: &[&str] = &[
    "/price",
    "/total_price",
    "/amount",
    "/cart/price",
    "/cart/total_price",
    "/cart/requested_item/price",
];

pub fn intent_price(intent: &Value) -> Option<f64> {
    PRICE_POINTERS
        .iter()
        .filter_map(|p| intent.pointer(p))
        .filter_map(parse_price)
        .reduce(f64::max)
}

/// Parses "12.5 EUR", "0,00 EUR", 12.5 or {"amount": 12.5, ...}.
pub fn parse_price(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => parse_price_str(s),
        Value::Object(o) => o.get("amount").and_then(parse_price),
        _ => None,
    }
    .filter(|p| p.is_finite() && *p >= 0.0)
}

pub fn parse_price_str(s: &str) -> Option<f64> {
    s.split_whitespace()
        .find_map(|t| t.replace(',', ".").parse::<f64>().ok())
        .filter(|p| p.is_finite() && *p >= 0.0)
}

pub struct Playtomic {
    http: reqwest::Client,
    session: Option<Session>,
}

impl Playtomic {
    pub fn new() -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("padel-autobook/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self { http, session: None })
    }

    pub async fn ensure_login(&mut self, creds: &Credentials) -> Result<()> {
        if self.session.as_ref().is_some_and(|s| s.created.elapsed() < SESSION_MAX_AGE) {
            return Ok(());
        }

        #[derive(Deserialize)]
        struct LoginResponse {
            access_token: String,
            user_id: String,
        }
        let resp = self
            .http
            .post(format!("{BASE_URL}/v3/auth/login"))
            // Without the role named, login fails with INVALID_TOKEN_SCOPE.
            .json(&json!({
                "email": creds.email,
                "password": creds.password,
                "requested_user_roles": ["ROLE_CUSTOMER"],
            }))
            .send()
            .await
            .context("login request failed")?;
        let login: LoginResponse = parse(resp).await.context("login failed")?;
        tracing::info!(user_id = %login.user_id, "logged in");
        self.session = Some(Session {
            access_token: login.access_token,
            user_id: login.user_id,
            created: Instant::now(),
        });
        Ok(())
    }

    fn session(&self) -> Result<&Session> {
        self.session.as_ref().context("not logged in")
    }

    fn authed(&self, rb: RequestBuilder) -> Result<RequestBuilder> {
        Ok(rb.bearer_auth(&self.session()?.access_token))
    }

    pub async fn search_tenants(&self, name: &str) -> Result<Vec<Tenant>> {
        let rb = self.http.get(format!("{BASE_URL}/v1/tenants")).query(&[
            ("tenant_name", name),
            ("sport_id", "PADEL"),
            ("playtomic_status", "ACTIVE"),
            ("size", "20"),
        ]);
        parse(self.authed(rb)?.send().await?).await
    }

    pub async fn tenant(&self, tenant_id: &str) -> Result<Tenant> {
        let rb = self.http.get(format!("{BASE_URL}/v1/tenants/{tenant_id}"));
        parse(self.authed(rb)?.send().await?).await
    }

    /// Free slots for one club-local day.
    pub async fn availability(&self, tenant_id: &str, date: NaiveDate) -> Result<Vec<ResourceAvailability>> {
        let min = format!("{date}T00:00:00");
        let max = format!("{date}T23:59:59");
        let rb = self.http.get(format!("{BASE_URL}/v1/availability")).query(&[
            ("user_id", "me"),
            ("tenant_id", tenant_id),
            ("sport_id", "PADEL"),
            ("local_start_min", &min),
            ("local_start_max", &max),
        ]);
        parse(self.authed(rb)?.send().await?).await
    }

    /// Starts a booking checkout. This only quotes the price; nothing is booked
    /// until [`Self::confirm_payment_intent`].
    pub async fn create_payment_intent(
        &self,
        tenant_id: &str,
        resource_id: &str,
        start: NaiveDateTime,
        duration: u32,
    ) -> Result<PaymentIntent> {
        let user_id = &self.session()?.user_id;
        let body = json!({
            "allowed_payment_method_types": [
                "OFFER", "CASH", "MERCHANT_WALLET", "DIRECT", "SWISH", "IDEAL",
                "BANCONTACT", "PAYTRAIL", "CREDIT_CARD", "QUICK_PAY"
            ],
            "user_id": user_id,
            "cart": {
                "requested_item": {
                    "cart_item_type": "CUSTOMER_MATCH",
                    "cart_item_voucher_id": null,
                    "cart_item_data": {
                        "supports_split_payment": true,
                        "number_of_players": 4,
                        "tenant_id": tenant_id,
                        "resource_id": resource_id,
                        "start": start.format("%Y-%m-%dT%H:%M:%S").to_string(),
                        "duration": duration,
                        "match_registrations": [{ "user_id": user_id, "pay_now": true }]
                    }
                }
            }
        });
        let rb = self.http.post(format!("{BASE_URL}/v1/payment_intents")).json(&body);
        PaymentIntent::from_value(parse(self.authed(rb)?.send().await?).await?)
    }

    pub async fn select_payment_method(&self, intent_id: &str, method_id: &str) -> Result<Value> {
        let rb = self
            .http
            .patch(format!("{BASE_URL}/v1/payment_intents/{intent_id}"))
            .json(&json!({ "selected_payment_method_id": method_id, "selected_payment_method_data": null }));
        parse(self.authed(rb)?.send().await?).await
    }

    pub async fn confirm_payment_intent(&self, intent_id: &str) -> Result<Value> {
        let rb = self.http.post(format!("{BASE_URL}/v1/payment_intents/{intent_id}/confirmation"));
        parse(self.authed(rb)?.send().await?).await
    }
}

async fn parse<T: DeserializeOwned>(resp: Response) -> Result<T> {
    let status = resp.status();
    let path = resp.url().path().to_owned();
    let body = resp.text().await?;
    if !status.is_success() {
        bail!("{path} returned {status}: {}", truncate(&body, 500));
    }
    serde_json::from_str(&body)
        .with_context(|| format!("unexpected response from {path}: {}", truncate(&body, 500)))
}

fn truncate(s: &str, max_chars: usize) -> String {
    match s.char_indices().nth(max_chars) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_price_formats() {
        assert_eq!(parse_price(&json!("0 EUR")), Some(0.0));
        assert_eq!(parse_price(&json!("24.5 EUR")), Some(24.5));
        assert_eq!(parse_price(&json!("EUR 7,50")), Some(7.5));
        assert_eq!(parse_price(&json!(12)), Some(12.0));
        assert_eq!(parse_price(&json!({ "amount": 0, "currency": "EUR" })), Some(0.0));
        assert_eq!(parse_price(&json!("free")), None);
        assert_eq!(parse_price(&json!("NaN EUR")), None);
        assert_eq!(parse_price(&json!(null)), None);
    }

    #[test]
    fn intent_price_takes_highest_known_field() {
        let v = json!({ "price": "0 EUR", "cart": { "total_price": "24 EUR" } });
        assert_eq!(intent_price(&v), Some(24.0));
        assert_eq!(intent_price(&json!({ "price": "0 EUR" })), Some(0.0));
        assert_eq!(intent_price(&json!({ "something_else": 0 })), None);
    }

    #[test]
    fn payment_intent_from_value() {
        let pi = PaymentIntent::from_value(json!({
            "payment_intent_id": "pi_1",
            "price": "0 EUR",
            "available_payment_methods": [
                { "payment_method_id": "m1", "method_type": "CREDIT_CARD", "name": "Visa" }
            ]
        }))
        .unwrap();
        assert_eq!(pi.id, "pi_1");
        assert_eq!(pi.price, Some(0.0));
        assert_eq!(pi.methods[0].method_type, "CREDIT_CARD");
        assert!(PaymentIntent::from_value(json!({})).is_err());
    }
}
