#[cfg(target_os = "macos")]
use async_trait::async_trait;
use lumen_edit_learning::{
    SurfaceAdapter, SurfaceError, SurfaceErrorKind, SurfaceReservation, TargetHint,
};
#[cfg(target_os = "macos")]
use lumen_edit_learning::{SurfaceDescriptor, SurfaceSnapshot, TextRange};
use std::sync::Arc;

const MAX_SNAPSHOT_CHARACTERS: i64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
struct GeometrySignature {
    x: i64,
    y: i64,
    width: i64,
    height: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReservedIdentity {
    role: String,
    subrole: String,
    identifier: Option<String>,
    geometry: Option<GeometrySignature>,
}

fn reserved_identity_matches(expected: &ReservedIdentity, current: &ReservedIdentity) -> bool {
    if expected.role != current.role || expected.subrole != current.subrole {
        return false;
    }
    match (&expected.identifier, &current.identifier) {
        (Some(expected), Some(current)) => expected == current,
        (None, None) => expected.geometry.is_some() && expected.geometry == current.geometry,
        _ => false,
    }
}

fn snapshot_length_supported(characters: Option<i64>) -> bool {
    characters.is_none_or(|characters| (0..=MAX_SNAPSHOT_CHARACTERS).contains(&characters))
}

#[cfg(test)]
mod identity_contract_tests {
    use super::{
        reserved_identity_matches, snapshot_length_supported, GeometrySignature, ReservedIdentity,
    };

    fn anonymous(y: i64) -> ReservedIdentity {
        ReservedIdentity {
            role: "AXTextField".into(),
            subrole: String::new(),
            identifier: None,
            geometry: Some(GeometrySignature {
                x: 10,
                y,
                width: 300,
                height: 40,
            }),
        }
    }

    #[test]
    fn anonymous_fields_require_matching_geometry_for_relocation() {
        assert!(reserved_identity_matches(&anonymous(20), &anonymous(20)));
        assert!(!reserved_identity_matches(&anonymous(20), &anonymous(120)));

        let mut no_geometry = anonymous(20);
        no_geometry.geometry = None;
        assert!(!reserved_identity_matches(&no_geometry, &no_geometry));
    }

    #[test]
    fn stable_identifiers_must_match_exactly() {
        let expected = ReservedIdentity {
            role: "AXTextArea".into(),
            subrole: String::new(),
            identifier: Some("message-composer".into()),
            geometry: None,
        };
        let mut current = expected.clone();
        assert!(reserved_identity_matches(&expected, &current));
        current.identifier = Some("search-field".into());
        assert!(!reserved_identity_matches(&expected, &current));
    }

    #[test]
    fn snapshot_length_rejects_invalid_or_oversized_fields() {
        assert!(snapshot_length_supported(None));
        assert!(snapshot_length_supported(Some(4096)));
        assert!(!snapshot_length_supported(Some(-1)));
        assert!(!snapshot_length_supported(Some(4097)));
    }
}

pub struct MacAccessibilitySurfaceAdapter;

impl SurfaceAdapter for MacAccessibilitySurfaceAdapter {
    fn reserve(&self, target: &TargetHint) -> Result<Arc<dyn SurfaceReservation>, SurfaceError> {
        let process_id = target.process_id.ok_or_else(|| SurfaceError {
            kind: SurfaceErrorKind::TemporarilyUnavailable,
            code: "target_process_id_unavailable".into(),
        })?;

        #[cfg(target_os = "macos")]
        {
            let element = ax::focused_element(process_id)?;
            let identity = ax::identity(&element);
            let surface_key = format!(
                "{}\u{001f}{}\u{001f}{}\u{001f}{}\u{001f}{}",
                target.bundle_id.as_deref().unwrap_or_default(),
                process_id,
                identity.role,
                identity.subrole,
                identity.identifier_or_instance
            );
            let descriptor = SurfaceDescriptor {
                adapter_kind: "macos_ax_direct_v1".into(),
                surface_key,
                target_app_name: target.app_name.clone(),
                target_bundle_id: target.bundle_id.clone(),
                target_fingerprint: format!(
                    "{}\u{001f}{}\u{001f}{}\u{001f}{}",
                    target.bundle_id.as_deref().unwrap_or_default(),
                    identity.role,
                    identity.subrole,
                    identity.identifier_or_instance
                ),
            };
            tracing::debug!(
                process_id,
                adapter = %descriptor.adapter_kind,
                role = %identity.role,
                has_identifier = identity.has_identifier,
                "reserved native accessibility edit surface"
            );
            Ok(Arc::new(MacAccessibilityReservation {
                descriptor,
                process_id,
                expected_identity: identity.reserved,
                element: Arc::new(std::sync::Mutex::new(element)),
            }))
        }

        #[cfg(not(target_os = "macos"))]
        {
            let _ = process_id;
            Err(SurfaceError {
                kind: SurfaceErrorKind::Unsupported,
                code: "macos_accessibility_unavailable".into(),
            })
        }
    }
}

#[cfg(target_os = "macos")]
struct MacAccessibilityReservation {
    descriptor: SurfaceDescriptor,
    process_id: u32,
    expected_identity: ReservedIdentity,
    element: Arc<std::sync::Mutex<ax::AxElement>>,
}

#[cfg(target_os = "macos")]
#[async_trait]
impl SurfaceReservation for MacAccessibilityReservation {
    fn descriptor(&self) -> &SurfaceDescriptor {
        &self.descriptor
    }

