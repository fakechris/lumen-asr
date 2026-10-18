//! Did a synthetic ⌘V actually reach the focused text field?
//!
//! Only editable fields that expose their contents through Accessibility can
//! be checked. Everything else (terminals, many web editors, secure fields)
//! yields [`Landing::Unknown`], which callers must treat like success:
//! unknown is not failed.

/// What Accessibility reported about the focused field at one moment.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct FieldState {
    /// `AXValue`, when the field is small enough to read cheaply.
    pub value: Option<String>,
    /// `AXNumberOfCharacters`.
    pub characters: Option<i64>,
    /// `AXSelectedTextRange` as (location, length) in UTF-16 units.
    pub selection: Option<(i64, i64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Landing {
    /// The field changed after the paste.
    Landed,
    /// The field exposes its contents and nothing changed.
    NotLanded,
    /// The field cannot tell us either way.
    Unknown,
}

/// Compare the focused field before and after a paste of non-empty text.
///
/// A non-empty paste always changes the contents or, when it replaces an
/// identical selection, the caret. So any observed change means it landed.
/// "Not landed" needs positive evidence: the contents (value or character
/// count) were readable both times and are identical, and the caret did not
/// move. A signal that appears or disappears between the two reads means the
/// field changed state in a way we cannot interpret, so the answer is unknown.
pub(crate) fn landing_decision(before: &FieldState, after: &FieldState) -> Landing {
    if before.value.is_some() != after.value.is_some()
        || before.characters.is_some() != after.characters.is_some()
        || before.selection.is_some() != after.selection.is_some()
    {
        return Landing::Unknown;
    }
    let value_changed = before.value != after.value;
    let characters_changed = before.characters != after.characters;
    let selection_changed = before.selection != after.selection;
    if value_changed || characters_changed || selection_changed {
        return Landing::Landed;
    }
    let contents_observable = before.value.is_some() || before.characters.is_some();
    if contents_observable {
        Landing::NotLanded
    } else {
        Landing::Unknown
    }
}

/// What to do after checking a paste.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PasteStep {
    /// Landed or unknown: finish as before (restore the clipboard if ours).
    Finish,
    /// Not landed and the clipboard still holds our text: paste once more.
    Retry,
    /// Still not landed after the retry: leave the text on the clipboard and
    /// tell the user, instead of restoring and losing the dictation.
    KeepTextOnClipboard,
}

/// `clipboard_ours` is whether the pasteboard still holds our write. When
/// someone else wrote in between, pasting again would insert their contents
/// and leaving "our" text is impossible, so fall back to the normal finish,
/// which keeps their newer contents.
pub(crate) fn next_paste_step(landing: Landing, clipboard_ours: bool, retried: bool) -> PasteStep {
    match (landing, clipboard_ours, retried) {
        (Landing::NotLanded, true, false) => PasteStep::Retry,
        (Landing::NotLanded, true, true) => PasteStep::KeepTextOnClipboard,
        _ => PasteStep::Finish,
    }
}

#[cfg(target_os = "macos")]
pub(crate) use ax::FocusedField;

#[cfg(target_os = "macos")]
mod ax {
    use super::{landing_decision, FieldState, Landing};
    use core_foundation::base::{CFRange, CFType, CFTypeRef, TCFType};
    use core_foundation::number::CFNumber;
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::c_void;
    use std::ptr;

    type AxUiElementRef = CFTypeRef;
    type AxError = i32;

    const AX_ERROR_SUCCESS: AxError = 0;
    const AX_VALUE_CF_RANGE_TYPE: u32 = 4;
    /// Above this size, compare the character count instead of copying the
    /// whole document on every poll.
    const MAX_VALUE_CHARACTERS: i64 = 20_000;
    /// A hung target app must not stall dictation for the default 6 s.
    const MESSAGING_TIMEOUT_SECS: f32 = 0.25;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXUIElementCreateSystemWide() -> AxUiElementRef;
        fn AXUIElementCopyAttributeValue(
            element: AxUiElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> AxError;
        fn AXUIElementIsAttributeSettable(
            element: AxUiElementRef,
            attribute: CFStringRef,
            settable: *mut u8,
        ) -> AxError;
        fn AXUIElementSetMessagingTimeout(element: AxUiElementRef, seconds: f32) -> AxError;
        fn AXUIElementGetPid(element: AxUiElementRef, pid: *mut i32) -> AxError;
        fn AXValueGetValue(value: CFTypeRef, value_type: u32, output: *mut c_void) -> bool;
    }

