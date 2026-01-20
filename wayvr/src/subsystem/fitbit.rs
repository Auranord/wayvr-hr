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
}

impl Default for FitbitState {
    fn default() -> Self {
        Self {
            last_rate: None,
            next_poll_at: Instant::now(),
            next_interval_index: 0,
            last_watch_visible: false,
            pending: None,
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
                        FetchResult::Ok(rate) => {
                            self.last_rate = rate;
                        }
                        FetchResult::Err(err) => {
                            log::warn!("Fitbit poll failed: {err}");
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

        let Some(token) = config
            .fitbit_access_token
            .as_deref()
            .filter(|token| !token.trim().is_empty())
        else {
            return;
        };

        let now = Instant::now();
        if now < self.next_poll_at {
            return;
        }

        let user_id = config
            .fitbit_user_id
            .as_deref()
            .filter(|id| !id.trim().is_empty())
            .unwrap_or("-");

        let url = format!(
            "https://api.fitbit.com/1/user/{user_id}/activities/heart/date/today/1d/1min.json"
        );

        if self.pending.is_some() {
            return;
        }

        let interval = FITBIT_POLL_INTERVALS
            .get(self.next_interval_index)
            .copied()
            .unwrap_or_else(|| *FITBIT_POLL_INTERVALS.last().unwrap());
        self.next_poll_at = now + interval;
        self.next_interval_index =
            (self.next_interval_index + 1).min(FITBIT_POLL_INTERVALS.len() - 1);

        let (sender, receiver) = channel();
        let url = url.clone();
        let token = token.to_string();
        std::thread::spawn(move || {
            let result = match fetch_latest_rate(&url, &token) {
                Ok(rate) => FetchResult::Ok(rate),
                Err(err) => FetchResult::Err(err.to_string()),
            };
            let _ = sender.send(result);
        });
        self.pending = Some(receiver);
    }

    pub const fn last_rate(&self) -> Option<u32> {
        self.last_rate
    }
}

enum FetchResult {
    Ok(Option<u32>),
    Err(String),
}

fn fetch_latest_rate(url: &str, token: &str) -> anyhow::Result<Option<u32>> {
    let output = Command::new("curl")
        .args([
            "--silent",
            "--show-error",
            "--fail",
            "--header",
            &format!("Authorization: Bearer {token}"),
            "--header",
            "Accept: application/json",
            url,
        ])
        .output()?;

    if !output.status.success() {
        anyhow::bail!(
            "curl exited with status {}",
            output.status.code().unwrap_or(-1)
        );
    }

    let response: FitbitHeartResponse = serde_json::from_slice(&output.stdout)?;
    Ok(response
        .intraday
        .dataset
        .last()
        .map(|entry| entry.value))
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
