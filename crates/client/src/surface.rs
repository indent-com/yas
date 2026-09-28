//! Surfaces: the windows GUI programs open on the server's compositor.
//!
//! When the server runs its compositor (`yas server` does unless
//! `YAS_SKIP_COMPOSITOR=1`; [`crate::host::HostOptions::compositor`] for a
//! hosted one), the programs it starts get a `WAYLAND_DISPLAY`, and each
//! window they map is a surface in the catalogue ([`Client::surfaces`]). A
//! client can capture one as an image ([`Client::capture_surface`]), click,
//! scroll and type into it, resize, focus and close it: what `yas surface`
//! does, for programs that drive GUIs (a screenshot, then a click).
//!
//! Input goes through a short-lived view, as the CLI's does: the server
//! delivers it to the window as if a user had clicked or typed there.

use std::collections::BTreeSet;

use yas_wire::{
    Decode, Encode, Extensions,
    core::ResultPrefix,
    family,
    schema::surface as schema,
    state::{Record, RecordKind, Watch as StateWatch},
    surface::{self as wire, request_kind},
    transfer::InlineOrTransfer,
};

use crate::client::{Client, DEFAULT_REQUEST_TIMEOUT, Hook};
use crate::error::{Error, Result};
use crate::state::{STATE_CREDIT, Subscription};
use crate::transfer::{collect_delivery, delivery_routes};

/// The most a capture may hold.
pub const MAX_CAPTURE_BYTES: u64 = 64 * 1024 * 1024;
/// Receive credit offered for a capture's Transfer.
const CAPTURE_CREDIT: u64 = 1024 * 1024;
/// Pixels of scrolling per wheel detent.
const WHEEL_DETENT_PIXELS: f64 = 120.0;

/// A window in the server's catalogue.
#[derive(Clone, Debug, PartialEq)]
pub struct SurfaceInfo {
    /// Its ID, the same for every client of the server.
    pub id: u64,
    /// Bumped whenever its state changes.
    pub revision: u64,
    /// The window it belongs to (a dialog's main window).
    pub parent: Option<u64>,
    pub title: String,
    /// The program's application ID (`app_id` on Wayland).
    pub app_id: String,
    /// Its size in pixels, as captured.
    pub width: u32,
    pub height: u32,
    /// Its size in logical pixels, as [`Client::resize_surface`] sets it.
    pub logical_width: f64,
    pub logical_height: f64,
}

impl SurfaceInfo {
    fn from_record(record: &wire::SurfaceRecord) -> Self {
        Self {
            id: record.surface_handle,
            revision: record.revision,
            parent: (record.parent_handle != 0).then_some(record.parent_handle),
            title: record.title.clone(),
            app_id: record.application_id.clone(),
            width: record.composite_width,
            height: record.composite_height,
            logical_width: from_fixed(record.logical_width_32_32),
            logical_height: from_fixed(record.logical_height_32_32),
        }
    }
}

/// An image format for [`Client::capture_surface`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureFormat {
    Png,
    Avif,
}

/// A pointer button for [`Client::click_surface`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// One key going down or up, in YAS's native key codes (USB HID usages) with
/// the modifiers held. [`key_combo`] and [`typed_keys`] build them from text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyEvent {
    pub code: u16,
    pub pressed: bool,
    /// `MODIFIER_*` bits from `yas_wire::schema::surface`.
    pub modifiers: u32,
}

impl Client {
    /// Every window on the server's compositor (none when it runs no
    /// compositor).
    pub async fn surfaces(&self) -> Result<Vec<SurfaceInfo>> {
        Ok(self
            .surface_records()
            .await?
            .values()
            .map(SurfaceInfo::from_record)
            .collect())
    }

    /// The window with this ID (a `NotFound` status error when there is
    /// none).
    pub async fn surface(&self, id: u64) -> Result<SurfaceInfo> {
        Ok(SurfaceInfo::from_record(&self.surface_record(id).await?))
    }

