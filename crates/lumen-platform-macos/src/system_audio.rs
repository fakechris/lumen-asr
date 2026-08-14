//! macOS system-audio capture via a Core Audio **process tap** (macOS 14.2+),
//! capability-gated.
//!
//! Dual-track meeting recording needs the *other* side of a call — the remote
//! participants' voices — which never pass through the microphone. On macOS
//! 14.2+ Core Audio can tap selected process objects. This module resolves an
//! explicit bundle-id allow-list to live process objects, creates a mono
//! mixdown tap for only those processes, wraps it in a private aggregate
//! device, and reads the tapped samples with a device IO proc. It never falls
//! back to a global system mix: an unmatched target degrades to mic-only.
//!
//! ## Capability gate (macOS 14.2+ only)
//! The tap entry points (`AudioHardwareCreateProcessTap`, …) and the
//! `CATapDescription` class only exist on macOS 14.2+. Rather than key off an
//! OS version string, [`capability_available`] resolves the symbols with
//! `dlsym` and looks the class up at runtime. On any host where they are
//! missing, [`SystemAudioCapture::start`] fails with
//! [`SystemAudioError::Unsupported`] and the caller records mic-only — a
//! missing capability is a degrade, never a crash.
//!
//! ## Permission (TCC)
//! Capturing system audio requires the user's consent
//! (`NSAudioCaptureUsageDescription`; macOS 14.4+ shows a dedicated "System
//! Audio Recording Only" prompt on first use). A denied permission surfaces as
//! a tap-creation error (or silence), which the caller likewise treats as a
//! degrade to mic-only.
//!
//! ## Verification note
//! A CI/sandbox host cannot exercise the live tap (no audio stack, no TCC).
//! This module is written to **compile** everywhere and **gate** at runtime;
//! the capture itself needs on-device validation in a real call.

use std::sync::Arc;
use thiserror::Error;

/// Failure modes of [`SystemAudioCapture`].
#[derive(Debug, Error)]
pub enum SystemAudioError {
    /// This build/host has no process-tap capability (non-macOS, or < 14.2).
    #[error("system audio capture unsupported on this host")]
    Unsupported,
    /// A capture is already running.
    #[error("system audio capture already running")]
    AlreadyRunning,
    /// None of the configured bundle ids currently has a Core Audio process
    /// object, so there is no safe per-process target to capture.
    #[error("no running audio process matched configured targets: {bundle_ids:?}")]
    NoMatchingProcesses { bundle_ids: Vec<String> },
    /// A Core Audio call failed (includes a denied TCC permission surfacing as
    /// a tap/aggregate creation error).
    #[error("core audio error in {stage}: status {status}")]
    CoreAudio { stage: &'static str, status: i32 },
    /// The Objective-C tap description could not be built.
    #[error("tap description error: {0}")]
    TapDescription(String),
}

/// Sink invoked from the capture callback with each mono `f32` chunk at the
/// native tap sample rate. Runs on a Core Audio dispatch queue — keep it fast
/// (the meeting recorder's sink only forwards to a writer thread).
pub type SystemAudioSink = Arc<dyn Fn(&[f32]) + Send + Sync>;

/// Explicit process-level selection for a system-audio tap. Bundle ids are
/// normalized and deduplicated; they come from the runtime app catalog rather
/// than a compiled application list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemAudioTarget {
    bundle_ids: Vec<String>,
}

impl SystemAudioTarget {
    pub fn new(bundle_ids: impl IntoIterator<Item = String>) -> Self {
        let mut normalized = Vec::new();
        for bundle_id in bundle_ids {
            let bundle_id = lumen_core::normalize_bundle_id(&bundle_id);
            if bundle_id.is_empty()
                || normalized
                    .iter()
                    .any(|existing: &String| existing.eq_ignore_ascii_case(&bundle_id))
            {
                continue;
            }
            normalized.push(bundle_id);
        }
        Self {
            bundle_ids: normalized,
        }
    }

    pub fn bundle_ids(&self) -> &[String] {
        &self.bundle_ids
    }