    async fn prepare_insertion(&self) -> Result<(), SurfaceError> {
        let process_id = self.process_id;
        let expected = self.expected_identity.clone();
        let element = self.element.clone();
        tokio::task::spawn_blocking(move || {
            let focused = ax::focused_element(process_id)?;
            let identity = ax::identity(&focused);
            let same_element = {
                let held = element.lock().map_err(|_| SurfaceError {
                    kind: SurfaceErrorKind::Internal,
                    code: "ax_element_lock_poisoned".into(),
                })?;
                ax::same_element(&held, &focused)
            };
            if !same_element && !reserved_identity_matches(&expected, &identity.reserved) {
                return Err(SurfaceError {
                    kind: SurfaceErrorKind::TargetRemoved,
                    code: "reserved_surface_is_not_focused".into(),
                });
            }
            *element.lock().map_err(|_| SurfaceError {
                kind: SurfaceErrorKind::Internal,
                code: "ax_element_lock_poisoned".into(),
            })? = focused;
            Ok(())
        })
        .await
        .map_err(|_| SurfaceError {
            kind: SurfaceErrorKind::Internal,
            code: "ax_prepare_blocking_task_failed".into(),
        })?
    }

    async fn snapshot(&self) -> Result<SurfaceSnapshot, SurfaceError> {
        let process_id = self.process_id;
        let expected = self.expected_identity.clone();
        let element = self.element.clone();
        let adapter = self.descriptor.adapter_kind.clone();
        tokio::task::spawn_blocking(move || {
            let direct = {
                let element = element.lock().map_err(|_| SurfaceError {
                    kind: SurfaceErrorKind::Internal,
                    code: "ax_element_lock_poisoned".into(),
                })?;
                ax::snapshot(&element)
            };
            match direct {
                Ok(snapshot) => Ok(snapshot),
                Err(direct_error) => {
                    let replacement = ax::focused_element(process_id).map_err(|_| direct_error)?;
                    let identity = ax::identity(&replacement);
                    if !reserved_identity_matches(&expected, &identity.reserved) {
                        return Err(SurfaceError {
                            kind: SurfaceErrorKind::TemporarilyUnavailable,
                            code: "focused_element_does_not_match_reserved_surface".into(),
                        });
                    }
                    let snapshot = ax::snapshot(&replacement)?;
                    *element.lock().map_err(|_| SurfaceError {
                        kind: SurfaceErrorKind::Internal,
                        code: "ax_element_lock_poisoned".into(),
                    })? = replacement;
                    tracing::info!(
                        process_id,
                        adapter,
                        "relocated native accessibility edit surface"
                    );
                    Ok(snapshot)
                }
            }
        })
        .await
        .map_err(|_| SurfaceError {
            kind: SurfaceErrorKind::Internal,
            code: "ax_snapshot_blocking_task_failed".into(),
        })?
    }
}

#[cfg(target_os = "macos")]
mod ax {
    use super::*;
    use core_foundation::base::{CFRange, CFType, CFTypeRef, TCFType};
    use core_foundation::number::CFNumber;
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::c_void;
    use std::ptr;