    /// What the window shows now, as an image.
    pub async fn capture_surface(&self, id: u64, format: CaptureFormat) -> Result<Vec<u8>> {
        let record = self.surface_record(id).await?;
        let hook: Hook = Box::new(|prefix: &ResultPrefix| {
            InlineOrTransfer::decode(&prefix.body)
                .map(|result| delivery_routes(&result))
                .unwrap_or_default()
        });
        let mut reply = self
            .call_ok(
                family::SURFACE,
                request_kind::CAPTURE,
                wire::Capture {
                    surface_handle: id,
                    revision: record.revision,
                    initial_receive_credit: CAPTURE_CREDIT,
                    formats: vec![match format {
                        CaptureFormat::Png => schema::CAPTURE_PNG as u8,
                        CaptureFormat::Avif => schema::CAPTURE_AVIF as u8,
                    }],
                    extensions: Extensions::default(),
                }
                .encode()?,
                Some(DEFAULT_REQUEST_TIMEOUT),
                Some(hook),
            )
            .await?;
        let result = InlineOrTransfer::decode(&reply.prefix.body)?;
        let frames = delivery_routes(&result)
            .into_iter()
            .next()
            .and_then(|route| reply.take(route));
        collect_delivery(self, result, frames, MAX_CAPTURE_BYTES).await
    }

    /// Ask the window's program to take this size, in logical pixels.
    pub async fn resize_surface(&self, id: u64, width: u32, height: u32) -> Result<()> {
        if width == 0 || height == 0 {
            return Err(Error::invalid("a surface needs a nonzero width and height"));
        }
        let _: wire::RevisionResult = self
            .request(
                family::SURFACE,
                request_kind::RESIZE,
                &wire::Resize {
                    surface_handle: id,
                    operation_id: operation_id(),
                    logical_width_32_32: i64::from(width) << 32,
                    logical_height_32_32: i64::from(height) << 32,
                    extensions: Extensions::default(),
                },
            )
            .await?;
        Ok(())
    }

    /// Give the window keyboard focus.
    pub async fn focus_surface(&self, id: u64) -> Result<()> {
        let _: wire::RevisionResult = self
            .request(
                family::SURFACE,
                request_kind::FOCUS,
                &wire::Focus {
                    surface_handle: id,
                    operation_id: operation_id(),
                    focused: true,
                    extensions: Extensions::default(),
                },
            )
            .await?;
        Ok(())
    }

    /// Ask the window's program to close it (as its close button would).
    pub async fn close_surface(&self, id: u64) -> Result<()> {
        self.surface_empty(
            request_kind::CLOSE,
            &wire::Close {
                surface_handle: id,
                operation_id: operation_id(),
                extensions: Extensions::default(),
            },
        )
        .await
    }

    /// Click at `(x, y)`, in pixels of the window as captured.
    pub async fn click_surface(
        &self,
        id: u64,
        x: u32,
        y: u32,
        button: PointerButton,
    ) -> Result<()> {
        let button = match button {
            PointerButton::Left => schema::POINTER_BUTTON_PRIMARY,
            PointerButton::Right => schema::POINTER_BUTTON_SECONDARY,
            PointerButton::Middle => schema::POINTER_BUTTON_MIDDLE,
            PointerButton::Back => schema::POINTER_BUTTON_BACK,
            PointerButton::Forward => schema::POINTER_BUTTON_FORWARD,
        } as u8;
        let view = self.open_input_view(id).await?;
        let x_32_32 = pointer_coordinate(x, view.width)?;
        let y_32_32 = pointer_coordinate(y, view.height)?;
        let sent = [schema::POINTER_PHASE_DOWN, schema::POINTER_PHASE_UP]
            .into_iter()
            .try_for_each(|phase| {
                self.send_surface_event(
                    wire::event_kind::POINTER,
                    &wire::Pointer {
                        view_id: view.result.view_id,
                        feedback: view.feedback(),
                        client_monotonic_ns: self.monotonic_ns(),
                        phase: phase as u8,
                        button,
                        x_32_32,
                        y_32_32,
                    },
                )
            });
        self.close_input_view(view, sent).await
    }

    /// Scroll the window by wheel detents (positive: down and right; `3.0`
    /// is three notches of a mouse wheel).
    pub async fn scroll_surface(&self, id: u64, dx: f64, dy: f64) -> Result<()> {
        let (dx_32_32, steps_x) = scroll_amount(dx)?;
        let (dy_32_32, steps_y) = scroll_amount(dy)?;
        let view = self.open_input_view(id).await?;
        let sent = self.send_surface_event(
            wire::event_kind::AXIS,
            &wire::Axis {
                view_id: view.result.view_id,
                feedback: view.feedback(),
                client_monotonic_ns: self.monotonic_ns(),
                source: schema::AXIS_SOURCE_WHEEL as u8,
                flags: 0,
                dx_32_32,
                dy_32_32,
                steps_x,
                steps_y,
            },
        );
        self.close_input_view(view, sent).await
    }

