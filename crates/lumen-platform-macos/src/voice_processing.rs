//! macOS microphone capture through the system **VoiceProcessingIO** audio
//! unit (`kAudioUnitSubType_VoiceProcessingIO`) — the same voice-processing
//! chain FaceTime uses, whose headline feature here is **system-level echo
//! cancellation (AEC)**: when a meeting plays through the loudspeaker, the
//! far-end voice the mic would pick back up is subtracted at the OS level,
//! removing the echo at the source instead of patching it in post.
//!
//! ## Implementation choice: raw AudioUnit C API (not AVAudioEngine)
//! Both roads lead to the same underlying VPIO unit (`AVAudioEngine`'s
//! `inputNode.setVoiceProcessingEnabled:` merely wraps it). The raw C API was
//! chosen because:
//! - it matches this crate's existing native pattern (`system_audio.rs`:
//!   Core Audio FFI, capability gate, callback → sink), so the capture path
//!   stays uniform and reviewable;
//! - it exposes the `kAUVoiceIOProperty_*` knobs directly (we disable AGC,
//!   below) where AVAudioEngine hides or version-gates them;
//! - it avoids the engine's implicit lifecycle (configuration-change restarts,
//!   tap buffer management) that is harder to reason about from Rust/objc2.
//!
//! The AudioUnit v2 C API has been stable for decades and needs no `dlsym`
//! weak linking; [`voice_processing_supported`] still probes the component at
//! runtime so a missing unit degrades instead of crashing.
//!
//! ## Known trade-off (why the caller must keep an opt-out)
//! VPIO bundles more than AEC: the chain includes noise suppression and
//! (optionally) AGC. Apple exposes no public switch to keep AEC while turning
//! noise suppression off, and the suppressor is tuned for near-field voice —
//! it can attenuate quiet **far-field conference-room speakers**. Callers must
//! therefore treat this path as *default-on but opt-out* (config
//! `meeting.mic_aec`) and fall back to the plain HAL capture when disabled or
//! when initialization fails: recording must never fail because of AEC. AGC is
//! disabled best-effort (`kAUVoiceIOProperty_VoiceProcessingEnableAGC = 0`) so
//! the recorded level stays honest; a failure to disable it is logged and
//! tolerated.
//!
//! ## Permission
//! VPIO reads the microphone through the normal input HAL, so it is covered by
//! the existing microphone TCC grant (`NSMicrophoneUsageDescription`). No new
//! entitlement or Info.plist key is required.
//!
//! ## Threading
//! The input callback runs on Core Audio's IO thread. It only renders into a
//! preallocated buffer and forwards the mono `f32` slice to the caller's sink
//! — the sink must be fast (the meeting path only pushes into a writer-thread
//! channel). No allocation happens in the callback in the steady state.
//!
//! ## Verification note
//! A CI/sandbox host has no live audio stack, so the unit cannot be exercised
//! in tests; this module is written to compile everywhere, gate at runtime,
//! and surface every failure as a typed error. The AEC itself needs on-device
//! validation in a real speakerphone meeting.

use std::sync::Arc;
use thiserror::Error;

