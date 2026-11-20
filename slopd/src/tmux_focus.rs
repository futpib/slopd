use libslop::{SlopdConfig, TmuxFocus};

use crate::tmux;

pub async fn switch(
    config: &SlopdConfig,
    window_id: Option<&str>,
    pane_id: Option<&str>,
) -> Result<TmuxFocus, String> {
    libslop::validate_tmux_focus_target(window_id, pane_id)?;
    // Resolve the configured name ourselves: list-panes' target resolution
    // can match a session prefix even with '='. Select only by stable IDs.
    // Never recreate a missing session as a side effect of a focus request.
    let sessions = tmux(config)
        .args(["list-sessions", "-F", "#{session_id}\t#{session_name}"])
        .output()
        .await
        .map_err(|error| format!("failed to list tmux sessions: {error}"))?;
    if !sessions.status.success() {
        return Err(format!(
            "cannot resolve configured slopd tmux session: {}",
            String::from_utf8_lossy(&sessions.stderr).trim()
        ));
    }
    let sessions = String::from_utf8_lossy(&sessions.stdout);
    let configured_name = config.tmux.session();
    let session = sessions
        .lines()
        .find_map(|line| {
            let (id, name) = line.split_once('\t')?;
            (name == configured_name).then_some(id)
        })
        .ok_or_else(|| "configured slopd tmux session does not exist".to_string())?;
    let listing = tmux(config)
        .args([
            "list-panes",
            "-s",
            "-t",
            &format!("{session}:"),
            "-F",
            "#{session_id} #{window_id} #{pane_id} #{window_linked}",
        ])
        .output()
        .await
        .map_err(|error| format!("failed to list tmux focus targets: {error}"))?;
    if !listing.status.success() {
        return Err(format!(
            "cannot resolve configured slopd tmux session: {}",
            String::from_utf8_lossy(&listing.stderr).trim()
        ));
    }
    let listing = String::from_utf8_lossy(&listing.stdout);
    let target = listing.lines().find_map(|line| {
        let fields: Vec<_> = line.split_whitespace().collect();
        (fields.len() == 4
            && fields[0] == session
            && window_id.is_none_or(|id| id == fields[1])
            && pane_id.is_none_or(|id| id == fields[2]))
        .then_some(fields)
    });
    let Some(target) = target else {
        return Err("focus target is not in the configured slopd tmux session, or pane_id does not belong to window_id".into());
    };
    let (session, window, pane) = (target[0], target[1], target[2]);
    if pane_id.is_some() && target[3] != "0" {
        return Err("cannot select a pane in a window linked to another session; use window_id alone to preserve other sessions' pane selection".into());
    }
    select(config, session, window, pane_id.map(|_| pane)).await
}

