use crate::config::Config;
use crate::gesture::distance::{DistanceManager, RELEASE_FINGERS_THRESHOLD_MS};
use crate::gesture::finger::FingerCounter;
use crate::hid::types::TouchpadContact;
use crate::input::mouse::MouseSimulator;
use crate::speed;

/// Three-finger drag state machine.
///
/// Ported from ThreeFingerDrag.cs. Handles start/move/stop of a drag operation
/// when exactly 3 fingers move on the touchpad.
pub struct ThreeFingerDrag {
    is_dragging: bool,
    distance_manager: DistanceManager,
    finger_counter: FingerCounter,
    // Cursor averaging state
    averaging_x: f32,
    averaging_y: f32,
    averaging_count: u32,
    /// Set when the fingers leave the pad and the drag-end timer has been armed.
    /// Guards against repeated empty frames extending the release window forever.
    release_timer_armed: bool,
}

impl ThreeFingerDrag {
    pub fn new() -> Self {
        Self {
            is_dragging: false,
            distance_manager: DistanceManager::new(),
            finger_counter: FingerCounter::new(),
            averaging_x: 0.0,
            averaging_y: 0.0,
            averaging_count: 0,
            release_timer_armed: false,
        }
    }

    pub fn is_dragging(&self) -> bool {
        self.is_dragging
    }

    /// Process a new frame of touchpad contacts.
    ///
    /// Returns the action the caller should apply to the drag-end timer.
    pub fn on_touchpad_contact(
        &mut self,
        device_id: &str,
        config: &Config,
        mouse: &mut MouseSimulator,
        old_contacts: &[TouchpadContact],
        contacts: &[TouchpadContact],
        elapsed: i64,
    ) -> TimerAction {
        // All fingers lifted: keep the button pressed for the release delay so
        // the user can put the fingers back and continue the same drag.
        if contacts.is_empty() {
            if !self.is_dragging {
                return TimerAction::None;
            }
            if !config.allow_release_and_restart {
                self.stop_drag(config, mouse);
                return TimerAction::StopTimer;
            }
            if self.release_timer_armed {
                return TimerAction::None;
            }
            self.release_timer_armed = true;
            let delay = self.get_release_delay(config);
            tracing::debug!("HOLD DRAG, waiting {}ms for fingers to return", delay);
            crate::debug::log_debug("DRAG", format!("Hold drag after release, waiting {}ms", delay));
            return TimerAction::StartTimer(delay);
        }

        let has_fingers_released = elapsed > RELEASE_FINGERS_THRESHOLD_MS;
        let mut are_contacts_ids_common =
            FingerCounter::are_contacts_ids_common(old_contacts, contacts);

        let (_longest_id, longest_dist_delta, longest_dist_2d) = self
            .distance_manager
            .get_longest_dist_2d(old_contacts, contacts, has_fingers_released);

        // Fingers were put back while the release timer still holds the drag:
        // treat this as a continuation so movement resumes immediately instead
        // of having to cross the start threshold again.
        let resumed = self.is_dragging
            && contacts.len() == 3
            && !are_contacts_ids_common;
        if resumed {
            are_contacts_ids_common = true;
            self.release_timer_armed = false;
            self.finger_counter
                .restore_continuation(contacts.len() as i32);
            self.distance_manager.trust_contacts(contacts);
            tracing::debug!("RESUME DRAG, click still down");
            crate::debug::log_debug("DRAG", "Resume drag after fingers returned");
        }

        let (fingers_count, short_delay_moving, long_delay_moving, original_count) = self
            .finger_counter
            .count_moving_fingers(
                device_id,
                config,
                contacts,
                are_contacts_ids_common,
                longest_dist_2d,
                has_fingers_released,
            );

        // START drag
        if fingers_count >= 3
            && are_contacts_ids_common
            && long_delay_moving == 3
            && original_count == 3
            && !self.is_dragging
        {
            self.is_dragging = true;
            self.release_timer_armed = false;
            mouse.drag_down(config.drag_button);
            tracing::debug!("START DRAG, click down");
            crate::debug::log_debug("DRAG", "Start drag (button down)");
        }
        // STOP drag
        else if self.is_dragging
            && (short_delay_moving < 2 || (original_count != 3 && original_count >= 2))
        {
            tracing::debug!("STOP DRAG, click up");
            crate::debug::log_debug("DRAG", "Stop drag (button up)");
            self.stop_drag(config, mouse);
            return TimerAction::StopTimer;
        }
        // MOVE while dragging
        else if fingers_count >= 2
            && original_count == 3
            && are_contacts_ids_common
            && self.is_dragging
        {
            let device_config = config.device_config(device_id);
            if device_config.cursor_move {
                // Discard large jumps if configured
                if config.max_finger_move_distance != 0
                    && longest_dist_2d > config.max_finger_move_distance as f32
                {
                    tracing::debug!(
                        "DISCARDING MOVE, (x, y) = ({}, {})",
                        longest_dist_delta.0,
                        longest_dist_delta.1
                    );
                } else if longest_dist_delta.0 != 0.0 || longest_dist_delta.1 != 0.0 {
                    let delta = speed::apply_speed_and_accel(
                        device_id,
                        config,
                        longest_dist_delta,
                        elapsed,
                    );

                    if config.cursor_averaging > 1 {
                        self.averaging_x += delta.0;
                        self.averaging_y += delta.1;
                        self.averaging_count += 1;
                        if self.averaging_count >= config.cursor_averaging {
                            mouse.shift_cursor_position(self.averaging_x, self.averaging_y);
                            self.averaging_x = 0.0;
                            self.averaging_y = 0.0;
                            self.averaging_count = 0;
                        }
                    } else {
                        mouse.shift_cursor_position(delta.0, delta.1);
                    }
                }
            }
        }

        // Only the hold-after-lift window needs a timer. A timer running while
        // "release and restart" is off releases the button on any gap between
        // reports; the immediate re-press at the same spot becomes a click,
        // and a second pair opens the dragged item.
        if self.is_dragging && config.allow_release_and_restart {
            return TimerAction::StartTimer(self.get_release_delay(config));
        }

        TimerAction::None
    }

