use std::collections::VecDeque;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

pub const MAX_WAIT_LEASE_TTL_MS: u64 = 86_400_000;
pub(crate) const WAIT_LEASE_POLL_INTERVAL: Duration = Duration::from_millis(100);
const WAIT_LEASE_RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);
const WAIT_LEASE_CLAIMED_RESPONSE_TIMEOUT: Duration = Duration::from_secs(1);
const WAIT_LEASE_PROTOCOL_VERSION: u32 = 1;
const TOKEN_BYTES: usize = 32;
const REQUEST_ID_BYTES: usize = 16;
const MAX_REQUEST_BYTES: u64 = 16 * 1024;
const MAX_TRACKED_WAIT_LEASE_REVOCATIONS: usize = 4_096;

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

#[derive(Debug, Clone, PartialEq, Eq)]
struct WaitLeaseRevocation {
    token: String,
    retain_until_ms: u64,
    released: bool,
}

#[derive(Debug, Default)]
pub(crate) struct WaitLeaseRevocations {
    entries: VecDeque<WaitLeaseRevocation>,
    reject_acquires_until_ms: Option<u64>,
}

impl WaitLeaseRevocations {
    pub(crate) fn token_is_revoked(&mut self, token: &str, now_ms: u64) -> bool {
        self.prune(now_ms);
        self.entries.iter().any(|entry| entry.token == token)
    }

    pub(crate) fn acquire_guard_saturated(&mut self, now_ms: u64) -> bool {
        self.prune(now_ms);
        self.reject_acquires_until_ms.is_some()
    }

    pub(crate) fn prior_release_result(&mut self, token: &str, now_ms: u64) -> Option<bool> {
        self.prune(now_ms);
        self.entries
            .iter()
            .find(|entry| entry.token == token)
            .map(|entry| entry.released)
    }

    pub(crate) fn record(
        &mut self,
        token: String,
        retain_until_ms: u64,
        released: bool,
        now_ms: u64,
    ) {
        self.prune(now_ms);
        if retain_until_ms <= now_ms {
            return;
        }
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.token == token) {
            entry.retain_until_ms = entry.retain_until_ms.max(retain_until_ms);
            entry.released |= released;
            return;
        }
        if let Some(reject_until_ms) = self.reject_acquires_until_ms.as_mut() {
            *reject_until_ms = (*reject_until_ms).max(retain_until_ms);
            return;
        }
        if self.entries.len() >= MAX_TRACKED_WAIT_LEASE_REVOCATIONS {
            let reject_until_ms = self
                .entries
                .iter()
                .map(|entry| entry.retain_until_ms)
                .max()
                .unwrap_or(retain_until_ms)
                .max(retain_until_ms);
            self.entries.clear();
            self.reject_acquires_until_ms = Some(reject_until_ms);
            return;
        }
        self.entries.push_back(WaitLeaseRevocation {
            token,
            retain_until_ms,
            released,
        });
    }

    pub(crate) fn clear(&mut self) {
        self.entries.clear();
        self.reject_acquires_until_ms = None;
    }

    fn prune(&mut self, now_ms: u64) {
        self.entries.retain(|entry| entry.retain_until_ms > now_ms);
        if self
            .reject_acquires_until_ms
            .is_some_and(|reject_until_ms| reject_until_ms <= now_ms)
        {
            self.reject_acquires_until_ms = None;
        }
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
        terminal_generation: u64,
        job_id: String,
        ttl_ms: u64,
        token: String,
        requested_at_ms: u64,
    },
    Release {
        pane_id: String,
        terminal_id: String,
        terminal_generation: u64,
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
    root: PathBuf,
    path: PathBuf,
    pub(crate) request: WaitLeaseRequest,
}

