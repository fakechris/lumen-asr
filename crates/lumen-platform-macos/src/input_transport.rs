//! Input device transport lookup (CoreAudio `kAudioDevicePropertyTransportType`).
//!
//! Capture picks devices by name, so this module reports each input device's
//! name next to how it is attached. The selection policy lives with the
//! caller; this module only answers "what is the default input, and how is
//! each input connected". Non-macOS builds and HAL failures yield an empty
//! list, which callers must treat as "keep the system default".

/// How an audio device is attached to the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputTransport {
    BuiltIn,
    Bluetooth,
    BluetoothLe,
    Usb,
    Virtual,
    Aggregate,
    /// Any other transport (AirPlay, Thunderbolt, HDMI, ...) or an unknown code.
    Other(u32),
}

impl InputTransport {
    /// Maps a raw `kAudioDeviceTransportType*` four-char code.
    pub fn from_code(code: u32) -> Self {
        match &code.to_be_bytes() {
            b"bltn" => Self::BuiltIn,
            b"blue" => Self::Bluetooth,
            b"blea" => Self::BluetoothLe,
            b"usb " => Self::Usb,
            b"virt" => Self::Virtual,
            b"grup" => Self::Aggregate,
            _ => Self::Other(code),
        }
    }

    pub fn is_bluetooth(self) -> bool {
        matches!(self, Self::Bluetooth | Self::BluetoothLe)
    }

    pub fn is_built_in(self) -> bool {
        self == Self::BuiltIn
    }

    /// Short stable label for logs.
    pub fn label(self) -> &'static str {
        match self {
            Self::BuiltIn => "built_in",
            Self::Bluetooth => "bluetooth",
            Self::BluetoothLe => "bluetooth_le",
            Self::Usb => "usb",
            Self::Virtual => "virtual",
            Self::Aggregate => "aggregate",
            Self::Other(_) => "other",
        }
    }
}

/// One device that exposes at least one input stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputDeviceTransport {
    /// `kAudioDevicePropertyDeviceNameCFString`, the same name cpal reports.
    pub name: String,
    pub transport: InputTransport,
    /// True for the current system default input device.
    pub is_default: bool,
}

