//! Opt-in semantic shortcuts. Text/IME commits are intentionally not synthesized.
use std::collections::{BTreeMap, BTreeSet};

use ndp_proto::{InputEvent, InputKind, KeyCode, Modifiers};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Platform {
    Mac,
    Windows,
    #[default]
    Linux,
}

impl Platform {
    pub fn local() -> Self {
        if cfg!(target_os = "macos") {
            Self::Mac
        } else if cfg!(target_os = "windows") {
            Self::Windows
        } else {
            Self::Linux
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Physical,
    Semantic,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Profile {
    #[default]
    Physical,
    Editing,
    Terminal,
}

/// Selected per published resource. No executable-name guessing or global remap.
#[derive(Clone, Copy, Debug, Default)]
pub struct Configuration {
    pub mode: Mode,
    pub profile: Profile,
    pub host: Option<Platform>,
}

/// Tracks physical source keys separately from their remote modifier state.
pub struct Mapper {
    client: Platform,
    host: Platform,
    mode: Mode,
    profile: Profile,
    held: BTreeMap<u32, InputEvent>,
    remote_mods: Modifiers,
    snapshot: Modifiers,
    inferred: BTreeSet<u32>,
    suppressed: Modifiers,
}

impl Mapper {
    pub fn modifiers(&self) -> Modifiers {
        self.remote_mods
    }

    pub fn source_modifiers(&self) -> Modifiers {
        Modifiers(self.snapshot.0 | self.suppressed.0)
    }

    pub fn new(client: Platform, host: Platform, mode: Mode, profile: Profile) -> Self {
        Self {
            client,
            host,
            mode,
            profile,
            held: BTreeMap::new(),
            remote_mods: Modifiers::NONE,
            snapshot: Modifiers::NONE,
            inferred: BTreeSet::new(),
            suppressed: Modifiers::NONE,
        }
    }

    fn semantic(&self) -> bool {
        self.mode == Mode::Semantic
            && self.profile == Profile::Editing
            && (self.client == Platform::Mac) != (self.host == Platform::Mac)
    }

    /// Snapshot presses are deferred until actual input, so focusing a window
    /// during Alt/Command+Tab does not replay the local switching chord.
    pub fn observe_modifiers(&mut self, snapshot: Modifiers) -> Vec<InputEvent> {
        self.suppressed.0 &= snapshot.0;
        self.snapshot = Modifiers(snapshot.0 & modifier_mask() & !self.suppressed.0);
        let released: Vec<_> = self
            .held
            .keys()
            .copied()
            .filter(|key| {
                modifier(*key) != Modifiers::NONE && self.snapshot.0 & modifier(*key).0 == 0
            })
            .collect();
        let mut out = Vec::new();
        for key in released {
            self.held.remove(&key);
            self.inferred.remove(&key);
            if !self.semantic() {
                self.remote_mods = self.desired_mods();
                out.push(InputEvent::key(
                    InputKind::KeyUp,
                    KeyCode(key),
                    self.remote_mods,
                ));
            }
        }
        if self.semantic() {
            self.reconcile(self.desired_mods(), &mut out);
        }
        out
    }

    /// Materialize only missing modifier families; known physical sides survive.
    pub fn restore_modifiers(&mut self) -> Vec<InputEvent> {
        let mut out = Vec::new();
        for (bit, key) in [
            (Modifiers::CONTROL, 0xe0),
            (Modifiers::SHIFT, 0xe1),
            (Modifiers::ALT, 0xe2),
            (Modifiers::META, 0xe3),
        ] {
            if self.snapshot.0 & bit.0 != 0 && !self.held.keys().any(|held| modifier(*held) == bit)
            {
                self.held.insert(
                    key,
                    InputEvent::key(InputKind::KeyDown, KeyCode(key), self.snapshot),
                );
                self.inferred.insert(key);
                if !self.semantic() {
                    self.remote_mods = self.desired_mods();
                    out.push(InputEvent::key(
                        InputKind::KeyDown,
                        KeyCode(key),
                        self.remote_mods,
                    ));
                }
            }
        }
        if self.semantic() {
            self.reconcile(self.desired_mods(), &mut out);
        }
        out
    }

    pub fn synthetic_key(&mut self, event: InputEvent) -> Vec<InputEvent> {
        if modifier(event.key.0) != Modifiers::NONE
            || (event.key.0 == 0x2b && self.local_switch_modifiers(event) != Modifiers::NONE)
        {
            self.key(event, false)
        } else {
            Vec::new()
        }
    }

    fn local_switch_modifiers(&self, event: InputEvent) -> Modifiers {
        Modifiers(
            (event.modifiers.0
                | self.suppressed.0
                | self.snapshot.0
                | self
                    .held
                    .keys()
                    .fold(0, |bits, key| bits | modifier(*key).0))
                & (Modifiers::ALT.0 | Modifiers::META.0),
        )
    }

    fn desired_mods(&self) -> Modifiers {
        let mut bits = 0;
        for key in self.held.keys() {
            bits |= modifier(*key).0;
        }
        let source = if self.client == Platform::Mac {
            Modifiers::META
        } else {
            Modifiers::CONTROL
        };
        let target = if self.host == Platform::Mac {
            Modifiers::META
        } else {
            Modifiers::CONTROL
        };
        if self.semantic()
            && bits & source.0 != 0
            && bits & (Modifiers::ALT.0 | target.0) == 0
            && self.held.keys().any(|key| editing_key(*key))
        {
            bits = (bits & !source.0) | target.0;
        }
        Modifiers(bits)
    }

    fn reconcile(&mut self, desired: Modifiers, out: &mut Vec<InputEvent>) {
        for (bit, key) in [
            (Modifiers::CONTROL, 0xe0),
            (Modifiers::SHIFT, 0xe1),
            (Modifiers::ALT, 0xe2),
            (Modifiers::META, 0xe3),
        ] {
            if self.remote_mods.0 & bit.0 != 0 && desired.0 & bit.0 == 0 {
                self.remote_mods.0 &= !bit.0;
                out.push(InputEvent::key(
                    InputKind::KeyUp,
                    KeyCode(key),
                    self.remote_mods,
                ));
            }
        }
        for (bit, key) in [
            (Modifiers::CONTROL, 0xe0),
            (Modifiers::SHIFT, 0xe1),
            (Modifiers::ALT, 0xe2),
            (Modifiers::META, 0xe3),
        ] {
            if self.remote_mods.0 & bit.0 == 0 && desired.0 & bit.0 != 0 {
                self.remote_mods.0 |= bit.0;
                out.push(InputEvent::key(
                    InputKind::KeyDown,
                    KeyCode(key),
                    self.remote_mods,
                ));
            }
        }
    }

    pub fn key(&mut self, mut event: InputEvent, repeat: bool) -> Vec<InputEvent> {
        let mut key = event.key.0;
        let down = event.kind == InputKind::KeyDown;
        if !matches!(event.kind, InputKind::KeyDown | InputKind::KeyUp) {
            return Vec::new();
        }
        // Task switching belongs to the local window manager, in either mode.
        if down && key == 0x2b && self.local_switch_modifiers(event) != Modifiers::NONE {
            let suppressed = self.local_switch_modifiers(event);
            let released = self.release_all();
            self.suppressed = suppressed;
            return released;
        }
        if modifier(key).0 & self.suppressed.0 != 0 {
            if down {
                return Vec::new();
            }
            self.suppressed.0 &= !modifier(key).0;
            self.snapshot.0 &= !modifier(key).0;
        }
        if repeat && !self.held.contains_key(&key) {
            return Vec::new();
        }
        let mut out = Vec::new();
        if down && modifier(key) != Modifiers::NONE {
            if let Some(inferred) = self
                .inferred
                .iter()
                .copied()
                .find(|held| modifier(*held) == modifier(key))
            {
                self.inferred.remove(&inferred);
                if inferred != key {
                    self.held.remove(&inferred);
                    if !self.semantic() {
                        self.remote_mods = self.desired_mods();
                        out.push(InputEvent::key(
                            InputKind::KeyUp,
                            KeyCode(inferred),
                            self.remote_mods,
                        ));
                    }
                }
            }
        }
        if down && modifier(key) != Modifiers::NONE && self.held.contains_key(&key) {
            return out;
        }
        if !down && !self.held.contains_key(&key) {
            let inferred = self
                .inferred
                .iter()
                .copied()
                .find(|held| modifier(*held) == modifier(key));
            let Some(inferred) = inferred.filter(|_| modifier(key) != Modifiers::NONE) else {
                return Vec::new();
            };
            key = inferred;
            event.key = KeyCode(key);
        }
        if modifier(key) == Modifiers::NONE {
            out.extend(self.restore_modifiers());
        }
        if down {
            self.held.insert(key, event);
            self.snapshot.0 |= modifier(key).0;
        } else {
            self.held.remove(&key);
            self.inferred.remove(&key);
            if !self
                .held
                .keys()
                .any(|held| modifier(*held) == modifier(key))
            {
                self.snapshot.0 &= !modifier(key).0;
            }
        }
        if !self.semantic() {
            // Physical mode retains left/right key identity.
            event.modifiers = self.desired_mods();
            if repeat {
                event.modifiers.0 |= Modifiers::REPEAT.0;
            }
            out.push(event);
            self.remote_mods = self.desired_mods();
            return out;
        }
        if modifier(key) == Modifiers::NONE && !down {
            event.modifiers = self.remote_mods;
            out.push(event);
        }
        self.reconcile(self.desired_mods(), &mut out);
        if modifier(key) == Modifiers::NONE && down {
            event.modifiers = self.remote_mods;
            if repeat {
                event.modifiers.0 |= Modifiers::REPEAT.0;
            }
            out.push(event);
        }
        out
    }

    pub fn release_all(&mut self) -> Vec<InputEvent> {
        let semantic = self.semantic();
        let mut out = Vec::new();
        let mut modifiers = self.remote_mods;
        for key in self.held.keys().rev() {
            if !semantic || modifier(*key) == Modifiers::NONE {
                if !semantic {
                    modifiers.0 &= !modifier(*key).0;
                }
                out.push(InputEvent::key(InputKind::KeyUp, KeyCode(*key), modifiers));
            }
        }
        self.held.clear();
        self.inferred.clear();
        self.snapshot = Modifiers::NONE;
        self.suppressed = Modifiers::NONE;
        if semantic {
            self.reconcile(Modifiers::NONE, &mut out);
        }
        self.remote_mods = Modifiers::NONE;
        out
    }
}

pub(crate) fn modifier(key: u32) -> Modifiers {
    match key {
        0xe0 | 0xe4 => Modifiers::CONTROL,
        0xe1 | 0xe5 => Modifiers::SHIFT,
        0xe2 | 0xe6 => Modifiers::ALT,
        0xe3 | 0xe7 => Modifiers::META,
        _ => Modifiers::NONE,
    }
}

fn modifier_mask() -> u16 {
    Modifiers::CONTROL.0 | Modifiers::SHIFT.0 | Modifiers::ALT.0 | Modifiers::META.0
}

fn editing_key(key: u32) -> bool {
    matches!(key, 0x04 | 0x06 | 0x09 | 0x16 | 0x19 | 0x1b | 0x1d)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn press(key: u32) -> InputEvent {
        InputEvent::key(InputKind::KeyDown, KeyCode(key), Modifiers::CONTROL)
    }

    #[test]
    fn focus_snapshot_restores_shift_for_pointer_and_keyboard() {
        for client in [Platform::Windows, Platform::Linux] {
            let mut keyboard =
                Mapper::new(client, Platform::Mac, Mode::Physical, Profile::Physical);
            keyboard.observe_modifiers(Modifiers::SHIFT);
            let keys = keyboard.key(
                InputEvent::key(InputKind::KeyDown, KeyCode(0x04), Modifiers::SHIFT),
                false,
            );
            assert_eq!(
                keys.iter().map(|event| event.key.0).collect::<Vec<_>>(),
                [0xe1, 0x04]
            );
            assert_eq!(keys.last().unwrap().modifiers, Modifiers::SHIFT);
            let mut mapper = Mapper::new(client, Platform::Mac, Mode::Physical, Profile::Physical);
            assert!(mapper.observe_modifiers(Modifiers::SHIFT).is_empty());
            let pointer_prefix = mapper.restore_modifiers();
            assert_eq!(pointer_prefix.len(), 1);
            assert_eq!(
                (pointer_prefix[0].kind, pointer_prefix[0].key.0),
                (InputKind::KeyDown, 0xe1)
            );
            let pointer = InputEvent::mouse_move(0.5, 0.5, mapper.modifiers());
            assert_eq!(pointer.modifiers, Modifiers::SHIFT);
            let key = mapper.key(
                InputEvent::key(InputKind::KeyDown, KeyCode(0x04), Modifiers::SHIFT),
                false,
            );
            assert_eq!(key.len(), 1);
            assert_eq!(key[0].modifiers, Modifiers::SHIFT);
            assert!(mapper.restore_modifiers().is_empty());
            let released = mapper.observe_modifiers(Modifiers::NONE);
            assert_eq!(
                (released[0].kind, released[0].key.0),
                (InputKind::KeyUp, 0xe1)
            );
            assert_eq!(mapper.modifiers(), Modifiers::NONE);
        }
    }

    #[test]
    fn terminal_control_survives_focus_release_and_new_window_snapshot() {
        let mut old = Mapper::new(
            Platform::Windows,
            Platform::Mac,
            Mode::Semantic,
            Profile::Terminal,
        );
        old.key(press(0xe0), false);
        assert_eq!(old.release_all().len(), 1);
        let mut next = Mapper::new(
            Platform::Windows,
            Platform::Mac,
            Mode::Semantic,
            Profile::Terminal,
        );
        next.observe_modifiers(Modifiers::CONTROL);
        let events = next.key(press(0x06), false);
        assert_eq!(
            events.iter().map(|event| event.key.0).collect::<Vec<_>>(),
            [0xe0, 0x06]
        );
        assert_eq!(events.last().unwrap().modifiers, Modifiers::CONTROL);
        next.key(
            InputEvent::key(InputKind::KeyUp, KeyCode(0x06), Modifiers::CONTROL),
            false,
        );
        // A family-only snapshot used left Control; a later right-side release
        // must release that actual forwarded key, not leave it stuck.
        let released = next.key(
            InputEvent::key(InputKind::KeyUp, KeyCode(0xe4), Modifiers::NONE),
            false,
        );
        assert_eq!(
            (released[0].kind, released[0].key.0),
            (InputKind::KeyUp, 0xe0)
        );
        assert!(next.release_all().is_empty());
    }

    #[test]
    fn semantic_snapshot_shortcut_releases_are_symmetric() {
        for focus_loss in [false, true] {
            let mut mapper = Mapper::new(
                Platform::Windows,
                Platform::Mac,
                Mode::Semantic,
                Profile::Editing,
            );
            mapper.observe_modifiers(Modifiers::CONTROL);
            let mut events = mapper.key(press(0x06), false);
            assert_eq!(events.last().unwrap().modifiers, Modifiers::META);
            if focus_loss {
                events.extend(mapper.release_all());
            } else {
                events.extend(mapper.key(
                    InputEvent::key(InputKind::KeyUp, KeyCode(0x06), Modifiers::CONTROL),
                    false,
                ));
                events.extend(mapper.observe_modifiers(Modifiers::NONE));
            }
            let c_up = events
                .iter()
                .find(|event| event.key.0 == 0x06 && event.kind == InputKind::KeyUp)
                .unwrap();
            assert_eq!(c_up.modifiers, Modifiers::META);
            let mut balance = BTreeMap::<u32, i32>::new();
            for event in events {
                let held = balance.entry(event.key.0).or_default();
                *held += if event.kind == InputKind::KeyDown {
                    1
                } else {
                    -1
                };
                assert!(*held >= 0, "release without forwarded press");
            }
            assert!(balance.values().all(|count| *count == 0));
            assert_eq!(mapper.modifiers(), Modifiers::NONE);
        }
    }

    #[test]
    fn synthetic_focus_keys_restore_modifiers_but_never_letters() {
        let mut mapper = Mapper::new(
            Platform::Windows,
            Platform::Mac,
            Mode::Physical,
            Profile::Physical,
        );
        assert!(mapper.synthetic_key(press(0x06)).is_empty());
        let modifier = mapper.synthetic_key(press(0xe4));
        assert_eq!(modifier[0].key.0, 0xe4);
        assert!(mapper.synthetic_key(press(0xe4)).is_empty());
        assert!(mapper.observe_modifiers(Modifiers::CONTROL).is_empty());
        let typed = mapper.key(press(0x06), false);
        assert_eq!(typed.len(), 1);
        assert_eq!(typed[0].modifiers, Modifiers::CONTROL);
        mapper.release_all();
        mapper.observe_modifiers(Modifiers::CONTROL);
        assert!(
            mapper.key(press(0x06), true).is_empty(),
            "focus must not resurrect a held letter from repeat"
        );
    }

    #[test]
    fn snapshot_inference_hands_off_to_the_known_physical_modifier_side() {
        let mut mapper = Mapper::new(
            Platform::Windows,
            Platform::Mac,
            Mode::Physical,
            Profile::Terminal,
        );
        mapper.observe_modifiers(Modifiers::CONTROL);
        assert_eq!(mapper.restore_modifiers()[0].key.0, 0xe0);
        let handoff = mapper.synthetic_key(press(0xe4));
        assert_eq!(
            handoff
                .iter()
                .map(|event| (event.kind, event.key.0))
                .collect::<Vec<_>>(),
            [(InputKind::KeyUp, 0xe0), (InputKind::KeyDown, 0xe4)]
        );
        let release = mapper.key(
            InputEvent::key(InputKind::KeyUp, KeyCode(0xe4), Modifiers::NONE),
            false,
        );
        assert_eq!(release.len(), 1);
        assert_eq!(
            (release[0].kind, release[0].key.0),
            (InputKind::KeyUp, 0xe4)
        );
        assert!(mapper.release_all().is_empty());
    }

    #[test]
    fn pending_focus_snapshot_cannot_replay_local_task_switch() {
        for modifier in [Modifiers::ALT, Modifiers::META] {
            let mut mapper = Mapper::new(
                Platform::Windows,
                Platform::Mac,
                Mode::Semantic,
                Profile::Editing,
            );
            mapper.observe_modifiers(modifier);
            let tab = InputEvent::key(InputKind::KeyDown, KeyCode(0x2b), modifier);
            assert!(mapper.synthetic_key(tab).is_empty());
            let physical = if modifier == Modifiers::ALT {
                0xe2
            } else {
                0xe3
            };
            assert!(mapper
                .synthetic_key(InputEvent::key(
                    InputKind::KeyDown,
                    KeyCode(physical),
                    modifier
                ))
                .is_empty());
            assert_eq!(mapper.source_modifiers(), modifier);
            assert!(mapper.observe_modifiers(modifier).is_empty());
            assert!(mapper.restore_modifiers().is_empty());
            assert!(mapper
                .synthetic_key(InputEvent::key(
                    InputKind::KeyUp,
                    KeyCode(physical),
                    Modifiers::NONE
                ))
                .is_empty());
            let tab = mapper.key(
                InputEvent::key(InputKind::KeyDown, KeyCode(0x2b), Modifiers::NONE),
                false,
            );
            assert_eq!(tab.len(), 1);
            assert_eq!(tab[0].key.0, 0x2b);
            assert_eq!(tab[0].modifiers, Modifiers::NONE);
        }
    }

    #[test]
    fn editing_control_c_switches_symmetrically_and_focus_loss_releases() {
        let mut mapper = Mapper::new(
            Platform::Windows,
            Platform::Mac,
            Mode::Semantic,
            Profile::Editing,
        );
        mapper.key(press(0xe0), false);
        let events = mapper.key(press(0x06), false);
        assert_eq!(
            events.iter().map(|e| (e.kind, e.key.0)).collect::<Vec<_>>(),
            [
                (InputKind::KeyUp, 0xe0),
                (InputKind::KeyDown, 0xe3),
                (InputKind::KeyDown, 0x06)
            ]
        );
        assert_eq!(events[2].modifiers, Modifiers::META);
        assert_eq!(mapper.modifiers(), Modifiers::META);
        let release = mapper.release_all();
        assert_eq!(release[0].modifiers, Modifiers::META);
        assert_eq!(
            release.iter().map(|e| e.key.0).collect::<Vec<_>>(),
            [0x06, 0xe3]
        );
        assert!(mapper.release_all().is_empty());
        assert_eq!(mapper.modifiers(), Modifiers::NONE);
        assert!(mapper.key(press(0x06), true).is_empty());
    }
    #[test]
    fn terminal_and_physical_never_map_control() {
        for profile in [Profile::Terminal, Profile::Physical] {
            for host in [Platform::Mac, Platform::Windows, Platform::Linux] {
                let mut mapper = Mapper::new(Platform::Windows, host, Mode::Semantic, profile);
                assert_eq!(mapper.key(press(0xe0), false)[0].key.0, 0xe0);
                assert_eq!(
                    mapper.key(press(0x06), false)[0].modifiers,
                    Modifiers::CONTROL
                );
            }
        }
    }
    #[test]
    fn reverse_mapping_and_key_release_restore_source_modifier() {
        let mut mapper = Mapper::new(
            Platform::Mac,
            Platform::Linux,
            Mode::Semantic,
            Profile::Editing,
        );
        mapper.key(press(0xe3), false);
        assert_eq!(
            mapper.key(press(0x16), false).last().unwrap().modifiers,
            Modifiers::CONTROL
        );
        let events = mapper.key(
            InputEvent::key(InputKind::KeyUp, KeyCode(0x16), Modifiers::META),
            false,
        );
        assert_eq!(events[0].key.0, 0x16);
        assert_eq!(events.last().unwrap().key.0, 0xe3);
    }

    #[test]
    fn task_switching_releases_remote_keys_and_never_sends_tab() {
        for client in [Platform::Windows, Platform::Mac, Platform::Linux] {
            let mut mapper = Mapper::new(client, Platform::Mac, Mode::Physical, Profile::Physical);
            mapper.key(press(0xe2), false);
            let events = mapper.key(
                InputEvent::key(InputKind::KeyDown, KeyCode(0x2b), Modifiers::ALT),
                false,
            );
            assert!(events
                .iter()
                .all(|e| e.kind == InputKind::KeyUp && e.key.0 != 0x2b));
            assert!(mapper
                .key(
                    InputEvent::key(InputKind::KeyUp, KeyCode(0x2b), Modifiers::NONE),
                    false
                )
                .is_empty());
        }
    }

    #[test]
    fn semantic_modifier_release_before_letter_is_balanced() {
        let mut mapper = Mapper::new(
            Platform::Windows,
            Platform::Mac,
            Mode::Semantic,
            Profile::Editing,
        );
        mapper.key(press(0xe0), false);
        mapper.key(press(0x06), false);
        let modifiers = mapper.key(
            InputEvent::key(InputKind::KeyUp, KeyCode(0xe0), Modifiers::NONE),
            false,
        );
        assert_eq!(modifiers.len(), 1);
        assert_eq!(
            (modifiers[0].kind, modifiers[0].key.0),
            (InputKind::KeyUp, 0xe3)
        );
        let key = mapper.key(
            InputEvent::key(InputKind::KeyUp, KeyCode(0x06), Modifiers::NONE),
            false,
        );
        assert_eq!(key.len(), 1);
        assert_eq!(key[0].modifiers, Modifiers::NONE);
        assert!(mapper.release_all().is_empty());
    }

    #[test]
    fn editing_shortcuts_follow_host_convention_for_every_platform_pair() {
        for client in [Platform::Mac, Platform::Windows, Platform::Linux] {
            for host in [Platform::Mac, Platform::Windows, Platform::Linux] {
                for letter in [0x04, 0x06, 0x09, 0x16, 0x19, 0x1b, 0x1d] {
                    let mut mapper = Mapper::new(client, host, Mode::Semantic, Profile::Editing);
                    let source = if client == Platform::Mac { 0xe3 } else { 0xe0 };
                    let expected = if host == Platform::Mac {
                        Modifiers::META
                    } else {
                        Modifiers::CONTROL
                    };
                    mapper.key(press(source), false);
                    let events = mapper.key(press(letter), false);
                    assert_eq!(
                        events.last().unwrap().modifiers,
                        expected,
                        "{client:?} -> {host:?}, {letter}"
                    );
                    let releases = mapper.release_all();
                    assert!(releases.iter().all(|event| event.kind == InputKind::KeyUp));
                    assert!(mapper.release_all().is_empty());
                }
            }
        }
    }
}
