//! Best-effort output attenuation. Never changes the mute switch or follows a
//! newly selected output device while a recording is active.
use std::sync::Mutex;

type Result<T> = std::result::Result<T, String>;

trait VolumeBackend {
    fn default_device(&self) -> Result<u32>;
    fn uid(&self, device: u32) -> Result<String>;
    fn volume(&self, device: u32) -> Result<f32>;
    fn set_volume(&self, device: u32, value: f32) -> Result<()>;
}

struct Lease {
    device: u32,
    uid: String,
    original: f32,
    written: f32,
    holders: usize,
}

struct Controller<B> {
    backend: B,
    lease: Option<Lease>,
}

impl<B: VolumeBackend> Controller<B> {
    fn acquire(&mut self, ceiling: f32) -> Result<()> {
        if !ceiling.is_finite() || !(0.0..=1.0).contains(&ceiling) {
            return Err("ducking volume must be between 0 and 1".into());
        }
        if let Some(lease) = &mut self.lease {
            lease.holders += 1;
            return Ok(());
        }
        let device = self.backend.default_device()?;
        let uid = self.backend.uid(device)?;
        let original = self.backend.volume(device)?;
        if !original.is_finite() || !(0.0..=1.0).contains(&original) {
            return Err("invalid device volume".into());
        }
        let written = original.min(ceiling);
        if written < original {
            self.backend.set_volume(device, written)?;
        }
        self.lease = Some(Lease {
            device,
            uid,
            original,
            written,
            holders: 1,
        });
        Ok(())
    }

    fn release(&mut self) -> Result<()> {
        let Some(lease) = &mut self.lease else {
            return Ok(());
        };
        lease.holders -= 1;
        if lease.holders != 0 {
            return Ok(());
        }
        let lease = self.lease.take().unwrap();
        if lease.original == lease.written {
            return Ok(());
        }
        // HAL object IDs may be reused after device removal. Check persistent
        // identity before reading or writing volume on the original object.
        if self.backend.uid(lease.device)? != lease.uid {
            return Ok(());
        }
        let current = self.backend.volume(lease.device)?;
        // Preserve an observed user/device adjustment. HAL offers no atomic CAS;
        // changes racing this final read/write cannot be completely excluded.
        if (current - lease.written).abs() <= 0.0001 {
            self.backend.set_volume(lease.device, lease.original)?;
        }
        Ok(())
    }
}

static CONTROLLER: Mutex<Controller<Native>> = Mutex::new(Controller {
    backend: Native,
    lease: None,
});

/// Owns one process-local lease. The last holder restores the original device
/// if its volume still matches ours. Unsupported devices return an error; the
/// caller should continue recording. Forced termination cannot run Drop.
pub struct AudioDuckingGuard {
    _private: (),
}

impl AudioDuckingGuard {
    pub fn acquire(ceiling: f32) -> Result<Self> {
        CONTROLLER
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .acquire(ceiling)?;
        Ok(Self { _private: () })
    }
}

impl Drop for AudioDuckingGuard {
    fn drop(&mut self) {
        if let Err(error) = CONTROLLER
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .release()
        {
            tracing::warn!(%error, "could not restore ducked output volume");
        }
    }
}

struct Native;

#[cfg(not(target_os = "macos"))]
impl VolumeBackend for Native {
    fn default_device(&self) -> Result<u32> {
        Err("output ducking requires macOS".into())
    }
    fn uid(&self, _: u32) -> Result<String> {
        Err("unsupported".into())
    }
    fn volume(&self, _: u32) -> Result<f32> {
        Err("unsupported".into())
    }
    fn set_volume(&self, _: u32, _: f32) -> Result<()> {
        Err("unsupported".into())
    }
}

#[cfg(target_os = "macos")]
mod hal {
    use super::*;
    use core_foundation::{
        base::TCFType,
        string::{CFString, CFStringRef},
    };
    use std::{ffi::c_void, mem::size_of, ptr};

    #[repr(C)]
    struct Address {
        selector: u32,
        scope: u32,
        element: u32,
    }
    const GLOBAL: u32 = u32::from_be_bytes(*b"glob");
    const OUTPUT: u32 = u32::from_be_bytes(*b"outp");
    const VOLUME: Address = Address {
        selector: u32::from_be_bytes(*b"volm"),
        scope: OUTPUT,
        element: 0,
    };

    #[link(name = "CoreAudio", kind = "framework")]
    extern "C" {
        fn AudioObjectGetPropertyData(
            object: u32,
            address: *const Address,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
            data: *mut c_void,
        ) -> i32;
        fn AudioObjectIsPropertySettable(
            object: u32,
            address: *const Address,
            settable: *mut u8,
        ) -> i32;
        fn AudioObjectSetPropertyData(
            object: u32,
            address: *const Address,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: u32,
            data: *const c_void,
        ) -> i32;
    }