/// Lists input-capable devices with their transport. Empty when unavailable.
pub fn input_device_transports() -> Vec<InputDeviceTransport> {
    imp::input_device_transports()
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use super::InputDeviceTransport;

    pub(super) fn input_device_transports() -> Vec<InputDeviceTransport> {
        Vec::new()
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{InputDeviceTransport, InputTransport};
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::c_void;
    use std::mem::size_of;
    use std::ptr;

    type AudioObjectID = u32;

    #[repr(C)]
    struct Address {
        selector: u32,
        scope: u32,
        element: u32,
    }

    const fn fourcc(s: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*s)
    }

    const SYSTEM_OBJECT: AudioObjectID = 1; // kAudioObjectSystemObject
    const SCOPE_GLOBAL: u32 = fourcc(b"glob"); // kAudioObjectPropertyScopeGlobal
    const SCOPE_INPUT: u32 = fourcc(b"inpt"); // kAudioObjectPropertyScopeInput
    const DEVICES: u32 = fourcc(b"dev#"); // kAudioHardwarePropertyDevices
    const DEFAULT_INPUT: u32 = fourcc(b"dIn "); // kAudioHardwarePropertyDefaultInputDevice
    const STREAMS: u32 = fourcc(b"stm#"); // kAudioDevicePropertyStreams
    const NAME: u32 = fourcc(b"lnam"); // kAudioDevicePropertyDeviceNameCFString
    const TRANSPORT: u32 = fourcc(b"tran"); // kAudioDevicePropertyTransportType

    // SAFETY: standard CoreAudio.framework HAL entry points.
    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectGetPropertyDataSize(
            object: AudioObjectID,
            address: *const Address,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
        ) -> i32;
        fn AudioObjectGetPropertyData(
            object: AudioObjectID,
            address: *const Address,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
            data: *mut c_void,
        ) -> i32;
    }

    fn addr(selector: u32, scope: u32) -> Address {
        Address {
            selector,
            scope,
            element: 0, // kAudioObjectPropertyElementMain
        }
    }

    fn data_size(object: AudioObjectID, address: &Address) -> Option<u32> {
        let mut size = 0u32;
        // SAFETY: size query on a well-known property.
        let status =
            unsafe { AudioObjectGetPropertyDataSize(object, address, 0, ptr::null(), &mut size) };
        (status == 0).then_some(size)
    }

    fn read_u32(object: AudioObjectID, selector: u32) -> Option<u32> {
        let address = addr(selector, SCOPE_GLOBAL);
        let mut value = 0u32;
        let mut size = size_of::<u32>() as u32;
        // SAFETY: reads a fixed-size scalar property into a local.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                &address,
                0,
                ptr::null(),
                &mut size,
                &mut value as *mut u32 as *mut c_void,
            )
        };
        (status == 0 && size == size_of::<u32>() as u32).then_some(value)
    }

    fn read_name(object: AudioObjectID) -> Option<String> {
        let address = addr(NAME, SCOPE_GLOBAL);
        let mut cf: CFStringRef = ptr::null();
        let mut size = size_of::<CFStringRef>() as u32;
        // SAFETY: the property yields a +1 (create-rule) CFStringRef we own.
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                &address,
                0,
                ptr::null(),
                &mut size,
                &mut cf as *mut CFStringRef as *mut c_void,
            )
        };
        if status != 0 || cf.is_null() {
            return None;
        }
        Some(unsafe { CFString::wrap_under_create_rule(cf) }.to_string())
    }

    fn device_ids() -> Vec<AudioObjectID> {
        let address = addr(DEVICES, SCOPE_GLOBAL);
        let Some(size) = data_size(SYSTEM_OBJECT, &address).filter(|s| *s > 0) else {
            return Vec::new();
        };
        let mut ids = vec![0 as AudioObjectID; size as usize / size_of::<AudioObjectID>()];
        let mut io_size = size;
        // SAFETY: standard size-then-data HAL pattern into a buffer of `size` bytes.
        let status = unsafe {
            AudioObjectGetPropertyData(
                SYSTEM_OBJECT,
                &address,
                0,
                ptr::null(),
                &mut io_size,
                ids.as_mut_ptr() as *mut c_void,
            )
        };
        if status != 0 {
            return Vec::new();
        }
        ids.truncate(io_size as usize / size_of::<AudioObjectID>());
        ids
    }

    fn has_input_streams(device: AudioObjectID) -> bool {
        data_size(device, &addr(STREAMS, SCOPE_INPUT)).is_some_and(|size| size > 0)
    }

    pub(super) fn input_device_transports() -> Vec<InputDeviceTransport> {
        let default_input = read_u32(SYSTEM_OBJECT, DEFAULT_INPUT).filter(|id| *id != 0);
        device_ids()
            .into_iter()
            .filter(|id| has_input_streams(*id))
            .filter_map(|id| {
                let name = read_name(id)?;
                let transport = read_u32(id, TRANSPORT)
                    .map(InputTransport::from_code)
                    .unwrap_or(InputTransport::Other(0));
                Some(InputDeviceTransport {
                    name,
                    transport,
                    is_default: Some(id) == default_input,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::InputTransport;

    #[test]
    fn transport_codes_map_to_the_documented_four_char_values() {
        let code = |s: &[u8; 4]| u32::from_be_bytes(*s);
        assert_eq!(
            InputTransport::from_code(code(b"bltn")),
            InputTransport::BuiltIn
        );
        assert_eq!(
            InputTransport::from_code(code(b"blue")),
            InputTransport::Bluetooth
        );
        assert_eq!(
            InputTransport::from_code(code(b"blea")),
            InputTransport::BluetoothLe
        );
        assert_eq!(
            InputTransport::from_code(code(b"usb ")),
            InputTransport::Usb
        );
        assert_eq!(
            InputTransport::from_code(code(b"airp")),
            InputTransport::Other(code(b"airp"))
        );
    }

    #[test]
    fn both_bluetooth_flavours_count_as_bluetooth() {
        assert!(InputTransport::Bluetooth.is_bluetooth());
        assert!(InputTransport::BluetoothLe.is_bluetooth());
        assert!(!InputTransport::BuiltIn.is_bluetooth());
        assert!(!InputTransport::Usb.is_bluetooth());
        assert!(InputTransport::BuiltIn.is_built_in());
    }
}
