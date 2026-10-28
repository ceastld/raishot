//! 与平台和界面无关的输入录制与触发器状态机。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    X1,
    X2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum Button {
    Key(u16),
    Mouse(MouseButton),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Axis {
    Vertical,
    Horizontal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WheelDirection {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputKind {
    Down(Button),
    Up(Button),
    Wheel { axis: Axis, delta: i32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InputEvent {
    pub kind: InputKind,
    pub injected: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Activation {
    Press(Button),
    Wheel(WheelDirection),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TriggerSpec {
    pub id: u64,
    pub held: Vec<Button>,
    pub activation: Activation,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Decision {
    pub consume: bool,
    pub replay: Vec<InputKind>,
    pub triggered: Vec<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistrationError {
    TooManyBindings,
    TooManyHeld {
        id: u64,
    },
    InvalidButton {
        id: u64,
        button: Button,
    },
    DuplicateId {
        id: u64,
    },
    DuplicateHeld {
        id: u64,
    },
    TerminalInHeld {
        id: u64,
    },
    DuplicateTrigger {
        first: u64,
        second: u64,
    },
    TerminalIsPrefix {
        terminal_binding: u64,
        prefix_binding: u64,
    },
}

impl fmt::Display for RegistrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::TooManyBindings => "最多可添加 64 个触发器，请删除不再使用的触发器后重试。",
            Self::TooManyHeld { .. } => "每个组合最多包含 8 个前缀按键，请减少同时按住的按键。",
            Self::InvalidButton { .. } => "组合包含不支持的按键，请重新录制。",
            Self::DuplicateId { .. } => "触发器编号重复，请重新添加该触发器。",
            Self::DuplicateHeld { .. } => "组合中的前缀按键重复，请重新录制。",
            Self::TerminalInHeld { .. } => "目标键不能同时作为当前组合的前缀，请调整组合后重试。",
            Self::DuplicateTrigger { .. } => "已存在相同组合，请编辑已有触发器。",
            Self::TerminalIsPrefix { .. } => {
                "目标键已作为另一组合的前缀，请调整组合或删除冲突触发器。"
            }
        })
    }
}
impl std::error::Error for RegistrationError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PressState {
    Pending,
    Consumed,
    Passed,
    Draining,
}

/// 每个组合要求当前按下集合与 held 完全一致；多余按键不会被静默忽略。
pub struct Engine {
    bindings: Vec<TriggerSpec>,
    pressed: BTreeMap<Button, PressState>,
    pending: Vec<InputKind>,
    wheel: [i64; 4],
    enabled: bool,
}

impl Engine {
    pub fn new(bindings: Vec<TriggerSpec>) -> Result<Self, RegistrationError> {
        Ok(Self {
            bindings: validate(bindings)?,
            pressed: BTreeMap::new(),
            pending: Vec::new(),
            wheel: [0; 4],
            enabled: true,
        })
    }
    pub fn replace_bindings(
        &mut self,
        bindings: Vec<TriggerSpec>,
    ) -> Result<(), RegistrationError> {
        let bindings = validate(bindings)?;
        self.cancel();
        self.bindings = bindings;
        Ok(())
    }
    pub fn set_enabled(&mut self, enabled: bool) {
        if self.enabled != enabled {
            self.cancel();
            self.enabled = enabled;
        }
    }
    pub fn cancel(&mut self) {
        self.pending.clear();
        self.wheel = [0; 4];
        for state in self.pressed.values_mut() {
            if matches!(*state, PressState::Pending | PressState::Consumed) {
                *state = PressState::Draining;
            }
        }
    }
    pub fn process(&mut self, event: InputEvent) -> Decision {
        if event.injected {
            return Decision::default();
        }
        match event.kind {
            InputKind::Down(button) => self.down(button),
            InputKind::Up(button) => self.up(button),
            InputKind::Wheel { axis, delta } => self.scroll(axis, delta),
        }
    }

    fn down(&mut self, button: Button) -> Decision {
        if let Some(state) = self.pressed.get(&button) {
            // 自动重复不是第二次按压；已消费的按压也不能通过重复事件泄漏。
            return Decision {
                consume: *state != PressState::Passed,
                ..Decision::default()
            };
        }
        if !self.enabled {
            self.pressed.insert(button, PressState::Passed);
            return Decision::default();
        }
        if let Some(id) = self
            .bindings
            .iter()
            .find(|binding| {
                binding.activation == Activation::Press(button) && self.held_matches(&binding.held)
            })
            .map(|binding| binding.id)
        {
            self.commit_pending();
            self.pressed.insert(button, PressState::Consumed);
            return Decision {
                consume: true,
                triggered: vec![id],
                ..Decision::default()
            };
        }
        let eligible = self
            .pressed
            .values()
            .all(|state| matches!(state, PressState::Pending | PressState::Consumed));
        let extends_prefix = eligible
            && self.bindings.iter().any(|binding| {
                binding.held.contains(&button)
                    && self
                        .pressed
                        .keys()
                        .all(|pressed| binding.held.contains(pressed))
            });
        if extends_prefix {
            self.wheel = [0; 4];
            self.pressed.insert(button, PressState::Pending);
            self.pending.push(InputKind::Down(button));
            return Decision {
                consume: true,
                ..Decision::default()
            };
        }
        let replay = self.flush_pending();
        self.pressed.insert(button, PressState::Passed);
        Decision {
            replay,
            ..Decision::default()
        }
    }

    fn up(&mut self, button: Button) -> Decision {
        let Some(state) = self.pressed.remove(&button) else {
            return Decision::default();
        };
        self.wheel = [0; 4];
        match state {
            PressState::Pending => {
                let mut replay = self.flush_pending();
                replay.push(InputKind::Up(button));
                Decision {
                    consume: true,
                    replay,
                    ..Decision::default()
                }
            }
            PressState::Consumed => {
                // 已完成组合的前缀释放后，不让另一个尚未完成的组合继续等待。
                let replay = self.flush_pending();
                Decision {
                    consume: true,
                    replay,
                    ..Decision::default()
                }
            }
            PressState::Draining => Decision {
                consume: true,
                ..Decision::default()
            },
            PressState::Passed => Decision::default(),
        }
    }

    fn scroll(&mut self, axis: Axis, delta: i32) -> Decision {
        if delta == 0 || !self.enabled {
            return Decision::default();
        }
        let direction = direction(axis, delta);
        let Some(id) = self
            .bindings
            .iter()
            .find(|binding| {
                binding.activation == Activation::Wheel(direction)
                    && self.held_matches(&binding.held)
            })
            .map(|binding| binding.id)
        else {
            return Decision {
                replay: self.flush_pending(),
                ..Decision::default()
            };
        };
        let index = wheel_index(direction);
        self.wheel[index] = self.wheel[index].saturating_add(i64::from(delta).abs());
        // 一条异常大的公共API输入不能让hook同步路径产生无界工作；剩余步数保留。
        let count = (self.wheel[index] / 120).min(64) as usize;
        self.wheel[index] -= count as i64 * 120;
        if count > 0 {
            self.commit_pending();
        } else if self
            .pressed
            .values()
            .any(|state| *state == PressState::Pending)
        {
            // 未命中时回放真实增量，避免丢掉高精度滚轮的半格输入。
            self.pending.push(InputKind::Wheel { axis, delta });
        }
        Decision {
            consume: true,
            triggered: vec![id; count],
            ..Decision::default()
        }
    }

    fn held_matches(&self, held: &[Button]) -> bool {
        self.pressed.len() == held.len()
            && self.pressed.iter().all(|(button, state)| {
                matches!(state, PressState::Pending | PressState::Consumed) && held.contains(button)
            })
    }

    fn commit_pending(&mut self) {
        self.pending.clear();
        for state in self.pressed.values_mut() {
            if *state == PressState::Pending {
                *state = PressState::Consumed;
            }
        }
    }

    fn flush_pending(&mut self) -> Vec<InputKind> {
        self.wheel = [0; 4];
        for state in self.pressed.values_mut() {
            if *state == PressState::Pending {
                *state = PressState::Passed;
            }
        }
        std::mem::take(&mut self.pending)
    }
}

fn validate(mut bindings: Vec<TriggerSpec>) -> Result<Vec<TriggerSpec>, RegistrationError> {
    if bindings.len() > 64 {
        return Err(RegistrationError::TooManyBindings);
    }
    let mut ids = BTreeSet::new();
    for binding in &mut bindings {
        if !ids.insert(binding.id) {
            return Err(RegistrationError::DuplicateId { id: binding.id });
        }
        if binding.held.len() > 8 {
            return Err(RegistrationError::TooManyHeld { id: binding.id });
        }
        for &button in binding.held.iter().chain(match &binding.activation {
            Activation::Press(button) => std::slice::from_ref(button),
            Activation::Wheel(_) => &[],
        }) {
            if matches!(button, Button::Key(key) if key == 0 || key > 254) {
                return Err(RegistrationError::InvalidButton {
                    id: binding.id,
                    button,
                });
            }
        }
        binding.held.sort_unstable();
        if binding.held.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(RegistrationError::DuplicateHeld { id: binding.id });
        }
        if let Activation::Press(button) = binding.activation
            && binding.held.contains(&button)
        {
            return Err(RegistrationError::TerminalInHeld { id: binding.id });
        }
    }
    for (index, left) in bindings.iter().enumerate() {
        for right in bindings.iter().skip(index + 1) {
            if left.held == right.held && left.activation == right.activation {
                return Err(RegistrationError::DuplicateTrigger {
                    first: left.id,
                    second: right.id,
                });
            }
        }
        if let Activation::Press(button) = left.activation
            && let Some(prefix) = bindings.iter().find(|other| other.held.contains(&button))
        {
            return Err(RegistrationError::TerminalIsPrefix {
                terminal_binding: left.id,
                prefix_binding: prefix.id,
            });
        }
    }
    Ok(bindings)
}

fn direction(axis: Axis, delta: i32) -> WheelDirection {
    match (axis, delta > 0) {
        (Axis::Vertical, true) => WheelDirection::Up,
        (Axis::Vertical, false) => WheelDirection::Down,
        (Axis::Horizontal, true) => WheelDirection::Right,
        (Axis::Horizontal, false) => WheelDirection::Left,
    }
}

fn wheel_index(direction: WheelDirection) -> usize {
    match direction {
        WheelDirection::Up => 0,
        WheelDirection::Down => 1,
        WheelDirection::Left => 2,
        WheelDirection::Right => 3,
    }
}

#[derive(Default)]
pub struct Recorder {
    pressed: BTreeSet<Button>,
    candidate: Option<TriggerSpec>,
    completed: bool,
}

impl Recorder {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn reset(&mut self) {
        *self = Self::default();
    }
    pub fn process(&mut self, event: InputEvent) -> Option<TriggerSpec> {
        if event.injected || self.completed {
            return None;
        }
        match event.kind {
            InputKind::Down(button) => {
                if self.pressed.insert(button) {
                    self.candidate = Some(TriggerSpec {
                        id: 0,
                        held: self
                            .pressed
                            .iter()
                            .copied()
                            .filter(|held| *held != button)
                            .collect(),
                        activation: Activation::Press(button),
                    });
                }
                None
            }
            InputKind::Up(button) => {
                if self.pressed.remove(&button) && self.pressed.is_empty() {
                    self.completed = true;
                    self.candidate.take()
                } else {
                    None
                }
            }
            InputKind::Wheel { axis, delta } if delta != 0 => {
                self.completed = true;
                Some(TriggerSpec {
                    id: 0,
                    held: self.pressed.iter().copied().collect(),
                    activation: Activation::Wheel(direction(axis, delta)),
                })
            }
            InputKind::Wheel { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPS: Button = Button::Key(0x14);
    const SHIFT: Button = Button::Key(0xA0);
    const W: Button = Button::Key(0x57);
    const Q: Button = Button::Key(0x51);
    const SIDE: Button = Button::Mouse(MouseButton::X1);

    fn event(kind: InputKind) -> InputEvent {
        InputEvent {
            kind,
            injected: false,
        }
    }
    fn press(id: u64, held: &[Button], terminal: Button) -> TriggerSpec {
        TriggerSpec {
            id,
            held: held.to_vec(),
            activation: Activation::Press(terminal),
        }
    }
    fn wheel(id: u64, held: &[Button], direction: WheelDirection) -> TriggerSpec {
        TriggerSpec {
            id,
            held: held.to_vec(),
            activation: Activation::Wheel(direction),
        }
    }
    fn scroll(delta: i32) -> InputEvent {
        event(InputKind::Wheel {
            axis: Axis::Vertical,
            delta,
        })
    }

    #[test]
    fn caps_chord_consumes_every_press_edge_and_ignores_repeat() {
        let mut engine = Engine::new(vec![press(1, &[CAPS], W)]).unwrap();
        assert!(engine.process(event(InputKind::Down(CAPS))).consume);
        assert!(engine.process(event(InputKind::Down(CAPS))).consume);
        assert_eq!(engine.process(event(InputKind::Down(W))).triggered, vec![1]);
        let repeat = engine.process(event(InputKind::Down(W)));
        assert!(repeat.consume && repeat.triggered.is_empty());
        assert!(engine.process(event(InputKind::Up(CAPS))).consume);
        assert!(engine.process(event(InputKind::Up(W))).consume);
        assert!(!engine.process(event(InputKind::Up(W))).consume);
    }

    #[test]
    fn standalone_prefix_replays_exactly_one_down_and_up() {
        let mut engine = Engine::new(vec![press(1, &[CAPS], W)]).unwrap();
        engine.process(event(InputKind::Down(CAPS)));
        engine.process(event(InputKind::Down(CAPS)));
        assert_eq!(
            engine.process(event(InputKind::Up(CAPS))),
            Decision {
                consume: true,
                replay: vec![InputKind::Down(CAPS), InputKind::Up(CAPS)],
                triggered: vec![],
            }
        );
    }

    #[test]
    fn unmatched_successor_flushes_before_passthrough_and_disarms_prefix() {
        let mut engine = Engine::new(vec![press(1, &[CAPS], W)]).unwrap();
        engine.process(event(InputKind::Down(CAPS)));
        let fallback = engine.process(event(InputKind::Down(Q)));
        assert!(!fallback.consume);
        assert_eq!(fallback.replay, vec![InputKind::Down(CAPS)]);
        assert!(!engine.process(event(InputKind::Up(Q))).consume);
        assert_eq!(
            engine.process(event(InputKind::Down(W))),
            Decision::default()
        );
        assert!(!engine.process(event(InputKind::Up(W))).consume);
        assert!(!engine.process(event(InputKind::Up(CAPS))).consume);
    }

    #[test]
    fn multiple_prefixes_accept_either_order_and_release_fallback_stays_paired() {
        let mut engine = Engine::new(vec![press(1, &[CAPS, SHIFT], W)]).unwrap();
        engine.process(event(InputKind::Down(SHIFT)));
        engine.process(event(InputKind::Down(CAPS)));
        let fallback = engine.process(event(InputKind::Up(SHIFT)));
        assert!(fallback.consume);
        assert_eq!(
            fallback.replay,
            vec![
                InputKind::Down(SHIFT),
                InputKind::Down(CAPS),
                InputKind::Up(SHIFT)
            ]
        );
        assert!(!engine.process(event(InputKind::Up(CAPS))).consume);
        engine.process(event(InputKind::Down(CAPS)));
        engine.process(event(InputKind::Down(SHIFT)));
        assert_eq!(engine.process(event(InputKind::Down(W))).triggered, vec![1]);
    }

    #[test]
    fn matching_uses_exact_pressed_set() {
        let mut engine =
            Engine::new(vec![press(1, &[CAPS], W), press(2, &[CAPS, SHIFT], W)]).unwrap();
        engine.process(event(InputKind::Down(CAPS)));
        engine.process(event(InputKind::Down(SHIFT)));
        assert_eq!(engine.process(event(InputKind::Down(W))).triggered, vec![2]);
    }

    #[test]
    fn side_button_wheel_accumulates_and_consumes_prefix_release() {
        let mut engine = Engine::new(vec![wheel(9, &[SIDE], WheelDirection::Up)]).unwrap();
        engine.process(event(InputKind::Down(SIDE)));
        let partial = engine.process(scroll(60));
        assert!(partial.consume && partial.triggered.is_empty());
        assert_eq!(engine.process(scroll(60)).triggered, vec![9]);
        assert_eq!(engine.process(scroll(240)).triggered, vec![9, 9]);
        let release = engine.process(event(InputKind::Up(SIDE)));
        assert!(release.consume && release.replay.is_empty());
    }

    #[test]
    fn partial_wheel_is_replayed_on_unused_prefix_release() {
        let mut engine = Engine::new(vec![wheel(9, &[SIDE], WheelDirection::Up)]).unwrap();
        engine.process(event(InputKind::Down(SIDE)));
        engine.process(scroll(60));
        assert_eq!(
            engine.process(event(InputKind::Up(SIDE))).replay,
            vec![
                InputKind::Down(SIDE),
                InputKind::Wheel {
                    axis: Axis::Vertical,
                    delta: 60
                },
                InputKind::Up(SIDE),
            ]
        );
    }

    #[test]
    fn opposite_directions_have_independent_accumulators() {
        let mut engine = Engine::new(vec![
            wheel(9, &[SIDE], WheelDirection::Up),
            wheel(10, &[SIDE], WheelDirection::Down),
        ])
        .unwrap();
        engine.process(event(InputKind::Down(SIDE)));
        assert!(engine.process(scroll(60)).triggered.is_empty());
        assert!(engine.process(scroll(-60)).triggered.is_empty());
        assert_eq!(engine.process(scroll(60)).triggered, vec![9]);
        assert_eq!(engine.process(scroll(-60)).triggered, vec![10]);
    }

    #[test]
    fn unmatched_wheel_replays_partial_before_current_input() {
        let mut engine = Engine::new(vec![wheel(9, &[SIDE], WheelDirection::Up)]).unwrap();
        engine.process(event(InputKind::Down(SIDE)));
        engine.process(scroll(60));
        let decision = engine.process(scroll(-120));
        assert!(!decision.consume);
        assert_eq!(
            decision.replay,
            vec![
                InputKind::Down(SIDE),
                InputKind::Wheel {
                    axis: Axis::Vertical,
                    delta: 60
                }
            ]
        );
        assert!(!engine.process(event(InputKind::Up(SIDE))).consume);
    }

    #[test]
    fn adding_another_prefix_does_not_borrow_wheel_delta_from_previous_chord() {
        let mut engine = Engine::new(vec![
            wheel(9, &[SIDE], WheelDirection::Up),
            wheel(10, &[SIDE, SHIFT], WheelDirection::Up),
        ])
        .unwrap();
        engine.process(event(InputKind::Down(SIDE)));
        engine.process(scroll(60));
        engine.process(event(InputKind::Down(SHIFT)));
        assert!(engine.process(scroll(60)).triggered.is_empty());
        assert_eq!(engine.process(scroll(60)).triggered, vec![10]);
    }

    #[test]
    fn horizontal_wheel_and_extreme_delta_are_bounded() {
        let mut engine = Engine::new(vec![wheel(1, &[], WheelDirection::Left)]).unwrap();
        assert_eq!(
            engine
                .process(event(InputKind::Wheel {
                    axis: Axis::Horizontal,
                    delta: -120
                }))
                .triggered,
            vec![1]
        );
        assert_eq!(
            engine
                .process(event(InputKind::Wheel {
                    axis: Axis::Horizontal,
                    delta: i32::MIN
                }))
                .triggered
                .len(),
            64
        );
        engine.cancel();
        assert!(
            engine
                .process(event(InputKind::Wheel {
                    axis: Axis::Horizontal,
                    delta: -1
                }))
                .triggered
                .is_empty()
        );
    }

    #[test]
    fn injected_events_are_passthrough_and_cannot_mutate_physical_state() {
        let mut engine = Engine::new(vec![press(1, &[CAPS], W)]).unwrap();
        engine.process(event(InputKind::Down(CAPS)));
        let injected = InputEvent {
            kind: InputKind::Up(CAPS),
            injected: true,
        };
        assert_eq!(engine.process(injected), Decision::default());
        assert_eq!(engine.process(event(InputKind::Down(W))).triggered, vec![1]);
    }

    #[test]
    fn disable_drains_suppressed_up_but_preserves_passed_up() {
        let mut engine = Engine::new(vec![press(1, &[CAPS], W)]).unwrap();
        engine.process(event(InputKind::Down(CAPS)));
        engine.set_enabled(false);
        assert!(engine.process(event(InputKind::Down(CAPS))).consume);
        assert!(engine.process(event(InputKind::Up(CAPS))).consume);
        assert!(!engine.process(event(InputKind::Down(W))).consume);
        engine.set_enabled(true);
        assert!(!engine.process(event(InputKind::Up(W))).consume);
    }

    #[test]
    fn cancel_and_replace_cannot_reuse_a_draining_press() {
        let mut engine = Engine::new(vec![press(1, &[CAPS], W)]).unwrap();
        engine.process(event(InputKind::Down(CAPS)));
        engine.replace_bindings(vec![press(2, &[CAPS], Q)]).unwrap();
        assert!(
            engine
                .process(event(InputKind::Down(Q)))
                .triggered
                .is_empty()
        );
        assert!(engine.process(event(InputKind::Up(CAPS))).consume);
        engine.process(event(InputKind::Up(Q)));
        engine.process(event(InputKind::Down(CAPS)));
        assert_eq!(engine.process(event(InputKind::Down(Q))).triggered, vec![2]);
    }

    #[test]
    fn invalid_replacement_is_transactional() {
        let mut engine = Engine::new(vec![press(1, &[CAPS], W)]).unwrap();
        engine.process(event(InputKind::Down(CAPS)));
        assert!(
            engine
                .replace_bindings(vec![press(2, &[CAPS, CAPS], Q)])
                .is_err()
        );
        assert_eq!(engine.process(event(InputKind::Down(W))).triggered, vec![1]);
    }

    #[test]
    fn registration_rejects_ambiguous_invalid_and_excessive_inputs() {
        assert!(matches!(
            Engine::new(vec![press(1, &[], CAPS), press(2, &[CAPS], W)]),
            Err(RegistrationError::TerminalIsPrefix { .. })
        ));
        assert!(matches!(
            Engine::new(vec![press(1, &[CAPS], W), press(2, &[CAPS], W)]),
            Err(RegistrationError::DuplicateTrigger { .. })
        ));
        assert!(matches!(
            Engine::new(vec![press(1, &[], W), press(1, &[], Q)]),
            Err(RegistrationError::DuplicateId { .. })
        ));
        assert!(matches!(
            Engine::new(vec![press(1, &[CAPS], CAPS)]),
            Err(RegistrationError::TerminalInHeld { .. })
        ));
        assert!(matches!(
            Engine::new(vec![press(1, &[], Button::Key(0))]),
            Err(RegistrationError::InvalidButton { .. })
        ));
        assert!(matches!(
            Engine::new(vec![press(1, &[], W); 65]),
            Err(RegistrationError::TooManyBindings)
        ));
        let many_held = (1..=9).map(Button::Key).collect::<Vec<_>>();
        assert!(matches!(
            Engine::new(vec![press(1, &many_held, W)]),
            Err(RegistrationError::TooManyHeld { .. })
        ));
    }

    #[test]
    fn recording_chord_waits_for_all_releases_and_ignores_mouse_up_from_start_button() {
        let mut recorder = Recorder::new();
        assert!(
            recorder
                .process(event(InputKind::Up(Button::Mouse(MouseButton::Left))))
                .is_none()
        );
        assert!(recorder.process(event(InputKind::Down(CAPS))).is_none());
        assert!(recorder.process(event(InputKind::Down(W))).is_none());
        assert!(recorder.process(event(InputKind::Down(W))).is_none());
        assert!(recorder.process(event(InputKind::Up(CAPS))).is_none());
        assert_eq!(
            recorder.process(event(InputKind::Up(W))),
            Some(press(0, &[CAPS], W))
        );
        assert!(recorder.process(event(InputKind::Down(Q))).is_none());
        assert!(recorder.process(event(InputKind::Up(Q))).is_none());
    }

    #[test]
    fn recorder_wheel_uses_first_nonzero_direction_and_reset_starts_new_recording() {
        let mut recorder = Recorder::new();
        assert!(
            recorder
                .process(InputEvent {
                    kind: InputKind::Down(CAPS),
                    injected: true
                })
                .is_none()
        );
        assert!(recorder.process(scroll(0)).is_none());
        recorder.process(event(InputKind::Down(SIDE)));
        assert_eq!(
            recorder.process(scroll(-1)),
            Some(wheel(0, &[SIDE], WheelDirection::Down))
        );
        assert!(recorder.process(scroll(120)).is_none());
        recorder.reset();
        recorder.process(event(InputKind::Down(Q)));
        assert_eq!(
            recorder.process(event(InputKind::Up(Q))),
            Some(press(0, &[], Q))
        );
    }
}