    /// Called when the drag-end timer fires (no input for release_delay_ms).
    pub fn on_timer_fired(&mut self, config: &Config, mouse: &mut MouseSimulator) {
        self.release_timer_armed = false;
        if self.is_dragging {
            tracing::debug!("STOP DRAG FROM TIMER, click up");
            crate::debug::log_debug("DRAG", "Stop drag after release timeout (button up)");
            self.stop_drag(config, mouse);
        }
    }

    /// Force stop the drag (e.g., when switching to a 4-finger gesture).
    pub fn force_stop(&mut self, config: &Config, mouse: &mut MouseSimulator) {
        if self.is_dragging {
            self.stop_drag(config, mouse);
        }
        self.release_timer_armed = false;
    }

    /// Reset all internal state.
    pub fn reset(&mut self) {
        self.is_dragging = false;
        self.release_timer_armed = false;
        self.distance_manager.reset();
        self.finger_counter.reset();
        self.averaging_x = 0.0;
        self.averaging_y = 0.0;
        self.averaging_count = 0;
    }

    fn stop_drag(&mut self, config: &Config, mouse: &mut MouseSimulator) {
        self.is_dragging = false;
        self.release_timer_armed = false;
        mouse.drag_up(config.drag_button);
    }

    pub(crate) fn get_release_delay(&self, config: &Config) -> u32 {
        if config.allow_release_and_restart {
            config.release_delay_ms.max(RELEASE_FINGERS_THRESHOLD_MS as u32)
        } else {
            RELEASE_FINGERS_THRESHOLD_MS as u32
        }
    }
}

