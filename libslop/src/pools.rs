use serde::{Deserialize, Serialize};

use crate::{Backend, ResolvedAccount, SlopdConfig};

/// An ordered set of independently authenticated accounts for one backend.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountPoolConfig {
    pub accounts: Vec<String>,
    /// Delay before retrying an exhausted account when no reset time is known.
    #[serde(default = "default_cooldown")]
    pub cooldown_secs: u64,
}

fn default_cooldown() -> u64 {
    300
}

impl SlopdConfig {
    /// Resolve and validate a pool before launching any processes.
    pub fn resolve_account_pool(&self, name: &str) -> Result<Vec<ResolvedAccount>, String> {
        let pool = self
            .account_pools
            .get(name)
            .ok_or_else(|| format!("unknown account pool {name:?}"))?;
        if pool.accounts.is_empty() || pool.accounts.len() > 64 {
            return Err(format!(
                "account pool {name:?} must contain 1 to 64 accounts"
            ));
        }
        if !(1..=86400).contains(&pool.cooldown_secs) {
            return Err(format!(
                "account pool {name:?}: cooldown_secs must be 1 to 86400"
            ));
        }
        let accounts = pool
            .accounts
            .iter()
            .map(|name| self.resolve_account(Some(name)))
            .collect::<Result<Vec<_>, _>>()?;
        let first = &accounts[0];
        if !matches!(first.backend, Backend::Codex | Backend::Claude) {
            return Err(format!(
                "account pool {name:?} only supports Codex or Claude"
            ));
        }
        for account in &accounts {
            if account.backend != first.backend {
                return Err(format!("account pool {name:?} mixes backends"));
            }
            // A pool switches credentials, not the configured model, permissions,
            // or MCP policy. Keep account presets consistent across its members.
            if account.executable.args() != first.executable.args() || account.mcp != first.mcp {
                return Err(format!(
                    "account pool {name:?} members must use matching executable arguments and MCP policy"
                ));
            }
        }
        Ok(accounts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_pool_members_and_policy() {
        let mut config: SlopdConfig = toml::from_str(
            r#"
            [accounts.a]
            backend = "codex"
            [accounts.b]
            backend = "claude"
            [account_pools.work]
            accounts = ["a", "b"]
        "#,
        )
        .unwrap();
        assert!(
            config
                .resolve_account_pool("work")
                .unwrap_err()
                .contains("mixes backends")
        );
        config.account_pools.get_mut("work").unwrap().accounts = vec!["a".into()];
        assert_eq!(config.resolve_account_pool("work").unwrap()[0].name, "a");
        config
            .account_pools
            .get_mut("work")
            .unwrap()
            .accounts
            .clear();
        assert!(config.resolve_account_pool("work").is_err());
        assert!(config.resolve_account_pool("missing").is_err());
    }
}
