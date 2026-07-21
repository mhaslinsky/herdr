use std::time::{Duration, Instant};

use crossterm::terminal;

use super::{
    background_update_check_enabled, repeat_key_identity, App, Mode, ANIMATION_INTERVAL,
    AUTO_UPDATE_CHECK_INTERVAL, GIT_REMOTE_STATUS_REFRESH_INTERVAL, MIN_RENDER_INTERVAL,
    RESIZE_POLL_INTERVAL, SELECTION_AUTOSCROLL_INTERVAL,
};
use crate::events::AppEvent;
use crate::workspace::{GitStatusCacheEntry, Workspace, WorkspaceGitStatus};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceGitRefreshItem {
    pub(crate) workspace_id: String,
    pub(crate) resolved_identity_cwd: std::path::PathBuf,
    pub(crate) cache_key: std::path::PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceGitRefreshTarget {
    pub(crate) workspace_id: String,
    pub(crate) resolved_identity_cwd: std::path::PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceGitRefreshJob {
    pub(crate) cache_key: std::path::PathBuf,
    pub(crate) status_cwd: std::path::PathBuf,
    pub(crate) cached: Option<GitStatusCacheEntry>,
    pub(crate) targets: Vec<WorkspaceGitRefreshTarget>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkspaceGitRefreshOutput {
    pub(crate) results: Vec<WorkspaceGitStatus>,
    pub(crate) cache_updates: Vec<(std::path::PathBuf, GitStatusCacheEntry)>,
}

fn wait_lease_error(
    request_id: String,
    code: impl Into<String>,
    message: impl Into<String>,
) -> (crate::terminal::WaitLeaseResponse, bool) {
    (
        crate::terminal::WaitLeaseResponse::error(request_id, code, message),
        false,
    )
}

fn retain_custom_command_after_wait(
    pid: u32,
    result: std::io::Result<Option<std::process::ExitStatus>>,
) -> bool {
    match result {
        Ok(None) => true,
        Ok(Some(_)) => false,
        Err(err) if err.kind() == std::io::ErrorKind::Interrupted => true,
        Err(err) => {
            tracing::warn!(pid, err = %err, "failed to reap detached custom command");
            false
        }
    }
}

impl App {
    pub(crate) fn sync_wait_lease_requests(&mut self) -> bool {
        let pending_requests = match crate::terminal::pending_wait_lease_requests() {
            Ok(pending_requests) => pending_requests,
            Err(error) => {
                tracing::warn!(err = %error, "failed to poll wait lease requests");
                return false;
            }
        };
        let mut changed = false;
        for pending in pending_requests {
            let (response, request_changed) = self.apply_wait_lease_request(&pending.request);
            changed |= request_changed;
            let request = pending.request.clone();
            if let Err(error) = crate::terminal::complete_wait_lease_request(pending, &response) {
                tracing::warn!(err = %error, "failed to complete wait lease request");
                if request_changed {
                    changed |= self.rollback_unacknowledged_wait_lease_acquire(&request);
                }
            }
        }
        changed
    }

    fn apply_wait_lease_request(
        &mut self,
        request: &crate::terminal::WaitLeaseRequest,
    ) -> (crate::terminal::WaitLeaseResponse, bool) {
        if request.version != 1 {
            return (
                crate::terminal::WaitLeaseResponse::error(
                    request.request_id.clone(),
                    "unsupported_wait_lease_version",
                    "wait lease request version is not supported",
                ),
                false,
            );
        }
        match &request.operation {
            crate::terminal::WaitLeaseOperation::Acquire {
                pane_id,
                terminal_id,
                terminal_generation,
                job_id,
                ttl_ms,
                token,
                requested_at_ms,
            } => self.apply_wait_lease_acquire(
                request.request_id.clone(),
                pane_id,
                terminal_id,
                *terminal_generation,
                job_id,
                *ttl_ms,
                token,
                *requested_at_ms,
            ),
            crate::terminal::WaitLeaseOperation::Release {
                pane_id,
                terminal_id,
                terminal_generation,
                job_id,
                token,
            } => self.apply_wait_lease_release(
                request.request_id.clone(),
                pane_id,
                terminal_id,
                *terminal_generation,
                job_id,
                token,
            ),
        }
    }

    #[allow(clippy::too_many_arguments)] // Mirrors the versioned on-disk acquire record.
    fn apply_wait_lease_acquire(
        &mut self,
        request_id: String,
        public_pane_id: &str,
        expected_terminal_id: &str,
        expected_terminal_generation: u64,
        job_id: &str,
        ttl_ms: u64,
        token: &str,
        requested_at_ms: u64,
    ) -> (crate::terminal::WaitLeaseResponse, bool) {
        let Some((workspace_index, pane_id)) = self.parse_pane_id(public_pane_id) else {
            return wait_lease_error(request_id, "pane_not_found", "pane was not found");
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(workspace_index)
            .and_then(|workspace| workspace.pane_state(pane_id))
            .map(|pane| pane.attached_terminal_id.clone())
        else {
            return wait_lease_error(request_id, "pane_not_found", "pane was not found");
        };
        if terminal_id.to_string() != expected_terminal_id {
            return wait_lease_error(
                request_id,
                "wait_lease_pane_generation_changed",
                "pane generation changed before the lease was acquired",
            );
        }
        let normalized_job_id = job_id.trim();
        if normalized_job_id.is_empty() || normalized_job_id.len() > 128 {
            return wait_lease_error(
                request_id,
                "invalid_wait_lease_job_id",
                "wait lease job id must be between 1 and 128 characters",
            );
        }
        if ttl_ms == 0 || ttl_ms > crate::terminal::MAX_WAIT_LEASE_TTL_MS {
            return wait_lease_error(
                request_id,
                "invalid_wait_lease_ttl",
                format!(
                    "wait lease ttl must be between 1 and {} milliseconds",
                    crate::terminal::MAX_WAIT_LEASE_TTL_MS
                ),
            );
        }
        if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return wait_lease_error(
                request_id,
                "invalid_wait_lease_token",
                "wait lease token is invalid",
            );
        }
        let now_ms = match crate::platform::continuous_clock_ms() {
            Ok(now_ms) => now_ms,
            Err(error) => {
                return wait_lease_error(
                    request_id,
                    "wait_lease_clock_unavailable",
                    error.to_string(),
                )
            }
        };
        if requested_at_ms > now_ms {
            return wait_lease_error(
                request_id,
                "wait_lease_clock_unverifiable",
                "wait lease request clock is ahead of the server clock",
            );
        }
        let Some(expires_at_ms) = requested_at_ms.checked_add(ttl_ms) else {
            return wait_lease_error(
                request_id,
                "invalid_wait_lease_ttl",
                "wait lease expiry overflowed the continuous clock",
            );
        };
        if now_ms >= expires_at_ms {
            return wait_lease_error(
                request_id,
                "wait_lease_request_expired",
                "wait lease request expired before the server processed it",
            );
        }

        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            return wait_lease_error(request_id, "pane_not_found", "pane was not found");
        };
        if terminal.runtime_generation != expected_terminal_generation {
            return wait_lease_error(
                request_id,
                "wait_lease_pane_generation_changed",
                "pane generation changed before the lease was acquired",
            );
        }
        if terminal
            .wait_lease_revocations
            .acquire_guard_saturated(now_ms)
        {
            return wait_lease_error(
                request_id,
                "wait_lease_replay_guard_saturated",
                "wait lease replay protection is saturated until prior revocations expire",
            );
        }
        if terminal
            .wait_lease_revocations
            .token_is_revoked(token, now_ms)
        {
            return wait_lease_error(
                request_id,
                "wait_lease_token_revoked",
                "wait lease token was already released",
            );
        }
        if let Some(existing) = terminal.wait_lease.as_ref() {
            if existing.is_active_at(now_ms) {
                if existing.token_matches(token) && existing.job_id() == normalized_job_id {
                    return (
                        crate::terminal::WaitLeaseResponse::acquired(
                            request_id,
                            normalized_job_id.to_string(),
                            existing.expires_at_ms().saturating_sub(now_ms),
                            token.to_string(),
                        ),
                        false,
                    );
                }
                return wait_lease_error(
                    request_id,
                    "wait_lease_active",
                    "pane already has an active wait lease",
                );
            }
            terminal.wait_lease = None;
        }

        terminal.wait_lease = Some(crate::terminal::WaitLease::new(
            normalized_job_id.to_string(),
            token.to_string(),
            expires_at_ms,
        ));
        terminal.revision = terminal.revision.wrapping_add(1);
        self.emit_pane_updated(workspace_index, pane_id);
        (
            crate::terminal::WaitLeaseResponse::acquired(
                request_id,
                normalized_job_id.to_string(),
                expires_at_ms.saturating_sub(now_ms),
                token.to_string(),
            ),
            true,
        )
    }

    fn apply_wait_lease_release(
        &mut self,
        request_id: String,
        public_pane_id: &str,
        expected_terminal_id: &str,
        expected_terminal_generation: u64,
        job_id: &str,
        token: &str,
    ) -> (crate::terminal::WaitLeaseResponse, bool) {
        let Some((workspace_index, pane_id)) = self.parse_pane_id(public_pane_id) else {
            return wait_lease_error(request_id, "pane_not_found", "pane was not found");
        };
        let Some(terminal_id) = self
            .state
            .workspaces
            .get(workspace_index)
            .and_then(|workspace| workspace.pane_state(pane_id))
            .map(|pane| pane.attached_terminal_id.clone())
        else {
            return wait_lease_error(request_id, "pane_not_found", "pane was not found");
        };
        if terminal_id.to_string() != expected_terminal_id {
            return wait_lease_error(
                request_id,
                "wait_lease_pane_generation_changed",
                "pane generation changed before the lease was released",
            );
        }
        let now_ms = match crate::platform::continuous_clock_ms() {
            Ok(now_ms) => now_ms,
            Err(error) => {
                return wait_lease_error(
                    request_id,
                    "wait_lease_clock_unavailable",
                    error.to_string(),
                )
            }
        };
        let Some(terminal) = self.state.terminals.get_mut(&terminal_id) else {
            return wait_lease_error(request_id, "pane_not_found", "pane was not found");
        };
        if terminal.runtime_generation != expected_terminal_generation {
            return wait_lease_error(
                request_id,
                "wait_lease_pane_generation_changed",
                "pane generation changed before the lease was released",
            );
        }
        let normalized_job_id = job_id.trim();
        if normalized_job_id.is_empty() || normalized_job_id.len() > 128 {
            return wait_lease_error(
                request_id,
                "invalid_wait_lease_job_id",
                "wait lease job id must be between 1 and 128 characters",
            );
        }
        if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return wait_lease_error(
                request_id,
                "invalid_wait_lease_token",
                "wait lease token is invalid",
            );
        }
        if let Some(released) = terminal
            .wait_lease_revocations
            .prior_release_result(token, now_ms)
        {
            return (
                crate::terminal::WaitLeaseResponse::released(request_id, released),
                false,
            );
        }
        let Some(lease) = terminal.wait_lease.as_ref() else {
            terminal.wait_lease_revocations.record(
                token.to_string(),
                now_ms.saturating_add(crate::terminal::MAX_WAIT_LEASE_TTL_MS),
                false,
                now_ms,
            );
            return (
                crate::terminal::WaitLeaseResponse::released(request_id, false),
                false,
            );
        };
        if lease.job_id() != normalized_job_id || !lease.token_matches(token) {
            return wait_lease_error(
                request_id,
                "wait_lease_token_mismatch",
                "wait lease owner token or job id does not match",
            );
        }
        let released = lease.is_active_at(now_ms);
        terminal.wait_lease = None;
        terminal.wait_lease_revocations.record(
            token.to_string(),
            now_ms.saturating_add(crate::terminal::MAX_WAIT_LEASE_TTL_MS),
            released,
            now_ms,
        );
        terminal.revision = terminal.revision.wrapping_add(1);
        // Releasing the lease can drop background-work to false while the pane is already
        // Idle — the falling edge of a completion that was suppressed while the lease was
        // outstanding. Route through the same recompute+apply path as detection updates so
        // that owed completion (seen flip, toast, sound) fires instead of being silently lost.
        if let Some(update) =
            self.state
                .recompute_and_apply_effective_state(workspace_index, pane_id, Instant::now())
        {
            self.emit_pane_state_update(&update);
        }
        self.emit_pane_updated(workspace_index, pane_id);
        (
            crate::terminal::WaitLeaseResponse::released(request_id, released),
            true,
        )
    }

    pub(crate) fn expire_wait_leases(&mut self) -> bool {
        let Ok(now_ms) = crate::platform::continuous_clock_ms() else {
            return false;
        };
        let mut expired = Vec::new();
        for (workspace_index, workspace) in self.state.workspaces.iter().enumerate() {
            for tab in &workspace.tabs {
                for (&pane_id, pane) in &tab.panes {
                    let Some(terminal) = self.state.terminals.get(&pane.attached_terminal_id)
                    else {
                        continue;
                    };
                    if terminal
                        .wait_lease
                        .as_ref()
                        .is_some_and(|lease| !lease.is_active_at(now_ms))
                    {
                        expired.push((workspace_index, pane_id, pane.attached_terminal_id.clone()));
                    }
                }
            }
        }
        for (workspace_index, pane_id, terminal_id) in &expired {
            if let Some(terminal) = self.state.terminals.get_mut(terminal_id) {
                terminal.wait_lease = None;
                terminal.revision = terminal.revision.wrapping_add(1);
            }
            // Same falling-edge concern as `apply_wait_lease_release`: expiry can drop
            // background-work to false while the pane is already Idle, and an owed completion
            // must fire for it rather than only bumping revision.
            if let Some(update) = self.state.recompute_and_apply_effective_state(
                *workspace_index,
                *pane_id,
                Instant::now(),
            ) {
                self.emit_pane_state_update(&update);
            }
            self.emit_pane_updated(*workspace_index, *pane_id);
        }
        !expired.is_empty()
    }

    fn rollback_unacknowledged_wait_lease_acquire(
        &mut self,
        request: &crate::terminal::WaitLeaseRequest,
    ) -> bool {
        let crate::terminal::WaitLeaseOperation::Acquire {
            pane_id,
            terminal_id,
            terminal_generation,
            token,
            ..
        } = &request.operation
        else {
            return false;
        };
        let Some((workspace_index, internal_pane_id)) = self.parse_pane_id(pane_id) else {
            return false;
        };
        let Some(attached_terminal_id) = self
            .state
            .workspaces
            .get(workspace_index)
            .and_then(|workspace| workspace.pane_state(internal_pane_id))
            .map(|pane| pane.attached_terminal_id.clone())
        else {
            return false;
        };
        if attached_terminal_id.to_string() != *terminal_id {
            return false;
        }
        let Ok(now_ms) = crate::platform::continuous_clock_ms() else {
            return false;
        };
        let Some(terminal) = self.state.terminals.get_mut(&attached_terminal_id) else {
            return false;
        };
        if terminal.runtime_generation != *terminal_generation {
            return false;
        }
        if !terminal
            .wait_lease
            .as_ref()
            .is_some_and(|lease| lease.token_matches(token))
        {
            return false;
        }
        terminal.wait_lease = None;
        terminal.wait_lease_revocations.record(
            token.clone(),
            now_ms.saturating_add(crate::terminal::MAX_WAIT_LEASE_TTL_MS),
            false,
            now_ms,
        );
        terminal.revision = terminal.revision.wrapping_add(1);
        self.emit_pane_updated(workspace_index, internal_pane_id);
        true
    }

    pub(crate) fn reap_finished_custom_commands(&mut self) {
        self.detached_custom_command_children
            .retain_mut(|child| retain_custom_command_after_wait(child.id(), child.try_wait()));
    }

    pub(crate) fn shutdown_detached_terminal_runtimes(&mut self) {
        let terminal_ids = std::mem::take(&mut self.state.terminal_runtime_shutdowns);
        for terminal_id in terminal_ids {
            if let Some(runtime) = self.terminal_runtimes.remove(&terminal_id) {
                runtime.shutdown();
            }
        }
    }

    pub(crate) fn drain_api_requests(&mut self) -> bool {
        let mut changed = false;
        while let Ok(msg) = self.api_rx.try_recv() {
            changed |= self.handle_api_request_message(msg);
            self.shutdown_detached_terminal_runtimes();
        }
        changed
    }

    pub(super) fn handle_api_request_message(
        &mut self,
        msg: crate::api::ApiRequestMessage,
    ) -> bool {
        let previous_mode = self.state.mode;
        let mut changed = self.expire_due_metadata(Instant::now());
        changed |= crate::api::request_changes_ui(&msg.request);
        let skip_default_workspace = matches!(
            &msg.request.method,
            crate::api::schema::Method::ServerStop(_)
                | crate::api::schema::Method::ServerLiveHandoff(_)
        );
        if matches!(
            &msg.request.method,
            crate::api::schema::Method::WorktreeCreate(_)
                | crate::api::schema::Method::WorktreeRemove(_)
        ) {
            self.drain_all_internal_events();
            let deferred_changed =
                self.handle_deferred_worktree_api_request(msg.request, msg.respond_to);
            if !skip_default_workspace {
                changed |= self.ensure_default_workspace();
            }
            self.sync_prefix_input_source(previous_mode);
            return changed | deferred_changed;
        }
        let response = self.handle_api_request(msg.request);
        if !skip_default_workspace {
            changed |= self.ensure_default_workspace();
        }
        let _ = msg.respond_to.send(response);
        self.sync_prefix_input_source(previous_mode);
        changed
    }

    pub(super) async fn handle_raw_input_batch(
        &mut self,
        first: crate::raw_input::RawInputEvent,
    ) -> bool {
        let mut changed = self.handle_raw_input_event(first).await;

        while let Some(rx) = self.input_rx.as_mut() {
            match rx.try_recv() {
                Ok(event) => changed |= self.handle_raw_input_event(event).await,
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                    self.input_rx = None;
                    break;
                }
            }
        }

        changed
    }

    pub(super) async fn handle_raw_input_event(
        &mut self,
        event: crate::raw_input::RawInputEvent,
    ) -> bool {
        let previous_mode = self.state.mode;
        let changed = match event {
            crate::raw_input::RawInputEvent::Key(key) => {
                let key_id = repeat_key_identity(&key);
                match key.kind {
                    crossterm::event::KeyEventKind::Press => {
                        if self.state.popup_pane.is_some() || self.state.mode == Mode::Terminal {
                            self.suppressed_repeat_keys.remove(&key_id);
                        } else {
                            self.suppressed_repeat_keys.insert(key_id);
                        }
                        self.handle_key(key).await;
                        true
                    }
                    crossterm::event::KeyEventKind::Repeat => {
                        if (self.state.popup_pane.is_some() || self.state.mode == Mode::Terminal)
                            && !self.suppressed_repeat_keys.contains(&key_id)
                        {
                            self.handle_key(key).await;
                            true
                        } else {
                            false
                        }
                    }
                    crossterm::event::KeyEventKind::Release => {
                        self.suppressed_repeat_keys.remove(&key_id);
                        false
                    }
                }
            }
            crate::raw_input::RawInputEvent::Paste(text) => {
                self.handle_paste(text).await;
                true
            }
            crate::raw_input::RawInputEvent::Mouse(mouse) => {
                if self.state.popup_pane.is_some() || self.state.mouse_capture {
                    self.handle_mouse(mouse);
                } else {
                    self.state
                        .handle_pane_mouse_only(&self.terminal_runtimes, mouse);
                }
                true
            }
            crate::raw_input::RawInputEvent::OuterFocusGained => {
                self.send_outer_focus_event(crate::ghostty::FocusEvent::Gained);
                if self.state.redraw_on_focus_gained {
                    self.request_full_redraw();
                }
                self.state.outer_terminal_focus = Some(true);
                self.state.mark_active_tab_seen();
                true
            }
            crate::raw_input::RawInputEvent::OuterFocusLost => {
                self.send_outer_focus_event(crate::ghostty::FocusEvent::Lost);
                self.state.outer_terminal_focus = Some(false);
                false
            }
            crate::raw_input::RawInputEvent::HostDefaultColor { kind, color } => {
                self.update_host_terminal_theme(kind, color)
            }
            crate::raw_input::RawInputEvent::HostColorSchemeChanged(appearance) => {
                self.query_host_terminal_theme();
                self.set_host_terminal_appearance(appearance, true)
            }
            crate::raw_input::RawInputEvent::Unsupported => false,
        };
        self.sync_prefix_input_source(previous_mode);
        self.shutdown_detached_terminal_runtimes();
        changed
    }

    fn handle_resize_poll(&mut self) -> bool {
        let Ok(size) = terminal::size() else {
            return false;
        };
        if self.last_terminal_size != Some(size) {
            self.last_terminal_size = Some(size);
            return true;
        }
        false
    }

    pub(crate) fn handle_scheduled_tasks(&mut self, now: Instant, geometry_dirty: bool) -> bool {
        let mut changed = false;
        let mut resized = false;

        self.sync_animation_timer(now);

        if now >= self.next_wait_lease_poll {
            changed |= self.sync_wait_lease_requests();
            changed |= self.expire_wait_leases();
            self.next_wait_lease_poll = now + crate::terminal::WAIT_LEASE_POLL_INTERVAL;
        }

        if now >= self.next_resize_poll {
            resized = self.handle_resize_poll();
            changed |= resized;
            self.next_resize_poll = now + RESIZE_POLL_INTERVAL;
        }

        if self
            .config_diagnostic_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.config_diagnostic_deadline = None;
            self.state.config_diagnostic = None;
            changed = true;
        }

        if self.toast_deadline.is_some_and(|deadline| now >= deadline) {
            self.toast_deadline = None;
            self.state.toast = None;
            changed = true;
        }

        if self
            .state
            .next_pending_agent_notification_deadline()
            .is_some_and(|deadline| now >= deadline)
        {
            let previous_toast = self.state.toast.clone();
            let mut deliveries = self.state.drain_due_agent_notifications(now);
            if !deliveries.is_empty() {
                self.refresh_agent_notification_delivery_contexts(&mut deliveries);
                self.emit_delayed_client_local_agent_notifications(&deliveries);
                self.sync_toast_deadline(previous_toast);
                changed = true;
            }
        }

        if self
            .state
            .next_managed_agent_deadline()
            .is_some_and(|deadline| now >= deadline)
        {
            let panes = self.state.reconcile_managed_agents_at(now);
            if !panes.is_empty() {
                for (ws_idx, pane_id) in panes {
                    self.emit_pane_updated(ws_idx, pane_id);
                }
                self.schedule_session_save();
                changed = true;
            }
        }

        if self
            .copy_feedback_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.copy_feedback_deadline = None;
            self.state.copy_feedback = None;
            changed = true;
        }

        if self
            .next_animation_tick
            .is_some_and(|deadline| now >= deadline)
        {
            self.state.spinner_tick = self.state.spinner_tick.wrapping_add(1);
            self.next_animation_tick = Some(now + ANIMATION_INTERVAL);
            changed = true;
        }

        if self
            .selection_autoscroll_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.tick_selection_autoscroll(now);
            changed = true;
        }

        changed |= self.clear_due_selection_highlight(now);

        self.start_git_status_refresh_if_due(now);

        if self
            .next_auto_update_check
            .is_some_and(|deadline| now >= deadline)
        {
            self.run_auto_update_check();
        }

        if self
            .next_agent_manifest_update_check
            .is_some_and(|deadline| now >= deadline)
        {
            self.run_agent_manifest_update_check();
        }

        if self
            .session_save_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.start_background_session_save();
        }

        changed |= self.expire_due_metadata(now);

        if geometry_dirty || resized {
            self.pending_agent_resume_deadline = None;
        } else {
            self.sync_pending_agent_resume_deadline(now);
            changed |= self.start_pending_agent_resumes(self.pending_agent_resume_due(now));
        }
        self.sync_animation_timer(now);
        changed
    }

    /// Clears temporary copied-token highlights, such as after double-click copy.
    pub(crate) fn clear_due_selection_highlight(&mut self, now: Instant) -> bool {
        if self
            .selection_highlight_clear_deadline
            .is_none_or(|deadline| now < deadline)
        {
            return false;
        }

        self.selection_highlight_clear_deadline = None;
        if self
            .state
            .selection
            .as_ref()
            .is_some_and(|selection| !selection.is_in_progress())
        {
            self.state.clear_selection();
            return true;
        }
        false
    }

    pub(crate) fn sync_agent_metadata_deadline(&mut self) {
        self.agent_metadata_deadline = self.state.next_agent_metadata_expiry();
    }

    pub(crate) fn expire_due_metadata(&mut self, now: Instant) -> bool {
        let Some(deadline) = self
            .agent_metadata_deadline
            .filter(|deadline| now >= *deadline)
        else {
            return false;
        };
        self.expire_metadata_at(deadline, now);
        true
    }

    pub(crate) fn expire_metadata_at(&mut self, deadline: Instant, now: Instant) {
        let previous_toast = self.state.toast.clone();
        for update in self.state.expire_agent_metadata_at(deadline, now) {
            self.refresh_new_herdr_toast_context_for_update(&update, &previous_toast);
            self.emit_pane_state_update(&update);
        }
        let (panes, workspaces) = self.state.expire_metadata_tokens(now);
        for (ws_idx, pane_id) in panes {
            self.emit_pane_updated(ws_idx, pane_id);
        }
        for ws_idx in workspaces {
            self.emit_workspace_token_updated(ws_idx);
        }
        self.sync_agent_metadata_deadline();
    }

    pub(crate) fn sync_animation_timer(&mut self, now: Instant) {
        self.sync_animation_timer_with_interval(now, ANIMATION_INTERVAL);
    }

    pub(crate) fn sync_headless_animation_timer(&mut self, now: Instant) {
        self.sync_animation_timer_with_interval(now, crate::app::HEADLESS_ANIMATION_INTERVAL);
    }

    fn sync_animation_timer_with_interval(&mut self, now: Instant, interval: Duration) {
        if self.agent_panel_has_animation() {
            self.next_animation_tick.get_or_insert(now + interval);
        } else {
            self.next_animation_tick = None;
        }
    }

    fn agent_panel_has_animation(&self) -> bool {
        self.state
            .workspaces
            .iter()
            .any(|ws| ws.has_working_pane(&self.state.terminals))
    }

    pub(crate) fn tick_selection_autoscroll(&mut self, now: Instant) {
        let Some(autoscroll) = self.state.selection_autoscroll.clone() else {
            // Self-heal: state cleared but deadline leaked
            self.selection_autoscroll_deadline = None;
            return;
        };

        // Selection must still be in progress for autoscroll to continue
        let Some(pane_id) = self.state.selection.as_ref().map(|s| s.pane_id) else {
            self.stop_selection_autoscroll();
            return;
        };
        if !self
            .state
            .selection
            .as_ref()
            .is_some_and(|s| s.is_dragging())
        {
            self.stop_selection_autoscroll();
            return;
        }

        // Rect-change detection: if inner_rect changed since drag, stop
        let current_rect = self
            .state
            .pane_info_by_id(pane_id)
            .map(|info| info.inner_rect);
        if current_rect != Some(autoscroll.inner_rect) {
            self.stop_selection_autoscroll();
            return;
        }

        // Scrollback boundary detection via ScrollMetrics — fail-closed if unavailable
        let Some(metrics) = self
            .state
            .pane_scroll_metrics(&self.terminal_runtimes, pane_id)
        else {
            self.stop_selection_autoscroll();
            return;
        };
        match autoscroll.direction {
            crate::app::state::SelectionAutoscrollDirection::Up => {
                let at_top = metrics.offset_from_bottom >= metrics.max_offset_from_bottom;
                if at_top {
                    self.stop_selection_autoscroll();
                    return;
                }
                self.state
                    .scroll_pane_up(&self.terminal_runtimes, pane_id, 1);
            }
            crate::app::state::SelectionAutoscrollDirection::Down => {
                let at_bottom = metrics.offset_from_bottom == 0;
                if at_bottom {
                    self.stop_selection_autoscroll();
                    return;
                }
                self.state
                    .scroll_pane_down(&self.terminal_runtimes, pane_id, 1);
            }
        }

        // Extend selection cursor to last known mouse position
        self.state.update_selection_cursor(
            &self.terminal_runtimes,
            pane_id,
            autoscroll.last_mouse_screen_col,
            autoscroll.last_mouse_screen_row,
        );

        // Reschedule
        self.selection_autoscroll_deadline = Some(now + SELECTION_AUTOSCROLL_INTERVAL);
    }

    pub(crate) fn stop_selection_autoscroll(&mut self) {
        self.state.stop_selection_autoscroll_state();
        self.selection_autoscroll_deadline = None;
    }

    pub(crate) fn can_render_now(&self, now: Instant) -> bool {
        match self.last_render_at {
            Some(last_render_at) => now.duration_since(last_render_at) >= MIN_RENDER_INTERVAL,
            None => true,
        }
    }

    pub(crate) fn run_auto_update_check(&mut self) {
        if !background_update_check_enabled(self.no_session, self.update_version_check_enabled) {
            self.next_auto_update_check = None;
            return;
        }

        self.next_auto_update_check = self
            .state
            .update_available
            .is_none()
            .then_some(Instant::now() + AUTO_UPDATE_CHECK_INTERVAL);

        if self.state.update_available.is_some() {
            return;
        }

        let update_tx = self.event_tx.clone();
        std::thread::spawn(move || crate::update::auto_update(update_tx));
    }

    pub(crate) fn run_agent_manifest_update_check(&mut self) {
        if !background_update_check_enabled(self.no_session, self.update_manifest_check_enabled) {
            self.next_agent_manifest_update_check = None;
            return;
        }

        self.next_agent_manifest_update_check = Some(Instant::now() + AUTO_UPDATE_CHECK_INTERVAL);

        let manifest_update_tx = self.event_tx.clone();
        std::thread::spawn(move || crate::detect::manifest_update::auto_update(manifest_update_tx));
    }

    pub(crate) fn start_git_status_refresh_if_due(&mut self, now: Instant) {
        let Some(deadline) = self.git_refresh_deadline() else {
            return;
        };

        if now < deadline {
            return;
        }

        let workspaces = self.workspace_git_refresh_items();

        if workspaces.is_empty() {
            self.last_git_remote_status_refresh = now;
            return;
        }

        self.git_refresh_in_flight = true;
        let event_tx = self.event_tx.clone();
        let cache = self.git_status_cache.clone();
        std::thread::spawn(move || {
            let output = refresh_workspace_git_statuses_with_cache(workspaces, &cache);
            let _ = event_tx.blocking_send(AppEvent::GitStatusRefreshed {
                results: output.results,
                cache_updates: output.cache_updates,
            });
        });
    }

    pub(crate) fn mark_git_status_refresh_due(&mut self, now: Instant) {
        if self.git_refresh_in_flight {
            self.git_refresh_due_after_in_flight = true;
            return;
        }
        self.last_git_remote_status_refresh = now
            .checked_sub(GIT_REMOTE_STATUS_REFRESH_INTERVAL)
            .unwrap_or(now);
        self.git_refresh_due_after_in_flight = false;
    }

    pub(crate) fn git_refresh_deadline(&self) -> Option<Instant> {
        (!self.git_refresh_in_flight && !self.state.workspaces.is_empty())
            .then_some(self.last_git_remote_status_refresh + GIT_REMOTE_STATUS_REFRESH_INTERVAL)
    }

    pub(crate) fn next_loop_deadline(&self, now: Instant, needs_render: bool) -> Option<Instant> {
        self.next_loop_deadline_with_resize_poll(now, needs_render, true, true)
    }

    pub(crate) fn next_headless_loop_deadline_with_git_refresh(
        &self,
        now: Instant,
        needs_render: bool,
        include_git_refresh: bool,
    ) -> Option<Instant> {
        self.next_loop_deadline_with_resize_poll(now, needs_render, false, include_git_refresh)
    }

    fn next_loop_deadline_with_resize_poll(
        &self,
        now: Instant,
        needs_render: bool,
        include_resize_poll: bool,
        include_git_refresh: bool,
    ) -> Option<Instant> {
        let render_deadline = if needs_render {
            self.last_render_at
                .map(|last_render_at| last_render_at + MIN_RENDER_INTERVAL)
                .filter(|deadline| *deadline > now)
        } else {
            None
        };

        [
            include_resize_poll.then_some(self.next_resize_poll),
            Some(self.next_wait_lease_poll),
            self.config_diagnostic_deadline,
            self.toast_deadline,
            self.state.next_pending_agent_notification_deadline(),
            self.state.next_managed_agent_deadline(),
            self.copy_feedback_deadline,
            self.next_animation_tick,
            include_git_refresh
                .then(|| self.git_refresh_deadline())
                .flatten(),
            self.next_auto_update_check,
            self.next_agent_manifest_update_check,
            self.agent_metadata_deadline,
            self.pending_agent_resume_deadline,
            self.session_save_deadline,
            self.selection_autoscroll_deadline,
            self.selection_highlight_clear_deadline,
            render_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    fn workspace_git_refresh_items(&self) -> Vec<WorkspaceGitRefreshItem> {
        self.state
            .workspaces
            .iter()
            .filter_map(|ws| {
                let cwd =
                    ws.resolved_identity_cwd_from(&self.state.terminals, &self.terminal_runtimes)?;
                let git_key = crate::workspace::git_status_cache_key(&cwd);
                let cache_key = git_key.unwrap_or_else(|| cwd.clone());
                Some(WorkspaceGitRefreshItem {
                    workspace_id: ws.id.clone(),
                    resolved_identity_cwd: cwd,
                    cache_key,
                })
            })
            .collect()
    }

    pub(crate) fn drain_internal_events(&mut self) -> bool {
        self.drain_internal_events_up_to(super::APP_EVENT_DRAIN_LIMIT)
    }

    pub(crate) fn drain_all_internal_events(&mut self) -> bool {
        let mut had_event = false;
        while self.drain_internal_events_up_to(super::APP_EVENT_DRAIN_LIMIT) {
            had_event = true;
        }
        had_event
    }

    fn drain_internal_events_up_to(&mut self, limit: usize) -> bool {
        let mut had_event = false;
        for _ in 0..limit {
            let Ok(ev) = self.event_rx.try_recv() else {
                break;
            };
            had_event = true;
            self.handle_internal_event_with_prefix_sync(ev);
        }
        had_event
    }
}

pub(crate) fn deduplicate_git_refresh_items(
    items: Vec<WorkspaceGitRefreshItem>,
    cache: &HashMap<std::path::PathBuf, GitStatusCacheEntry>,
) -> Vec<WorkspaceGitRefreshJob> {
    let mut indexes = HashMap::<std::path::PathBuf, usize>::new();
    let mut jobs = Vec::<WorkspaceGitRefreshJob>::new();

    for item in items {
        let target = WorkspaceGitRefreshTarget {
            workspace_id: item.workspace_id,
            resolved_identity_cwd: item.resolved_identity_cwd.clone(),
        };
        if let Some(&index) = indexes.get(&item.cache_key) {
            jobs[index].targets.push(target);
            continue;
        }

        let status_cwd = item.cache_key.clone();
        let cached = cache.get(&item.cache_key).cloned();
        indexes.insert(item.cache_key, jobs.len());
        jobs.push(WorkspaceGitRefreshJob {
            cache_key: status_cwd.clone(),
            status_cwd,
            cached,
            targets: vec![target],
        });
    }

    jobs
}

pub(crate) fn refresh_workspace_git_statuses_with_cache(
    items: Vec<WorkspaceGitRefreshItem>,
    cache: &HashMap<std::path::PathBuf, GitStatusCacheEntry>,
) -> WorkspaceGitRefreshOutput {
    let mut results = Vec::new();
    let mut cache_updates = Vec::new();

    for job in deduplicate_git_refresh_items(items, cache) {
        let (snapshot, cache_entry) =
            Workspace::git_status_snapshot_for_cwd_with_cache(&job.status_cwd, job.cached.as_ref());
        if let Some(cache_entry) = cache_entry {
            cache_updates.push((job.cache_key.clone(), cache_entry));
        }
        results.extend(job.targets.into_iter().map(move |target| {
            snapshot
                .clone()
                .into_workspace_status(target.workspace_id, target.resolved_identity_cwd)
        }));
    }

    WorkspaceGitRefreshOutput {
        results,
        cache_updates,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::state;
    use crate::workspace::Workspace;
    use std::path::PathBuf;

    #[test]
    fn interrupted_custom_command_wait_keeps_child_for_retry() {
        let interrupted = std::io::Error::new(std::io::ErrorKind::Interrupted, "test interrupt");

        assert!(retain_custom_command_after_wait(42, Err(interrupted)));
    }

    fn test_app_with_pane() -> (super::super::App, crate::layout::PaneId) {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        let ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        app.state.workspaces.push(ws);
        app.state.active = Some(0);
        app.state.view.pane_infos.push(crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, 80, 24),
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::NONE,
            is_focused: true,
        });
        (app, pane_id)
    }

    fn wait_lease_test_app() -> (super::super::App, String, String) {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        let workspace = Workspace::test_new("wait-lease-test");
        let pane_id = workspace.tabs[0].root_pane;
        app.state.workspaces.push(workspace);
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let public_pane_id = app.public_pane_id(0, pane_id).unwrap();
        let terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .unwrap()
            .attached_terminal_id
            .to_string();
        (app, public_pane_id, terminal_id)
    }

    /// Like `wait_lease_test_app`, but with a second, non-active workspace holding the pane
    /// under test. `apply_pane_state_change`'s active-tab quiet rule suppresses the seen flip
    /// and toast for the active tab's own pane, which would mask the deferred-completion
    /// assertions below; a background-workspace pane isn't subject to that suppression.
    fn wait_lease_release_test_app() -> (super::super::App, String, String, crate::layout::PaneId) {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        app.state.toast_config.delivery = crate::config::ToastDelivery::Herdr;
        app.state.toast_config.delay_seconds = 0;
        app.state.workspaces.push(Workspace::test_new("active"));
        let bg_workspace = Workspace::test_new("background");
        let bg_pane_id = bg_workspace.tabs[0].root_pane;
        app.state.workspaces.push(bg_workspace);
        app.state.ensure_test_terminals();
        app.state.active = Some(0);
        let public_pane_id = app.public_pane_id(1, bg_pane_id).unwrap();
        let terminal_id = app.state.workspaces[1]
            .pane_state(bg_pane_id)
            .unwrap()
            .attached_terminal_id
            .to_string();
        (app, public_pane_id, terminal_id, bg_pane_id)
    }

    #[test]
    fn wait_lease_release_fires_deferred_completion_through_wire_protocol() {
        let (mut app, public_pane_id, terminal_id, pane_id) = wait_lease_release_test_app();
        let attached_terminal_id = app.state.workspaces[1]
            .pane_state(pane_id)
            .unwrap()
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&attached_terminal_id)
            .unwrap()
            .state = crate::detect::AgentState::Working;
        let now_ms = crate::platform::continuous_clock_ms().unwrap();
        let token = "9".repeat(64);

        let (_, changed) = app.apply_wait_lease_request(&acquire_request(
            &public_pane_id,
            &terminal_id,
            &token,
            now_ms,
        ));
        assert!(changed);

        // Working->Idle while the lease is held: the completion is suppressed, not lost —
        // owed until the lease clears.
        app.state.handle_app_event(AppEvent::StateChanged {
            pane_id,
            agent: Some(crate::detect::Agent::Pi),
            state: crate::detect::AgentState::Idle,
            visible_blocker: false,
            visible_working: false,
            background_work: false,
            process_exited: false,
            observed_at: Instant::now(),
        });
        assert!(
            app.state.workspaces[1].panes.get(&pane_id).unwrap().seen,
            "must be suppressed while the lease is held"
        );

        let (released, changed) = app.apply_wait_lease_request(&release_request(
            &public_pane_id,
            &terminal_id,
            "background-review",
            &token,
        ));
        assert!(changed);
        assert!(matches!(
            released.result,
            crate::terminal::WaitLeaseResponseResult::Released { released: true }
        ));

        // This is the bug this fix closes: releasing the lease through the same request path
        // agents use must fire the completion that was owed, not just clear the lease bit.
        assert!(
            !app.state.workspaces[1].panes.get(&pane_id).unwrap().seen,
            "releasing the lease through the wire protocol must fire the owed completion"
        );
        assert!(
            matches!(
                app.state.toast.as_ref().map(|toast| toast.kind),
                Some(crate::app::state::ToastKind::Finished)
            ),
            "releasing the lease through the wire protocol must fire the owed completion toast"
        );
    }

    fn acquire_request(
        public_pane_id: &str,
        terminal_id: &str,
        token: &str,
        requested_at_ms: u64,
    ) -> crate::terminal::WaitLeaseRequest {
        acquire_request_for_job(
            public_pane_id,
            terminal_id,
            "background-review",
            token,
            requested_at_ms,
        )
    }

    fn acquire_request_for_job(
        public_pane_id: &str,
        terminal_id: &str,
        job_id: &str,
        token: &str,
        requested_at_ms: u64,
    ) -> crate::terminal::WaitLeaseRequest {
        crate::terminal::WaitLeaseRequest {
            version: 1,
            request_id: "acquire-request".into(),
            operation: crate::terminal::WaitLeaseOperation::Acquire {
                pane_id: public_pane_id.into(),
                terminal_id: terminal_id.into(),
                terminal_generation: 0,
                job_id: job_id.into(),
                ttl_ms: 60_000,
                token: token.into(),
                requested_at_ms,
            },
        }
    }

    fn release_request(
        public_pane_id: &str,
        terminal_id: &str,
        job_id: &str,
        token: &str,
    ) -> crate::terminal::WaitLeaseRequest {
        crate::terminal::WaitLeaseRequest {
            version: 1,
            request_id: "release-request".into(),
            operation: crate::terminal::WaitLeaseOperation::Release {
                pane_id: public_pane_id.into(),
                terminal_id: terminal_id.into(),
                terminal_generation: 0,
                job_id: job_id.into(),
                token: token.into(),
            },
        }
    }

    #[test]
    fn wait_lease_is_orthogonal_and_release_requires_its_owner_token() {
        let (mut app, public_pane_id, terminal_id) = wait_lease_test_app();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let attached_terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .unwrap()
            .attached_terminal_id
            .clone();
        app.state
            .terminals
            .get_mut(&attached_terminal_id)
            .unwrap()
            .state = crate::detect::AgentState::Working;
        let requested_at_ms = crate::platform::continuous_clock_ms().unwrap();
        let token = "a".repeat(64);

        let (acquired, changed) = app.apply_wait_lease_request(&acquire_request(
            &public_pane_id,
            &terminal_id,
            &token,
            requested_at_ms,
        ));

        assert!(changed);
        assert!(matches!(
            acquired.result,
            crate::terminal::WaitLeaseResponseResult::Acquired { .. }
        ));
        let terminal = app.state.terminals.get(&attached_terminal_id).unwrap();
        assert_eq!(terminal.state, crate::detect::AgentState::Working);
        assert_eq!(
            terminal.wait_lease.as_ref().map(|lease| lease.job_id()),
            Some("background-review")
        );

        let wrong_release = crate::terminal::WaitLeaseRequest {
            version: 1,
            request_id: "wrong-release".into(),
            operation: crate::terminal::WaitLeaseOperation::Release {
                pane_id: public_pane_id.clone(),
                terminal_id: terminal_id.clone(),
                terminal_generation: 0,
                job_id: "background-review".into(),
                token: "b".repeat(64),
            },
        };
        let (rejected, changed) = app.apply_wait_lease_request(&wrong_release);
        assert!(!changed);
        assert!(matches!(
            rejected.result,
            crate::terminal::WaitLeaseResponseResult::Error { ref code, .. }
                if code == "wait_lease_token_mismatch"
        ));

        let owner_release = crate::terminal::WaitLeaseRequest {
            version: 1,
            request_id: "owner-release".into(),
            operation: crate::terminal::WaitLeaseOperation::Release {
                pane_id: public_pane_id,
                terminal_id,
                terminal_generation: 0,
                job_id: "background-review".into(),
                token,
            },
        };
        let (released, changed) = app.apply_wait_lease_request(&owner_release);
        assert!(changed);
        assert!(matches!(
            released.result,
            crate::terminal::WaitLeaseResponseResult::Released { released: true }
        ));
        assert!(app
            .state
            .terminals
            .get(&attached_terminal_id)
            .unwrap()
            .wait_lease
            .is_none());
    }

    #[test]
    fn wait_lease_rejects_stale_pane_generation_and_expired_request() {
        let (mut app, public_pane_id, terminal_id) = wait_lease_test_app();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let attached_terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .unwrap()
            .attached_terminal_id
            .clone();
        let now_ms = crate::platform::continuous_clock_ms().unwrap();
        let token = "c".repeat(64);
        let stale_generation = acquire_request(&public_pane_id, "term_stale", &token, now_ms);
        let (response, changed) = app.apply_wait_lease_request(&stale_generation);
        assert!(!changed);
        assert!(matches!(
            response.result,
            crate::terminal::WaitLeaseResponseResult::Error { ref code, .. }
                if code == "wait_lease_pane_generation_changed"
        ));

        let expired = acquire_request(
            &public_pane_id,
            &terminal_id,
            &token,
            now_ms.saturating_sub(60_001),
        );
        let (response, changed) = app.apply_wait_lease_request(&expired);
        assert!(!changed);
        assert!(matches!(
            response.result,
            crate::terminal::WaitLeaseResponseResult::Error { ref code, .. }
                if code == "wait_lease_request_expired"
        ));

        app.state
            .terminals
            .get_mut(&attached_terminal_id)
            .unwrap()
            .clear_wait_lease_runtime_identity();
        let stale_runtime = acquire_request(&public_pane_id, &terminal_id, &token, now_ms);
        let (response, changed) = app.apply_wait_lease_request(&stale_runtime);
        assert!(!changed);
        assert!(matches!(
            response.result,
            crate::terminal::WaitLeaseResponseResult::Error { ref code, .. }
                if code == "wait_lease_pane_generation_changed"
        ));
    }

    #[test]
    fn wait_lease_revocations_reject_old_and_preemptively_released_tokens() {
        let (mut app, public_pane_id, terminal_id) = wait_lease_test_app();
        let now_ms = crate::platform::continuous_clock_ms().unwrap();
        let token_a = "a".repeat(64);
        let token_b = "b".repeat(64);
        let token_c = "c".repeat(64);

        let (_, changed) = app.apply_wait_lease_request(&acquire_request(
            &public_pane_id,
            &terminal_id,
            &token_a,
            now_ms,
        ));
        assert!(changed);
        let (_, changed) = app.apply_wait_lease_request(&release_request(
            &public_pane_id,
            &terminal_id,
            "background-review",
            &token_a,
        ));
        assert!(changed);

        let (_, changed) = app.apply_wait_lease_request(&acquire_request(
            &public_pane_id,
            &terminal_id,
            &token_b,
            now_ms,
        ));
        assert!(changed);
        let (_, changed) = app.apply_wait_lease_request(&release_request(
            &public_pane_id,
            &terminal_id,
            "background-review",
            &token_b,
        ));
        assert!(changed);

        let (replayed, changed) = app.apply_wait_lease_request(&acquire_request(
            &public_pane_id,
            &terminal_id,
            &token_a,
            now_ms,
        ));
        assert!(!changed);
        assert!(matches!(
            replayed.result,
            crate::terminal::WaitLeaseResponseResult::Error { ref code, .. }
                if code == "wait_lease_token_revoked"
        ));

        let (released, changed) = app.apply_wait_lease_request(&release_request(
            &public_pane_id,
            &terminal_id,
            "background-review",
            &token_c,
        ));
        assert!(!changed);
        assert!(matches!(
            released.result,
            crate::terminal::WaitLeaseResponseResult::Released { released: false }
        ));
        let (replayed, changed) = app.apply_wait_lease_request(&acquire_request(
            &public_pane_id,
            &terminal_id,
            &token_c,
            now_ms,
        ));
        assert!(!changed);
        assert!(matches!(
            replayed.result,
            crate::terminal::WaitLeaseResponseResult::Error { ref code, .. }
                if code == "wait_lease_token_revoked"
        ));
    }

    #[test]
    fn wait_lease_release_normalizes_job_id_and_reports_expiry_as_not_released() {
        let (mut app, public_pane_id, terminal_id) = wait_lease_test_app();
        let pane_id = app.state.workspaces[0].tabs[0].root_pane;
        let attached_terminal_id = app.state.workspaces[0]
            .pane_state(pane_id)
            .unwrap()
            .attached_terminal_id
            .clone();
        let now_ms = crate::platform::continuous_clock_ms().unwrap();
        let token = "d".repeat(64);

        let (_, changed) = app.apply_wait_lease_request(&acquire_request_for_job(
            &public_pane_id,
            &terminal_id,
            " background-review ",
            &token,
            now_ms,
        ));
        assert!(changed);
        let (released, changed) = app.apply_wait_lease_request(&release_request(
            &public_pane_id,
            &terminal_id,
            " background-review ",
            &token,
        ));
        assert!(changed);
        assert!(matches!(
            released.result,
            crate::terminal::WaitLeaseResponseResult::Released { released: true }
        ));

        let expired_token = "e".repeat(64);
        app.state
            .terminals
            .get_mut(&attached_terminal_id)
            .unwrap()
            .wait_lease = Some(crate::terminal::WaitLease::new(
            "background-review".into(),
            expired_token.clone(),
            now_ms,
        ));
        assert!(app.expire_wait_leases());
        let (released, changed) = app.apply_wait_lease_request(&release_request(
            &public_pane_id,
            &terminal_id,
            "background-review",
            &expired_token,
        ));
        assert!(!changed);
        assert!(matches!(
            released.result,
            crate::terminal::WaitLeaseResponseResult::Released { released: false }
        ));
    }

    #[test]
    fn unacknowledged_wait_lease_acquire_is_rolled_back_and_revoked() {
        let (mut app, public_pane_id, terminal_id) = wait_lease_test_app();
        let now_ms = crate::platform::continuous_clock_ms().unwrap();
        let token = "f".repeat(64);
        let request = acquire_request(&public_pane_id, &terminal_id, &token, now_ms);

        let (_, changed) = app.apply_wait_lease_request(&request);
        assert!(changed);
        assert!(app.rollback_unacknowledged_wait_lease_acquire(&request));

        let (replayed, changed) = app.apply_wait_lease_request(&request);
        assert!(!changed);
        assert!(matches!(
            replayed.result,
            crate::terminal::WaitLeaseResponseResult::Error { ref code, .. }
                if code == "wait_lease_token_revoked"
        ));
    }

    #[test]
    fn git_refresh_deduplicates_workspaces_with_same_cache_key() {
        let repo =
            std::env::temp_dir().join(format!("herdr-git-refresh-dedupe-{}", std::process::id()));
        let nested = repo.join("nested");
        let other = repo.join("other");
        std::fs::create_dir_all(&nested).expect("create nested dir");
        std::fs::create_dir_all(&other).expect("create other dir");
        std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .arg("init")
            .output()
            .expect("run git init");

        let output = refresh_workspace_git_statuses_with_cache(
            vec![
                WorkspaceGitRefreshItem {
                    workspace_id: "one".into(),
                    resolved_identity_cwd: nested.clone(),
                    cache_key: repo.clone(),
                },
                WorkspaceGitRefreshItem {
                    workspace_id: "two".into(),
                    resolved_identity_cwd: other.clone(),
                    cache_key: repo.clone(),
                },
            ],
            &HashMap::new(),
        );

        assert_eq!(output.cache_updates.len(), 1);
        assert_eq!(output.cache_updates[0].0, repo);
        assert_eq!(output.results.len(), 2);
        assert_eq!(output.results[0].workspace_id, "one");
        assert_eq!(
            output.results[0].resolved_identity_cwd,
            PathBuf::from(&nested)
        );
        assert_eq!(output.results[1].workspace_id, "two");
        assert_eq!(
            output.results[1].resolved_identity_cwd,
            PathBuf::from(&other)
        );

        let _ = std::fs::remove_dir_all(repo);
    }

    #[test]
    fn git_refresh_items_use_cwd_cache_key_for_non_git_cwd() {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        let cwd = std::env::temp_dir().join(format!("herdr-non-git-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&cwd).expect("create temp cwd");
        let mut ws = Workspace::test_new("test");
        ws.identity_cwd = cwd.clone();
        ws.tabs.clear();
        app.state.workspaces.push(ws);

        let items = app.workspace_git_refresh_items();

        assert_eq!(items.len(), 1);
        assert_eq!(items[0].cache_key, cwd);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn headless_deadline_can_suppress_git_refresh_timer() {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        app.state.workspaces.push(Workspace::test_new("test"));
        let now = Instant::now();
        app.last_git_remote_status_refresh = now - super::super::GIT_REMOTE_STATUS_REFRESH_INTERVAL;
        app.next_wait_lease_poll = now + Duration::from_secs(5);

        assert_eq!(
            app.next_headless_loop_deadline_with_git_refresh(now, false, false),
            Some(app.next_wait_lease_poll)
        );
        assert_eq!(
            app.next_headless_loop_deadline_with_git_refresh(now, false, true),
            Some(now)
        );
    }

    #[test]
    fn git_refresh_due_request_survives_in_flight_refresh() {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        let now = Instant::now();
        app.git_refresh_in_flight = true;

        app.mark_git_status_refresh_due(now);
        assert!(app.git_refresh_due_after_in_flight);

        app.handle_internal_event(crate::events::AppEvent::GitStatusRefreshed {
            results: Vec::new(),
            cache_updates: Vec::new(),
        });

        assert!(!app.git_refresh_in_flight);
        assert!(!app.git_refresh_due_after_in_flight);
        assert_eq!(app.git_refresh_deadline(), None);

        app.state.workspaces.push(Workspace::test_new("test"));
        let deadline = app
            .git_refresh_deadline()
            .expect("refresh should be due once a workspace exists");
        assert!(deadline <= Instant::now());
    }

    #[test]
    fn tick_selection_autoscroll_stops_when_metrics_unavailable() {
        // Without a runtime, pane_scroll_metrics returns None.
        // Fail-closed: stop autoscroll instead of rescheduling forever.
        let (mut app, pane_id) = test_app_with_pane();
        let now = Instant::now();
        let mut sel = crate::selection::Selection::anchor(pane_id, 0, 0, None);
        // Drag to a different cell so it becomes Dragging
        sel.drag(5, 5, ratatui::layout::Rect::new(0, 0, 80, 24), None);
        app.state.selection = Some(sel);
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Down,
            last_mouse_screen_col: 5,
            last_mouse_screen_row: 23,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        // Should stop because no runtime metrics available
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    #[test]
    fn tick_selection_autoscroll_stops_when_selection_done() {
        let (mut app, pane_id) = test_app_with_pane();
        let now = Instant::now();
        // Create a selection that is already finished (not in progress)
        let mut sel = crate::selection::Selection::anchor(pane_id, 0, 0, None);
        // Drag to a different cell so it becomes visible, then finish
        sel.drag(5, 5, ratatui::layout::Rect::new(0, 0, 80, 24), None);
        sel.finish(); // now it's Done, not in progress
        app.state.selection = Some(sel);
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Down,
            last_mouse_screen_col: 0,
            last_mouse_screen_row: 23,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    #[test]
    fn tick_selection_autoscroll_stops_when_selection_cleared() {
        let (mut app, _pane_id) = test_app_with_pane();
        let now = Instant::now();
        app.state.selection = None;
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Down,
            last_mouse_screen_col: 0,
            last_mouse_screen_row: 23,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    #[test]
    fn tick_selection_autoscroll_stops_when_selection_anchored() {
        // Anchored (click, no drag) should not keep the timer running.
        let (mut app, pane_id) = test_app_with_pane();
        let now = Instant::now();
        app.state.selection = Some(crate::selection::Selection::anchor(pane_id, 0, 0, None));
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Down,
            last_mouse_screen_col: 0,
            last_mouse_screen_row: 23,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    /// Creates an app with a real TerminalRuntime (no PTY) so scroll_metrics
    /// returns meaningful data. Uses test_with_scrollback_bytes.
    fn test_app_with_runtime(
        cols: u16,
        rows: u16,
        bytes: &[u8],
    ) -> (super::super::App, crate::layout::PaneId) {
        let mut app = super::super::App::new(
            &crate::config::Config::default(),
            true,
            None,
            tokio::sync::mpsc::unbounded_channel().1,
            crate::api::EventHub::default(),
        );
        let mut ws = Workspace::test_new("test");
        let pane_id = ws.tabs[0].root_pane;
        let runtime =
            crate::terminal::TerminalRuntime::test_with_scrollback_bytes(cols, rows, 0, bytes);
        ws.tabs[0].runtimes.insert(pane_id, runtime);
        app.state.workspaces.push(ws);
        app.state.active = Some(0);
        app.state.view.pane_infos.push(crate::layout::PaneInfo {
            id: pane_id,
            rect: ratatui::layout::Rect::new(0, 0, cols, rows),
            inner_rect: ratatui::layout::Rect::new(0, 0, cols, rows),
            scrollbar_rect: None,
            borders: ratatui::widgets::Borders::NONE,
            is_focused: true,
        });
        (app, pane_id)
    }

    #[tokio::test]
    async fn tick_selection_autoscroll_stops_at_scrollback_top() {
        // Create a runtime with no scrollback content — we're already at
        // the top (offset_from_bottom == max_offset_from_bottom).
        let (mut app, pane_id) = test_app_with_runtime(80, 24, &[]);
        let now = Instant::now();
        let mut sel = crate::selection::Selection::anchor(pane_id, 5, 5, None);
        sel.drag(0, 0, ratatui::layout::Rect::new(0, 0, 80, 24), None);
        app.state.selection = Some(sel);
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Up,
            last_mouse_screen_col: 0,
            last_mouse_screen_row: 0,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        // At scrollback top, can't scroll further up — should stop
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    #[tokio::test]
    async fn tick_selection_autoscroll_stops_at_scrollback_bottom() {
        // Create a runtime with no scrollback content — we're already at
        // the bottom (offset_from_bottom == 0).
        let (mut app, pane_id) = test_app_with_runtime(80, 24, &[]);
        let now = Instant::now();
        let mut sel = crate::selection::Selection::anchor(pane_id, 0, 0, None);
        sel.drag(5, 5, ratatui::layout::Rect::new(0, 0, 80, 24), None);
        app.state.selection = Some(sel);
        app.state.selection_autoscroll = Some(state::SelectionAutoscroll {
            direction: state::SelectionAutoscrollDirection::Down,
            last_mouse_screen_col: 5,
            last_mouse_screen_row: 23,
            inner_rect: ratatui::layout::Rect::new(0, 0, 80, 24),
        });
        app.selection_autoscroll_deadline = Some(now);
        app.tick_selection_autoscroll(now);
        // At scrollback bottom, can't scroll further down — should stop
        assert!(app.state.selection_autoscroll.is_none());
        assert!(app.selection_autoscroll_deadline.is_none());
    }

    #[tokio::test]
    async fn raw_input_batch_does_not_start_pending_agent_resume_before_render() {
        let (mut app, pane_id) = test_app_with_pane();
        app.state.ensure_test_terminals();
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .cloned()
            .expect("test pane should have a terminal");
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });

        assert!(
            app.handle_raw_input_batch(crate::raw_input::RawInputEvent::HostDefaultColor {
                kind: crate::terminal_theme::DefaultColorKind::Foreground,
                color: crate::terminal_theme::RgbColor {
                    r: 220,
                    g: 220,
                    b: 220,
                },
            })
            .await
        );
        assert!(
            app.terminal_runtimes.get(&terminal_id).is_none(),
            "raw input can mutate active geometry; pending resumes must wait for render to refresh pane_infos"
        );
        assert!(app
            .state
            .terminals
            .get(&terminal_id)
            .expect("test terminal should still exist")
            .pending_agent_resume_plan
            .is_some());
    }

    #[tokio::test]
    async fn scheduled_tasks_do_not_start_pending_agent_resume_when_geometry_dirty() {
        let (mut app, pane_id) = test_app_with_pane();
        app.state.ensure_test_terminals();
        app.state.host_terminal_theme = crate::terminal_theme::TerminalTheme {
            foreground: Some(crate::terminal_theme::RgbColor {
                r: 220,
                g: 220,
                b: 220,
            }),
            background: Some(crate::terminal_theme::RgbColor {
                r: 20,
                g: 20,
                b: 20,
            }),
        };
        let terminal_id = app.state.workspaces[0]
            .terminal_id(pane_id)
            .cloned()
            .expect("test pane should have a terminal");
        app.state
            .terminals
            .get_mut(&terminal_id)
            .expect("test terminal should exist")
            .pending_agent_resume_plan = Some(crate::agent_resume::AgentResumePlan {
            agent: "codex".into(),
            argv: vec!["/bin/sh".into(), "-c".into(), "sleep 5".into()],
            dedupe_key: "herdr:codex\0codex\0Id\0codex-session".into(),
        });
        app.pending_agent_resume_deadline = Some(Instant::now() - Duration::from_millis(1));

        assert!(!app.handle_scheduled_tasks(Instant::now(), true));
        assert!(app.terminal_runtimes.get(&terminal_id).is_none());
        assert!(app
            .state
            .terminals
            .get(&terminal_id)
            .expect("test terminal should still exist")
            .pending_agent_resume_plan
            .is_some());
        assert!(app.pending_agent_resume_deadline.is_none());
    }
}