    /// Enter `text` into the window as committed text (what an input method
    /// sends): any characters, no key codes involved.
    pub async fn type_surface_text(&self, id: u64, text: &str) -> Result<()> {
        let view = self.open_input_view(id).await?;
        let sent = self.send_surface_event(
            wire::event_kind::TEXT,
            &wire::Text {
                view_id: view.result.view_id,
                feedback: view.feedback(),
                client_monotonic_ns: self.monotonic_ns(),
                text: text.to_owned(),
            },
        );
        self.close_input_view(view, sent).await
    }

    /// Press and release keys, in order: `&key_combo("ctrl+c")?`,
    /// `&typed_keys("hello{enter}")?`.
    pub async fn press_surface_keys(&self, id: u64, keys: &[KeyEvent]) -> Result<()> {
        let view = self.open_input_view(id).await?;
        let sent = keys.iter().try_for_each(|key| {
            self.send_surface_event(
                wire::event_kind::KEY,
                &wire::Key {
                    view_id: view.result.view_id,
                    feedback: view.feedback(),
                    client_monotonic_ns: self.monotonic_ns(),
                    key_code: key.code,
                    state: if key.pressed {
                        schema::KEY_STATE_PRESSED
                    } else {
                        schema::KEY_STATE_RELEASED
                    } as u8,
                    modifiers: key.modifiers,
                },
            )
        });
        self.close_input_view(view, sent).await
    }

    async fn surface_records(
        &self,
    ) -> Result<std::collections::BTreeMap<u64, wire::SurfaceRecord>> {
        let mut subscription = Subscription::open(
            self,
            family::SURFACE,
            request_kind::WATCH,
            request_kind::UNWATCH,
            StateWatch {
                initial_credit: STATE_CREDIT,
                resume: None,
                extensions: Extensions::default(),
            }
            .encode()?,
        )
        .await?;
        let mut catalogue = std::collections::BTreeMap::new();
        for record in &subscription.snapshot().await? {
            fold(&mut catalogue, record)?;
        }
        Ok(catalogue)
    }

    async fn surface_record(&self, id: u64) -> Result<wire::SurfaceRecord> {
        self.surface_records().await?.remove(&id).ok_or_else(|| {
            Error::status_from(
                format!("surface {id}"),
                yas_wire::core::Status::NotFound,
                Extensions::default(),
            )
        })
    }

    async fn surface_empty<Q: Encode>(&self, kind: u16, request: &Q) -> Result<()> {
        let reply = self
            .call_ok(
                family::SURFACE,
                kind,
                request.encode()?,
                Some(DEFAULT_REQUEST_TIMEOUT),
                None,
            )
            .await?;
        if reply.prefix.body.is_empty() {
            Ok(())
        } else {
            Err(Error::protocol(format!(
                "YAS Surface Result {kind:#06x} had an unexpected body"
            )))
        }
    }

    fn send_surface_event<E: Encode>(&self, kind: u16, event: &E) -> Result<()> {
        if !self.supports(family::SURFACE, yas_wire::Class::Event, kind) {
            return Err(Error::Unsupported(
                "this YAS session cannot send input to surfaces (read-only, or no compositor)"
                    .into(),
            ));
        }
        self.send_event(family::SURFACE, kind, event, true)
    }

    /// An input-only view of the window at its captured size.
    async fn open_input_view(&self, id: u64) -> Result<InputView> {
        let record = self.surface_record(id).await?;
        let (width, height) = (record.composite_width, record.composite_height);
        let result: wire::ViewResult = self
            .request(
                family::SURFACE,
                request_kind::OPEN_VIEW,
                &wire::OpenView {
                    surface_handle: id,
                    width,
                    height,
                    max_fps: 1,
                    decoder_capacity: 1,
                    // Input-only views never decode a frame. Offering one
                    // codec spares OPEN_VIEW the host's multi-codec encoder
                    // probes before input can be delivered.
                    codec_versions: vec![schema::CODEC_H264_V1 as u16],
                    extensions: Extensions::default(),
                },
            )
            .await?;
        Ok(InputView {
            result,
            width,
            height,
        })
    }

    /// Close the view after its events went out (the Request follows them on
    /// the session); `sent` is how sending them went.
    async fn close_input_view(&self, view: InputView, sent: Result<()>) -> Result<()> {
        let closed = self
            .surface_empty(
                request_kind::CLOSE_VIEW,
                &wire::CloseView {
                    view_id: view.result.view_id,
                },
            )
            .await;
        sent.and(closed)
    }
}

