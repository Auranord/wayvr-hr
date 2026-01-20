use std::process::Command;
use std::sync::mpsc::{Receiver, TryRecvError, channel};
use std::time::{Duration, Instant};

use serde::Deserialize;
use wlx_common::config::GeneralConfig;

const FITBIT_POLL_INTERVALS: [Duration; 4] = [
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(10),
    Duration::from_secs(30),
];

pub struct FitbitState {
    last_rate: Option<u32>,
    next_poll_at: Instant,
    next_interval_index: usize,
    last_watch_visible: bool,
    pending: Option<Receiver<FetchResult>>,
    access_token: Option<String>,
    access_token_expires_at: Option<Instant>,
    refresh_token: Option<String>,
}

impl Default for FitbitState {
    fn default() -> Self {
        Self {
            last_rate: None,
            next_poll_at: Instant::now(),
            next_interval_index: 0,
            last_watch_visible: false,
            pending: None,
            access_token: None,
            access_token_expires_at: None,
            refresh_token: None,
        }
    }
}

impl FitbitState {
    pub fn update(&mut self, config: &GeneralConfig, watch_visible: bool) {
        if let Some(receiver) = self.pending.as_ref() {
            match receiver.try_recv() {
                Ok(result) => {
                    self.pending = None;
                    match result {
                        FetchResult::Ok { rate, token } => {
                            self.last_rate = rate;
                            if let Some(token) = token {
                                self.apply_token_update(token);
                            }
                            log::debug!("Fitbit poll success.");
                        }
                        FetchResult::Err { message, status } => {
                            if status == 429 {
                                log::warn!("Fitbit poll rate limited (429). Backing off.");
                                self.next_poll_at = Instant::now() + Duration::from_secs(60);
                                self.next_interval_index = FITBIT_POLL_INTERVALS.len() - 1;
                            } else {
                                log::warn!("Fitbit poll failed: {message}");
                            }
                        }
                    }
                }
                Err(TryRecvError::Disconnected) => {
                    self.pending = None;
                }
                Err(TryRecvError::Empty) => {}
            }
        }

        if !watch_visible {
            self.last_watch_visible = false;
            return;
        }

        if watch_visible && !self.last_watch_visible {
            self.next_poll_at = Instant::now();
            self.next_interval_index = 0;
            self.last_watch_visible = true;
        }

        let config_access_token = config
            .fitbit_access_token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
            .map(|token| token.to_string());

        if self.access_token.is_none() {
            self.access_token.clone_from(&config_access_token);
        }

        let now = Instant::now();
        if now < self.next_poll_at {
            return;
        }

        let user_id = config
            .fitbit_user_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or("-");

        let refresh_token = config
            .fitbit_refresh_token
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.to_string())
            .or_else(|| self.refresh_token.clone());

        let client_id = config
            .fitbit_client_id
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.to_string());

        let client_secret = config
            .fitbit_client_secret
            .as_deref()
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.to_string());

        let url = format!(
            "https://api.fitbit.com/1/user/{user_id}/activities/heart/date/today/1d/1min.json"
        );

        if self.pending.is_some() {
            return;
        }

        let access_token = self.access_token.clone();
        let token_expiry = self.access_token_expires_at;
        log::debug!("Fitbit poll attempt.");

        let interval = FITBIT_POLL_INTERVALS
            .get(self.next_interval_index)
            .copied()
            .unwrap_or_else(|| *FITBIT_POLL_INTERVALS.last().unwrap());
        self.next_poll_at = now + interval;
        self.next_interval_index =
            (self.next_interval_index + 1).min(FITBIT_POLL_INTERVALS.len() - 1);

        let (sender, receiver) = channel();
        let url = url.clone();
        std::thread::spawn(move || {
            let result = fetch_latest_rate(
                &url,
                config_access_token,
                access_token,
                token_expiry,
                refresh_token,
                client_id,
                client_secret,
            );
            let _ = sender.send(result);
        });
        self.pending = Some(receiver);
    }

    pub const fn last_rate(&self) -> Option<u32> {
        self.last_rate
    }

    fn apply_token_update(&mut self, update: TokenUpdate) {
        self.access_token = Some(update.access_token);
        self.access_token_expires_at = Some(Instant::now() + update.expires_in);
        if let Some(refresh_token) = update.refresh_token {
            self.refresh_token = Some(refresh_token);
        }
    }
}

enum FetchResult {
    Ok {
        rate: Option<u32>,
        token: Option<TokenUpdate>,
    },
    Err { message: String, status: u16 },
}

struct TokenUpdate {
    access_token: String,
    expires_in: Duration,
    refresh_token: Option<String>,
}