/// Failure modes of [`VoiceProcessingInput`].
#[derive(Debug, Error)]
pub enum VoiceProcessingError {
    /// This build/host has no VoiceProcessingIO unit (non-macOS, or the
    /// component is missing).
    #[error("voice-processing capture unsupported on this host")]
    Unsupported,
    /// A capture is already running.
    #[error("voice-processing capture already running")]
    AlreadyRunning,
    /// An AudioUnit / Core Audio call failed. `stage` names the exact call so
    /// a field log pinpoints the failure; `status` is the raw `OSStatus`.
    #[error("core audio error in {stage}: status {status}")]
    CoreAudio { stage: &'static str, status: i32 },
}

/// Sink invoked from the VPIO input callback with each mono `f32` chunk at
/// the unit's client sample rate. Runs on the Core Audio IO thread — keep it
/// fast (forward to a channel; do no file or heavy work here).
pub type VoiceInputSink = Arc<dyn Fn(&[f32]) + Send + Sync>;

/// Whether this build/host exposes the VoiceProcessingIO audio unit. `false`
/// on non-macOS; on macOS a cheap, side-effect-free component probe.
pub fn voice_processing_supported() -> bool {
    #[cfg(target_os = "macos")]
    {
        imp::component_available()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Microphone capture handle backed by the system voice-processing unit
/// (AEC + noise suppression; AGC disabled best-effort).
///
/// Cross-platform shell: on non-macOS (and on macOS without the component)
/// [`start`](Self::start) returns [`VoiceProcessingError::Unsupported`] and
/// the caller uses its plain capture path instead.
pub struct VoiceProcessingInput {
    #[cfg(target_os = "macos")]
    session: Option<imp::VpioSession>,
}

impl Default for VoiceProcessingInput {
    fn default() -> Self {
        Self::new()
    }
}

impl VoiceProcessingInput {
    pub fn new() -> Self {
        Self {
            #[cfg(target_os = "macos")]
            session: None,
        }
    }

    /// Whether a capture session is currently running.
    pub fn is_running(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.session.is_some()
        }
        #[cfg(not(target_os = "macos"))]
        {
            false
        }
    }

    /// Start capturing the microphone through the voice-processing unit and
    /// deliver mono `f32` chunks to `sink`. `preferred_device` selects an
    /// input device by its HAL name (the same name cpal reports); `None` or an
    /// unknown name uses the system default input (a mismatch is logged, never
    /// an error — mirroring the plain capture path's device fallback).
    /// Returns the client-format sample rate the sink will receive.
    pub fn start(
        &mut self,
        preferred_device: Option<&str>,
        sink: VoiceInputSink,
    ) -> Result<u32, VoiceProcessingError> {
        #[cfg(target_os = "macos")]
        {
            if self.session.is_some() {
                return Err(VoiceProcessingError::AlreadyRunning);
            }
            let session = imp::VpioSession::start(preferred_device, sink)?;
            let sample_rate = session.sample_rate();
            self.session = Some(session);
            Ok(sample_rate)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (preferred_device, sink);
            Err(VoiceProcessingError::Unsupported)
        }
    }

    /// Stop the capture and dispose the audio unit. Idempotent; a no-op when
    /// nothing is running.
    pub fn stop(&mut self) {
        #[cfg(target_os = "macos")]
        {
            if let Some(session) = self.session.take() {
                session.stop();
            }
        }
    }
}

impl Drop for VoiceProcessingInput {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{VoiceInputSink, VoiceProcessingError};
    use core_foundation::base::TCFType;
    use core_foundation::string::{CFString, CFStringRef};
    use std::ffi::c_void;
    use std::sync::atomic::{AtomicU32, Ordering};

    type OSStatus = i32;
    type AudioUnit = *mut c_void;
    type AudioComponent = *mut c_void;
    type AudioObjectID = u32;

    #[repr(C)]
    struct AudioComponentDescription {
        component_type: u32,
        component_sub_type: u32,
        component_manufacturer: u32,
        component_flags: u32,
        component_flags_mask: u32,
    }

    #[repr(C)]
    #[derive(Debug, Clone, Copy, Default)]
    struct AudioStreamBasicDescription {
        m_sample_rate: f64,
        m_format_id: u32,
        m_format_flags: u32,
        m_bytes_per_packet: u32,
        m_frames_per_packet: u32,
        m_bytes_per_frame: u32,
        m_channels_per_frame: u32,
        m_bits_per_channel: u32,
        m_reserved: u32,
    }

    #[repr(C)]
    struct AudioBuffer {
        m_number_channels: u32,
        m_data_byte_size: u32,
        m_data: *mut c_void,
    }

    #[repr(C)]
    struct AudioBufferList {
        m_number_buffers: u32,
        m_buffers: [AudioBuffer; 1],
    }

    #[repr(C)]
    struct AudioObjectPropertyAddress {
        m_selector: u32,
        m_scope: u32,
        m_element: u32,
    }

    /// `AURenderCallback` from AudioUnit/AUComponent.h. The timestamp is only
    /// forwarded back into `AudioUnitRender`, so it stays opaque here.
    type AURenderCallback = unsafe extern "C" fn(
        in_ref_con: *mut c_void,
        io_action_flags: *mut u32,
        in_time_stamp: *const c_void,
        in_bus_number: u32,
        in_number_frames: u32,
        io_data: *mut AudioBufferList,
    ) -> OSStatus;

    #[repr(C)]
    struct AURenderCallbackStruct {
        input_proc: Option<AURenderCallback>,
        input_proc_ref_con: *mut c_void,
    }

    const fn fourcc(s: &[u8; 4]) -> u32 {
        ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
    }

    // Component description of the voice-processing IO unit.
    const AU_TYPE_OUTPUT: u32 = fourcc(b"auou"); // kAudioUnitType_Output
    const AU_SUBTYPE_VOICE_PROCESSING_IO: u32 = fourcc(b"vpio"); // kAudioUnitSubType_VoiceProcessingIO
    const AU_MANUFACTURER_APPLE: u32 = fourcc(b"appl"); // kAudioUnitManufacturer_Apple

    // AudioUnit scopes / elements.
    const SCOPE_GLOBAL: u32 = 0; // kAudioUnitScope_Global
    const SCOPE_INPUT: u32 = 1; // kAudioUnitScope_Input
    const SCOPE_OUTPUT: u32 = 2; // kAudioUnitScope_Output
    const ELEMENT_OUTPUT: u32 = 0; // speaker-side bus of an IO unit
    const ELEMENT_INPUT: u32 = 1; // microphone-side bus of an IO unit

    // AudioUnit / AUHAL properties.
    const PROP_STREAM_FORMAT: u32 = 8; // kAudioUnitProperty_StreamFormat
    const PROP_MAX_FRAMES_PER_SLICE: u32 = 14; // kAudioUnitProperty_MaximumFramesPerSlice
    const PROP_CURRENT_DEVICE: u32 = 2000; // kAudioOutputUnitProperty_CurrentDevice
    const PROP_ENABLE_IO: u32 = 2003; // kAudioOutputUnitProperty_EnableIO
    const PROP_SET_INPUT_CALLBACK: u32 = 2005; // kAudioOutputUnitProperty_SetInputCallback

    // Voice-processing knobs (AudioUnit/AudioUnitProperties.h).
    const PROP_VOICE_PROCESSING_ENABLE_AGC: u32 = 2101; // kAUVoiceIOProperty_VoiceProcessingEnableAGC

    // Linear PCM client format.
    const FORMAT_LINEAR_PCM: u32 = fourcc(b"lpcm"); // kAudioFormatLinearPCM
    const FORMAT_FLAG_IS_FLOAT: u32 = 1; // kAudioFormatFlagIsFloat
    const FORMAT_FLAG_IS_PACKED: u32 = 8; // kAudioFormatFlagIsPacked

    // HAL properties for device resolution.
    const SYSTEM_OBJECT: AudioObjectID = 1; // kAudioObjectSystemObject
    const HAL_SCOPE_GLOBAL: u32 = fourcc(b"glob"); // kAudioObjectPropertyScopeGlobal
    const HAL_SCOPE_INPUT: u32 = fourcc(b"inpt"); // kAudioObjectPropertyScopeInput
    const HAL_ELEMENT_MAIN: u32 = 0; // kAudioObjectPropertyElementMain
    const HAL_DEFAULT_INPUT_DEVICE: u32 = fourcc(b"dIn "); // kAudioHardwarePropertyDefaultInputDevice
    const HAL_DEVICES: u32 = fourcc(b"dev#"); // kAudioHardwarePropertyDevices
    const HAL_OBJECT_NAME: u32 = fourcc(b"lnam"); // kAudioObjectPropertyName
    const HAL_DEVICE_STREAMS: u32 = fourcc(b"stm#"); // kAudioDevicePropertyStreams

    /// Upper bound configured for one IO callback; the render buffer is
    /// preallocated to this so the steady state never allocates.
    const MAX_FRAMES_PER_SLICE: u32 = 4096;

    /// Fallback client sample rate when the hardware format cannot be read.
    const FALLBACK_SAMPLE_RATE: f64 = 48_000.0;

    // SAFETY: long-stable AudioUnit v2 / HAL C entry points. The 14.x-era
    // voice-processing *properties* are plain integers set on a unit that is
    // probed at runtime, so nothing here needs weak linking.
    #[link(name = "AudioToolbox", kind = "framework")]
    extern "C" {
        fn AudioComponentFindNext(
            in_component: AudioComponent,
            in_desc: *const AudioComponentDescription,
        ) -> AudioComponent;
        fn AudioComponentInstanceNew(
            in_component: AudioComponent,
            out_instance: *mut AudioUnit,
        ) -> OSStatus;
        fn AudioComponentInstanceDispose(in_instance: AudioUnit) -> OSStatus;
        fn AudioUnitSetProperty(
            in_unit: AudioUnit,
            in_id: u32,
            in_scope: u32,
            in_element: u32,
            in_data: *const c_void,
            in_data_size: u32,
        ) -> OSStatus;
        fn AudioUnitGetProperty(
            in_unit: AudioUnit,
            in_id: u32,
            in_scope: u32,
            in_element: u32,
            out_data: *mut c_void,
            io_data_size: *mut u32,
        ) -> OSStatus;
        fn AudioUnitInitialize(in_unit: AudioUnit) -> OSStatus;
        fn AudioUnitUninitialize(in_unit: AudioUnit) -> OSStatus;
        fn AudioOutputUnitStart(in_unit: AudioUnit) -> OSStatus;
        fn AudioOutputUnitStop(in_unit: AudioUnit) -> OSStatus;
        fn AudioUnitRender(
            in_unit: AudioUnit,
            io_action_flags: *mut u32,
            in_time_stamp: *const c_void,
            in_output_bus_number: u32,
            in_number_frames: u32,
            io_data: *mut AudioBufferList,
        ) -> OSStatus;
    }

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectGetPropertyData(
            in_object: AudioObjectID,
            in_address: *const AudioObjectPropertyAddress,
            in_qualifier_data_size: u32,
            in_qualifier_data: *const c_void,
            io_data_size: *mut u32,
            out_data: *mut c_void,
        ) -> OSStatus;
        fn AudioObjectGetPropertyDataSize(
            in_object: AudioObjectID,
            in_address: *const AudioObjectPropertyAddress,
            in_qualifier_data_size: u32,
            in_qualifier_data: *const c_void,
            out_data_size: *mut u32,
        ) -> OSStatus;
    }

    fn find_component() -> AudioComponent {
        let desc = AudioComponentDescription {
            component_type: AU_TYPE_OUTPUT,
            component_sub_type: AU_SUBTYPE_VOICE_PROCESSING_IO,
            component_manufacturer: AU_MANUFACTURER_APPLE,
            component_flags: 0,
            component_flags_mask: 0,
        };
        // SAFETY: NULL start component + valid description is the documented
        // "find first match" call; a missing component returns NULL.
        unsafe { AudioComponentFindNext(std::ptr::null_mut(), &desc) }
    }

    pub(super) fn component_available() -> bool {
        !find_component().is_null()
    }

    fn hal_addr(selector: u32, scope: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            m_selector: selector,
            m_scope: scope,
            m_element: HAL_ELEMENT_MAIN,
        }
    }