    /// The editable field that had key focus right before the paste.
    pub(crate) struct FocusedField {
        element: CFType,
        before: FieldState,
    }

    impl FocusedField {
        /// `None` when there is no focused element, it is not an editable
        /// text field, it is a secure field, or it exposes neither its value
        /// nor its character count — all cases where landing is unknowable.
        pub(crate) fn capture() -> Option<Self> {
            let element = focused_element()?;
            let raw = element.as_CFTypeRef();
            unsafe {
                AXUIElementSetMessagingTimeout(raw, MESSAGING_TIMEOUT_SECS);
            }
            if string_attribute(raw, "AXSubrole").as_deref() == Some("AXSecureTextField") {
                return None;
            }
            // Read-only text views (terminal scrollback, rendered documents)
            // change on their own schedule or not at all; only fields the user
            // can type into give a meaningful before/after.
            if !is_settable(raw, "AXValue") {
                return None;
            }
            let before = read_state(raw);
            if before.value.is_none() && before.characters.is_none() {
                return None;
            }
            Some(Self { element, before })
        }

        /// Compare the field now with its state at capture. Focus moving to a
        /// different element is unknown, not failure.
        pub(crate) fn check(&self) -> Landing {
            match focused_element() {
                Some(now) if now == self.element => {}
                _ => return Landing::Unknown,
            }
            landing_decision(&self.before, &read_state(self.element.as_CFTypeRef()))
        }

        /// Process owning the field, for posting the retry straight to it.
        pub(crate) fn pid(&self) -> Option<i32> {
            let mut pid = 0;
            let error = unsafe { AXUIElementGetPid(self.element.as_CFTypeRef(), &mut pid) };
            (error == AX_ERROR_SUCCESS && pid > 0).then_some(pid)
        }
    }

    fn focused_element() -> Option<CFType> {
        let system = unsafe { AXUIElementCreateSystemWide() };
        if system.is_null() {
            return None;
        }
        let system = unsafe { CFType::wrap_under_create_rule(system) };
        unsafe {
            AXUIElementSetMessagingTimeout(system.as_CFTypeRef(), MESSAGING_TIMEOUT_SECS);
        }
        copy_attribute(system.as_CFTypeRef(), "AXFocusedUIElement")
    }

    fn read_state(element: AxUiElementRef) -> FieldState {
        let characters = copy_attribute(element, "AXNumberOfCharacters")
            .and_then(|value| value.downcast_into::<CFNumber>())
            .and_then(|value| value.to_i64());
        let value = match characters {
            Some(count) if count > MAX_VALUE_CHARACTERS => None,
            _ => string_attribute(element, "AXValue"),
        };
        FieldState {
            value,
            characters,
            selection: selected_range(element),
        }
    }

    fn selected_range(element: AxUiElementRef) -> Option<(i64, i64)> {
        let value = copy_attribute(element, "AXSelectedTextRange")?;
        let mut range = CFRange {
            location: 0,
            length: 0,
        };
        let ok = unsafe {
            AXValueGetValue(
                value.as_CFTypeRef(),
                AX_VALUE_CF_RANGE_TYPE,
                &mut range as *mut CFRange as *mut c_void,
            )
        };
        ok.then_some((range.location as i64, range.length as i64))
    }

    fn is_settable(element: AxUiElementRef, name: &str) -> bool {
        let attribute = CFString::new(name);
        let mut settable = 0u8;
        let error = unsafe {
            AXUIElementIsAttributeSettable(element, attribute.as_concrete_TypeRef(), &mut settable)
        };
        error == AX_ERROR_SUCCESS && settable != 0
    }