    type AxUiElementRef = CFTypeRef;
    type AxValueRef = CFTypeRef;
    type AxError = i32;

    const AX_ERROR_SUCCESS: AxError = 0;
    const AX_VALUE_CG_POINT_TYPE: u32 = 1;
    const AX_VALUE_CG_SIZE_TYPE: u32 = 2;
    const AX_VALUE_CF_RANGE_TYPE: u32 = 4;

    #[repr(C)]
    #[derive(Default)]
    struct AxPoint {
        x: f64,
        y: f64,
    }

    #[repr(C)]
    #[derive(Default)]
    struct AxSize {
        width: f64,
        height: f64,
    }

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn AXUIElementCreateApplication(pid: i32) -> AxUiElementRef;
        fn AXUIElementCopyAttributeValue(
            element: AxUiElementRef,
            attribute: CFStringRef,
            value: *mut CFTypeRef,
        ) -> AxError;
        fn AXUIElementCopyParameterizedAttributeValue(
            element: AxUiElementRef,
            attribute: CFStringRef,
            parameter: CFTypeRef,
            value: *mut CFTypeRef,
        ) -> AxError;
        fn AXValueCreate(value_type: u32, value: *const c_void) -> AxValueRef;
        fn AXValueGetValue(value: AxValueRef, value_type: u32, output: *mut c_void) -> bool;
    }

    #[derive(Clone)]
    pub(super) struct AxElement {
        value: CFType,
    }

    unsafe impl Send for AxElement {}
    unsafe impl Sync for AxElement {}

    impl AxElement {
        fn as_raw(&self) -> AxUiElementRef {
            self.value.as_CFTypeRef()
        }
    }

    pub(super) fn same_element(left: &AxElement, right: &AxElement) -> bool {
        left.value == right.value
    }

    pub(super) struct AxIdentity {
        pub role: String,
        pub subrole: String,
        pub identifier_or_instance: String,
        pub has_identifier: bool,
        pub reserved: ReservedIdentity,
    }

    pub(super) fn focused_element(process_id: u32) -> Result<AxElement, SurfaceError> {
        let app = unsafe { AXUIElementCreateApplication(process_id as i32) };
        if app.is_null() {
            return Err(unavailable("ax_application_unavailable"));
        }
        let app = unsafe { CFType::wrap_under_create_rule(app) };
        let focused = copy_attribute(app.as_CFTypeRef(), "AXFocusedUIElement")
            .ok_or_else(|| unavailable("ax_focused_element_unavailable"))?;
        Ok(AxElement { value: focused })
    }

    pub(super) fn identity(element: &AxElement) -> AxIdentity {
        let role = string_attribute(element.as_raw(), "AXRole").unwrap_or_default();
        let subrole = string_attribute(element.as_raw(), "AXSubrole").unwrap_or_default();
        let identifier = string_attribute(element.as_raw(), "AXIdentifier")
            .or_else(|| string_attribute(element.as_raw(), "AXDOMIdentifier"))
            .filter(|value| !value.is_empty());
        let geometry = geometry_signature(element.as_raw());
        let has_identifier = identifier.is_some();
        let identifier_or_instance = identifier
            .clone()
            .or_else(|| {
                geometry.as_ref().map(|geometry| {
                    format!(
                        "geometry:{}:{}:{}:{}",
                        geometry.x, geometry.y, geometry.width, geometry.height
                    )
                })
            })
            .unwrap_or_else(|| format!("unrelocatable:{:x}", element.as_raw() as usize));
        AxIdentity {
            reserved: ReservedIdentity {
                role: role.clone(),
                subrole: subrole.clone(),
                identifier,
                geometry,
            },
            role,
            subrole,
            identifier_or_instance,
            has_identifier,
        }
    }