    /// System default input device, or `None` when the HAL has none.
    fn default_input_device() -> Option<AudioObjectID> {
        let a = hal_addr(HAL_DEFAULT_INPUT_DEVICE, HAL_SCOPE_GLOBAL);
        let mut device: AudioObjectID = 0;
        let mut io_size = std::mem::size_of::<AudioObjectID>() as u32;
        // SAFETY: fixed-size scalar property read on the system object.
        let status = unsafe {
            AudioObjectGetPropertyData(
                SYSTEM_OBJECT,
                &a,
                0,
                std::ptr::null(),
                &mut io_size,
                &mut device as *mut AudioObjectID as *mut c_void,
            )
        };
        (status == 0 && device != 0).then_some(device)
    }

    /// Whether `device` has at least one input stream (i.e. is a capture
    /// device, not an output-only one that happens to share a product name).
    fn has_input_streams(device: AudioObjectID) -> bool {
        let a = hal_addr(HAL_DEVICE_STREAMS, HAL_SCOPE_INPUT);
        let mut size: u32 = 0;
        // SAFETY: size query only; no out buffer.
        let status =
            unsafe { AudioObjectGetPropertyDataSize(device, &a, 0, std::ptr::null(), &mut size) };
        status == 0 && size > 0
    }

    /// HAL name of `device` (the same `kAudioObjectPropertyName` cpal reports
    /// as the device name).
    fn device_name(device: AudioObjectID) -> Option<String> {
        let a = hal_addr(HAL_OBJECT_NAME, HAL_SCOPE_GLOBAL);
        let mut name_ref: CFStringRef = std::ptr::null();
        let mut io_size = std::mem::size_of::<CFStringRef>() as u32;
        // SAFETY: the property returns a CFString the caller owns (Copy/Get
        // rule for HAL property data: caller releases), which
        // `wrap_under_create_rule` encodes.
        let status = unsafe {
            AudioObjectGetPropertyData(
                device,
                &a,
                0,
                std::ptr::null(),
                &mut io_size,
                &mut name_ref as *mut CFStringRef as *mut c_void,
            )
        };
        if status != 0 || name_ref.is_null() {
            return None;
        }
        // SAFETY: non-null CFStringRef we own per the create rule.
        let name = unsafe { CFString::wrap_under_create_rule(name_ref) };
        Some(name.to_string())
    }

