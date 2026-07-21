// SPDX-License-Identifier: MPL-2.0

use std::{
    ops::{Add, Sub},
    sync::{LazyLock, OnceLock},
    time::{Duration, SystemTime},
};

use gst::prelude::MulDiv as _;
use log::{info, warn};

/// Number of seconds to add to UNIX time to convert to UTC.
///
/// * UTC and NTP time epoch:       01/01/1900 00:00:00.00
/// * UNIX time and PTP time epoch: 01/01/1970 00:00:00.00
///
/// Also to be considered:
///
/// * UNIX time and NTP time follow UTC in the sense that neither add leap seconds.
/// * PTP time includes a variable number of leap seconds. See [`UTC_TO_TAI_LEAP_SECONDS`].
pub const UNIX_TIME_TO_UTC_OFFSET_SECONDS: u64 = (365 * 70 + 17) * 24 * 60 * 60;

/// Number of seconds to add to UNIX time to convert to NTP time.
///
/// This is the same as [`UNIX_TO_UTC_EPOCH_OFFSET_SECONDS`]
pub const UNIX_TO_NTP_TIME_OFFSET_SECONDS: u64 = UNIX_TIME_TO_UTC_OFFSET_SECONDS;

/// Offset as a Duration between NTP time (UTC epoch) and UNIX time
///
/// See [`UNIX_TIME_TO_UTC_EPOCH_OFFSET_SECONDS`].
pub const UNIX_TO_NTP_TIME_OFFSET: Duration = Duration::from_secs(UNIX_TO_NTP_TIME_OFFSET_SECONDS);

/// Current variable number of leap seconds applicable to UTC compared to TAI
/// as of 07/2026, since 01/01/2017 00:00:00 UTC
pub const UTC_TO_TAI_LEAP_SECONDS_DEFAULT: u64 = 37;

/// Number of seconds to add to UNIX time to convert to PTP time.
///
/// * UNIX time follows UTC in the sense that neither add leap seconds.
/// * PTP time follows TAI with regard to leap seconds.
pub static UNIX_TO_PTP_TIME_OFFSET_SECONDS: LazyLock<u64> =
    LazyLock::new(|| *UTC_TO_TAI_LEAP_SECONDS);

/// Number of seconds to substract from NTP time to convert to PTP time.
///
/// * NTP time epoch is the same as UTC.
/// * PTP time epoch is the same as UNIX time.
/// * PTP time follows TAI with regard to leap seconds.
pub static NTP_TO_PTP_TIME_OFFSET_SECONDS: LazyLock<u64> =
    LazyLock::new(|| UNIX_TIME_TO_UTC_OFFSET_SECONDS - *UTC_TO_TAI_LEAP_SECONDS);

/// Env var for the number of leap seconds applicable to UTC compared to TAI
/// See [`UTC_TO_TAI_LEAP_SECONDS`] for more details.
pub const UTC_TO_TAI_LEAP_SECONDS_ENV_VAR: &str = "GST_UTC_TO_TAI_LEAP_SECONDS";

/// Number of current leap seconds applicable to UTC compared to TAI
///
/// This is the variable part of the offset between:
///
/// * TAI (also PTP time)
/// * and UTC (also NTP time, UNIX time).
///
/// Note that this doesn't account for the constant difference in epochs.
/// See: [`UNIX_TIME_TO_UTC_OFFSET_SECONDS`] & [`UNIX_TIME_TO_NTP_TIME_OFFSET_SECONDS`].
///
/// Defaults to [`UTC_TO_TAI_LEAP_SECONDS_DEFAULT`] if the environment variable
/// named by [`UTC_TO_TAI_LEAP_SECONDS_ENV_VAR`] is not defined or invalid.
pub static UTC_TO_TAI_LEAP_SECONDS: LazyLock<u64> = LazyLock::new(|| {
    const {
        assert!(
            UTC_TO_TAI_LEAP_SECONDS_DEFAULT <= UNIX_TIME_TO_UTC_OFFSET_SECONDS,
            "NTP time to PTP time code assumes UTC_TO_TAI_LEAP_SECONDS_DEFAULT <= UNIX_TO_UTC_EPOCH_OFFSET_S"
        );
    }

    match std::env::var(UTC_TO_TAI_LEAP_SECONDS_ENV_VAR) {
        Ok(val) => match val.parse() {
            Ok(val) => {
                if val > UNIX_TIME_TO_UTC_OFFSET_SECONDS {
                    warn!(
                        "{UTC_TO_TAI_LEAP_SECONDS_ENV_VAR}: invalid value \
                         greater than UNIX to UTC epoch seconds ({UNIX_TIME_TO_UTC_OFFSET_SECONDS}) \
                         => using default"
                    );

                    UTC_TO_TAI_LEAP_SECONDS_DEFAULT
                } else {
                    info!("{UTC_TO_TAI_LEAP_SECONDS_ENV_VAR} defined: {val}");
                    val
                }
            }
            Err(err) => {
                warn!(
                    "{UTC_TO_TAI_LEAP_SECONDS_ENV_VAR}: invalid value '{val}' ({err}) => using default"
                );
                UTC_TO_TAI_LEAP_SECONDS_DEFAULT
            }
        },
        Err(std::env::VarError::NotPresent) => {
            info!("{UTC_TO_TAI_LEAP_SECONDS_ENV_VAR} undefined => using default");
            UTC_TO_TAI_LEAP_SECONDS_DEFAULT
        }
        Err(err) => {
            warn!("{UTC_TO_TAI_LEAP_SECONDS_ENV_VAR}: invalid value ({err}) => using default");
            UTC_TO_TAI_LEAP_SECONDS_DEFAULT
        }
    }
});

