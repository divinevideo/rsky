use crate::RFC3339_VARIANT;
use anyhow::Result;
use chrono::offset::Utc as UtcOffset;
use chrono::{DateTime, NaiveDateTime, Utc};
use std::time::SystemTime;

pub const SECOND: i32 = 1000;
pub const MINUTE: i32 = SECOND * 60;
pub const HOUR: i32 = MINUTE * 60;
pub const DAY: i32 = HOUR * 24;

pub fn less_than_ago_s(time: DateTime<UtcOffset>, range: i32) -> bool {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("timestamp in micros since UNIX epoch")
        .as_secs() as usize;
    let x = time.timestamp() as usize + range as usize;
    now < x
}

pub fn from_str_to_micros(str: &String) -> i64 {
    NaiveDateTime::parse_from_str(str, RFC3339_VARIANT)
        .unwrap()
        .and_utc()
        .timestamp_micros()
}

pub fn from_str_to_millis(str: &String) -> Result<i64> {
    Ok(NaiveDateTime::parse_from_str(str, RFC3339_VARIANT)?
        .and_utc()
        .timestamp_millis())
}

pub fn from_str_to_utc(str: &String) -> DateTime<UtcOffset> {
    NaiveDateTime::parse_from_str(str, RFC3339_VARIANT)
        .unwrap()
        .and_utc()
}

pub fn from_micros_to_utc(micros: i64) -> DateTime<UtcOffset> {
    // NaiveDateTime::from_timestamp takes SECONDS; passing microseconds here
    // used to overflow chrono's range and panic ("invalid or out-of-range
    // datetime"), which broke every refreshSession call.
    DateTime::from_timestamp_micros(micros)
        .unwrap_or_else(|| panic!("timestamp out of range: {micros} micros"))
}

pub fn from_micros_to_str(micros: i64) -> String {
    format!("{}", from_micros_to_utc(micros).format(RFC3339_VARIANT))
}

pub fn from_millis_to_utc(millis: i64) -> DateTime<UtcOffset> {
    DateTime::<Utc>::from_utc(NaiveDateTime::from_timestamp_millis(millis).unwrap(), Utc)
}

pub fn from_millis_to_str(millis: i64) -> String {
    format!("{}", from_millis_to_utc(millis).format(RFC3339_VARIANT))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// A fixed, realistic "current era" instant. Every timestamp the session
    /// code passes to `from_micros_to_utc` looks like this: ~1.7e15 microseconds.
    fn sample() -> DateTime<UtcOffset> {
        Utc.with_ymd_and_hms(2026, 7, 11, 12, 34, 56).unwrap()
    }

    /// Regression: `from_micros_to_utc` used to hand its MICROsecond argument to
    /// `NaiveDateTime::from_timestamp`, which interprets it as SECONDS. Any real
    /// timestamp (~1.7e15) is then ~55 million years past chrono's range, so the
    /// call panicked with "invalid or out-of-range datetime". `rotate_refresh_token`
    /// reaches this via `from_micros_to_str`, so `com.atproto.server.refreshSession`
    /// returned a 500 on every single call.
    #[test]
    fn from_micros_to_utc_does_not_panic_on_a_current_era_timestamp() {
        let expected = sample();
        let micros = expected.timestamp_micros();
        // Guard the guard: if this is not a ~16-digit micros value, the test below
        // would be exercising a range that never occurs in production.
        assert!(
            micros > 1_700_000_000_000_000,
            "sample must be a realistic microsecond timestamp, got {micros}"
        );

        let actual = from_micros_to_utc(micros);

        assert_eq!(actual, expected);
        assert_eq!(actual.timestamp_micros(), micros);
    }

    /// Sub-second precision must survive the conversion, and the stray hard-coded
    /// 230ms the old implementation stamped onto every value must be gone.
    #[test]
    fn from_micros_to_utc_preserves_sub_second_precision() {
        let expected = sample() + chrono::Duration::microseconds(789_012);

        let actual = from_micros_to_utc(expected.timestamp_micros());

        assert_eq!(actual, expected);
        assert_eq!(actual.timestamp_subsec_micros(), 789_012);
    }

    /// `from_micros_to_str` is what actually writes `refresh_token.expiresAt`.
    /// It must render the instant it was given -- not panic, and not the old
    /// hard-coded ".230Z" millisecond suffix.
    #[test]
    fn from_micros_to_str_renders_the_given_instant() {
        let formatted = from_micros_to_str(sample().timestamp_micros());

        assert_eq!(formatted, "2026-07-11T12:34:56.000Z");
    }

    /// The store/read contract used by `store_refresh_token` (writes via
    /// `from_micros_to_utc`) and `rotate_refresh_token` (reads via
    /// `from_str_to_micros`). These two must agree on the unit, or the refresh
    /// grace period is computed against a garbage expiry.
    #[test]
    fn micros_to_str_round_trips_through_from_str_to_micros() {
        let micros = sample().timestamp_micros();

        let round_tripped = from_str_to_micros(&from_micros_to_str(micros));

        assert_eq!(round_tripped, micros);
    }

    /// The exact call the session code makes: `SystemTime::now()` -> micros ->
    /// back to a `DateTime`. This is the panic that took down refreshSession.
    #[test]
    fn from_micros_to_utc_accepts_now() {
        let now = Utc::now();

        let round_tripped = from_micros_to_utc(now.timestamp_micros());

        assert_eq!(round_tripped.timestamp_micros(), now.timestamp_micros());
    }

    /// The time constants are expressed in MILLIseconds. Callers that add them to
    /// a microsecond timestamp must scale by 1000 -- `rotate_refresh_token` did
    /// not, which shrank the 2h refresh grace window to 7.2 seconds. Pin the unit
    /// so the constants cannot be silently redefined out from under that caller.
    #[test]
    fn time_constants_are_milliseconds() {
        assert_eq!(SECOND, 1_000);
        assert_eq!(MINUTE, 60_000);
        assert_eq!(HOUR, 3_600_000);
        assert_eq!(DAY, 86_400_000);
    }
}