struct InputView {
    result: wire::ViewResult,
    width: u32,
    height: u32,
}

impl InputView {
    fn feedback(&self) -> wire::FrameFeedback {
        wire::FrameFeedback {
            presented_sequence: self.result.first_sequence.saturating_sub(1),
            decoder_queue_depth: 0,
            available_slots: self.result.max_inflight_frames,
        }
    }
}

/// Fold one catalogue state record into `catalogue`, by surface ID.
fn fold(
    catalogue: &mut std::collections::BTreeMap<u64, wire::SurfaceRecord>,
    record: &Record,
) -> Result<()> {
    match record.kind {
        RecordKind::Add | RecordKind::Replace => {
            let surface = wire::surface_from_state_record(record)?;
            catalogue.insert(surface.surface_handle, surface);
        }
        RecordKind::Patch => {
            let patch = wire::SurfacePatch::decode(&record.body)?;
            if let Some(surface) = catalogue.get_mut(&patch.surface_handle) {
                surface.revision = patch.revision;
                for extension in patch.extensions.0 {
                    surface
                        .extensions
                        .0
                        .retain(|existing| existing.tag != extension.tag);
                    surface.extensions.0.push(extension);
                }
            }
        }
        RecordKind::Remove => {
            catalogue.remove(&wire::RemovedSurface::decode(&record.body)?.surface_handle);
        }
        RecordKind::Family(_) => {}
    }
    Ok(())
}

/// The keys of a chord such as `ctrl+shift+t`, `enter` or `a`: the
/// modifiers go down, the key goes down and up, the modifiers come up.
/// Modifiers: `ctrl`/`control`, `shift`, `alt`, `super`/`meta`. Keys: a
/// letter, digit or US-layout punctuation character, `enter`/`return`,
/// `escape`/`esc`, `tab`, `backspace`, `space`, arrows (`up`, `down`, `left`,
/// `right`), `home`, `end`, `pageup`, `pagedown`, `insert`, `delete`,
/// `f1`–`f12`, `minus`, `equal`, or a modifier on its own.
pub fn key_combo(combo: &str) -> Result<Vec<KeyEvent>> {
    let parts = combo.split('+').collect::<Vec<_>>();
    let main = parts
        .last()
        .copied()
        .filter(|part| !part.is_empty())
        .ok_or_else(|| Error::invalid("empty key"))?;
    let mut modifiers = Vec::new();
    let mut seen = BTreeSet::new();
    for part in &parts[..parts.len().saturating_sub(1)] {
        let modifier = modifier_key(part)
            .ok_or_else(|| Error::invalid(format!("unknown modifier: {part}")))?;
        if !seen.insert(modifier.1) {
            return Err(Error::invalid(format!(
                "modifier {part:?} appears more than once"
            )));
        }
        modifiers.push(modifier);
    }
    let main = key_name(main).ok_or_else(|| Error::invalid(format!("unknown key: {main}")))?;
    Ok(key_chord(&modifiers, main))
}

/// The keys that type `text` on a US layout (letters, digits, punctuation,
/// space, tab, newline), with `{chord}` for a [`key_combo`] such as
/// `{enter}` or `{ctrl+a}`. For other characters, use
/// [`Client::type_surface_text`].
pub fn typed_keys(text: &str) -> Result<Vec<KeyEvent>> {
    let chars = text.chars().collect::<Vec<_>>();
    let mut events = Vec::new();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '{' {
            let end = chars[index..]
                .iter()
                .position(|character| *character == '}')
                .ok_or_else(|| Error::invalid("unclosed { in type string"))?;
            let inner = chars[index + 1..index + end].iter().collect::<String>();
            events.extend(key_combo(&inner)?);
            index += end + 1;
            continue;
        }
        let (code, shift) = character_key(chars[index])
            .ok_or_else(|| Error::invalid(format!("unsupported character: {}", chars[index])))?;
        let modifiers = if shift {
            vec![modifier_key("shift").expect("known Shift modifier")]
        } else {
            Vec::new()
        };
        events.extend(key_chord(&modifiers, code));
        index += 1;
    }
    Ok(events)
}