fn fetch_latest_rate(
    url: &str,
    config_access_token: Option<String>,
    cached_access_token: Option<String>,
    cached_expiry: Option<Instant>,
    refresh_token: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
) -> FetchResult {
    let mut token = cached_access_token.or(config_access_token);
    let expired = cached_expiry.map_or(false, |expiry| Instant::now() >= expiry);
    let can_refresh = refresh_token.is_some() && client_id.is_some() && client_secret.is_some();
    let mut token_update = None;

    if expired && can_refresh {
        match refresh_access_token(refresh_token.clone(), client_id.clone(), client_secret.clone()) {
            Ok(update) => {
                token = Some(update.access_token.clone());
                token_update = Some(update);
            }
            Err(err) => {
                return FetchResult::Err {
                    message: err.to_string(),
                    status: err.status,
                };
            }
        }
    } else if token.is_none() && can_refresh {
        match refresh_access_token(refresh_token.clone(), client_id.clone(), client_secret.clone()) {
            Ok(update) => {
                token = Some(update.access_token.clone());
                token_update = Some(update);
            }
            Err(err) => {
                return FetchResult::Err {
                    message: err.to_string(),
                    status: err.status,
                };
            }
        }
    }

    let Some(token) = token else {
        return FetchResult::Err {
            message: "Fitbit access token is missing".to_string(),
            status: 0,
        };
    };

    match request_heart_rate(url, &token) {
        Ok(rate) => FetchResult::Ok {
            rate,
            token: token_update,
        },
        Err(err) => {
            if err.status == 401 {
                match refresh_access_token(refresh_token, client_id, client_secret) {
                    Ok(update) => {
                        let token = update.access_token.clone();
                        match request_heart_rate(url, &token) {
                            Ok(rate) => FetchResult::Ok {
                                rate,
                                token: Some(update),
                            },
                            Err(err) => {
                                log::debug!("Fitbit poll failed after refresh: {err}");
                                FetchResult::Err {
                                    message: err.to_string(),
                                    status: err.status,
                                }
                            }
                        }
                    }
                    Err(err) => FetchResult::Err {
                        message: err.to_string(),
                        status: 0,
                    },
                }
            } else {
                log::debug!("Fitbit poll failed: {err}");
                FetchResult::Err {
                    message: err.to_string(),
                    status: err.status,
                }
            }
        }
    }
}

fn request_heart_rate(url: &str, token: &str) -> Result<Option<u32>, FitbitRequestError> {
    let (status, body) = curl_with_status(vec![
        "--header".into(),
        format!("Authorization: Bearer {token}"),
        "--header".into(),
        "Accept: application/json".into(),
        url.into(),
    ])
    .map_err(|err| FitbitRequestError::new(0, err.to_string()))?;

    if status >= 400 {
        return Err(FitbitRequestError::new(
            status,
            "Fitbit heart rate request failed",
        ));
    }

    let response: FitbitHeartResponse =
        serde_json::from_slice(&body).map_err(|err| FitbitRequestError::new(0, err.to_string()))?;
    Ok(response
        .intraday
        .dataset
        .last()
        .map(|entry| entry.value))
}

fn refresh_access_token(
    refresh_token: Option<String>,
    client_id: Option<String>,
    client_secret: Option<String>,
) -> anyhow::Result<TokenUpdate> {
    let refresh_token =
        refresh_token.ok_or_else(|| anyhow::anyhow!("Fitbit refresh token is missing"))?;
    let client_id = client_id.ok_or_else(|| anyhow::anyhow!("Fitbit client ID is missing"))?;
    let client_secret =
        client_secret.ok_or_else(|| anyhow::anyhow!("Fitbit client secret is missing"))?;

    let form = format!("grant_type=refresh_token&refresh_token={refresh_token}");
    let (status, body) = curl_with_status(vec![
        "--request".into(),
        "POST".into(),
        "--user".into(),
        format!("{client_id}:{client_secret}"),
        "--header".into(),
        "Content-Type: application/x-www-form-urlencoded".into(),
        "--data".into(),
        form,
        "https://api.fitbit.com/oauth2/token".into(),
    ])?;

    if status >= 400 {
        return Err(anyhow::anyhow!("Fitbit refresh failed ({status})"));
    }

    let response: FitbitTokenResponse = serde_json::from_slice(&body)?;
    Ok(TokenUpdate {
        access_token: response.access_token,
        expires_in: Duration::from_secs(response.expires_in),
        refresh_token: response.refresh_token,
    })
}

fn curl_with_status(args: Vec<String>) -> anyhow::Result<(u16, Vec<u8>)> {
    let mut full_args = vec![
        "--silent".into(),
        "--show-error".into(),
        "--location".into(),
        "--write-out".into(),
        "\n%{http_code}".into(),
    ];
    full_args.extend(args);

    let output = Command::new("curl").args(full_args).output()?;
    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "curl exited with status {}",
            output.status.code().unwrap_or(-1)
        ));
    }

    let mut parts = output.stdout.split(|b| *b == b'\n').collect::<Vec<_>>();
    let status_bytes = parts
        .pop()
        .ok_or_else(|| anyhow::anyhow!("missing status code"))?;
    let status = std::str::from_utf8(status_bytes)?.parse::<u16>()?;
    let body = parts.join(&b'\n');
    Ok((status, body))
}

#[derive(Deserialize)]
struct FitbitHeartResponse {
    #[serde(rename = "activities-heart-intraday")]
    intraday: FitbitIntraday,
}

#[derive(Deserialize)]
struct FitbitIntraday {
    #[serde(default)]
    dataset: Vec<FitbitDatasetEntry>,
}

#[derive(Deserialize)]
struct FitbitDatasetEntry {
    value: u32,
}

#[derive(Deserialize)]
struct FitbitTokenResponse {
    access_token: String,
    expires_in: u64,
    #[serde(default)]
    refresh_token: Option<String>,
}

#[derive(Debug)]
struct FitbitRequestError {
    status: u16,
    message: String,
}

impl FitbitRequestError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for FitbitRequestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (status {})", self.message, self.status)
    }
}

impl std::error::Error for FitbitRequestError {}