    /// Resolve `preferred` (a HAL device name) to an input device id, falling
    /// back to the system default input when absent or not found — the same
    /// "warn and use default" contract as the plain cpal capture path.
    fn resolve_input_device(preferred: Option<&str>) -> Option<AudioObjectID> {
        if let Some(name) = preferred.filter(|n| !n.is_empty()) {
            let a = hal_addr(HAL_DEVICES, HAL_SCOPE_GLOBAL);
            let mut size: u32 = 0;
            // SAFETY: size query, then a matching-size list read.
            let status = unsafe {
                AudioObjectGetPropertyDataSize(SYSTEM_OBJECT, &a, 0, std::ptr::null(), &mut size)
            };
            if status == 0 && size > 0 {
                let count = size as usize / std::mem::size_of::<AudioObjectID>();
                let mut devices = vec![0 as AudioObjectID; count];
                let mut io_size = size;
                // SAFETY: buffer sized from the query above.
                let status = unsafe {
                    AudioObjectGetPropertyData(
                        SYSTEM_OBJECT,
                        &a,
                        0,
                        std::ptr::null(),
                        &mut io_size,
                        devices.as_mut_ptr() as *mut c_void,
                    )
                };
                if status == 0 {
                    for &device in &devices {
                        if device != 0
                            && has_input_streams(device)
                            && device_name(device).as_deref() == Some(name)
                        {
                            return Some(device);
                        }
                    }
                }
            }
            tracing::warn!(%name, "preferred device not found for voice processing, using default");
        }
        default_input_device()
    }