// One second as nanoseconds
pub const SECOND: u64 = 1_000_000_000;

static CURRENT_TIME: OnceLock<SystemTime> = OnceLock::new();

pub fn get_or_init_current_time<'a>() -> &'a SystemTime {
    CURRENT_TIME.get_or_init(SystemTime::now)
}

pub fn ntp_era_from_system_time(st: &SystemTime) -> u64 {
    st.duration_since(SystemTime::UNIX_EPOCH)
        .expect("NTP time is before unix epoch?!")
        .add(UNIX_TO_NTP_TIME_OFFSET)
        .as_secs()
        / (1 << 32)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct NtpTime(u64);

impl NtpTime {
    pub fn from_duration(dur: Duration) -> Self {
        let seconds = dur.as_secs();
        let fractional = (dur.subsec_nanos() as u64)
            .mul_div_ceil(1 << 32, SECOND)
            .unwrap();

        let ntp = seconds << 32 | fractional;

        Self(ntp)
    }

    /// Converts to a duration relative to the prime epoch (1900-01-01 at 00:00).
    pub fn as_duration(&self) -> Duration {
        self.as_duration_with_current_time(get_or_init_current_time())
    }

    /// Converts to a duration relative to the prime epoch (1900-01-01 at 00:00).
    pub fn as_duration_with_current_time(&self, current_time: &SystemTime) -> Duration {
        let current_ntp_time = system_time_to_ntp_time_u64(*current_time);
        let mut timestamp_era = ntp_era_from_system_time(current_time);

        if current_ntp_time.0 > self.0 && current_ntp_time.0 - self.0 > 1 << 63 {
            timestamp_era += 1;
        } else if current_ntp_time.0 < self.0 && self.0 - current_ntp_time.0 > 1 << 63 {
            timestamp_era -= 1;
        }

        let nanos = self
            .0
            .mul_div_ceil(SECOND, 1 << 32)
            .expect("result doesn't fit?!");

        Duration::from_nanos(nanos) + Duration::from_secs(timestamp_era << 32)
    }

    /// Middle 32 bit of the NTP timestamp (16.16 seconds).
    pub fn as_u32(self) -> u32 {
        ((self.0 >> 16) & 0xffff_ffff) as u32
    }

    /// Full 64 bit NTP timestamp (32.32 seconds).
    pub fn as_u64(self) -> u64 {
        self.0
    }

    pub fn as_nanos(self) -> u64 {
        self.0
            .mul_div_ceil(SECOND, 1 << 32)
            .expect("result doesn't fit?!")
    }
}

impl Sub for NtpTime {
    type Output = NtpTime;
    fn sub(self, rhs: Self) -> Self::Output {
        NtpTime(self.0 - rhs.0)
    }
}

impl Add for NtpTime {
    type Output = NtpTime;
    fn add(self, rhs: Self) -> Self::Output {
        NtpTime(self.0 + rhs.0)
    }
}

impl std::fmt::Display for NtpTime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{:?}", self.as_duration()))
    }
}

pub fn system_time_to_ntp_time_u64(time: SystemTime) -> NtpTime {
    let dur = time
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("time is before unix epoch?!")
        + UNIX_TO_NTP_TIME_OFFSET;

    NtpTime::from_duration(dur)
}

impl From<u64> for NtpTime {
    fn from(value: u64) -> Self {
        NtpTime(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ntp_rollover() {
        let st: SystemTime = chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:15+00:00")
            .unwrap()
            .into();

        let ntpt = system_time_to_ntp_time_u64(st);

        assert_eq!(ntpt.as_u64(), (u32::MAX as u64) << 32);

        let st: SystemTime = chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:16+00:00")
            .unwrap()
            .into();

        let ntpt = system_time_to_ntp_time_u64(st);

        assert_eq!(ntpt.as_u64(), 0);
    }

    #[test]
    fn ntp_time_as_duration_before_rollover() {
        let current_time: SystemTime =
            chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:15+00:00")
                .unwrap()
                .into();

        let st: SystemTime = chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:15+00:00")
            .unwrap()
            .into();

        let ntpt = system_time_to_ntp_time_u64(st);

        assert_eq!(
            ntpt.as_duration_with_current_time(&current_time).as_secs(),
            4294967295
        );

        let st: SystemTime = chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:16+00:00")
            .unwrap()
            .into();

        let ntpt = system_time_to_ntp_time_u64(st);

        assert_eq!(
            ntpt.as_duration_with_current_time(&current_time).as_secs(),
            4294967296
        );
    }

    #[test]
    fn ntp_time_as_duration_after_rollover() {
        let current_time: SystemTime =
            chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:16+00:00")
                .unwrap()
                .into();

        let st: SystemTime = chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:15+00:00")
            .unwrap()
            .into();

        let ntpt = system_time_to_ntp_time_u64(st);

        assert_eq!(
            ntpt.as_duration_with_current_time(&current_time).as_secs(),
            4294967295
        );

        let st: SystemTime = chrono::DateTime::parse_from_rfc3339("2036-02-07T06:28:16+00:00")
            .unwrap()
            .into();

        let ntpt = system_time_to_ntp_time_u64(st);

        assert_eq!(
            ntpt.as_duration_with_current_time(&current_time).as_secs(),
            4294967296
        );
    }
}