    fn geometry_signature(element: AxUiElementRef) -> Option<GeometrySignature> {
        let position = copy_attribute(element, "AXPosition")?;
        let size = copy_attribute(element, "AXSize")?;
        let mut point = AxPoint::default();
        let mut dimensions = AxSize::default();
        let point_ok = unsafe {
            AXValueGetValue(
                position.as_CFTypeRef(),
                AX_VALUE_CG_POINT_TYPE,
                &mut point as *mut AxPoint as *mut c_void,
            )
        };
        let size_ok = unsafe {
            AXValueGetValue(
                size.as_CFTypeRef(),
                AX_VALUE_CG_SIZE_TYPE,
                &mut dimensions as *mut AxSize as *mut c_void,
            )
        };
        (point_ok && size_ok).then(|| GeometrySignature {
            x: point.x.round() as i64,
            y: point.y.round() as i64,
            width: dimensions.width.round() as i64,
            height: dimensions.height.round() as i64,
        })
    }

    pub(super) fn snapshot(element: &AxElement) -> Result<SurfaceSnapshot, SurfaceError> {
        let text = read_text(element.as_raw())?;
        let selection = selected_range(element.as_raw());
        Ok(SurfaceSnapshot {
            text,
            selection,
            observed_at: chrono::Utc::now(),
        })
    }

    fn read_text(element: AxUiElementRef) -> Result<String, SurfaceError> {
        let characters = number_of_characters(element);
        if !snapshot_length_supported(characters) {
            return Err(SurfaceError {
                kind: SurfaceErrorKind::Unsupported,
                code: "ax_text_exceeds_tracking_limit".into(),
            });
        }
        let text = string_attribute(element, "AXValue")
            .or_else(|| string_for_complete_range(element, characters))
            .ok_or_else(|| unavailable("ax_text_value_unavailable"))?;
        if text
            .chars()
            .take(MAX_SNAPSHOT_CHARACTERS as usize + 1)
            .count()
            > MAX_SNAPSHOT_CHARACTERS as usize
        {
            return Err(SurfaceError {
                kind: SurfaceErrorKind::Unsupported,
                code: "ax_text_exceeds_tracking_limit".into(),
            });
        }
        Ok(text)
    }

    fn number_of_characters(element: AxUiElementRef) -> Option<i64> {
        copy_attribute(element, "AXNumberOfCharacters")?
            .downcast_into::<CFNumber>()?
            .to_i64()
    }

    fn string_for_complete_range(
        element: AxUiElementRef,
        characters: Option<i64>,
    ) -> Option<String> {
        let characters = characters?;
        let range = CFRange {
            location: 0,
            length: characters as isize,
        };
        let range_value = unsafe {
            AXValueCreate(
                AX_VALUE_CF_RANGE_TYPE,
                &range as *const CFRange as *const c_void,
            )
        };
        if range_value.is_null() {
            return None;
        }
        let range_value = unsafe { CFType::wrap_under_create_rule(range_value) };
        let attribute = CFString::new("AXStringForRange");
        let mut value: CFTypeRef = ptr::null();
        let error = unsafe {
            AXUIElementCopyParameterizedAttributeValue(
                element,
                attribute.as_concrete_TypeRef(),
                range_value.as_CFTypeRef(),
                &mut value,
            )
        };
        if error != AX_ERROR_SUCCESS || value.is_null() {
            return None;
        }
        unsafe { CFType::wrap_under_create_rule(value) }
            .downcast_into::<CFString>()
            .map(|value| value.to_string())
    }

    fn selected_range(element: AxUiElementRef) -> Option<TextRange> {
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
        if !ok || range.location < 0 || range.length < 0 {
            return None;
        }
        Some(TextRange {
            location_utf16: range.location as usize,
            length_utf16: range.length as usize,
        })
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

    fn unavailable(code: &str) -> SurfaceError {
        SurfaceError {
            kind: SurfaceErrorKind::TemporarilyUnavailable,
            code: code.into(),
        }
    }
}
