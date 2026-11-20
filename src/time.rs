//! Clock and the `date` formats the client prints.

use jiff::{Timestamp, tz::TimeZone};

pub fn now() -> i64 {
    Timestamp::now().as_second()
}

fn local(epoch: i64, fmt: &str) -> String {
    match Timestamp::from_second(epoch) {
        Ok(ts) => ts
            .to_zoned_datetime(TimeZone::system())
            .strftime(fmt)
            .to_string(),
        Err(_) => String::new(),
    }
}

/// `date -d @EPOCH +%H:%M`.
pub fn hm(epoch: i64) -> String {
    local(epoch, "%H:%M")
}

/// `date -d @EPOCH '+%F %H:%M'`.
pub fn date_hm(epoch: i64) -> String {
    local(epoch, "%Y-%m-%d %H:%M")
}

/// `date -d @EPOCH '+%a %H:%M'`.
pub fn weekday_hm(epoch: i64) -> String {
    local(epoch, "%a %H:%M")
}

/// `date +%T`.
pub fn now_hms() -> String {
    local(now(), "%H:%M:%S")
}

/// `date -u +%FT%TZ`.
pub fn utc_iso() -> String {
    Timestamp::now().strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// jq's `fromdateiso8601` after the client's normalization: the fraction
/// removed and exactly one trailing Z. None where jq yields null.
pub fn created_epoch(created_at: &str) -> Option<i64> {
    let re_frac = regex::Regex::new(r"\.[0-9]+").ok()?;
    let s = re_frac.replacen(created_at, 1, "").into_owned();
    let s = match s.strip_suffix('Z') {
        Some(rest) => format!("{rest}Z"),
        None => format!("{s}Z"),
    };
    if s == "Z" {
        return None;
    }
    let re = regex::Regex::new(
        r"^\s*([0-9]{1,4})-\s*([0-9]{1,2})-\s*([0-9]{1,2})T\s*([0-9]{1,2}):\s*([0-9]{1,2}):\s*([0-9]{1,2})Z$",
    )
    .ok()?;
    let c = re.captures(&s)?;
    let n = |i: usize| c[i].parse::<i64>().ok();
    // The fallible constructor: an impossible date is null, as in jq.
    let dt = jiff::civil::DateTime::new(
        i16::try_from(n(1)?).ok()?,
        i8::try_from(n(2)?).ok()?,
        i8::try_from(n(3)?).ok()?,
        i8::try_from(n(4)?).ok()?,
        i8::try_from(n(5)?).ok()?,
        i8::try_from(n(6)?).ok()?,
        0,
    )
    .ok()?;
    dt.to_zoned_datetime(TimeZone::UTC)
        .ok()
        .map(|z| z.timestamp().as_second())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn created() {
        assert_eq!(created_epoch("1970-01-01T00:01:00Z"), Some(60));
        assert_eq!(created_epoch("1970-01-01T00:01:00.123456"), Some(60));
        assert_eq!(created_epoch(""), None);
        assert_eq!(created_epoch("1970-01-01T00:01:00+00:00"), None);
        assert_eq!(created_epoch("1970-13-01T00:01:00Z"), None);
    }
}
