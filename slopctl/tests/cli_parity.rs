//! Ensures every subcommand and option that slopctl supports is also present in
//! iroh-slopctl. iroh-slopctl may have *extra* commands (e.g. `info`) or global
//! flags (e.g. `--endpoint`), but it must be a superset of slopctl.
//!
//! Commands that are intentionally only in one binary must be listed in
//! SLOPCTL_ONLY_COMMANDS or IROH_ONLY_COMMANDS below. The test will fail if an
//! unlisted command is missing from either side, catching accidental omissions.

use libsloptest::{build_bin, cargo_bin};
use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

/// Commands that only make sense locally (requires $TMUX_PANE / Unix socket).
const SLOPCTL_ONLY_COMMANDS: &[&str] = &["hook"];

/// Commands that only make sense for the remote iroh client.
const IROH_ONLY_COMMANDS: &[&str] = &["info"];

/// Run `<bin> --help` and return stdout.
fn help_output(bin: &str, args: &[&str]) -> String {
    let output = Command::new(cargo_bin(bin))
        .args(args)
        .output()
        .unwrap_or_else(|e| panic!("failed to run {} {:?}: {}", bin, args, e));
    assert!(
        output.status.success(),
        "{} {:?} failed: {}",
        bin,
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// Parse the "Commands:" section from `--help` output, returning command names.
fn parse_subcommands(help: &str) -> BTreeSet<String> {
    let mut in_commands = false;
    let mut commands = BTreeSet::new();
    for line in help.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Commands:") {
            in_commands = true;
            continue;
        }
        if in_commands {
            if trimmed.is_empty() {
                break;
            }
            // Each command line looks like "  status  Show slopd uptime..."
            if let Some(name) = trimmed.split_whitespace().next() {
                if name != "help" {
                    commands.insert(name.to_string());
                }
            }
        }
    }
    commands
}

/// Parse the "Options:" and "Arguments:" sections from `--help` output,
/// returning long flag names and positional argument names.
///
/// Clap separates the flag column from the description with two or more spaces.
/// We only look at the flag column to avoid matching `--` tokens in descriptions.
fn parse_options(help: &str) -> BTreeMap<String, String> {
    let mut in_section = false;
    let mut options = BTreeMap::new();
    for line in help.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("Options:") || trimmed.starts_with("Arguments:") {
            in_section = true;
            continue;
        }
        if in_section && trimmed.is_empty() {
            in_section = false;
            continue;
        }
        if !in_section {
            continue;
        }
        // Extract just the flag/argument column (before the description).
        // Clap uses two+ spaces to separate columns, e.g.:
        //   -c, --start-directory <DIR>  Working directory for ...
        //       --replay <N>             Replay the last N ...
        //   [EXTRA_ARGS]...              Extra arguments ...
        let flag_col = match trimmed.find("  ") {
            Some(pos) => &trimmed[..pos],
            None => trimmed,
        };
        for token in flag_col.split_whitespace() {
            let token = token.trim_end_matches(',');
            if let Some(flag) = token.strip_prefix("--") {
                if !flag.is_empty() {
                    options.insert(flag.to_string(), trimmed.to_string());
                }
            }
        }
        // Positional arguments look like: <PANE_ID> or [EXTRA_ARGS]...
        if let Some(first) = flag_col.split_whitespace().next() {
            if first.starts_with('<') || first.starts_with('[') {
                options.insert(first.to_string(), trimmed.to_string());
            }
        }
    }
    options
}