    fn check(status: i32) -> Result<()> {
        if status == 0 {
            Ok(())
        } else {
            Err(format!("Core Audio status {status}"))
        }
    }
    // Only called with the exact scalar/pointer type specified by the selector.
    fn read<T: Copy + Default>(device: u32, address: &Address) -> Result<T> {
        let mut value = T::default();
        let mut size = size_of::<T>() as u32;
        unsafe {
            check(AudioObjectGetPropertyData(
                device,
                address,
                0,
                ptr::null(),
                &mut size,
                &mut value as *mut T as *mut c_void,
            ))?;
        }
        if size != size_of::<T>() as u32 {
            return Err("unexpected HAL property size".into());
        }
        Ok(value)
    }

    impl VolumeBackend for Native {
        fn default_device(&self) -> Result<u32> {
            let device = read(
                1,
                &Address {
                    selector: u32::from_be_bytes(*b"dOut"),
                    scope: GLOBAL,
                    element: 0,
                },
            )?;
            if device == 0 {
                return Err("no default output device".into());
            }
            Ok(device)
        }
        fn uid(&self, device: u32) -> Result<String> {
            let value: CFStringRef = read(
                device,
                &Address {
                    selector: u32::from_be_bytes(*b"uid "),
                    scope: GLOBAL,
                    element: 0,
                },
            )?;
            if value.is_null() {
                return Err("missing output device UID".into());
            }
            // AudioHardwareBase.h: caller releases the returned CFString.
            Ok(unsafe { CFString::wrap_under_create_rule(value) }.to_string())
        }
        fn volume(&self, device: u32) -> Result<f32> {
            read(device, &VOLUME)
        }
        fn set_volume(&self, device: u32, value: f32) -> Result<()> {
            let mut settable = 0u8;
            unsafe {
                check(AudioObjectIsPropertySettable(
                    device,
                    &VOLUME,
                    &mut settable,
                ))?;
                if settable == 0 {
                    return Err("output master volume is read-only".into());
                }
                check(AudioObjectSetPropertyData(
                    device,
                    &VOLUME,
                    0,
                    ptr::null(),
                    size_of::<f32>() as u32,
                    &value as *const f32 as *const c_void,
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    struct Mock {
        device: Cell<u32>,
        uid: RefCell<String>,
        volume: Cell<f32>,
        writes: RefCell<Vec<(u32, f32)>>,
        fail: Cell<bool>,
    }
    impl VolumeBackend for Mock {
        fn default_device(&self) -> Result<u32> {
            Ok(self.device.get())
        }
        fn uid(&self, _: u32) -> Result<String> {
            Ok(self.uid.borrow().clone())
        }
        fn volume(&self, _: u32) -> Result<f32> {
            Ok(self.volume.get())
        }
        fn set_volume(&self, device: u32, value: f32) -> Result<()> {
            if self.fail.get() {
                return Err("unsupported".into());
            }
            self.writes.borrow_mut().push((device, value));
            self.volume.set(value);
            Ok(())
        }
    }
    fn controller() -> Controller<Mock> {
        Controller {
            backend: Mock {
                device: Cell::new(7),
                uid: RefCell::new("speaker".into()),
                volume: Cell::new(0.8),
                writes: RefCell::new(vec![]),
                fail: Cell::new(false),
            },
            lease: None,
        }
    }
    #[test]
    fn last_lease_restores_original_device_even_after_default_switch() {
        let mut c = controller();
        c.acquire(0.15).unwrap();
        c.acquire(0.0).unwrap();
        c.backend.device.set(9);
        c.release().unwrap();
        assert_eq!(c.backend.volume.get(), 0.15);
        c.release().unwrap();
        assert_eq!(*c.backend.writes.borrow(), vec![(7, 0.15), (7, 0.8)]);
    }
    #[test]
    fn user_adjustment_and_recycled_device_are_preserved() {
        let mut c = controller();
        c.acquire(0.15).unwrap();
        c.backend.volume.set(0.4);
        c.release().unwrap();
        assert_eq!(c.backend.volume.get(), 0.4);
        c.acquire(0.15).unwrap();
        *c.backend.uid.borrow_mut() = "different".into();
        c.release().unwrap();
        assert_eq!(c.backend.volume.get(), 0.15);
    }
    #[test]
    fn quiet_output_is_never_raised_and_invalid_config_never_writes() {
        let mut c = controller();
        c.backend.volume.set(0.05);
        c.acquire(0.15).unwrap();
        c.release().unwrap();
        for value in [f32::NAN, -0.1, 1.1] {
            assert!(c.acquire(value).is_err());
        }
        assert!(c.backend.writes.borrow().is_empty());
    }
    #[test]
    fn failed_acquisition_does_not_leave_a_lease() {
        let mut c = controller();
        c.backend.fail.set(true);
        assert!(c.acquire(0.15).is_err());
        assert!(c.lease.is_none());
        c.backend.fail.set(false);
        c.acquire(0.15).unwrap();
        c.release().unwrap();
        assert_eq!(c.backend.volume.get(), 0.8);
    }
    #[test]
    fn failed_restore_does_not_poison_the_next_recording() {
        let mut c = controller();
        c.acquire(0.15).unwrap();
        c.backend.fail.set(true);
        assert!(c.release().is_err());
        assert!(c.lease.is_none());
        c.backend.fail.set(false);
        c.backend.volume.set(0.6);
        c.acquire(0.15).unwrap();
        c.release().unwrap();
        assert_eq!(c.backend.volume.get(), 0.6);
    }
}
