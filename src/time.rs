//! Timecode text — the one time language every surface speaks.
//!
//! Durations cross the wire and the page as `HH:MM:SS.fff` strings, and are
//! typed in the lenient forms `SS`, `MM:SS` or `HH:MM:SS`, each with an
//! optional `.fff` fraction. A bare number is seconds, so `3.2` and `0.005`
//! mean what they look like.
//!
//! Kept in the lib so both the content/control layers ([`crate::control`]
//! spells durations as timecodes in its JSON) and the command surface speak
//! one time text; [`crate::cli`] delegates to it.

use std::time::Duration;

/// Parse `SS`, `MM:SS` or `HH:MM:SS` (optional `.fff` fraction) into a
/// duration. `:` separates the fields; every field is a plain number, so a
/// bare field is seconds.
pub fn parse(s: &str) -> Result<Duration, String> {
    let s = s.trim();
    if s.is_empty() {
        return Err("empty timecode".to_string());
    }
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() > 3 {
        return Err(format!("timecode has too many fields: {s:?}"));
    }
    let mut total = 0.0_f64;
    let mut scale = 1.0_f64;
    for part in parts.iter().rev() {
        let value: f64 = part.parse().map_err(|_| format!("bad timecode {s:?}"))?;
        total += value * scale;
        scale *= 60.0;
    }
    if !total.is_finite() || total < 0.0 {
        return Err(format!("bad timecode {s:?}"));
    }
    Ok(Duration::from_secs_f64(total))
}

/// Format a duration as `HH:MM:SS.fff`. Exact integer math, no floats; the
/// millisecond is the text's resolution, so a finer duration rounds down.
pub fn format(d: Duration) -> String {
    let total_ms = d.as_secs().saturating_mul(1000) + u64::from(d.subsec_millis());
    let ms = total_ms % 1000;
    let s = (total_ms / 1000) % 60;
    let m = (total_ms / 60_000) % 60;
    let h = total_ms / 3_600_000;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing_is_lenient_and_formatting_is_canonical() {
        assert_eq!(parse("3.2").unwrap(), Duration::from_secs_f64(3.2));
        assert_eq!(parse("0.005").unwrap(), Duration::from_secs_f64(0.005));
        assert_eq!(parse("1:30").unwrap(), Duration::from_secs(90));
        assert_eq!(parse("00:00:03.200").unwrap(), Duration::from_secs_f64(3.2));
        assert_eq!(
            format(Duration::from_secs_f64(3.2)),
            "00:00:03.200"
        );
        assert_eq!(format(Duration::from_millis(5)), "00:00:00.005");
        assert_eq!(format(Duration::from_secs(3723)), "01:02:03.000");
        assert!(parse("").is_err());
        assert!(parse("1:2:3:4").is_err());
        assert!(parse("x").is_err());
        assert!(parse("-1").is_err());
    }
}