    /// State shared with the IO-thread input callback via a raw pointer.
    /// The callback is the sole accessor while the unit runs; the owning
    /// session frees it only after the unit is stopped and disposed.
    struct RenderCtx {
        unit: AudioUnit,
        sink: VoiceInputSink,
        /// Preallocated mono render target (never reallocated in steady state).
        buffer: Vec<f32>,
        /// Render errors seen so far; only the first is logged (IO thread).
        render_errors: AtomicU32,
    }

    /// VPIO input callback: render the just-captured (voice-processed) frames
    /// into the preallocated buffer and forward them to the sink. Runs on the
    /// Core Audio IO thread — no allocation, no locks beyond the sink's own.
    unsafe extern "C" fn input_callback(
        in_ref_con: *mut c_void,
        io_action_flags: *mut u32,
        in_time_stamp: *const c_void,
        in_bus_number: u32,
        in_number_frames: u32,
        _io_data: *mut AudioBufferList,
    ) -> OSStatus {
        let ctx = &mut *(in_ref_con as *mut RenderCtx);
        let frames = in_number_frames as usize;
        if frames == 0 {
            return 0;
        }
        if ctx.buffer.len() < frames {
            // Only reachable if the HAL exceeds MaximumFramesPerSlice; resize
            // is a rare, defensive path.
            ctx.buffer.resize(frames, 0.0);
        }
        let mut list = AudioBufferList {
            m_number_buffers: 1,
            m_buffers: [AudioBuffer {
                m_number_channels: 1,
                m_data_byte_size: (frames * std::mem::size_of::<f32>()) as u32,
                m_data: ctx.buffer.as_mut_ptr() as *mut c_void,
            }],
        };
        let status = AudioUnitRender(
            ctx.unit,
            io_action_flags,
            in_time_stamp,
            in_bus_number,
            in_number_frames,
            &mut list,
        );
        if status != 0 {
            if ctx.render_errors.fetch_add(1, Ordering::Relaxed) == 0 {
                tracing::warn!(status, "voice-processing AudioUnitRender failed");
            }
            return status;
        }
        let rendered =
            (list.m_buffers[0].m_data_byte_size as usize / std::mem::size_of::<f32>()).min(frames);
        if rendered > 0 {
            (ctx.sink)(&ctx.buffer[..rendered]);
        }
        0
    }

