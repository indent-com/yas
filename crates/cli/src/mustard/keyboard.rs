//! Kitty keyboard encoding for the native terminal viewer. The shared fixture
//! corpus also exercises the browser encoder.
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers, MediaKeyCode, ModifierKeyCode,
};

#[cfg(unix)]
pub(super) const CAPTURE_FLAGS: u8 = 1 | 2;

#[derive(Clone, Default)]
pub(super) struct Key {
    pub key: u32,
    pub modifiers: u8,
    pub event_type: u8,
    pub text: String,
    pub shifted_key: Option<u32>,
    pub base_key: Option<u32>,
}

pub(super) fn modifier_bit(key: u32) -> u8 {
    [1, 4, 2, 8, 16, 32][((key - 57441) % 6) as usize]
}

pub(super) fn from_event(event: KeyEvent) -> Option<Key> {
    let mut modifiers = u8::from(event.modifiers.contains(KeyModifiers::SHIFT))
        | (u8::from(event.modifiers.contains(KeyModifiers::ALT)) << 1)
        | (u8::from(event.modifiers.contains(KeyModifiers::CONTROL)) << 2)
        | (u8::from(event.modifiers.contains(KeyModifiers::SUPER)) << 3)
        | (u8::from(event.modifiers.contains(KeyModifiers::HYPER)) << 4)
        | (u8::from(event.modifiers.contains(KeyModifiers::META)) << 5)
        | (u8::from(event.state.contains(KeyEventState::CAPS_LOCK)) << 6)
        | (u8::from(event.state.contains(KeyEventState::NUM_LOCK)) << 7);
    let mut text = String::new();
    let mut shifted_key = None;
    let mut key = match event.code {
        KeyCode::Char(c) => {
            let lower = c.to_lowercase().next().unwrap_or(c);
            if modifiers & 1 != 0 && c.is_alphabetic() {
                shifted_key = c.to_uppercase().next().map(u32::from);
            }
            if modifiers & 62 == 0 {
                if modifiers & 1 == 0 {
                    text.push(c);
                } else if c.is_alphabetic() {
                    text.extend(c.to_uppercase());
                    shifted_key = text.chars().next().map(u32::from);
                }
                // Crossterm exposes no layout map or associated-text field.
                // Do not invent the shifted character for punctuation/digits.
            }
            lower as u32
        }
        KeyCode::Null => {
            modifiers |= 4;
            32
        }
        KeyCode::Enter => 13,
        KeyCode::Tab => 9,
        KeyCode::BackTab => {
            modifiers |= 1;
            9
        }
        KeyCode::Backspace => 127,
        KeyCode::Esc => 27,
        KeyCode::Insert => 57348,
        KeyCode::Delete => 57349,
        KeyCode::Left => 57350,
        KeyCode::Right => 57351,
        KeyCode::Up => 57352,
        KeyCode::Down => 57353,
        KeyCode::PageUp => 57354,
        KeyCode::PageDown => 57355,
        KeyCode::Home => 57356,
        KeyCode::End => 57357,
        KeyCode::CapsLock => 57358,
        KeyCode::ScrollLock => 57359,
        KeyCode::NumLock => 57360,
        KeyCode::PrintScreen => 57361,
        KeyCode::Pause => 57362,
        KeyCode::Menu => 57363,
        KeyCode::F(n @ 1..=35) => 57363 + u32::from(n),
        KeyCode::KeypadBegin => 57427,
        KeyCode::Media(m) => match m {
            MediaKeyCode::Play => 57428,
            MediaKeyCode::Pause => 57429,
            MediaKeyCode::PlayPause => 57430,
            MediaKeyCode::Reverse => 57431,
            MediaKeyCode::Stop => 57432,
            MediaKeyCode::FastForward => 57433,
            MediaKeyCode::Rewind => 57434,
            MediaKeyCode::TrackNext => 57435,
            MediaKeyCode::TrackPrevious => 57436,
            MediaKeyCode::Record => 57437,
            MediaKeyCode::LowerVolume => 57438,
            MediaKeyCode::RaiseVolume => 57439,
            MediaKeyCode::MuteVolume => 57440,
        },
        KeyCode::Modifier(m) => match m {
            ModifierKeyCode::LeftShift => 57441,
            ModifierKeyCode::LeftControl => 57442,
            ModifierKeyCode::LeftAlt => 57443,
            ModifierKeyCode::LeftSuper => 57444,
            ModifierKeyCode::LeftHyper => 57445,
            ModifierKeyCode::LeftMeta => 57446,
            ModifierKeyCode::RightShift => 57447,
            ModifierKeyCode::RightControl => 57448,
            ModifierKeyCode::RightAlt => 57449,
            ModifierKeyCode::RightSuper => 57450,
            ModifierKeyCode::RightHyper => 57451,
            ModifierKeyCode::RightMeta => 57452,
            ModifierKeyCode::IsoLevel3Shift => 57453,
            ModifierKeyCode::IsoLevel5Shift => 57454,
        },
        _ => return None,
    };
    // Crossterm 0.29 adds a modifier's own bit even on release.
    if event.kind == KeyEventKind::Release && (57441..=57452).contains(&key) {
        modifiers &= !modifier_bit(key);
    }
    if event.state.contains(KeyEventState::KEYPAD) {
        key = match key {
            48..=57 => 57399 + key - 48,
            46 => 57409,
            47 => 57410,
            42 => 57411,
            45 => 57412,
            43 => 57413,
            13 => 57414,
            61 => 57415,
            44 => 57416,
            57350..=57357 => key - 57350 + 57417,
            57348 => 57425,
            57349 => 57426,
            other => other,
        };
    }
    Some(Key {
        key,
        modifiers,
        event_type: match event.kind {
            KeyEventKind::Press => 1,
            KeyEventKind::Repeat => 2,
            KeyEventKind::Release => 3,
        },
        text,
        shifted_key,
        base_key: None,
    })
}