fn key_chord(modifiers: &[(u16, u32)], main: u16) -> Vec<KeyEvent> {
    let mut mask = 0u32;
    let mut events = Vec::with_capacity(2 + modifiers.len() * 2);
    for &(code, bit) in modifiers {
        mask |= bit;
        events.push(KeyEvent {
            code,
            pressed: true,
            modifiers: mask,
        });
    }
    events.push(KeyEvent {
        code: main,
        pressed: true,
        modifiers: mask,
    });
    events.push(KeyEvent {
        code: main,
        pressed: false,
        modifiers: mask,
    });
    for &(code, bit) in modifiers.iter().rev() {
        events.push(KeyEvent {
            code,
            pressed: false,
            modifiers: mask,
        });
        mask &= !bit;
    }
    events
}

fn modifier_key(name: &str) -> Option<(u16, u32)> {
    let (code, bit) = match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => (schema::KEY_CONTROL_LEFT, schema::MODIFIER_CONTROL),
        "shift" => (schema::KEY_SHIFT_LEFT, schema::MODIFIER_SHIFT),
        "alt" => (schema::KEY_ALT_LEFT, schema::MODIFIER_ALT),
        "super" | "meta" => (schema::KEY_SUPER_LEFT, schema::MODIFIER_SUPER),
        _ => return None,
    };
    Some((code as u16, bit as u32))
}

fn key_name(name: &str) -> Option<u16> {
    let name = name.to_ascii_lowercase();
    if name.chars().count() == 1 {
        return character_key(name.chars().next()?).map(|(code, _)| code);
    }
    let value = match name.as_str() {
        "return" | "enter" => schema::KEY_ENTER,
        "escape" | "esc" => schema::KEY_ESCAPE,
        "tab" => schema::KEY_TAB,
        "backspace" | "bs" => schema::KEY_BACKSPACE,
        "space" => schema::KEY_SPACE,
        "up" => schema::KEY_ARROW_UP,
        "down" => schema::KEY_ARROW_DOWN,
        "left" => schema::KEY_ARROW_LEFT,
        "right" => schema::KEY_ARROW_RIGHT,
        "home" => schema::KEY_HOME,
        "end" => schema::KEY_END,
        "pageup" | "page_up" => schema::KEY_PAGE_UP,
        "pagedown" | "page_down" => schema::KEY_PAGE_DOWN,
        "insert" => schema::KEY_INSERT,
        "delete" | "del" => schema::KEY_DELETE,
        "f1" => schema::KEY_F1,
        "f2" => schema::KEY_F2,
        "f3" => schema::KEY_F3,
        "f4" => schema::KEY_F4,
        "f5" => schema::KEY_F5,
        "f6" => schema::KEY_F6,
        "f7" => schema::KEY_F7,
        "f8" => schema::KEY_F8,
        "f9" => schema::KEY_F9,
        "f10" => schema::KEY_F10,
        "f11" => schema::KEY_F11,
        "f12" => schema::KEY_F12,
        "minus" => schema::KEY_MINUS,
        "equal" => schema::KEY_EQUAL,
        "ctrl" | "control" => schema::KEY_CONTROL_LEFT,
        "shift" => schema::KEY_SHIFT_LEFT,
        "alt" => schema::KEY_ALT_LEFT,
        "super" | "meta" => schema::KEY_SUPER_LEFT,
        _ => return None,
    };
    Some(value as u16)
}

fn character_key(character: char) -> Option<(u16, bool)> {
    let (code, shift) = match character {
        'a'..='z' => (schema::KEY_A + (character as u64 - 'a' as u64), false),
        'A'..='Z' => (schema::KEY_A + (character as u64 - 'A' as u64), true),
        '1'..='9' => (schema::KEY_1 + (character as u64 - '1' as u64), false),
        '0' => (schema::KEY_0, false),
        ' ' => (schema::KEY_SPACE, false),
        '-' => (schema::KEY_MINUS, false),
        '=' => (schema::KEY_EQUAL, false),
        '[' => (schema::KEY_BRACKET_LEFT, false),
        ']' => (schema::KEY_BRACKET_RIGHT, false),
        '\\' => (schema::KEY_BACKSLASH, false),
        ';' => (schema::KEY_SEMICOLON, false),
        '\'' => (schema::KEY_QUOTE, false),
        '`' => (schema::KEY_BACKQUOTE, false),
        ',' => (schema::KEY_COMMA, false),
        '.' => (schema::KEY_PERIOD, false),
        '/' => (schema::KEY_SLASH, false),
        '\t' => (schema::KEY_TAB, false),
        '\n' => (schema::KEY_ENTER, false),
        '!' => (schema::KEY_1, true),
        '@' => (schema::KEY_2, true),
        '#' => (schema::KEY_3, true),
        '$' => (schema::KEY_4, true),
        '%' => (schema::KEY_5, true),
        '^' => (schema::KEY_6, true),
        '&' => (schema::KEY_7, true),
        '*' => (schema::KEY_8, true),
        '(' => (schema::KEY_9, true),
        ')' => (schema::KEY_0, true),
        '_' => (schema::KEY_MINUS, true),
        '+' => (schema::KEY_EQUAL, true),
        '{' => (schema::KEY_BRACKET_LEFT, true),
        '}' => (schema::KEY_BRACKET_RIGHT, true),
        '|' => (schema::KEY_BACKSLASH, true),
        ':' => (schema::KEY_SEMICOLON, true),
        '"' => (schema::KEY_QUOTE, true),
        '~' => (schema::KEY_BACKQUOTE, true),
        '<' => (schema::KEY_COMMA, true),
        '>' => (schema::KEY_PERIOD, true),
        '?' => (schema::KEY_SLASH, true),
        _ => return None,
    };
    Some((code as u16, shift))
}