async fn select(
    config: &SlopdConfig,
    session: &str,
    window: &str,
    pane_id: Option<&str>,
) -> Result<TmuxFocus, String> {
    let window_target = format!("{session}:{window}");
    let target = if let Some(pane) = pane_id {
        format!("{window_target}.{pane}")
    } else {
        window_target.clone()
    };
    let mut conditions = vec![
        format!("#{{==:#{{session_id}},{session}}}"),
        format!("#{{==:#{{window_id}},{window}}}"),
    ];
    let mut commands = String::new();
    if let Some(pane) = pane_id {
        conditions.push(format!("#{{==:#{{pane_id}},{pane}}}"));
        conditions.push("#{==:#{window_linked},0}".into());
        commands.push_str(&format!("select-pane -t '{target}' ; "));
    }
    commands.push_str(&format!(
        "select-window -t '{window_target}' ; display-message -p -t '{session}:' '#{{session_id}} #{{window_id}} #{{pane_id}}'"
    ));
    let condition = conditions
        .into_iter()
        .reduce(|left, right| format!("#{{&&:{left},{right}}}"))
        .unwrap();
    // Recheck membership and links in tmux's command queue immediately before
    // selection. -F evaluates a format, never a shell command. All interpolated
    // targets are stable IDs from tmux, and every selection includes the session.
    let output = tmux(config)
        .args([
            "if-shell",
            "-F",
            "-t",
            &target,
            &condition,
            &commands,
            "display-message -p 'slopd-focus-rejected'",
        ])
        .output()
        .await
        .map_err(|error| format!("failed to switch tmux focus: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "tmux focus target changed or selection failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let output = String::from_utf8_lossy(&output.stdout);
    let fields: Vec<_> = output.split_whitespace().collect();
    if fields.len() != 3
        || fields[0] != session
        || fields[1] != window
        || pane_id.is_some_and(|id| id != fields[2])
    {
        return Err("tmux focus target changed before selection could be confirmed".into());
    }
    Ok(TmuxFocus {
        session_id: fields[0].into(),
        window_id: fields[1].into(),
        pane_id: fields[2].into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use libsloptest::TmuxServer;

    fn command(server: &TmuxServer, args: &[&str]) -> String {
        let output = server.tmux().args(args).output().unwrap();
        assert!(output.status.success(), "{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    fn config(server: &TmuxServer, session: &str) -> SlopdConfig {
        let mut config = SlopdConfig::default();
        config.tmux.socket = Some(server.socket.clone());
        config.tmux.session = Some(session.into());
        config
    }

    #[tokio::test]
    async fn missing_session_never_falls_back_to_prefix_or_recreates_it() {
        let server = TmuxServer::start().expect("tmux is required for focus tests");
        let pane = command(
            &server,
            &[
                "new-session",
                "-d",
                "-s",
                "focus-other",
                "-P",
                "-F",
                "#{pane_id}",
            ],
        );
        let before = command(
            &server,
            &["list-sessions", "-F", "#{session_id} #{session_name}"],
        );
        let error = switch(&config(&server, "focus"), None, Some(&pane))
            .await
            .unwrap_err();
        assert!(error.contains("configured slopd tmux session"), "{error}");
        assert_eq!(
            command(
                &server,
                &["list-sessions", "-F", "#{session_id} #{session_name}"]
            ),
            before
        );
    }

    #[tokio::test]
    async fn queued_selection_rejects_changed_membership_and_new_links() {
        let server = TmuxServer::start().expect("tmux is required for focus tests");
        let config = config(&server, "test");
        let original = command(
            &server,
            &[
                "display-message",
                "-p",
                "-t",
                "=test:",
                "#{session_id} #{window_id} #{pane_id}",
            ],
        );
        let ids: Vec<_> = original.split_whitespace().collect();
        let pane = command(
            &server,
            &["split-window", "-d", "-t", ids[2], "-P", "-F", "#{pane_id}"],
        );
        let outside = command(
            &server,
            &["new-session", "-d", "-s", "other", "-P", "-F", "#{pane_id}"],
        );
        command(&server, &["join-pane", "-d", "-s", &pane, "-t", &outside]);
        let snapshot = || {
            command(
                &server,
                &[
                    "list-panes",
                    "-a",
                    "-F",
                    "#{session_id} #{window_id} #{window_active} #{pane_id} #{pane_active}",
                ],
            )
        };
        let before = snapshot();
        assert!(select(&config, ids[0], ids[1], Some(&pane)).await.is_err());
        assert_eq!(snapshot(), before);

        command(&server, &["join-pane", "-d", "-s", &pane, "-t", ids[2]]);
        command(
            &server,
            &["link-window", "-d", "-s", ids[1], "-t", "other:"],
        );
        let before = snapshot();
        assert!(select(&config, ids[0], ids[1], Some(&pane)).await.is_err());
        assert_eq!(snapshot(), before);

        command(
            &server,
            &["unlink-window", "-t", &format!("{}:{}", ids[0], ids[1])],
        );
        let before = snapshot();
        assert!(select(&config, ids[0], ids[1], None).await.is_err());
        assert_eq!(snapshot(), before);
    }
}