pub(crate) fn acquire_wait_lease(
    pane_id: String,
    terminal_id: String,
    terminal_generation: u64,
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
            terminal_generation,
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
    terminal_generation: u64,
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
            terminal_generation,
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
        if let Some(result) = read_response_result(&response_path, &request)? {
            let _ = fs::remove_file(&request_path);
            return Ok(result);
        }
        if Instant::now() >= deadline {
            match fs::remove_file(&request_path) {
                Ok(()) => return Err(wait_lease_timeout()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(_) => break,
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let claimed_deadline = Instant::now() + WAIT_LEASE_CLAIMED_RESPONSE_TIMEOUT;
    loop {
        if let Some(result) = read_response_result(&response_path, &request)? {
            return Ok(result);
        }
        if Instant::now() >= claimed_deadline {
            return Err(wait_lease_timeout());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn read_response_result(
    response_path: &Path,
    request: &WaitLeaseRequest,
) -> std::io::Result<Option<WaitLeaseResponseResult>> {
    let bytes = match fs::read(response_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let _ = fs::remove_file(response_path);
    let response: WaitLeaseResponse = serde_json::from_slice(&bytes)?;
    if response.version != WAIT_LEASE_PROTOCOL_VERSION || response.request_id != request.request_id
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "wait lease response did not match its request",
        ));
    }
    Ok(Some(response.result))
}

fn wait_lease_timeout() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "Herdr did not process the wait lease request within 5 seconds",
    )
}

pub(crate) fn pending_wait_lease_requests() -> std::io::Result<Vec<PendingWaitLeaseRequest>> {
    pending_wait_lease_requests_at(wait_lease_root())
}

fn pending_wait_lease_requests_at(root: PathBuf) -> std::io::Result<Vec<PendingWaitLeaseRequest>> {
    let requests_dir = root.join("requests");
    let processing_dir = root.join("processing");
    fs::create_dir_all(&processing_dir)?;
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
        let Some(file_name) = path.file_name() else {
            continue;
        };
        let claimed_path = processing_dir.join(file_name);
        match fs::rename(&path, &claimed_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                tracing::warn!(path = %path.display(), err = %error, "failed to claim wait lease request");
                continue;
            }
        }
        let metadata = match fs::metadata(&claimed_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                tracing::warn!(path = %claimed_path.display(), err = %error, "failed to inspect wait lease request");
                continue;
            }
        };
        if !metadata.is_file() || metadata.len() > MAX_REQUEST_BYTES {
            tracing::warn!(path = %claimed_path.display(), "discarding invalid wait lease request file");
            let _ = fs::remove_file(&claimed_path);
            continue;
        }
        let request = match fs::read(&claimed_path)
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(std::io::Error::other))
        {
            Ok(request) => request,
            Err(error) => {
                tracing::warn!(path = %claimed_path.display(), err = %error, "discarding unreadable wait lease request");
                let _ = fs::remove_file(&claimed_path);
                continue;
            }
        };
        pending.push(PendingWaitLeaseRequest {
            root: root.clone(),
            path: claimed_path,
            request,
        });
    }
    Ok(pending)
}

pub(crate) fn complete_wait_lease_request(
    pending: PendingWaitLeaseRequest,
    response: &WaitLeaseResponse,
) -> std::io::Result<()> {
    let response_path = response_path(&pending.root, &pending.request.request_id);
    let write_result = atomic_write_json(&response_path, response);
    if let Err(error) = fs::remove_file(&pending.path) {
        if error.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(path = %pending.path.display(), err = %error, "failed to remove completed wait lease request");
        }
    }
    write_result
}

fn wait_lease_root() -> PathBuf {
    let socket_path = crate::session::active_api_socket_path();
    wait_lease_root_for_socket(&socket_path)
}

fn wait_lease_root_for_socket(socket_path: &Path) -> PathBuf {
    let parent = socket_path.parent().unwrap_or_else(|| Path::new("."));
    let socket_name = socket_path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("herdr.sock");
    parent.join(format!("{socket_name}.wait-leases"))
}

