use super::*;

pub(super) const GENERATION_TAG: &str = "acp-generation-";

pub(super) fn generation(tags: &[String]) -> u128 {
    tags.iter()
        .filter_map(|tag| tag.strip_prefix(GENERATION_TAG)?.parse().ok())
        .max()
        .unwrap_or(0)
}

impl Adapter {
    pub(super) async fn run_turn(
        &self,
        session_id: &str,
        lease: &mut TurnLease,
        accepted: Arc<AtomicBool>,
        sender: &Sender,
    ) -> Result<TurnResult, String> {
        let deadline = tokio::time::Instant::now() + self.config.turn_timeout;
        let mut attempted = Vec::new();
        let mut continuing = false;
        loop {
            if lease.cancel.is_cancelled() {
                return Ok(TurnResult {
                    accepted: accepted.load(Ordering::Acquire),
                    stop_reason: "cancelled",
                    depleted: false,
                });
            }
            if let Some(pool) = &self.config.account_pool {
                let current = self
                    .sessions
                    .lock()
                    .await
                    .get(session_id)
                    .and_then(|s| s.account.clone());
                let mut client = self.config.transport.connect().await?;
                let selected = client
                    .select_account_pool(pool.clone(), current.clone(), attempted.clone())
                    .await
                    .map_err(|e| e.to_string())?;
                if selected.backend != lease.backend {
                    return Err("account pool changed backend during session".into());
                }
                if current.as_deref() != Some(&selected.name) {
                    self.switch_account(session_id, lease, &selected.name, deadline)
                        .await?;
                    if continuing {
                        lease.prompt = "continue".into();
                    }
                }
                attempted.push(selected.name);
            }
            if lease.cancel.is_cancelled() {
                continue;
            }
            let result = match tokio::time::timeout_at(
                deadline,
                self.run_turn_once(session_id, lease, Arc::clone(&accepted), sender),
            )
            .await
            {
                Ok(result) => result?,
                Err(_) => {
                    self.interrupt_pane(&lease.pane_id).await?;
                    return Err(format!(
                        "turn timed out after {} seconds",
                        self.config.turn_timeout.as_secs()
                    ));
                }
            };
            if !result.depleted {
                return Ok(result);
            }
            continuing = true;
        }
    }

    async fn switch_account(
        &self,
        session_id: &str,
        lease: &mut TurnLease,
        account: &str,
        deadline: tokio::time::Instant,
    ) -> Result<(), String> {
        // Serialize replacement with eviction, reconciliation, and steering.
        let _creation = self.session_creation.lock().await;
        let steering = Arc::clone(&lease.steering);
        let _steering = steering.acquire().await.map_err(|e| e.to_string())?;
        if lease.cancel.is_cancelled() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err("turn timed out before account failover".into());
        }
        let (cwd, start_directory, resumable) = {
            let sessions = self.sessions.lock().await;
            let session = sessions
                .get(session_id)
                .ok_or("session closed during failover")?;
            (
                session.cwd.to_string_lossy().into_owned(),
                session.start_directory.clone(),
                session.system_prompt_delivered,
            )
        };
        // Cancels any scheduled retry on the depleted process before importing
        // history. Never resubmit the original request after an accepted turn.
        self.interrupt_pane(&lease.pane_id).await?;
        let mut client = self.config.transport.connect().await?;
        let mut subscription = client
            .subscribe(vec![
                event_filter("slopd", "DetailedStateChange"),
                event_filter("slopd", "PaneDestroyed"),
                event_filter("hook", "SessionEnd"),
            ])
            .await
            .map_err(|e| e.to_string())?;
        // Await the spawn response even if cancellation arrives meanwhile, so
        // the replacement can be explicitly cleaned up instead of orphaned.
        let (pane_id, native_id) = if resumable {
            let (pane, native) = client
                .fork_to_account(
                    lease.pane_id.clone(),
                    Some(start_directory),
                    self.config.env.clone(),
                    self.config.extra_args.clone(),
                    Some(account.into()),
                )
                .await
                .map_err(|e| e.to_string())?;
            (pane, Some(native))
        } else {
            let started = self
                .start_pane(start_directory, None, Some(account))
                .await?;
            (started.pane_id, started.native_session_id)
        };
        let prepared = async {
            tokio::select! {
                result = wait_until_live(&self.config.transport, &mut subscription, &pane_id,
                    self.config.ready_timeout.min(deadline.saturating_duration_since(tokio::time::Instant::now()))) => result?,
                _ = lease.cancel.cancelled() => return Err("session cancelled during account failover".into()),
            }
            if lease.cancel.is_cancelled() {
                return Err("session cancelled during account failover".into());
            }
            let generation = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos();
            client
                .tag(pane_id.clone(), format!("{GENERATION_TAG}{generation}"))
                .await
                .map_err(|e| e.to_string())?;
            self.tag_session_pane(&pane_id, session_id, &cwd, resumable)
                .await
        }
        .await;
        if let Err(error) = prepared {
            self.kill_pane(&pane_id).await?;
            if lease.cancel.is_cancelled() {
                return Ok(());
            }
            return Err(error);
        }
        {
            let mut sessions = self.sessions.lock().await;
            let Some(session) = sessions.get_mut(session_id).filter(|s| {
                s.active_turn
                    .as_ref()
                    .is_some_and(|t| t.id == lease.turn_id)
            }) else {
                drop(sessions);
                self.kill_pane(&pane_id).await?;
                return Err("session closed during failover".into());
            };
            session.account = Some(account.into());
            session.pane_id = Some(pane_id.clone());
            session.grave_id = None;
            session.native_session_id = native_id.clone();
            session.pending_resume_history = resumable;
        }
        let old = std::mem::replace(&mut lease.pane_id, pane_id);
        lease.native_session_id = native_id;
        lease.discard_replayed_history = resumable;
        // The replacement's generation makes it authoritative even if the
        // adapter dies between publication and retiring the source pane.
        client
            .tag(old.clone(), ACP_DELETED_TAG.into())
            .await
            .map_err(|e| e.to_string())?;
        self.kill_pane(&old).await?;
        tracing::info!(
            session_id,
            account,
            pane_id = lease.pane_id,
            "continued session on another account"
        );
        Ok(())
    }
}
