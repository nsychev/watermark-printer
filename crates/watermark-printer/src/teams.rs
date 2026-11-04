//! Resolves a client IP address into the watermark content.
//!
//! Lua is not re-entrant, so the interpreter lives behind a mutex.

use std::net::IpAddr;
use std::sync::Mutex;

use anyhow::{Context, Result};
use mlua::{Function, Lua};

/// Default mapping is zero-padded third octet of an IPv4.
pub const DEFAULT_SCRIPT: &str = include_str!("default_team_id_script.lua");

/// A loaded `get_team_id` function.
pub struct TeamIds {
    inner: Mutex<Script>,
    source: String,
}

struct Script {
    // Kept alive for as long as the function borrowed from it.
    _lua: Lua,
    get_team_id: Function,
}

impl std::fmt::Debug for TeamIds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeamIds")
            .field("source", &self.source)
            .finish()
    }
}

impl TeamIds {
    /// Loads a script, or the built-in one when `path` is `None`.
    pub fn load(path: Option<&str>) -> Result<Self> {
        let (source, code) = match path {
            Some(path) => (
                path.to_string(),
                std::fs::read_to_string(path)
                    .with_context(|| format!("reading the team id script {path}"))?,
            ),
            None => (
                "the built-in script".to_string(),
                DEFAULT_SCRIPT.to_string(),
            ),
        };

        let lua = Lua::new();
        lua.load(&code)
            .exec()
            .with_context(|| format!("loading {source}"))?;
        let get_team_id: Function = lua
            .globals()
            .get("get_team_id")
            .with_context(|| format!("{source} defines no get_team_id function"))?;

        Ok(TeamIds {
            inner: Mutex::new(Script {
                _lua: lua,
                get_team_id,
            }),
            source,
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// The team for an address, or `None` if it may not print.
    pub fn lookup(&self, address: IpAddr) -> Result<Option<String>> {
        // Convert IPv4-mapped IPv6 addresses to IPv4.
        let address = match address {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(address),
            other => other,
        };

        let script = self.inner.lock().expect("the team id script panicked");
        let team: Option<String> = script
            .get_team_id
            .call(address.to_string())
            .context("calling get_team_id")?;

        Ok(team
            .map(|team| team.trim().to_string())
            .filter(|t| !t.is_empty()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn script(code: &str) -> TeamIds {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), code).unwrap();
        TeamIds::load(Some(file.path().to_str().unwrap())).unwrap()
    }

    #[test]
    fn the_built_in_script_uses_the_third_octet() {
        let teams = TeamIds::load(None).unwrap();
        assert_eq!(
            teams.lookup("10.21.32.43".parse().unwrap()).unwrap(),
            Some("032".into())
        );
        assert_eq!(
            teams.lookup("10.21.7.1".parse().unwrap()).unwrap(),
            Some("007".into())
        );
        // No IPv4 address, no team.
        assert_eq!(teams.lookup("2001:db8::1".parse().unwrap()).unwrap(), None);
    }

    #[test]
    fn an_ipv4_mapped_address_is_treated_as_ipv4() {
        let teams = TeamIds::load(None).unwrap();
        // This is how a dual-stack listener reports an IPv4 client.
        assert_eq!(
            teams.lookup("::ffff:10.21.32.43".parse().unwrap()).unwrap(),
            Some("032".into())
        );
    }

    #[test]
    fn a_script_may_refuse_an_address() {
        let teams = script("function get_team_id(ip) return nil end");
        assert_eq!(teams.lookup("10.0.0.1".parse().unwrap()).unwrap(), None);
    }

    #[test]
    fn blank_answers_count_as_a_refusal() {
        let teams = script("function get_team_id(ip) return '   ' end");
        assert_eq!(teams.lookup("10.0.0.1".parse().unwrap()).unwrap(), None);
    }

    #[test]
    fn answers_are_trimmed() {
        let teams = script("function get_team_id(ip) return ' 42 ' end");
        assert_eq!(
            teams.lookup("10.0.0.1".parse().unwrap()).unwrap(),
            Some("42".into())
        );
    }

    #[test]
    fn the_script_sees_the_address_it_was_given() {
        let teams = script("function get_team_id(ip) return ip end");
        assert_eq!(
            teams.lookup("192.168.5.9".parse().unwrap()).unwrap(),
            Some("192.168.5.9".into())
        );
    }

    #[test]
    fn a_broken_script_fails_at_load_rather_than_at_print_time() {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), "this is not lua(((").unwrap();
        assert!(TeamIds::load(Some(file.path().to_str().unwrap())).is_err());

        // So does one that loads but defines nothing useful.
        std::fs::write(file.path(), "x = 1").unwrap();
        assert!(TeamIds::load(Some(file.path().to_str().unwrap())).is_err());
    }

    #[test]
    fn a_missing_file_is_reported_clearly() {
        let error = TeamIds::load(Some("/nonexistent/script.lua")).unwrap_err();
        assert!(error.to_string().contains("team id script"), "{error}");
    }

    #[test]
    fn an_error_inside_the_script_is_reported_per_job() {
        let teams = script("function get_team_id(ip) error('boom') end");
        assert!(teams.lookup("10.0.0.1".parse().unwrap()).is_err());
    }
}