/// A pixel coordinate as a 32.32 fraction of the view's extent.
fn pointer_coordinate(pixel: u32, extent: u32) -> Result<i64> {
    if extent == 0 {
        return Err(Error::protocol("YAS Surface view has a zero extent"));
    }
    let pixel = u128::from(pixel.min(extent));
    i64::try_from((pixel << 32) / u128::from(extent))
        .map_err(|_| Error::invalid("pointer coordinate out of range"))
}

/// Wheel detents as a 32.32 pixel distance and whole steps.
fn scroll_amount(detents: f64) -> Result<(i64, i32)> {
    let pixels = detents * WHEEL_DETENT_PIXELS * 4_294_967_296.0;
    let steps = detents.round();
    if !detents.is_finite()
        || pixels < i64::MIN as f64
        || pixels > i64::MAX as f64
        || steps < f64::from(i32::MIN)
        || steps > f64::from(i32::MAX)
    {
        return Err(Error::invalid("scroll amount out of range"));
    }
    Ok((pixels.round() as i64, steps as i32))
}

fn from_fixed(value: i64) -> f64 {
    value as f64 / 4_294_967_296.0
}

fn operation_id() -> [u8; 16] {
    let mut value: [u8; 16] = rand::random();
    if value == [0; 16] {
        value[15] = 1;
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_combos_use_native_hid_codes_and_modifier_bits() {
        let events = key_combo("ctrl+a").unwrap();
        assert_eq!(events.len(), 4);
        assert_eq!(events[0].code, schema::KEY_CONTROL_LEFT as u16);
        assert!(events[0].pressed);
        assert_eq!(events[1].code, schema::KEY_A as u16);
        assert_eq!(events[1].modifiers, schema::MODIFIER_CONTROL as u32);
        assert!(!events[3].pressed);
        assert_eq!(events[3].modifiers, schema::MODIFIER_CONTROL as u32);
        assert!(key_combo("ctrl+ctrl+a").is_err());
        assert!(key_combo("hyper+a").is_err());
        assert!(key_combo("").is_err());
    }

    #[test]
    fn typed_keys_shift_capitals_and_take_chords_in_braces() {
        let events = typed_keys("Hi{enter}").unwrap();
        // Shift down, H down, H up, Shift up; i down, i up; Enter down, up.
        assert_eq!(events.len(), 8);
        assert_eq!(events[0].code, schema::KEY_SHIFT_LEFT as u16);
        assert_eq!(events[1].modifiers, schema::MODIFIER_SHIFT as u32);
        assert_eq!(events[4].modifiers, 0);
        assert_eq!(events[6].code, schema::KEY_ENTER as u16);
        assert!(typed_keys("{enter").is_err());
        assert!(typed_keys("é").is_err());
    }

    #[test]
    fn pointer_pixels_are_fractions_of_the_view_and_scrolls_are_detents() {
        assert_eq!(pointer_coordinate(0, 1920).unwrap(), 0);
        assert_eq!(pointer_coordinate(960, 1920).unwrap(), 1_i64 << 31);
        assert_eq!(pointer_coordinate(5000, 1920).unwrap(), 1_i64 << 32);
        assert!(pointer_coordinate(0, 0).is_err());
        assert_eq!(scroll_amount(1.0).unwrap(), (120_i64 << 32, 1));
        assert_eq!(scroll_amount(-0.4).unwrap().1, 0);
        assert!(scroll_amount(f64::NAN).is_err());
    }
}