    fn string_attribute(element: AxUiElementRef, name: &str) -> Option<String> {
        copy_attribute(element, name)?
            .downcast_into::<CFString>()
            .map(|value| value.to_string())
    }

    fn copy_attribute(element: AxUiElementRef, name: &str) -> Option<CFType> {
        let attribute = CFString::new(name);
        let mut value: CFTypeRef = ptr::null();
        let error = unsafe {
            AXUIElementCopyAttributeValue(element, attribute.as_concrete_TypeRef(), &mut value)
        };
        if error != AX_ERROR_SUCCESS || value.is_null() {
            return None;
        }
        Some(unsafe { CFType::wrap_under_create_rule(value) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(
        value: Option<&str>,
        characters: Option<i64>,
        caret: Option<(i64, i64)>,
    ) -> FieldState {
        FieldState {
            value: value.map(str::to_owned),
            characters,
            selection: caret,
        }
    }

    #[test]
    fn changed_value_means_landed() {
        let before = field(Some("hello "), Some(6), Some((6, 0)));
        let after = field(Some("hello world"), Some(11), Some((11, 0)));
        assert_eq!(landing_decision(&before, &after), Landing::Landed);
    }

    #[test]
    fn identical_readable_contents_mean_not_landed() {
        let before = field(Some("hello "), Some(6), Some((6, 0)));
        assert_eq!(
            landing_decision(&before, &before.clone()),
            Landing::NotLanded
        );
    }

    #[test]
    fn character_count_alone_is_enough_evidence() {
        let before = field(None, Some(50_000), None);
        assert_eq!(
            landing_decision(&before, &before.clone()),
            Landing::NotLanded
        );
        let after = field(None, Some(50_005), None);
        assert_eq!(landing_decision(&before, &after), Landing::Landed);
    }

    #[test]
    fn replacing_an_identical_selection_lands_through_the_caret() {
        let before = field(Some("say hi"), Some(6), Some((4, 2)));
        let after = field(Some("say hi"), Some(6), Some((6, 0)));
        assert_eq!(landing_decision(&before, &after), Landing::Landed);
    }

    #[test]
    fn caret_only_field_is_unknown_when_unchanged() {
        let before = field(None, None, Some((3, 0)));
        assert_eq!(landing_decision(&before, &before.clone()), Landing::Unknown);
    }

    #[test]
    fn nothing_readable_is_unknown_not_failed() {
        let before = FieldState::default();
        assert_eq!(landing_decision(&before, &before.clone()), Landing::Unknown);
    }

    #[test]
    fn a_signal_that_vanishes_is_unknown() {
        let before = field(Some("abc"), Some(3), Some((3, 0)));
        let after = field(None, Some(3), Some((3, 0)));
        assert_eq!(landing_decision(&before, &after), Landing::Unknown);
    }

    #[test]
    fn retries_once_while_the_clipboard_is_ours() {
        assert_eq!(
            next_paste_step(Landing::NotLanded, true, false),
            PasteStep::Retry
        );
        assert_eq!(
            next_paste_step(Landing::NotLanded, true, true),
            PasteStep::KeepTextOnClipboard
        );
    }

    #[test]
    fn landed_or_unknown_finishes_normally() {
        for retried in [false, true] {
            assert_eq!(
                next_paste_step(Landing::Landed, true, retried),
                PasteStep::Finish
            );
            assert_eq!(
                next_paste_step(Landing::Unknown, true, retried),
                PasteStep::Finish
            );
        }
    }

    #[test]
    fn someone_else_owning_the_clipboard_stops_the_retry() {
        assert_eq!(
            next_paste_step(Landing::NotLanded, false, false),
            PasteStep::Finish
        );
        assert_eq!(
            next_paste_step(Landing::NotLanded, false, true),
            PasteStep::Finish
        );
    }
}