pub(crate) fn prepare_wait_lease_runtime(socket_path: &Path) -> std::io::Result<()> {
    let root = wait_lease_root_for_socket(socket_path);
    match fs::remove_dir_all(&root) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    fs::create_dir_all(root.join("requests"))?;
    fs::create_dir_all(root.join("processing"))?;
    fs::create_dir_all(root.join("responses"))
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

    fn unique_test_socket(label: &str) -> PathBuf {
        std::env::temp_dir()
            .join(format!(
                "herdr-wait-lease-{label}-{}-{}",
                std::process::id(),
                random_hex(8).unwrap()
            ))
            .join("herdr.sock")
    }

    fn test_request(request_id: &str) -> WaitLeaseRequest {
        WaitLeaseRequest {
            version: WAIT_LEASE_PROTOCOL_VERSION,
            request_id: request_id.into(),
            operation: WaitLeaseOperation::Acquire {
                pane_id: "w1:p1".into(),
                terminal_id: "term_1".into(),
                terminal_generation: 0,
                job_id: "review".into(),
                ttl_ms: 60_000,
                token: "a".repeat(64),
                requested_at_ms: 1_000,
            },
        }
    }

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

    #[test]
    fn revocation_capacity_fails_closed_until_retention_window_ends() {
        let mut revocations = WaitLeaseRevocations::default();
        for token_index in 0..MAX_TRACKED_WAIT_LEASE_REVOCATIONS {
            revocations.record(
                format!("{token_index:064x}"),
                MAX_WAIT_LEASE_TTL_MS,
                true,
                0,
            );
        }

        revocations.record("f".repeat(64), MAX_WAIT_LEASE_TTL_MS, true, 0);

        assert!(revocations.acquire_guard_saturated(0));
        assert!(!revocations.acquire_guard_saturated(MAX_WAIT_LEASE_TTL_MS));
    }

    #[test]
    fn server_start_clears_stale_wait_lease_transport() {
        let socket_path = unique_test_socket("startup-cleanup");
        let root = wait_lease_root_for_socket(&socket_path);
        let stale_request = root.join("requests/stale.json");
        fs::create_dir_all(stale_request.parent().unwrap()).unwrap();
        fs::write(&stale_request, b"stale").unwrap();

        prepare_wait_lease_runtime(&socket_path).unwrap();

        assert!(!stale_request.exists());
        assert!(root.join("requests").is_dir());
        assert!(root.join("processing").is_dir());
        assert!(root.join("responses").is_dir());
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn server_claims_request_before_processing_it() {
        let socket_path = unique_test_socket("claim");
        prepare_wait_lease_runtime(&socket_path).unwrap();
        let root = wait_lease_root_for_socket(&socket_path);
        let request = test_request("claim-request");
        let submitted_path = request_path(&root, &request.request_id);
        atomic_write_json(&submitted_path, &request).unwrap();

        let mut pending = pending_wait_lease_requests_at(root.clone()).unwrap();

        assert_eq!(pending.len(), 1);
        assert!(!submitted_path.exists());
        assert!(pending[0].path.starts_with(root.join("processing")));
        let claimed_path = pending[0].path.clone();
        complete_wait_lease_request(
            pending.pop().unwrap(),
            &WaitLeaseResponse::released("claim-request".into(), false),
        )
        .unwrap();
        assert!(!claimed_path.exists());
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn failed_response_write_still_discards_claimed_request() {
        let socket_path = unique_test_socket("response-failure");
        prepare_wait_lease_runtime(&socket_path).unwrap();
        let root = wait_lease_root_for_socket(&socket_path);
        let claimed_path = root.join("processing/response-failure.json");
        fs::write(&claimed_path, b"claimed").unwrap();
        fs::remove_dir(root.join("responses")).unwrap();
        fs::write(root.join("responses"), b"not-a-directory").unwrap();
        let pending = PendingWaitLeaseRequest {
            root: root.clone(),
            path: claimed_path.clone(),
            request: test_request("response-failure"),
        };

        assert!(complete_wait_lease_request(
            pending,
            &WaitLeaseResponse::released("response-failure".into(), false),
        )
        .is_err());
        assert!(!claimed_path.exists());
        fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }
}
