//! Generic portals provide consent, not a verifiable published-application identity.

pub(super) const APP_UNAVAILABLE: &str =
    "Wayland application sessions require a compositor backend that binds every selected \
     window to the launched application instance. The standard ScreenCast portal exposes \
     a consented PipeWire source, not a trusted PID/window identity, and RemoteDesktop \
     input is not application-scoped. Automatic APP targeting and scoped input are unavailable; \
     desktop/monitor capture is never used as a fallback.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SelectedSource {
    Window,
    Monitor,
    Unknown,
}

/// A PipeWire node is scoped to this portal session, not a global window ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct WindowSelection {
    pub node: u32,
    pub width: u32,
    pub height: u32,
}

pub(super) fn validate_window_selection(
    count: usize,
    kind: SelectedSource,
    node: u32,
    size: Option<(i32, i32)>,
) -> Result<WindowSelection, &'static str> {
    if count != 1 {
        return Err("consent must select exactly one window");
    }
    if kind != SelectedSource::Window {
        return Err("portal did not prove a window-only source; monitor fallback is forbidden");
    }
    let Some((width, height)) = size else {
        return Err("portal omitted the selected window's geometry");
    };
    if node == 0 || width <= 0 || height <= 0 {
        return Err("portal returned invalid window geometry or PipeWire node");
    }
    Ok(WindowSelection {
        node,
        width: width as u32,
        height: height as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monitor_or_missing_source_type_never_becomes_a_window() {
        for kind in [SelectedSource::Monitor, SelectedSource::Unknown] {
            assert!(validate_window_selection(1, kind, 4, Some((800, 600))).is_err());
        }
    }

    #[test]
    fn one_consented_window_keeps_its_session_scoped_pipewire_node() {
        assert_eq!(
            validate_window_selection(1, SelectedSource::Window, 42, Some((800, 600))),
            Ok(WindowSelection {
                node: 42,
                width: 800,
                height: 600
            })
        );
    }

    #[test]
    fn ambiguous_or_invalid_selections_fail_closed() {
        for count in [0, 2] {
            assert!(
                validate_window_selection(count, SelectedSource::Window, 4, Some((1, 1))).is_err()
            );
        }
        for size in [None, Some((0, 20)), Some((20, -1))] {
            assert!(validate_window_selection(1, SelectedSource::Window, 4, size).is_err());
        }
        assert!(validate_window_selection(1, SelectedSource::Window, 0, Some((1, 1))).is_err());
    }

    #[test]
    fn consent_does_not_claim_application_binding() {
        assert!(APP_UNAVAILABLE.contains("not a trusted PID/window identity"));
        assert!(APP_UNAVAILABLE.contains("never used as a fallback"));
    }
}