    /// A live VPIO capture session: the unit plus the callback context it
    /// borrows. Field order is irrelevant (teardown is explicit in `stop`).
    pub(super) struct VpioSession {
        unit: AudioUnit,
        /// Raw `Box<RenderCtx>` handed to the callback; freed in `stop` after
        /// the unit is disposed.
        ctx: *mut RenderCtx,
        sample_rate: u32,
    }

    // SAFETY: the AudioUnit handle is a thread-safe C API object; the ctx
    // pointer is only dereferenced by the IO callback (managed by Core Audio)
    // and freed after disposal, never dereferenced by us across threads.
    unsafe impl Send for VpioSession {}

    impl VpioSession {
        pub(super) fn sample_rate(&self) -> u32 {
            self.sample_rate
        }

        pub(super) fn start(
            preferred_device: Option<&str>,
            sink: VoiceInputSink,
        ) -> Result<Self, VoiceProcessingError> {
            let component = find_component();
            if component.is_null() {
                return Err(VoiceProcessingError::Unsupported);
            }
            let mut unit: AudioUnit = std::ptr::null_mut();
            // SAFETY: valid component from the probe above; out param local.
            let status = unsafe { AudioComponentInstanceNew(component, &mut unit) };
            if status != 0 || unit.is_null() {
                return Err(VoiceProcessingError::CoreAudio {
                    stage: "AudioComponentInstanceNew",
                    status,
                });
            }
            // Every failure past this point disposes the unit.
            let fail = |stage: &'static str, status: OSStatus, unit: AudioUnit| {
                // SAFETY: unit was created above and not yet disposed.
                unsafe {
                    let _ = AudioComponentInstanceDispose(unit);
                }
                VoiceProcessingError::CoreAudio { stage, status }
            };
            let set_u32 = |unit: AudioUnit, prop: u32, scope: u32, element: u32, value: u32| {
                // SAFETY: scalar property write on a live unit.
                unsafe {
                    AudioUnitSetProperty(
                        unit,
                        prop,
                        scope,
                        element,
                        &value as *const u32 as *const c_void,
                        std::mem::size_of::<u32>() as u32,
                    )
                }
            };

            // 1. IO topology: input bus on (microphone), output bus off — we
            //    only capture. macOS VPIO takes its echo reference from the
            //    system output stream, so AEC works without us rendering
            //    playback through the unit.
            let status = set_u32(unit, PROP_ENABLE_IO, SCOPE_INPUT, ELEMENT_INPUT, 1);
            if status != 0 {
                return Err(fail("EnableIO(input)", status, unit));
            }
            let status = set_u32(unit, PROP_ENABLE_IO, SCOPE_OUTPUT, ELEMENT_OUTPUT, 0);
            if status != 0 {
                return Err(fail("EnableIO(output)", status, unit));
            }

