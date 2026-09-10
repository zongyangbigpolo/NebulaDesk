//! Pure ownership and geometry checks shared by discovery, capture and input.

pub(super) fn owns_window(
    launched_pid: u32,
    window_pid: u32,
    process_alive: bool,
    generation_matches: bool,
) -> bool {
    launched_pid != 0 && launched_pid == window_pid && process_alive && generation_matches
}

pub(super) fn integrity_permits(agent: u32, target: u32) -> bool {
    // Do not turn an elevated agent into a cross-integrity input/capture broker.
    agent == target
}

pub(super) fn capture_available(visible: bool, minimized: bool) -> bool {
    visible && !minimized
}

pub(super) fn window_point(bounds: [i32; 4], x: f32, y: f32) -> Result<(i32, i32), &'static str> {
    let [left, top, right, bottom] = bounds;
    let width = i64::from(right) - i64::from(left);
    let height = i64::from(bottom) - i64::from(top);
    if width <= 0 || height <= 0 {
        return Err("window has empty or inverted geometry");
    }
    if !x.is_finite() || !y.is_finite() || !(0.0..=1.0).contains(&x) || !(0.0..=1.0).contains(&y) {
        return Err("pointer must be finite normalized window coordinates");
    }
    Ok((
        (i64::from(left) + (f64::from(x) * (width - 1) as f64).round() as i64) as i32,
        (i64::from(top) + (f64::from(y) * (height - 1) as f64).round() as i64) as i32,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ownership_requires_live_launched_process_and_window_generation() {
        assert!(owns_window(12, 12, true, true));
        assert!(!owns_window(12, 13, true, true));
        assert!(!owns_window(12, 12, false, true));
        assert!(!owns_window(12, 12, true, false));
        assert!(!owns_window(0, 0, true, true));
    }

    #[test]
    fn differing_integrity_levels_are_refused_in_both_directions() {
        assert!(integrity_permits(0x2000, 0x2000));
        assert!(!integrity_permits(0x2000, 0x3000));
        assert!(!integrity_permits(0x3000, 0x2000));
    }

    #[test]
    fn hidden_and_minimized_windows_suspend_capture_without_revoking_ownership() {
        assert!(capture_available(true, false));
        for (visible, minimized) in [(false, false), (false, true), (true, true)] {
            assert!(!capture_available(visible, minimized));
            assert!(owns_window(12, 12, true, true));
        }
    }

    #[test]
    fn geometry_uses_live_window_bounds_including_negative_desktop_origins() {
        assert_eq!(
            window_point([-800, -100, 0, 500], 0.0, 0.0),
            Ok((-800, -100))
        );
        assert_eq!(window_point([-800, -100, 0, 500], 1.0, 1.0), Ok((-1, 499)));
        assert_eq!(window_point([100, 200, 101, 201], 0.5, 0.5), Ok((100, 200)));
    }

    #[test]
    fn invalid_or_outside_coordinates_are_not_clamped_into_a_window() {
        for (x, y) in [
            (f32::NAN, 0.0),
            (0.0, f32::INFINITY),
            (-0.1, 0.5),
            (0.5, 1.1),
        ] {
            assert!(window_point([0, 0, 800, 600], x, y).is_err());
        }
        assert!(window_point([0, 0, 0, 100], 0.5, 0.5).is_err());
        assert!(window_point([100, 0, 0, 100], 0.5, 0.5).is_err());
    }
}