#[test]
fn iroh_slopctl_is_superset_of_slopctl() {
    build_bin("slopctl");
    build_bin("iroh-slopctl");

    let slopctl_only: BTreeSet<String> =
        SLOPCTL_ONLY_COMMANDS.iter().map(|s| s.to_string()).collect();
    let iroh_only: BTreeSet<String> =
        IROH_ONLY_COMMANDS.iter().map(|s| s.to_string()).collect();

    // --- Check subcommands ---
    let slopctl_help = help_output("slopctl", &["--help"]);
    let iroh_help = help_output("iroh-slopctl", &["--help"]);

    let slopctl_cmds = parse_subcommands(&slopctl_help);
    let iroh_cmds = parse_subcommands(&iroh_help);

    // Every slopctl command (except slopctl-only) must exist in iroh-slopctl.
    let missing_from_iroh: BTreeSet<_> = slopctl_cmds
        .difference(&iroh_cmds)
        .filter(|c| !slopctl_only.contains(*c))
        .collect();
    assert!(
        missing_from_iroh.is_empty(),
        "subcommands in slopctl but missing from iroh-slopctl: {:?}\n\
         (if intentional, add to SLOPCTL_ONLY_COMMANDS)",
        missing_from_iroh
    );

    // Every iroh-slopctl command (except iroh-only) must exist in slopctl.
    let missing_from_slopctl: BTreeSet<_> = iroh_cmds
        .difference(&slopctl_cmds)
        .filter(|c| !iroh_only.contains(*c))
        .collect();
    assert!(
        missing_from_slopctl.is_empty(),
        "subcommands in iroh-slopctl but missing from slopctl: {:?}\n\
         (if intentional, add to IROH_ONLY_COMMANDS)",
        missing_from_slopctl
    );

    // Verify the allowlists are accurate (no stale entries).
    for cmd in &slopctl_only {
        assert!(
            slopctl_cmds.contains(cmd),
            "SLOPCTL_ONLY_COMMANDS lists {:?} but slopctl doesn't have it",
            cmd
        );
        assert!(
            !iroh_cmds.contains(cmd),
            "SLOPCTL_ONLY_COMMANDS lists {:?} but iroh-slopctl also has it — remove from allowlist",
            cmd
        );
    }
    for cmd in &iroh_only {
        assert!(
            iroh_cmds.contains(cmd),
            "IROH_ONLY_COMMANDS lists {:?} but iroh-slopctl doesn't have it",
            cmd
        );
        assert!(
            !slopctl_cmds.contains(cmd),
            "IROH_ONLY_COMMANDS lists {:?} but slopctl also has it — remove from allowlist",
            cmd
        );
    }

    // --- Check global options ---
    let slopctl_global = parse_options(&slopctl_help);
    let iroh_global = parse_options(&iroh_help);
    let missing_global: BTreeSet<_> = slopctl_global
        .keys()
        .filter(|k| !iroh_global.contains_key(*k))
        .collect();
    assert!(
        missing_global.is_empty(),
        "global options in slopctl but missing from iroh-slopctl: {:?}\nslopctl:      {:?}\niroh-slopctl: {:?}",
        missing_global,
        slopctl_global.keys().collect::<Vec<_>>(),
        iroh_global.keys().collect::<Vec<_>>(),
    );

    // --- Check per-subcommand options ---
    let shared_cmds: BTreeSet<_> = slopctl_cmds
        .intersection(&iroh_cmds)
        .collect();
    let mut failures = Vec::new();
    for cmd in &shared_cmds {
        let slopctl_cmd_help = help_output("slopctl", &[cmd, "--help"]);
        let iroh_cmd_help = help_output("iroh-slopctl", &[cmd, "--help"]);

        let slopctl_opts = parse_options(&slopctl_cmd_help);
        let iroh_opts = parse_options(&iroh_cmd_help);

        let missing: BTreeSet<_> = slopctl_opts
            .keys()
            .filter(|k| !iroh_opts.contains_key(*k))
            .collect();
        if !missing.is_empty() {
            failures.push(format!(
                "  {}: missing {:?}\n    slopctl:      {:?}\n    iroh-slopctl: {:?}",
                cmd,
                missing,
                slopctl_opts.keys().collect::<Vec<_>>(),
                iroh_opts.keys().collect::<Vec<_>>(),
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "per-subcommand options in slopctl but missing from iroh-slopctl:\n{}",
        failures.join("\n")
    );
}
