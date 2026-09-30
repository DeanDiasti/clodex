use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

pub const RETRY_INITIAL_DELAY_MS: u64 = 2000;
pub const RETRY_MAX_DELAY_MS: u64 = 30_000;
pub const RETRY_BACKOFF_FACTOR: u64 = 2;
pub const MAX_RATE_LIMIT_RETRIES: u32 = 3;

#[derive(Debug, Clone, Copy)]
pub struct BackoffOutcome {
    pub wait_ms: u64,
    pub exceeds_budget: bool,
}

pub fn should_retry_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

pub fn compute_backoff_delay(attempt: u32, retry_after: Option<&str>) -> BackoffOutcome {
    if let Some(target_ms) = retry_after.and_then(retry_after_ms) {
        return BackoffOutcome {
            wait_ms: target_ms.min(RETRY_MAX_DELAY_MS),
            exceeds_budget: target_ms > RETRY_MAX_DELAY_MS,
        };
    }

    let mut exp =
        RETRY_INITIAL_DELAY_MS.saturating_mul(RETRY_BACKOFF_FACTOR.saturating_pow(attempt));
    if exp > RETRY_MAX_DELAY_MS {
        exp = RETRY_MAX_DELAY_MS;
    }
    let jitter = exp / 2;
    let wait_ms = (exp / 2) + (jitter / 2);
    BackoffOutcome {
        wait_ms,
        exceeds_budget: false,
    }
}

/// The wait a `Retry-After` value asks for: delay seconds, or an HTTP-date
/// still in the future. Anything else leaves the exponential schedule in
/// charge, so a negative or past value cannot trigger an immediate retry.
fn retry_after_ms(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if let Ok(secs) = raw.parse::<f64>() {
        return (secs.is_finite() && secs >= 0.0).then(|| (secs * 1000.0).ceil() as u64);
    }
    let date =
        time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc2822).ok()?;
    let remaining = date - time::OffsetDateTime::now_utc();
    remaining
        .is_positive()
        .then(|| u64::try_from(remaining.whole_milliseconds()).unwrap_or(u64::MAX))
}

static ZERO_RETRY_DELAY_FOR_TESTS: AtomicBool = AtomicBool::new(false);

/// Make retry sleeps return immediately so exhaustion paths can be exercised
/// in tests without waiting out the real backoff schedule.
pub fn set_zero_retry_delay_for_tests(enabled: bool) {
    ZERO_RETRY_DELAY_FOR_TESTS.store(enabled, Ordering::SeqCst);
}

pub async fn sleep(ms: u64) {
    if ZERO_RETRY_DELAY_FOR_TESTS.load(Ordering::SeqCst) {
        return;
    }
    tokio::time::sleep(Duration::from_millis(ms)).await;
}

#[cfg(test)]
pub async fn retry_on_statuses<T, E, F>(mut next: F) -> Result<T, E>
where
    E: std::fmt::Debug,
    F: FnMut(u32) -> Result<T, E>,
{
    let mut attempt = 0;
    loop {
        attempt += 1;
        if attempt > MAX_RATE_LIMIT_RETRIES + 1 {
            break;
        }
        match next(attempt) {
            Ok(value) => return Ok(value),
            Err(err) if attempt <= MAX_RATE_LIMIT_RETRIES + 1 => {
                if attempt > MAX_RATE_LIMIT_RETRIES {
                    return Err(err);
                }
                sleep(compute_backoff_delay(attempt, None).wait_ms).await;
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!()
}