/// Actions the caller should take regarding the drag-end timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerAction {
    /// No timer action needed.
    None,
    /// Start/restart the drag-end timer with the given delay in ms.
    StartTimer(u32),
    /// Stop the drag-end timer.
    StopTimer,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DeviceDragConfig, DragButton};

    fn test_config(allow_release_and_restart: bool) -> Config {
        let mut config = Config::default();
        config.drag_button = DragButton::None;
        config.allow_release_and_restart = allow_release_and_restart;
        config.release_delay_ms = 500;
        config.start_threshold = 40.0;
        config.stop_threshold = 5.0;
        config.device_configs.insert(
            "default".into(),
            DeviceDragConfig {
                cursor_move: false,
                ..Default::default()
            },
        );
        config
    }

    fn contacts(offset: i32) -> Vec<TouchpadContact> {
        vec![
            TouchpadContact::new(1, 100 + offset, 100),
            TouchpadContact::new(2, 200 + offset, 200),
            TouchpadContact::new(3, 300 + offset, 300),
        ]
    }

    fn start_drag(drag: &mut ThreeFingerDrag, config: &Config, mouse: &mut MouseSimulator) {
        let mut old: Vec<TouchpadContact> = Vec::new();
        for i in 0..6 {
            let new = contacts(i * 10);
            drag.on_touchpad_contact("test", config, mouse, &old, &new, 10);
            old = new;
            if i == 0 {
                // Let the new contacts leave the 15ms quarantine.
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        assert!(drag.is_dragging(), "drag should start once the start threshold is crossed");
    }

    #[test]
    fn lift_keeps_drag_alive_until_timer_fires() {
        let config = test_config(true);
        let mut mouse = MouseSimulator::new();
        let mut drag = ThreeFingerDrag::new();
        start_drag(&mut drag, &config, &mut mouse);

        let lifted = drag.on_touchpad_contact("test", &config, &mut mouse, &contacts(50), &[], 10);
        assert_eq!(lifted, TimerAction::StartTimer(500));
        assert!(drag.is_dragging());

        // Repeated empty frames must not extend the release window.
        let repeated = drag.on_touchpad_contact("test", &config, &mut mouse, &[], &[], 10);
        assert_eq!(repeated, TimerAction::None);
        assert!(drag.is_dragging());

        drag.on_timer_fired(&config, &mut mouse);
        assert!(!drag.is_dragging());
    }

    #[test]
    fn lift_stops_drag_immediately_when_restart_is_disabled() {
        let config = test_config(false);
        let mut mouse = MouseSimulator::new();
        let mut drag = ThreeFingerDrag::new();
        start_drag(&mut drag, &config, &mut mouse);

        let lifted = drag.on_touchpad_contact("test", &config, &mut mouse, &contacts(50), &[], 10);
        assert_eq!(lifted, TimerAction::StopTimer);
        assert!(!drag.is_dragging());
    }

    #[test]
    fn disabled_restart_does_not_arm_timer_while_fingers_stay_down() {
        let config = test_config(false);
        let mut mouse = MouseSimulator::new();
        let mut drag = ThreeFingerDrag::new();
        start_drag(&mut drag, &config, &mut mouse);

        // A pause in reporting while the fingers are still down must not
        // release the button: the release/repress pair is seen by Windows as
        // a click, and a repeated pair opens the dragged item.
        let action = drag.on_touchpad_contact(
            "test",
            &config,
            &mut mouse,
            &contacts(50),
            &contacts(52),
            10,
        );
        assert_eq!(action, TimerAction::None);
        assert!(drag.is_dragging());

        // The drag must only end on a real contact loss.
        let lifted = drag.on_touchpad_contact("test", &config, &mut mouse, &contacts(52), &[], 10);
        assert_eq!(lifted, TimerAction::StopTimer);
        assert!(!drag.is_dragging());
    }

    #[test]
    fn retouch_within_release_window_resumes_without_start_threshold() {
        let config = test_config(true);
        let mut mouse = MouseSimulator::new();
        let mut drag = ThreeFingerDrag::new();
        start_drag(&mut drag, &config, &mut mouse);

        drag.on_touchpad_contact("test", &config, &mut mouse, &contacts(50), &[], 10);
        assert!(drag.is_dragging());

        // Fingers come back within the release window.
        let resumed =
            drag.on_touchpad_contact("test", &config, &mut mouse, &[], &contacts(60), 200);
        assert!(drag.is_dragging());
        assert_eq!(resumed, TimerAction::StartTimer(500));
        assert_eq!(drag.finger_counter.original_count(), 3);

        // A move well below the start threshold must already be treated as a drag move.
        let continued =
            drag.on_touchpad_contact("test", &config, &mut mouse, &contacts(60), &contacts(62), 10);
        assert!(drag.is_dragging());
        assert_eq!(continued, TimerAction::StartTimer(500));
        assert_eq!(drag.finger_counter.original_count(), 3);
    }

    #[test]
    fn two_fingers_can_continue_an_active_drag() {
        let config = test_config(true);
        let mut mouse = MouseSimulator::new();
        let mut drag = ThreeFingerDrag::new();
        start_drag(&mut drag, &config, &mut mouse);

        let two = vec![
            TouchpadContact::new(1, 150, 100),
            TouchpadContact::new(2, 250, 200),
        ];
        drag.on_touchpad_contact("test", &config, &mut mouse, &contacts(50), &two, 10);
        assert!(drag.is_dragging());

        let two_moved = vec![
            TouchpadContact::new(1, 152, 100),
            TouchpadContact::new(2, 252, 200),
        ];
        let action = drag.on_touchpad_contact("test", &config, &mut mouse, &two, &two_moved, 10);
        assert_eq!(action, TimerAction::StartTimer(500));
        assert!(drag.is_dragging());
    }

    #[test]
    fn two_fingers_continue_drag_without_timer_when_restart_is_disabled() {
        let config = test_config(false);
        let mut mouse = MouseSimulator::new();
        let mut drag = ThreeFingerDrag::new();
        start_drag(&mut drag, &config, &mut mouse);

        // A momentary 2-finger report must not end the drag, otherwise the
        // release/repress pair becomes a click.
        let two = vec![
            TouchpadContact::new(1, 150, 100),
            TouchpadContact::new(2, 250, 200),
        ];
        drag.on_touchpad_contact("test", &config, &mut mouse, &contacts(50), &two, 10);
        assert!(drag.is_dragging());

        let two_moved = vec![
            TouchpadContact::new(1, 152, 100),
            TouchpadContact::new(2, 252, 200),
        ];
        let action = drag.on_touchpad_contact("test", &config, &mut mouse, &two, &two_moved, 10);
        assert_eq!(action, TimerAction::None);
        assert!(drag.is_dragging());
    }
}
