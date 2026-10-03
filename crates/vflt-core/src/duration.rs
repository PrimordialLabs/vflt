//! Human-friendly durations stored as strings ("15m", "45m", "2h"). An empty
//! string means "unset".

use std::time::Duration;

use crate::error::{Error, Result};

/// Parse a humantime duration. Empty or whitespace-only input yields `None`.
pub fn parse_opt(s: &str) -> Result<Option<Duration>> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    humantime::parse_duration(s)
        .map(Some)
        .map_err(|e| Error::invalid("duration", format!("{s:?}: {e}")))
}

/// Parse a required humantime duration.
pub fn parse(s: &str) -> Result<Duration> {
    parse_opt(s)?.ok_or_else(|| Error::invalid("duration", "empty"))
}

pub fn format(d: Duration) -> String {
    humantime::format_duration(d).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_treats_empty_as_none() {
        assert_eq!(parse_opt("").unwrap(), None);
        assert_eq!(parse_opt("15m").unwrap(), Some(Duration::from_secs(900)));
        assert!(parse_opt("nonsense").is_err());
    }
}
