use std::process::Command;
use std::time::{Duration, Instant};

use serde::Deserialize;
use wlx_common::config::GeneralConfig;

const FITBIT_POLL_INTERVAL: Duration = Duration::from_secs(15);

pub struct FitbitState {
    last_rate: Option<u32>,
    next_poll_at: Instant,
}

impl Default for FitbitState {
    fn default() -> Self {
        Self {
            last_rate: None,
            next_poll_at: Instant::now(),
        }
    }
}

impl FitbitState {
    pub fn update(&mut self, config: &GeneralConfig, watch_visible: bool) {
        if !watch_visible {
            return;
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

        self.next_poll_at = now + FITBIT_POLL_INTERVAL;

        match fetch_latest_rate(&url, token) {
            Ok(rate) => {
                self.last_rate = rate;
            }
            Err(err) => {
                log::warn!("Fitbit poll failed: {err}");
            }
        }
    }

    pub const fn last_rate(&self) -> Option<u32> {
        self.last_rate
    }
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