    pub fn is_empty(&self) -> bool {
        self.bundle_ids.is_empty()
    }
}

/// Whether this build/host exposes the Core Audio process-tap API
/// (macOS 14.2+). `false` on non-macOS and on older macOS.
pub fn capability_available() -> bool {
    #[cfg(target_os = "macos")]
    {
        imp::tap_api_available()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Capture handle for the system-output (remote participants) audio track.
///
/// Cross-platform shell: on non-macOS (and on macOS without the capability)
/// [`start`](Self::start) returns [`SystemAudioError::Unsupported`] and the
/// caller records mic-only.
pub struct SystemAudioCapture {
    #[cfg(target_os = "macos")]
    session: Option<imp::TapSession>,
}

impl Default for SystemAudioCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemAudioCapture {
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

    /// Create a process tap for `target`, wrap it in a private aggregate
    /// device, and start delivering mono `f32` chunks to `sink`. Returns the
    /// native sample rate of the tap stream.
    pub fn start(
        &mut self,
        target: &SystemAudioTarget,
        sink: SystemAudioSink,
    ) -> Result<u32, SystemAudioError> {
        #[cfg(target_os = "macos")]
        {
            if self.session.is_some() {
                return Err(SystemAudioError::AlreadyRunning);
            }
            if !imp::tap_api_available() {
                return Err(SystemAudioError::Unsupported);
            }
            let session = imp::TapSession::start(target, sink)?;
            let sample_rate = session.sample_rate();
            self.session = Some(session);
            Ok(sample_rate)
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (target, sink);
            Err(SystemAudioError::Unsupported)
        }
    }

    /// Stop the capture and tear down the IO proc, aggregate device, and tap.
    /// Idempotent; a no-op when nothing is running.
    pub fn stop(&mut self) {
        #[cfg(target_os = "macos")]
        {
            if let Some(session) = self.session.take() {
                session.stop();
            }
        }
    }
}

impl Drop for SystemAudioCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{SystemAudioError, SystemAudioSink, SystemAudioTarget};
    use std::ffi::{c_char, c_void};

    use block2::{Block, RcBlock};
    use core_foundation::array::CFArray;
    use core_foundation::base::{CFType, TCFType};
    use core_foundation::dictionary::CFDictionary;
    use core_foundation::number::CFNumber;
    use core_foundation::string::{CFString, CFStringRef};
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject, Bool};
    use objc2_foundation::{NSArray, NSNumber, NSString};

    type AudioObjectID = u32;
    type OSStatus = i32;
    /// Opaque IO proc identifier returned by
    /// `AudioDeviceCreateIOProcIDWithBlock` (a function pointer under the
    /// hood; only ever passed back to Core Audio).
    type AudioDeviceIOProcID = *mut c_void;

    #[repr(C)]
    struct AudioObjectPropertyAddress {
        m_selector: u32,
        m_scope: u32,
        m_element: u32,
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
        // Variable-length in C; indexed via pointer arithmetic below.
        m_buffers: [AudioBuffer; 1],
    }

    const fn fourcc(s: &[u8; 4]) -> u32 {
        ((s[0] as u32) << 24) | ((s[1] as u32) << 16) | ((s[2] as u32) << 8) | (s[3] as u32)
    }

    const SYSTEM_OBJECT: AudioObjectID = 1; // kAudioObjectSystemObject
    const SCOPE_GLOBAL: u32 = fourcc(b"glob"); // kAudioObjectPropertyScopeGlobal
    const ELEMENT_MAIN: u32 = 0; // kAudioObjectPropertyElementMain
    const TRANSLATE_PID_TO_PROCESS_OBJECT: u32 = fourcc(b"id2p"); // kAudioHardwarePropertyTranslatePIDToProcessObject
    const PROCESS_OBJECT_LIST: u32 = fourcc(b"prs#"); // kAudioHardwarePropertyProcessObjectList
    const PROCESS_BUNDLE_ID: u32 = fourcc(b"pbid"); // kAudioProcessPropertyBundleID
    const TAP_PROPERTY_FORMAT: u32 = fourcc(b"tfmt"); // kAudioTapPropertyFormat
    const FORMAT_FLAG_IS_FLOAT: u32 = 1; // kAudioFormatFlagIsFloat

    // SAFETY: standard CoreAudio.framework HAL entry points that have existed
    // for many releases (the 14.2+ tap entry points are resolved via dlsym in
    // `tap_fns` instead, so this library loads on older macOS too).
    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectGetPropertyDataSize(
            in_object: AudioObjectID,
            in_address: *const AudioObjectPropertyAddress,
            in_qualifier_data_size: u32,
            in_qualifier_data: *const c_void,
            out_data_size: *mut u32,
        ) -> OSStatus;
        fn AudioObjectGetPropertyData(
            in_object: AudioObjectID,
            in_address: *const AudioObjectPropertyAddress,
            in_qualifier_data_size: u32,
            in_qualifier_data: *const c_void,
            io_data_size: *mut u32,
            out_data: *mut c_void,
        ) -> OSStatus;
        fn AudioHardwareCreateAggregateDevice(
            in_description: core_foundation::dictionary::CFDictionaryRef,
            out_device: *mut AudioObjectID,
        ) -> OSStatus;
        fn AudioHardwareDestroyAggregateDevice(in_device: AudioObjectID) -> OSStatus;
        fn AudioDeviceCreateIOProcIDWithBlock(
            out_proc_id: *mut AudioDeviceIOProcID,
            in_device: AudioObjectID,
            in_dispatch_queue: *mut c_void,
            in_io_block: *mut c_void,
        ) -> OSStatus;
        fn AudioDeviceDestroyIOProcID(
            in_device: AudioObjectID,
            in_proc_id: AudioDeviceIOProcID,
        ) -> OSStatus;
        fn AudioDeviceStart(in_device: AudioObjectID, in_proc_id: AudioDeviceIOProcID) -> OSStatus;
        fn AudioDeviceStop(in_device: AudioObjectID, in_proc_id: AudioDeviceIOProcID) -> OSStatus;
    }

    // libSystem: runtime symbol resolution + a serial queue for the IO block.
    extern "C" {
        fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
        fn dispatch_queue_create(label: *const c_char, attr: *const c_void) -> *mut c_void;
        fn dispatch_release(object: *mut c_void);
    }

    /// `RTLD_DEFAULT` on macOS: search every image already loaded.
    const RTLD_DEFAULT: *mut c_void = -2isize as *mut c_void;

    type CreateProcessTapFn = unsafe extern "C" fn(*mut AnyObject, *mut AudioObjectID) -> OSStatus;
    type DestroyProcessTapFn = unsafe extern "C" fn(AudioObjectID) -> OSStatus;

    /// Resolve the 14.2+ tap entry points at runtime. `None` on older macOS.
    fn tap_fns() -> Option<(CreateProcessTapFn, DestroyProcessTapFn)> {
        // SAFETY: dlsym with valid, NUL-terminated names; a missing symbol
        // yields NULL which we turn into None.
        unsafe {
            let create = dlsym(RTLD_DEFAULT, c"AudioHardwareCreateProcessTap".as_ptr());
            let destroy = dlsym(RTLD_DEFAULT, c"AudioHardwareDestroyProcessTap".as_ptr());
            if create.is_null() || destroy.is_null() {
                return None;
            }
            Some((
                std::mem::transmute::<*mut c_void, CreateProcessTapFn>(create),
                std::mem::transmute::<*mut c_void, DestroyProcessTapFn>(destroy),
            ))
        }
    }

    fn tap_description_class() -> Option<&'static AnyClass> {
        AnyClass::get(c"CATapDescription")
    }

    pub(super) fn tap_api_available() -> bool {
        tap_fns().is_some() && tap_description_class().is_some()
    }

    fn addr(selector: u32) -> AudioObjectPropertyAddress {
        AudioObjectPropertyAddress {
            m_selector: selector,
            m_scope: SCOPE_GLOBAL,
            m_element: ELEMENT_MAIN,
        }
    }

    /// Core Audio process object for `pid`, so the tap can exclude this
    /// process's own output. `None` when the process plays no audio / is
    /// unknown to the HAL — then nothing needs excluding.
    fn process_object_for_pid(pid: i32) -> Option<AudioObjectID> {
        let a = addr(TRANSLATE_PID_TO_PROCESS_OBJECT);
        let mut object: AudioObjectID = 0;
        let mut io_size = std::mem::size_of::<AudioObjectID>() as u32;
        // SAFETY: qualifier is the pid; out buffer is a fixed-size scalar.
        let status = unsafe {
            AudioObjectGetPropertyData(
                SYSTEM_OBJECT,
                &a,
                std::mem::size_of::<i32>() as u32,
                &pid as *const i32 as *const c_void,
                &mut io_size,
                &mut object as *mut AudioObjectID as *mut c_void,
            )
        };
        (status == 0 && object != 0).then_some(object)
    }

    fn process_object_ids() -> Vec<AudioObjectID> {
        let a = addr(PROCESS_OBJECT_LIST);
        let mut size = 0u32;
        let status = unsafe {
            AudioObjectGetPropertyDataSize(SYSTEM_OBJECT, &a, 0, std::ptr::null(), &mut size)
        };
        if status != 0 || size == 0 {
            return Vec::new();
        }
        let count = size as usize / std::mem::size_of::<AudioObjectID>();
        let mut objects = vec![0; count];
        let status = unsafe {
            AudioObjectGetPropertyData(
                SYSTEM_OBJECT,
                &a,
                0,
                std::ptr::null(),
                &mut size,
                objects.as_mut_ptr() as *mut c_void,
            )
        };
        if status != 0 {
            Vec::new()
        } else {
            objects
        }
    }

    fn process_bundle_id(object: AudioObjectID) -> Option<String> {
        let a = addr(PROCESS_BUNDLE_ID);
        let mut value: CFStringRef = std::ptr::null();
        let mut size = std::mem::size_of::<CFStringRef>() as u32;
        let status = unsafe {
            AudioObjectGetPropertyData(
                object,
                &a,
                0,
                std::ptr::null(),
                &mut size,
                &mut value as *mut CFStringRef as *mut c_void,
            )
        };
        if status != 0 || value.is_null() {
            return None;
        }
        let value = unsafe { CFString::wrap_under_create_rule(value) };
        Some(value.to_string())
    }

    fn matching_process_objects(target: &SystemAudioTarget) -> Vec<AudioObjectID> {
        let own_process = process_object_for_pid(std::process::id() as i32);
        let wanted: Vec<String> = target
            .bundle_ids()
            .iter()
            .map(|bundle_id| bundle_id.to_ascii_lowercase())
            .collect();
        process_object_ids()
            .into_iter()
            .filter(|object| Some(*object) != own_process)
            .filter(|object| {
                process_bundle_id(*object).is_some_and(|bundle_id| {
                    let bundle_id = lumen_core::normalize_bundle_id(&bundle_id);
                    wanted
                        .iter()
                        .any(|candidate| candidate.eq_ignore_ascii_case(&bundle_id))
                })
            })
            .collect()
    }

    /// Stream format of the tap (mono mixdown → 1 channel Float32 at the
    /// system output rate).
    fn tap_stream_format(tap: AudioObjectID) -> Option<AudioStreamBasicDescription> {
        let a = addr(TAP_PROPERTY_FORMAT);
        let mut asbd = AudioStreamBasicDescription::default();
        let mut io_size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
        // SAFETY: fixed-size struct property read on the tap object.
        let status = unsafe {
            AudioObjectGetPropertyData(
                tap,
                &a,
                0,
                std::ptr::null(),
                &mut io_size,
                &mut asbd as *mut AudioStreamBasicDescription as *mut c_void,
            )
        };
        (status == 0 && asbd.m_sample_rate > 0.0).then_some(asbd)
    }

    /// Build a `CATapDescription` for a mono mixdown of the selected processes,
    /// and return it together with its tap UUID string (needed to reference the
    /// tap from the aggregate device's tap list).
    fn build_tap_description(
        target: &SystemAudioTarget,
    ) -> Result<(Retained<AnyObject>, String), SystemAudioError> {
        let class = tap_description_class().ok_or(SystemAudioError::Unsupported)?;

        let matched = matching_process_objects(target);
        if matched.is_empty() {
            return Err(SystemAudioError::NoMatchingProcesses {
                bundle_ids: target.bundle_ids().to_vec(),
            });
        }
        let included: Vec<Retained<NSNumber>> = matched
            .iter()
            .map(|object| NSNumber::new_u32(*object))
            .collect();
        let include_array = NSArray::from_retained_slice(&included);

        // SAFETY: alloc + the documented CATapDescription initializer
        // `initMonoMixdownOfProcesses:` (macOS 14.2+; existence of the class
        // was checked above). The init consumes the alloc.
        let desc: Retained<AnyObject> = unsafe {
            let allocated: *mut AnyObject = msg_send![class, alloc];
            let initialized: *mut AnyObject = msg_send![
                allocated,
                initMonoMixdownOfProcesses: &*include_array
            ];
            Retained::from_raw(initialized).ok_or_else(|| {
                SystemAudioError::TapDescription("initMonoMixdownOfProcesses returned nil".into())
            })?
        };
        // Track a configured bundle through process restarts when Core Audio
        // can restore it; this avoids silently losing remote audio after an app
        // update/relaunch during a long recording.
        unsafe {
            let _: () = msg_send![&*desc, setProcessRestoreEnabled: Bool::YES];
        }

        // SAFETY: `UUID` / `UUIDString` are documented properties.
        let uuid_string: String = unsafe {
            let uuid: *mut AnyObject = msg_send![&*desc, UUID];
            if uuid.is_null() {
                return Err(SystemAudioError::TapDescription(
                    "tap description has no UUID".into(),
                ));
            }
            let s: Retained<NSString> = msg_send![uuid, UUIDString];
            s.to_string()
        };
        Ok((desc, uuid_string))
    }

    /// A live tap → aggregate-device → IO-proc chain.
    ///
    /// All handles are plain HAL object ids (thread-safe C API); the retained
    /// block is only invoked by Core Audio on `queue` and released via
    /// `AudioDeviceDestroyIOProcID`. Safe to move across threads.
    pub(super) struct TapSession {
        tap: AudioObjectID,
        aggregate: AudioObjectID,
        proc_id: AudioDeviceIOProcID,
        queue: *mut c_void,
        destroy_tap: DestroyProcessTapFn,
        sample_rate: u32,
        // Keep the IO block alive for the session's lifetime (Core Audio
        // copies it, but holding our reference makes the ownership obvious).
        _io_block: RcBlock<dyn Fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void, *mut c_void)>,
    }

    // SAFETY: see the struct docs — ids are value-type HAL handles, the HAL C
    // API is documented thread-safe, and the block/queue pointers are only
    // handed back to Core Audio (never dereferenced by us).
    unsafe impl Send for TapSession {}

    impl TapSession {
        pub(super) fn sample_rate(&self) -> u32 {
            self.sample_rate
        }

        pub(super) fn start(
            target: &SystemAudioTarget,
            sink: SystemAudioSink,
        ) -> Result<Self, SystemAudioError> {
            let (create_tap, destroy_tap) = tap_fns().ok_or(SystemAudioError::Unsupported)?;
            let (desc, tap_uuid) = build_tap_description(target)?;

            // 1. The process tap itself. This is the step that trips the TCC
            //    system-audio prompt; a denial surfaces as a non-zero status.
            let mut tap: AudioObjectID = 0;
            // SAFETY: `desc` is a valid CATapDescription; out param is local.
            let status = unsafe { create_tap(Retained::as_ptr(&desc) as *mut AnyObject, &mut tap) };
            if status != 0 || tap == 0 {
                return Err(SystemAudioError::CoreAudio {
                    stage: "AudioHardwareCreateProcessTap",
                    status,
                });
            }
            // From here on, tear down on every failure path.
            let fail = |stage: &'static str,
                        status: OSStatus,
                        tap: AudioObjectID,
                        aggregate: Option<AudioObjectID>| {
                if let Some(aggregate) = aggregate {
                    // SAFETY: created below in this function.
                    unsafe {
                        let _ = AudioHardwareDestroyAggregateDevice(aggregate);
                    }
                }
                // SAFETY: `tap` came from create_tap above.
                unsafe {
                    let _ = destroy_tap(tap);
                }
                SystemAudioError::CoreAudio { stage, status }
            };

            let Some(format) = tap_stream_format(tap) else {
                return Err(fail("kAudioTapPropertyFormat", -1, tap, None));
            };
            if format.m_format_flags & FORMAT_FLAG_IS_FLOAT == 0 || format.m_bits_per_channel != 32
            {
                return Err(fail("tap format is not float32", -1, tap, None));
            }
            let channels = format.m_channels_per_frame.max(1) as usize;
            let sample_rate = format.m_sample_rate.round() as u32;

            // 2. A private aggregate device that contains only the tap, so no
            //    other audio client ever sees it.
            let device_uid = format!("com.lumen.asr.systemaudio.{tap_uuid}");
            let tap_entry = CFDictionary::from_CFType_pairs(&[(
                CFString::from_static_string("uid").as_CFType(),
                CFString::new(&tap_uuid).as_CFType(),
            )]);
            let taps = CFArray::from_CFTypes(&[tap_entry.as_CFType()]);
            let agg_desc: CFDictionary<CFString, CFType> = CFDictionary::from_CFType_pairs(&[
                (
                    CFString::from_static_string("uid"),
                    CFString::new(&device_uid).as_CFType(),
                ),
                (
                    CFString::from_static_string("name"),
                    CFString::from_static_string("Lumen System Audio").as_CFType(),
                ),
                (
                    CFString::from_static_string("private"),
                    CFNumber::from(1i32).as_CFType(),
                ),
                (
                    CFString::from_static_string("tapautostart"),
                    CFNumber::from(1i32).as_CFType(),
                ),
                (CFString::from_static_string("taps"), taps.as_CFType()),
            ]);
            let mut aggregate: AudioObjectID = 0;
            // SAFETY: valid CFDictionary description; out param is local.
            let status = unsafe {
                AudioHardwareCreateAggregateDevice(agg_desc.as_concrete_TypeRef(), &mut aggregate)
            };
            if status != 0 || aggregate == 0 {
                return Err(fail(
                    "AudioHardwareCreateAggregateDevice",
                    status,
                    tap,
                    None,
                ));
            }

            // 3. IO proc reading the tapped input. The block copies each
            //    callback's samples out (down-mixing if the tap ever reports
            //    more than one channel) and forwards them to the sink.
            let io_block = RcBlock::new(
                move |_now: *mut c_void,
                      input: *mut c_void,
                      _input_time: *mut c_void,
                      _output: *mut c_void,
                      _output_time: *mut c_void| {
                    // The raw block ABI carries untyped pointers; `input` is
                    // the device's input AudioBufferList (the tap stream).
                    let input = input as *const AudioBufferList;
                    if input.is_null() {
                        return;
                    }
                    // SAFETY: Core Audio hands a valid AudioBufferList whose
                    // buffers hold float32 PCM in the tap format (verified at
                    // start). Buffer count/size are read from the list itself.
                    unsafe {
                        let list = &*input;
                        let buffers = std::slice::from_raw_parts(
                            list.m_buffers.as_ptr(),
                            list.m_number_buffers as usize,
                        );
                        for buffer in buffers {
                            if buffer.m_data.is_null() {
                                continue;
                            }
                            let n = buffer.m_data_byte_size as usize / std::mem::size_of::<f32>();
                            if n == 0 {
                                continue;
                            }
                            let samples =
                                std::slice::from_raw_parts(buffer.m_data as *const f32, n);
                            let buffer_channels = (buffer.m_number_channels as usize)
                                .max(1)
                                .min(channels.max(1));
                            if buffer_channels <= 1 {
                                sink(samples);
                            } else {
                                let mono: Vec<f32> = samples
                                    .chunks(buffer_channels)
                                    .map(|frame| frame.iter().sum::<f32>() / buffer_channels as f32)
                                    .collect();
                                sink(&mono);
                            }
                        }
                    }
                },
            );

            // SAFETY: label is a static NUL-terminated string; NULL attr =
            // serial queue.
            let queue = unsafe {
                dispatch_queue_create(c"com.lumen.asr.system-audio-tap".as_ptr(), std::ptr::null())
            };
            let mut proc_id: AudioDeviceIOProcID = std::ptr::null_mut();
            // SAFETY: aggregate is live; the block pointer stays valid for the
            // call (Core Audio copies it).
            let status = unsafe {
                AudioDeviceCreateIOProcIDWithBlock(
                    &mut proc_id,
                    aggregate,
                    queue,
                    &*io_block as *const Block<_> as *mut c_void,
                )
            };
            if status != 0 || proc_id.is_null() {
                // SAFETY: queue created above.
                unsafe { dispatch_release(queue) };
                return Err(fail(
                    "AudioDeviceCreateIOProcIDWithBlock",
                    status,
                    tap,
                    Some(aggregate),
                ));
            }

            // SAFETY: device + proc id are the pair created above.
            let status = unsafe { AudioDeviceStart(aggregate, proc_id) };
            if status != 0 {
                // SAFETY: tearing down the objects created above, in order.
                unsafe {
                    let _ = AudioDeviceDestroyIOProcID(aggregate, proc_id);
                    dispatch_release(queue);
                }
                return Err(fail("AudioDeviceStart", status, tap, Some(aggregate)));
            }

            tracing::info!(
                sample_rate,
                channels,
                tap_uuid = %tap_uuid,
                targets = ?target.bundle_ids(),
                "system audio tap capture started"
            );
            Ok(Self {
                tap,
                aggregate,
                proc_id,
                queue,
                destroy_tap,
                sample_rate,
                _io_block: io_block,
            })
        }

        pub(super) fn stop(self) {
            // SAFETY: tearing down the objects this session created, in
            // reverse creation order. Statuses are best-effort logged.
            unsafe {
                let status = AudioDeviceStop(self.aggregate, self.proc_id);
                if status != 0 {
                    tracing::warn!(status, "AudioDeviceStop failed");
                }
                let status = AudioDeviceDestroyIOProcID(self.aggregate, self.proc_id);
                if status != 0 {
                    tracing::warn!(status, "AudioDeviceDestroyIOProcID failed");
                }
                let status = AudioHardwareDestroyAggregateDevice(self.aggregate);
                if status != 0 {
                    tracing::warn!(status, "AudioHardwareDestroyAggregateDevice failed");
                }
                let status = (self.destroy_tap)(self.tap);
                if status != 0 {
                    tracing::warn!(status, "AudioHardwareDestroyProcessTap failed");
                }
                dispatch_release(self.queue);
            }
            tracing::info!("system audio tap capture stopped");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_probe_never_panics() {
        // Purely a compile+call check; on macOS 14.2+ this is true, elsewhere
        // false — either way it must be a cheap, side-effect-free probe.
        let _ = capability_available();
    }

    #[test]
    fn start_without_capability_reports_unsupported_and_stop_is_idempotent() {
        let mut capture = SystemAudioCapture::new();
        assert!(!capture.is_running());
        if !capability_available() {
            let sink: SystemAudioSink = Arc::new(|_samples: &[f32]| {});
            let target = SystemAudioTarget::new(["com.example.meeting".to_string()]);
            let err = capture.start(&target, sink).unwrap_err();
            assert!(matches!(err, SystemAudioError::Unsupported));
        }
        // stop() with nothing running is a no-op, twice.
        capture.stop();
        capture.stop();
        assert!(!capture.is_running());
    }

    #[test]
    fn target_normalizes_deduplicates_and_drops_empty_ids() {
        let target = SystemAudioTarget::new([
            " com.google.Chrome.helper.Renderer ".to_string(),
            "com.google.chrome".to_string(),
            " ".to_string(),
        ]);
        assert_eq!(target.bundle_ids(), &["com.google.Chrome"]);
    }
}
