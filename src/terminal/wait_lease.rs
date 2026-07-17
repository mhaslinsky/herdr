use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const MAX_WAIT_LEASE_TTL_MS: u64 = 86_400_000;
pub(crate) const WAIT_LEASE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const WAIT_LEASE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const WAIT_LEASE_PROTOCOL_VERSION: u32 = 1;
const TOKEN_BYTES: usize = 32;
const REQUEST_ID_BYTES: usize = 16;
const MAX_REQUEST_BYTES: u64 = 16 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WaitLease {
    job_id: String,
    token: String,
    expires_at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveWaitLease {
    pub job_id: String,
    pub remaining_ms: u64,
}

impl WaitLease {
    pub(crate) fn new(job_id: String, token: String, expires_at_ms: u64) -> Self {
        Self {
            job_id,
            token,
            expires_at_ms,
        }
    }

    pub fn job_id(&self) -> &str {
        &self.job_id
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn expires_at_ms(&self) -> u64 {
        self.expires_at_ms
    }

    pub fn is_active_at(&self, now_ms: u64) -> bool {
        now_ms < self.expires_at_ms
    }

    pub fn active_at(&self, now_ms: u64) -> Option<ActiveWaitLease> {
        self.is_active_at(now_ms).then(|| ActiveWaitLease {
            job_id: self.job_id.clone(),
            remaining_ms: self.expires_at_ms.saturating_sub(now_ms),
        })
    }

    pub fn token_matches(&self, token: &str) -> bool {
        self.token == token
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WaitLeaseRequest {
    pub(crate) version: u32,
    pub(crate) request_id: String,
    pub(crate) operation: WaitLeaseOperation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum WaitLeaseOperation {
    Acquire {
        pane_id: String,
        terminal_id: String,
        job_id: String,
        ttl_ms: u64,
        token: String,
        requested_at_ms: u64,
    },
    Release {
        pane_id: String,
        terminal_id: String,
        job_id: String,
        token: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WaitLeaseResponse {
    version: u32,
    request_id: String,
    pub(crate) result: WaitLeaseResponseResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum WaitLeaseResponseResult {
    Acquired {
        job_id: String,
        remaining_ms: u64,
        token: String,
    },
    Released {
        released: bool,
    },
    Error {
        code: String,
        message: String,
    },
}

impl WaitLeaseResponse {
    pub(crate) fn acquired(
        request_id: String,
        job_id: String,
        remaining_ms: u64,
        token: String,
    ) -> Self {
        Self {
            version: WAIT_LEASE_PROTOCOL_VERSION,
            request_id,
            result: WaitLeaseResponseResult::Acquired {
                job_id,
                remaining_ms,
                token,
            },
        }
    }

    pub(crate) fn released(request_id: String, released: bool) -> Self {
        Self {
            version: WAIT_LEASE_PROTOCOL_VERSION,
            request_id,
            result: WaitLeaseResponseResult::Released { released },
        }
    }

    pub(crate) fn error(
        request_id: String,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            version: WAIT_LEASE_PROTOCOL_VERSION,
            request_id,
            result: WaitLeaseResponseResult::Error {
                code: code.into(),
                message: message.into(),
            },
        }
    }
}

pub(crate) struct PendingWaitLeaseRequest {
    path: PathBuf,
    pub(crate) request: WaitLeaseRequest,
}

pub(crate) fn acquire_wait_lease(
    pane_id: String,
    terminal_id: String,
    job_id: String,
    ttl_ms: u64,
) -> std::io::Result<WaitLeaseResponseResult> {
    let token = random_hex(TOKEN_BYTES)?;
    let request_id = random_hex(REQUEST_ID_BYTES)?;
    let requested_at_ms = crate::platform::continuous_clock_ms()?;
    submit_request(WaitLeaseRequest {
        version: WAIT_LEASE_PROTOCOL_VERSION,
        request_id,
        operation: WaitLeaseOperation::Acquire {
            pane_id,
            terminal_id,
            job_id,
            ttl_ms,
            token,
            requested_at_ms,
        },
    })
}

pub(crate) fn release_wait_lease(
    pane_id: String,
    terminal_id: String,
    job_id: String,
    token: String,
) -> std::io::Result<WaitLeaseResponseResult> {
    let request_id = random_hex(REQUEST_ID_BYTES)?;
    submit_request(WaitLeaseRequest {
        version: WAIT_LEASE_PROTOCOL_VERSION,
        request_id,
        operation: WaitLeaseOperation::Release {
            pane_id,
            terminal_id,
            job_id,
            token,
        },
    })
}

fn submit_request(request: WaitLeaseRequest) -> std::io::Result<WaitLeaseResponseResult> {
    let root = wait_lease_root();
    let request_path = request_path(&root, &request.request_id);
    let response_path = response_path(&root, &request.request_id);
    atomic_write_json(&request_path, &request)?;

    let deadline = Instant::now() + WAIT_LEASE_RESPONSE_TIMEOUT;
    loop {
        match fs::read(&response_path) {
            Ok(bytes) => {
                let _ = fs::remove_file(&response_path);
                let _ = fs::remove_file(&request_path);
                let response: WaitLeaseResponse = serde_json::from_slice(&bytes)?;
                if response.version != WAIT_LEASE_PROTOCOL_VERSION
                    || response.request_id != request.request_id
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "wait lease response did not match its request",
                    ));
                }
                return Ok(response.result);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            let _ = fs::remove_file(&request_path);
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "Herdr did not process the wait lease request within 5 seconds",
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub(crate) fn pending_wait_lease_requests() -> std::io::Result<Vec<PendingWaitLeaseRequest>> {
    let requests_dir = wait_lease_root().join("requests");
    let entries = match fs::read_dir(&requests_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().and_then(|value| value.to_str()) == Some("json"))
        .collect::<Vec<_>>();
    paths.sort();

    let mut pending = Vec::new();
    for path in paths.into_iter().take(64) {
        let metadata = match fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::warn!(path = %path.display(), err = %error, "failed to inspect wait lease request");
                continue;
            }
        };
        if !metadata.is_file() || metadata.len() > MAX_REQUEST_BYTES {
            tracing::warn!(path = %path.display(), "discarding invalid wait lease request file");
            let _ = fs::remove_file(&path);
            continue;
        }
        let request = match fs::read(&path)
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(std::io::Error::other))
        {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(path = %path.display(), err = %error, "discarding unreadable wait lease request");
                let _ = fs::remove_file(&path);
                continue;
            }
        };
        pending.push(PendingWaitLeaseRequest { path, request });
    }
    Ok(pending)
}

pub(crate) fn complete_wait_lease_request(
    pending: PendingWaitLeaseRequest,
    response: &WaitLeaseResponse,
) -> std::io::Result<()> {
    let response_path = response_path(&wait_lease_root(), &pending.request.request_id);
    atomic_write_json(&response_path, response)?;
    fs::remove_file(pending.path)
}

fn wait_lease_root() -> PathBuf {
    let socket_path = crate::session::active_api_socket_path();
    let parent = socket_path.parent().unwrap_or_else(|| Path::new("."));
    let socket_name = socket_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("herdr.sock");
    parent.join(format!("{socket_name}.wait-leases"))
}

fn request_path(root: &Path, request_id: &str) -> PathBuf {
    root.join("requests").join(format!("{request_id}.json"))
}

fn response_path(root: &Path, request_id: &str) -> PathBuf {
    root.join("responses").join(format!("{request_id}.json"))
}

fn atomic_write_json(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("wait lease path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temp_path = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("wait-lease"),
        random_hex(REQUEST_ID_BYTES)?
    ));
    let bytes = serde_json::to_vec(value)?;
    if let Err(error) = crate::platform::write_private_file(&temp_path, &bytes)
        .and_then(|()| fs::rename(&temp_path, path))
    {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }
    Ok(())
}

fn random_hex(byte_count: usize) -> std::io::Result<String> {
    let mut bytes = vec![0_u8; byte_count];
    getrandom::fill(&mut bytes).map_err(|error| {
        std::io::Error::other(format!("random token generation failed: {error}"))
    })?;
    Ok(bytes
        .into_iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_is_inactive_at_its_expiry() {
        let lease = WaitLease::new("job-42".into(), "token".into(), 2_000);

        assert!(lease.is_active_at(1_999));
        assert!(!lease.is_active_at(2_000));
    }

    #[test]
    fn lease_token_matches_only_its_owner() {
        let lease = WaitLease::new("job-42".into(), "owner-token".into(), 2_000);

        assert!(lease.token_matches("owner-token"));
        assert!(!lease.token_matches("other-token"));
    }
}
