use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

/// Read text from a path, `-` meaning stdin.
pub fn read_text(path: &Path) -> Result<String> {
    if path.as_os_str() == "-" {
        let mut s = String::new();
        std::io::stdin()
            .read_to_string(&mut s)
            .context("reading stdin")?;
        Ok(s)
    } else {
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))
    }
}

pub fn read_stdin() -> Result<String> {
    let mut s = String::new();
    std::io::stdin()
        .read_to_string(&mut s)
        .context("reading stdin")?;
    Ok(s)
}

/// Parse `--since`: RFC3339, or a humantime duration meaning "that long ago".
pub fn parse_since(s: &str) -> Result<DateTime<Utc>> {
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Ok(d.with_timezone(&Utc));
    }
    let dur = humantime::parse_duration(s)
        .with_context(|| format!("--since {s:?} is neither RFC3339 nor a duration"))?;
    let dur = chrono::Duration::from_std(dur).context("duration too large")?;
    Ok(Utc::now() - dur)
}

pub fn age(since: DateTime<Utc>) -> String {
    compact((Utc::now() - since).num_seconds().max(0) as u64)
}

/// One unit, rounded down: "45s", "12m", "5h", "18d".
fn compact(secs: u64) -> String {
    match secs {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

pub fn truncate(s: &str, n: usize) -> String {
    let s = s.lines().next().unwrap_or("");
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Fixed-width text table.
pub fn table(headers: &[&str], rows: &[Vec<String>]) -> String {
    let cols = headers.len();
    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for r in rows {
        for (i, c) in r.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(c.chars().count());
        }
    }
    let fmt_row = |cells: Vec<&str>| -> String {
        let mut line = String::new();
        for (i, c) in cells.iter().enumerate() {
            if i > 0 {
                line.push_str("  ");
            }
            if i + 1 == cols {
                line.push_str(c);
            } else {
                line.push_str(c);
                for _ in c.chars().count()..widths[i] {
                    line.push(' ');
                }
            }
        }
        line.trim_end().to_string()
    };
    let mut out = fmt_row(headers.to_vec());
    out.push('\n');
    for r in rows {
        out.push_str(&fmt_row(r.iter().map(String::as_str).collect()));
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn ages_are_compact() {
        assert_eq!(super::compact(5), "5s");
        assert_eq!(super::compact(125), "2m");
        assert_eq!(super::compact(7_300), "2h");
        assert_eq!(super::compact(70 * 86_400 + 9_000), "70d");
    }
}