            // 2. Bind the capture device (preferred by name, else default).
            let Some(device) = resolve_input_device(preferred_device) else {
                return Err(fail("resolve input device", -1, unit));
            };
            // SAFETY: scalar property write (device id) on a live unit.
            let status = unsafe {
                AudioUnitSetProperty(
                    unit,
                    PROP_CURRENT_DEVICE,
                    SCOPE_GLOBAL,
                    ELEMENT_OUTPUT,
                    &device as *const AudioObjectID as *const c_void,
                    std::mem::size_of::<AudioObjectID>() as u32,
                )
            };
            if status != 0 {
                return Err(fail("CurrentDevice", status, unit));
            }

            // 3. Client format on the input bus's output scope (what our
            //    callback renders): mono float32 at the hardware rate, so the
            //    unit does no avoidable rate conversion and downstream keeps
            //    the same "native capture rate" contract as the plain path.
            let mut hw_format = AudioStreamBasicDescription::default();
            let mut io_size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
            // SAFETY: fixed-size struct property read on a live unit.
            let status = unsafe {
                AudioUnitGetProperty(
                    unit,
                    PROP_STREAM_FORMAT,
                    SCOPE_INPUT,
                    ELEMENT_INPUT,
                    &mut hw_format as *mut AudioStreamBasicDescription as *mut c_void,
                    &mut io_size,
                )
            };
            let sample_rate_f64 = if status == 0 && hw_format.m_sample_rate > 0.0 {
                hw_format.m_sample_rate
            } else {
                FALLBACK_SAMPLE_RATE
            };
            let client_format = AudioStreamBasicDescription {
                m_sample_rate: sample_rate_f64,
                m_format_id: FORMAT_LINEAR_PCM,
                m_format_flags: FORMAT_FLAG_IS_FLOAT | FORMAT_FLAG_IS_PACKED,
                m_bytes_per_packet: 4,
                m_frames_per_packet: 1,
                m_bytes_per_frame: 4,
                m_channels_per_frame: 1,
                m_bits_per_channel: 32,
                m_reserved: 0,
            };
            // SAFETY: fixed-size struct property write on a live unit.
            let status = unsafe {
                AudioUnitSetProperty(
                    unit,
                    PROP_STREAM_FORMAT,
                    SCOPE_OUTPUT,
                    ELEMENT_INPUT,
                    &client_format as *const AudioStreamBasicDescription as *const c_void,
                    std::mem::size_of::<AudioStreamBasicDescription>() as u32,
                )
            };
            if status != 0 {
                return Err(fail("StreamFormat(client)", status, unit));
            }
            let sample_rate = sample_rate_f64.round() as u32;

            // 4. Keep only the processing we came for: AGC off (recorded
            //    levels stay honest), AEC + the rest of the chain on. Apple
            //    exposes no public switch for noise suppression alone — that
            //    residual trade-off is why the caller keeps a config opt-out.
            //    Best-effort: some OS builds reject the property; log and go on.
            let status = set_u32(
                unit,
                PROP_VOICE_PROCESSING_ENABLE_AGC,
                SCOPE_GLOBAL,
                ELEMENT_INPUT,
                0,
            );
            if status != 0 {
                tracing::warn!(
                    status,
                    "could not disable voice-processing AGC (continuing)"
                );
            }

            // 5. Bound the per-callback frame count and preallocate the render
            //    buffer to it, so the IO callback never allocates.
            let status = set_u32(
                unit,
                PROP_MAX_FRAMES_PER_SLICE,
                SCOPE_GLOBAL,
                ELEMENT_OUTPUT,
                MAX_FRAMES_PER_SLICE,
            );
            if status != 0 {
                tracing::warn!(status, "could not set MaximumFramesPerSlice (continuing)");
            }