fn control_byte(key: u32) -> Option<u8> {
    Some(match key {
        97..=122 => (key - 96) as u8,
        65..=90 => (key - 64) as u8,
        32 | 64 | 50 => 0,
        51 | 91 => 27,
        52 | 92 => 28,
        53 | 93 => 29,
        54 | 94 | 126 => 30,
        55 | 95 | 47 => 31,
        56 | 63 => 127,
        _ => return None,
    })
}
fn keypad_equivalent(key: u32) -> u32 {
    match key {
        57399..=57408 => key - 57399 + 48,
        57409 => 46,
        57410 => 47,
        57411 => 42,
        57412 => 45,
        57413 => 43,
        57414 => 13,
        57415 => 61,
        57416 => 44,
        57417..=57424 => key - 57417 + 57350,
        57425 => 57348,
        57426 => 57349,
        _ => key,
    }
}

pub(super) fn encode(input: &Key, flags: u8, app_cursor: bool) -> Option<Vec<u8>> {
    let all = flags & 8 != 0;
    let disambiguate = all || flags & 1 != 0;
    let events = flags & 2 != 0;
    let release = input.event_type == 3;
    if release && !events {
        return None;
    }
    let key = if disambiguate {
        input.key
    } else {
        keypad_equivalent(input.key)
    };
    let functional = (57348..=57454).contains(&key);
    if (57441..=57454).contains(&key) && !all {
        return None;
    }
    let modifiers = input.modifiers & if all || functional { 255 } else { 63 };
    let chord = modifiers & 63;
    let c0 = matches!(key, 13 | 9 | 127);
    if release && c0 && !all {
        return None;
    }
    let escape =
        all || (disambiguate && (functional || key == 27 || chord & !1 != 0 || (c0 && chord != 0)));
    let character = || {
        if input.text.is_empty() {
            char::from_u32(if chord & 1 != 0 {
                input.shifted_key.unwrap_or(key)
            } else {
                key
            })
            .unwrap_or('\u{fffd}')
            .to_string()
        } else {
            input.text.clone()
        }
    };
    if !escape && !release {
        if !functional && key >= 32 && key != 127 && chord & !1 == 0 {
            return Some(character().into_bytes());
        }
        if c0 || key == 27 {
            if key == 13 && chord & 4 != 0 {
                return Some(format!("\x1b[13;{}u", u16::from(modifiers) + 1).into_bytes());
            }
            if chord & !7 == 0 {
                let mut bytes = if chord & 2 != 0 { vec![27] } else { Vec::new() };
                if key == 9 && chord & 1 != 0 {
                    bytes.extend_from_slice(b"\x1b[Z");
                } else {
                    bytes.push(if key == 127 && chord & 4 != 0 {
                        8
                    } else {
                        key as u8
                    });
                }
                return Some(bytes);
            }
        }
        if !functional && key >= 32 && key != 127 && chord & !7 == 0 && chord & 5 != 5 {
            let mut bytes = if chord & 2 != 0 { vec![27] } else { Vec::new() };
            if let Some(value) = (chord & 4 != 0).then(|| control_byte(key)).flatten() {
                bytes.push(value);
            } else {
                bytes.extend_from_slice(character().as_bytes());
            }
            return Some(bytes);
        }
    }
    let mut number = key.to_string();
    let mut suffix = 'u';
    let letter = match key {
        57350 => Some('D'),
        57351 => Some('C'),
        57352 => Some('A'),
        57353 => Some('B'),
        57356 => Some('H'),
        57357 => Some('F'),
        57364 => Some('P'),
        57365 => Some('Q'),
        57367 => Some('S'),
        57427 => Some('E'),
        _ => None,
    };
    let tilde = match key {
        57348 => Some(2),
        57349 => Some(3),
        57354 => Some(5),
        57355 => Some(6),
        57368 => Some(15),
        57369 => Some(17),
        57370 => Some(18),
        57371 => Some(19),
        57372 => Some(20),
        57373 => Some(21),
        57374 => Some(23),
        57375 => Some(24),
        _ => None,
    };
    if key == 57366 {
        if flags == 0 && modifiers == 0 && !release {
            return Some(b"\x1bOR".to_vec());
        }
        number = if flags != 0 { "13" } else { "1" }.into();
        suffix = if flags != 0 { '~' } else { 'R' };
    } else if let Some(letter) = letter {
        suffix = letter;
        if flags == 0
            && modifiers == 0
            && !release
            && ((57364..=57367).contains(&key) || (app_cursor && (57350..=57353).contains(&key)))
        {
            return Some(format!("\x1bO{letter}").into_bytes());
        }
        number = "1".into();
    } else if let Some(tilde) = tilde {
        number = tilde.to_string();
        suffix = '~';
    } else if key == 57363 && flags == 0 {
        number = "29".into();
        suffix = '~';
    }
    if suffix == 'u' && flags & 4 != 0 {
        let shifted = input
            .shifted_key
            .filter(|shifted| chord & 1 != 0 && *shifted != key);
        let base = input.base_key.filter(|base| *base != key);
        if let Some(shifted) = shifted {
            number.push_str(&format!(":{shifted}"));
        } else if base.is_some() {
            number.push(':');
        }
        if let Some(base) = base {
            number.push_str(&format!(":{base}"));
        }
    }
    let event = if events && input.event_type != 1 {
        format!(":{}", input.event_type)
    } else {
        String::new()
    };
    let associated =
        if all && flags & 16 != 0 && !release && !input.text.chars().any(char::is_control) {
            input
                .text
                .chars()
                .map(|c| (c as u32).to_string())
                .collect::<Vec<_>>()
                .join(":")
        } else {
            String::new()
        };
    let parameters = if modifiers != 0 || !event.is_empty() || !associated.is_empty() {
        format!(";{}{event}", u16::from(modifiers) + 1)
    } else {
        String::new()
    };
    if number == "1" && suffix != 'u' && suffix != '~' && parameters.is_empty() {
        number.clear();
    }
    let text = if associated.is_empty() {
        String::new()
    } else {
        format!(";{associated}")
    };
    Some(format!("\x1b[{number}{parameters}{text}{suffix}").into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn modifier_releases_clear_only_the_released_modifier() {
        for (code, bit) in [
            (ModifierKeyCode::LeftShift, KeyModifiers::SHIFT),
            (ModifierKeyCode::RightControl, KeyModifiers::CONTROL),
            (ModifierKeyCode::LeftAlt, KeyModifiers::ALT),
        ] {
            let event = KeyEvent::new_with_kind(
                KeyCode::Modifier(code),
                KeyModifiers::SHIFT | KeyModifiers::CONTROL | KeyModifiers::ALT,
                KeyEventKind::Release,
            );
            let normalized = from_event(event).unwrap();
            let expected =
                from_event(KeyEvent::new(KeyCode::Char('a'), event.modifiers & !bit)).unwrap();
            assert_eq!(normalized.modifiers, expected.modifiers);
        }
    }

    #[test]
    fn shared_keyboard_vectors() {
        let vectors: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../protocol/yas/keyboard-vectors.json"
        ))
        .unwrap();
        for vector in vectors.as_array().unwrap() {
            let input = &vector["input"];
            let key = Key {
                key: input["key"].as_u64().unwrap() as u32,
                modifiers: input["modifiers"].as_u64().unwrap() as u8,
                event_type: input["eventType"].as_u64().unwrap() as u8,
                text: input["text"].as_str().unwrap().into(),
                shifted_key: input["shiftedKey"].as_u64().map(|n| n as u32),
                base_key: input["baseKey"].as_u64().map(|n| n as u32),
            };
            assert_eq!(
                encode(&key, vector["flags"].as_u64().unwrap() as u8, false),
                vector["expected"].as_str().map(|s| s.as_bytes().to_vec()),
                "{}",
                vector["name"]
            );
        }
    }
}
