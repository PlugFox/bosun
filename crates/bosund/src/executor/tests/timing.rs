//! The scene harness's own timing (#165): the slowdown factor of its waits, and a scene that
//! starts at a fixed local time of day.

use super::*;

/// A wait stretches by its factor: a check that holds only after 500 ms passes a 200 ms wait
/// slowed four times, and so does every other deadline (`mutant_test_slowdown_ignored` ignores
/// the factor: the wait times out, and a deadline is not stretched).
#[tokio::test]
async fn the_slowdown_factor_stretches_a_scene_wait() {
    assert_eq!(slowdown_of(None), 1);
    assert_eq!(slowdown_of(Some("")), 1);
    assert_eq!(slowdown_of(Some("3")), 3);
    assert_eq!(
        scaled_by(Duration::from_secs(15), 3),
        Duration::from_secs(45)
    );

    let start = Instant::now();
    wait_until("the flag", Duration::from_millis(200), 4, async || {
        start.elapsed() >= Duration::from_millis(500)
    })
    .await;
}

/// An invalid `BOSUN_TEST_SLOWDOWN` panics and names the variable, never falls back to 1.
#[test]
#[should_panic(expected = "BOSUN_TEST_SLOWDOWN")]
fn an_invalid_slowdown_factor_panics() {
    slowdown_of(Some("fast"));
}

/// A zero factor is refused like any other value that is not a positive integer.
#[test]
#[should_panic(expected = "BOSUN_TEST_SLOWDOWN")]
fn a_zero_slowdown_factor_panics() {
    slowdown_of(Some("0"));
}

/// The daily-cap scene started at 23:59 local parks the queue, and the jump to one minute past
/// midnight releases it (`mutant_test_daily_cap_auto_pause` keeps the automatic pause, which
/// lasts 30 minutes past the session: item 1 never runs).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daily_cap_scene_started_at_23_59_local_runs_after_midnight() {
    let minutes = 23 * 60 + 59;
    let clock = TestClock {
        system: SystemClock::new(),
        ahead_ms: AtomicI64::new(0),
    };
    clock.start_at_local(minutes);
    let now = clock.wall_ms();
    let of_day = now - bosun_proc::last_local_midnight(now);
    assert!(
        (of_day - minutes * 60_000).abs() < 1_000,
        "{of_day} ms past local midnight"
    );
    assert!(now >= SystemClock::new().wall_ms(), "behind the real clock");

    super::control::daily_cap_scene(minutes).await;
}