            // 6. Install the input callback.
            let ctx = Box::into_raw(Box::new(RenderCtx {
                unit,
                sink,
                buffer: vec![0.0f32; MAX_FRAMES_PER_SLICE as usize],
                render_errors: AtomicU32::new(0),
            }));
            // Frees `ctx` too on the remaining failure paths.
            let fail_ctx = |stage: &'static str, status: OSStatus, unit: AudioUnit| {
                // SAFETY: ctx was leaked just above and the callback can no
                // longer fire once the unit is disposed (it never started).
                unsafe {
                    let _ = AudioComponentInstanceDispose(unit);
                    drop(Box::from_raw(ctx));
                }
                VoiceProcessingError::CoreAudio { stage, status }
            };
            let cb = AURenderCallbackStruct {
                input_proc: Some(input_callback),
                input_proc_ref_con: ctx as *mut c_void,
            };
            // SAFETY: struct property write; Core Audio copies the struct.
            let status = unsafe {
                AudioUnitSetProperty(
                    unit,
                    PROP_SET_INPUT_CALLBACK,
                    SCOPE_GLOBAL,
                    ELEMENT_OUTPUT,
                    &cb as *const AURenderCallbackStruct as *const c_void,
                    std::mem::size_of::<AURenderCallbackStruct>() as u32,
                )
            };
            if status != 0 {
                return Err(fail_ctx("SetInputCallback", status, unit));
            }

            // 7. Initialize and start. This is where a misconfiguration (or a
            //    denied microphone permission) typically surfaces; the caller
            //    falls back to the plain capture path on any error.
            // SAFETY: fully configured unit.
            let status = unsafe { AudioUnitInitialize(unit) };
            if status != 0 {
                return Err(fail_ctx("AudioUnitInitialize", status, unit));
            }
            // SAFETY: initialized unit.
            let status = unsafe { AudioOutputUnitStart(unit) };
            if status != 0 {
                // SAFETY: initialized above; uninitialize before disposal.
                unsafe {
                    let _ = AudioUnitUninitialize(unit);
                }
                return Err(fail_ctx("AudioOutputUnitStart", status, unit));
            }

            tracing::info!(
                sample_rate,
                device,
                "voice-processing (system AEC) mic capture started"
            );
            Ok(Self {
                unit,
                ctx,
                sample_rate,
            })
        }

        pub(super) fn stop(self) {
            // SAFETY: tearing down the unit this session created, in reverse
            // order. Statuses are best-effort logged.
            unsafe {
                let status = AudioOutputUnitStop(self.unit);
                if status != 0 {
                    tracing::warn!(status, "AudioOutputUnitStop failed");
                }
                let status = AudioUnitUninitialize(self.unit);
                if status != 0 {
                    tracing::warn!(status, "AudioUnitUninitialize failed");
                }
                let status = AudioComponentInstanceDispose(self.unit);
                if status != 0 {
                    tracing::warn!(status, "AudioComponentInstanceDispose failed");
                }
            }
            // Give any in-flight IO callback a moment to unwind before the
            // context is freed (mirrors the cpal path's zombie-callback grace).
            std::thread::sleep(std::time::Duration::from_millis(60));
            // SAFETY: the unit is disposed, so the callback can no longer run;
            // this is the unique owner of the leaked context.
            unsafe {
                drop(Box::from_raw(self.ctx));
            }
            tracing::info!("voice-processing mic capture stopped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_probe_never_panics() {
        // On macOS the VPIO component should exist; elsewhere this is false.
        // Either way the probe is cheap and side-effect free.
        let _ = voice_processing_supported();
    }

    #[test]
    fn start_without_capability_reports_unsupported_and_stop_is_idempotent() {
        let mut capture = VoiceProcessingInput::new();
        assert!(!capture.is_running());
        // Only exercise a real start where the component is absent — a live
        // start needs an audio stack + mic TCC no CI/sandbox host has.
        if !voice_processing_supported() {
            let sink: VoiceInputSink = Arc::new(|_samples: &[f32]| {});
            let err = capture.start(None, sink).unwrap_err();
            assert!(matches!(err, VoiceProcessingError::Unsupported));
        }
        capture.stop();
        capture.stop();
        assert!(!capture.is_running());
    }

    #[test]
    fn errors_render_with_stage_and_status() {
        let err = VoiceProcessingError::CoreAudio {
            stage: "AudioUnitInitialize",
            status: -50,
        };
        assert_eq!(
            err.to_string(),
            "core audio error in AudioUnitInitialize: status -50"
        );
        assert!(VoiceProcessingError::Unsupported
            .to_string()
            .contains("unsupported"));
    }
}
