//! Which microphone a recording should open.
//!
//! Capture opens devices by name; `None` means "system default input". When
//! the user has not picked a device and the system default input is a
//! Bluetooth headset, opening its microphone switches the headset into the
//! low-quality call profile and interrupts whatever it is playing. With
//! `audio.prefer_builtin_over_bluetooth` on (the default) we record from the
//! built-in microphone instead. An explicit user choice always wins, and with
//! no built-in microphone the system default is kept.

use lumen_platform_macos::InputDeviceTransport;

use crate::config::AudioConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputChoiceReason {
    /// The user picked this device in settings.
    Explicit,
    /// The fallback is turned off in config.
    PolicyDisabled,
    /// The default input is not Bluetooth (or its transport is unknown).
    DefaultNotBluetooth,
    /// Default input is Bluetooth; recording from the built-in mic instead.
    BluetoothDefaultUsingBuiltIn,
    /// Default input is Bluetooth but no built-in microphone exists.
    BluetoothDefaultNoBuiltIn,
}

impl InputChoiceReason {
    fn label(self) -> &'static str {
        match self {
            Self::Explicit => "explicit_user_choice",
            Self::PolicyDisabled => "bluetooth_fallback_disabled",
            Self::DefaultNotBluetooth => "default_not_bluetooth",
            Self::BluetoothDefaultUsingBuiltIn => "bluetooth_default_using_built_in",
            Self::BluetoothDefaultNoBuiltIn => "bluetooth_default_no_built_in",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputChoice {
    /// Device name to open; `None` keeps the system default input.
    pub device: Option<String>,
    pub reason: InputChoiceReason,
}

/// Pure selection policy over a snapshot of the input devices.
pub fn choose_input_device(
    explicit: Option<&str>,
    prefer_builtin_over_bluetooth: bool,
    devices: &[InputDeviceTransport],
) -> InputChoice {
    if let Some(name) = explicit.map(str::trim).filter(|n| !n.is_empty()) {
        return InputChoice {
            device: Some(name.to_string()),
            reason: InputChoiceReason::Explicit,
        };
    }
    let keep_default = |reason| InputChoice {
        device: None,
        reason,
    };
    if !prefer_builtin_over_bluetooth {
        return keep_default(InputChoiceReason::PolicyDisabled);
    }
    let default_is_bluetooth = devices
        .iter()
        .any(|d| d.is_default && d.transport.is_bluetooth());
    if !default_is_bluetooth {
        return keep_default(InputChoiceReason::DefaultNotBluetooth);
    }
    match devices.iter().find(|d| d.transport.is_built_in()) {
        Some(built_in) => InputChoice {
            device: Some(built_in.name.clone()),
            reason: InputChoiceReason::BluetoothDefaultUsingBuiltIn,
        },
        None => keep_default(InputChoiceReason::BluetoothDefaultNoBuiltIn),
    }
}

/// Resolves the device for one recording and logs the decision. `purpose`
/// names the caller ("dictation", "meeting", ...) in the log line.
pub fn effective_input_device(audio: &AudioConfig, purpose: &str) -> Option<String> {
    let explicit = audio.device_name.as_deref();
    let has_explicit = explicit.is_some_and(|n| !n.trim().is_empty());
    // Only query the HAL when the policy can actually change the outcome.
    let devices = if has_explicit || !audio.prefer_builtin_over_bluetooth {
        Vec::new()
    } else {
        lumen_platform_macos::input_device_transports()
    };
    let choice = choose_input_device(explicit, audio.prefer_builtin_over_bluetooth, &devices);
    let default_input = devices.iter().find(|d| d.is_default);
    tracing::info!(
        purpose,
        device = choice.device.as_deref().unwrap_or("<system default>"),
        reason = choice.reason.label(),
        default_input = default_input
            .map(|d| d.name.as_str())
            .unwrap_or("<unknown>"),
        default_transport = default_input
            .map(|d| d.transport.label())
            .unwrap_or("unknown"),
        "input device selected"
    );
    choice.device
}

#[cfg(test)]
mod tests {
    use super::*;
    use lumen_platform_macos::InputTransport;

    fn device(name: &str, transport: InputTransport, is_default: bool) -> InputDeviceTransport {
        InputDeviceTransport {
            name: name.into(),
            transport,
            is_default,
        }
    }

    fn headset_default_with_built_in() -> Vec<InputDeviceTransport> {
        vec![
            device("Headset", InputTransport::Bluetooth, true),
            device("Built-in Microphone", InputTransport::BuiltIn, false),
            device("USB Mic", InputTransport::Usb, false),
        ]
    }

    #[test]
    fn bluetooth_default_switches_to_built_in_mic() {
        let choice = choose_input_device(None, true, &headset_default_with_built_in());
        assert_eq!(choice.device.as_deref(), Some("Built-in Microphone"));
        assert_eq!(
            choice.reason,
            InputChoiceReason::BluetoothDefaultUsingBuiltIn
        );
    }

    #[test]
    fn bluetooth_le_default_also_switches() {
        let devices = vec![
            device("Built-in Microphone", InputTransport::BuiltIn, false),
            device("LE Headset", InputTransport::BluetoothLe, true),
        ];
        let choice = choose_input_device(None, true, &devices);
        assert_eq!(choice.device.as_deref(), Some("Built-in Microphone"));
    }

    #[test]
    fn explicit_choice_wins_even_when_it_is_bluetooth() {
        let choice = choose_input_device(Some("Headset"), true, &headset_default_with_built_in());
        assert_eq!(choice.device.as_deref(), Some("Headset"));
        assert_eq!(choice.reason, InputChoiceReason::Explicit);
    }

    #[test]
    fn blank_explicit_choice_counts_as_no_choice() {
        let choice = choose_input_device(Some("  "), true, &headset_default_with_built_in());
        assert_eq!(choice.device.as_deref(), Some("Built-in Microphone"));
    }

    #[test]
    fn disabled_policy_keeps_system_default() {
        let choice = choose_input_device(None, false, &headset_default_with_built_in());
        assert_eq!(choice.device, None);
        assert_eq!(choice.reason, InputChoiceReason::PolicyDisabled);
    }

    #[test]
    fn no_built_in_mic_keeps_bluetooth_default() {
        let devices = vec![
            device("Headset", InputTransport::Bluetooth, true),
            device("USB Mic", InputTransport::Usb, false),
        ];
        let choice = choose_input_device(None, true, &devices);
        assert_eq!(choice.device, None);
        assert_eq!(choice.reason, InputChoiceReason::BluetoothDefaultNoBuiltIn);
    }

    #[test]
    fn non_bluetooth_default_is_left_alone() {
        let devices = vec![
            device("USB Mic", InputTransport::Usb, true),
            device("Built-in Microphone", InputTransport::BuiltIn, false),
        ];
        let choice = choose_input_device(None, true, &devices);
        assert_eq!(choice.device, None);
        assert_eq!(choice.reason, InputChoiceReason::DefaultNotBluetooth);
    }

    #[test]
    fn unknown_device_list_keeps_system_default() {
        let choice = choose_input_device(None, true, &[]);
        assert_eq!(choice.device, None);
        assert_eq!(choice.reason, InputChoiceReason::DefaultNotBluetooth);
    }
}
