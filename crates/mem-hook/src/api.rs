//! Blocking HTTP client for the memory API. Every call has an explicit timeout.

use std::time::Duration;

use serde_json::Value;

use crate::config::Config;

pub struct Api {
    agent: ureq::Agent,
    base: String,
    token: String,
    device: String,
}

pub struct Resp {
    pub status: u16,
    pub body: String,
}

impl Resp {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }

    pub fn json(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }
}

impl Api {
    pub fn new(cfg: &Config) -> Api {
        // The server lives on the tailnet, so environment proxies are deliberately ignored.
        let agent: ureq::Agent = ureq::Agent::config_builder().http_status_as_error(false).proxy(None).build().into();
        Api { agent, base: cfg.url.clone(), token: cfg.token.clone(), device: cfg.device.clone() }
    }

    /// GET `path` (with query). Err is a transport error (connect, timeout, TLS).
    pub fn get(&self, path: &str, timeout: Duration) -> Result<Resp, String> {
        let req = self.agent.get(format!("{}{path}", self.base)).config().timeout_global(Some(timeout)).build();
        finish(
            req.header("Authorization", format!("Bearer {}", self.token))
                .header("X-Memory-Device", &self.device)
                .call(),
        )
    }

    pub fn post(&self, path: &str, body: &Value, timeout: Duration) -> Result<Resp, String> {
        let req = self.agent.post(format!("{}{path}", self.base)).config().timeout_global(Some(timeout)).build();
        finish(
            req.header("Authorization", format!("Bearer {}", self.token))
                .header("X-Memory-Device", &self.device)
                .header("Content-Type", "application/json")
                .send(body.to_string()),
        )
    }
}

fn finish(result: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<Resp, String> {
    let mut resp = result.map_err(|e| e.to_string())?;
    let status = resp.status().as_u16();
    let body = resp.body_mut().read_to_string().map_err(|e| e.to_string())?;
    Ok(Resp { status, body })
}
