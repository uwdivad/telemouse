//! HID device identity for [`telemouse_core::RawEvent::device_ix`].
//!
//! Raw input reports which device produced a frame as an opaque `HANDLE` in
//! `RAWINPUT.header.hDevice`. Consumers want a stable *name*, so the agent
//! enumerates the pointing devices once at startup, ships the names in
//! `SessionConfig.devices`, and puts the index on every event.
//!
//! Index 0 is reserved for [`UNKNOWN_DEVICE`], matching the core contract's
//! "0 when unknown" — so `devices[device_ix]` always resolves, and a handle we
//! could not name is honestly reported as unknown rather than blamed on the
//! first real mouse.
//!
//! The lookup is a linear scan over a handful of `isize`s: no hashing, no
//! allocation, and it stays in one cache line for a typical two-mouse desk.

/// The name occupying index 0 of `SessionConfig.devices`.
pub const UNKNOWN_DEVICE: &str = "unknown";

/// `RawEvent::device_ix` is a `u8`, so index 255 is the last usable slot.
pub const MAX_DEVICES: usize = 256;

/// Handle → index mapping, owned by T1 and mutated only from the hot path.
#[derive(Debug, Clone)]
pub struct DeviceTable {
    /// Parallel to `names[1..]`: `handles[i]` maps to index `i + 1`.
    handles: Vec<isize>,
    names: Vec<String>,
    /// Handles we already failed to resolve, so a device we cannot name costs
    /// one failed query per session rather than one per event.
    unresolved: Vec<isize>,
}

impl Default for DeviceTable {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl DeviceTable {
    /// Build from the startup enumeration, in `(handle, name)` order.
    pub fn new(enumerated: Vec<(isize, String)>) -> Self {
        let mut table = Self {
            handles: Vec::with_capacity(enumerated.len() + 2),
            names: vec![UNKNOWN_DEVICE.to_string()],
            unresolved: Vec::new(),
        };
        for (handle, name) in enumerated {
            table.append(handle, name);
        }
        table
    }

    /// Device names indexed by `device_ix`, index 0 being [`UNKNOWN_DEVICE`].
    /// This is exactly what goes into `SessionConfig.devices`.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        // Index 0 is always present, so "empty" means "no real devices".
        self.names.len() <= 1
    }

    /// Index for a handle, or `None` if it is not known yet. Hot path.
    #[inline]
    pub fn index_of(&self, handle: isize) -> Option<u8> {
        let mut i = 0;
        while i < self.handles.len() {
            if self.handles[i] == handle {
                return Some((i + 1) as u8);
            }
            i += 1;
        }
        None
    }

    /// True once we have already tried and failed to name this handle.
    pub fn is_unresolved(&self, handle: isize) -> bool {
        self.unresolved.contains(&handle)
    }

    /// Remember that `handle` could not be named; it maps to 0 from now on.
    pub fn mark_unresolved(&mut self, handle: isize) {
        if !self.unresolved.contains(&handle) {
            self.unresolved.push(handle);
        }
    }

    /// Append a newly discovered device. Returns its index, or 0 if the table
    /// is full (255 pointing devices is not a real desk).
    pub fn append(&mut self, handle: isize, name: String) -> u8 {
        if let Some(existing) = self.index_of(handle) {
            return existing;
        }
        if self.names.len() >= MAX_DEVICES {
            self.mark_unresolved(handle);
            return 0;
        }
        self.handles.push(handle);
        self.names.push(name);
        (self.names.len() - 1) as u8
    }
}

#[cfg(windows)]
pub use win::enumerate_mice;

#[cfg(windows)]
mod win {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::UI::Input::{
        GetRawInputDeviceInfoW, GetRawInputDeviceList, RAWINPUTDEVICELIST, RID_DEVICE_INFO_TYPE,
        RIDI_DEVICENAME, RIM_TYPEMOUSE,
    };

    /// Name of one raw-input device, e.g. `\\?\HID#VID_1532&PID_0099#...`.
    pub(super) fn device_name(handle: isize) -> Option<String> {
        let h = HANDLE(handle as *mut core::ffi::c_void);
        let mut chars = 0u32;
        // First call with a null buffer asks for the required length.
        let probe =
            unsafe { GetRawInputDeviceInfoW(Some(h), RIDI_DEVICENAME, None, &mut chars) };
        if probe == u32::MAX || chars == 0 || chars > 4096 {
            return None;
        }
        let mut buf = vec![0u16; chars as usize];
        let written = unsafe {
            GetRawInputDeviceInfoW(
                Some(h),
                RIDI_DEVICENAME,
                Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
                &mut chars,
            )
        };
        if written == u32::MAX {
            return None;
        }
        let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        if len == 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len]))
    }

    /// Every pointing device the system currently knows about, as
    /// `(hDevice, name)` pairs in enumeration order.
    pub fn enumerate_mice() -> Vec<(isize, String)> {
        let mut count = 0u32;
        let size = size_of::<RAWINPUTDEVICELIST>() as u32;
        let probe = unsafe { GetRawInputDeviceList(None, &mut count, size) };
        if probe == u32::MAX || count == 0 {
            return Vec::new();
        }
        let mut list = vec![RAWINPUTDEVICELIST::default(); count as usize];
        let got = unsafe { GetRawInputDeviceList(Some(list.as_mut_ptr()), &mut count, size) };
        if got == u32::MAX {
            return Vec::new();
        }
        list.truncate(got as usize);
        list.into_iter()
            .filter(|d| d.dwType == RID_DEVICE_INFO_TYPE(RIM_TYPEMOUSE.0))
            .filter_map(|d| {
                let handle = d.hDevice.0 as isize;
                device_name(handle).map(|name| (handle, name))
            })
            .take(super::MAX_DEVICES - 1)
            .collect()
    }
}

/// Re-query the OS for a handle's name. Always `None` off Windows.
#[cfg(windows)]
pub fn resolve_name(handle: isize) -> Option<String> {
    win::device_name(handle)
}

#[cfg(not(windows))]
pub fn resolve_name(_handle: isize) -> Option<String> {
    None
}

#[cfg(not(windows))]
pub fn enumerate_mice() -> Vec<(isize, String)> {
    Vec::new()
}

/// Resolve `handle` to an index, learning it if this is a device we have not
/// seen (one OS query per new handle, then never again).
pub fn index_for(table: &mut DeviceTable, handle: isize) -> u8 {
    if let Some(ix) = table.index_of(handle) {
        return ix;
    }
    if handle == 0 || table.is_unresolved(handle) {
        return 0;
    }
    match resolve_name(handle) {
        Some(name) => {
            let ix = table.append(handle, name);
            if ix == 0 {
                tracing::warn!(handle, "device table full; events attributed to index 0");
            }
            ix
        }
        None => {
            table.mark_unresolved(handle);
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> DeviceTable {
        DeviceTable::new(vec![
            (0x11, r"\\?\HID#VID_1532&PID_0099#a".to_string()),
            (0x22, r"\\?\HID#VID_046D&PID_C08B#b".to_string()),
        ])
    }

    #[test]
    fn index_zero_is_reserved_for_unknown() {
        let t = table();
        assert_eq!(t.names()[0], UNKNOWN_DEVICE);
        assert_eq!(t.len(), 3);
        assert!(!t.is_empty());
        assert!(DeviceTable::default().is_empty());
        assert_eq!(DeviceTable::default().names(), [UNKNOWN_DEVICE]);
    }

    #[test]
    fn enumerated_handles_map_to_their_enumeration_order() {
        let t = table();
        assert_eq!(t.index_of(0x11), Some(1));
        assert_eq!(t.index_of(0x22), Some(2));
        assert_eq!(t.index_of(0x33), None);
        // The name at the index is the device that produced the event.
        assert!(t.names()[1].contains("VID_1532"));
        assert!(t.names()[2].contains("VID_046D"));
    }

    #[test]
    fn appending_is_idempotent_per_handle() {
        let mut t = table();
        assert_eq!(t.append(0x33, "c".into()), 3);
        assert_eq!(t.append(0x33, "c-again".into()), 3);
        assert_eq!(t.len(), 4);
        assert_eq!(t.names()[3], "c");
    }

    #[test]
    fn unresolvable_handles_are_queried_once_then_map_to_zero() {
        let mut t = table();
        // Off Windows `resolve_name` always fails, which is the "cannot name
        // this device" path we want: it must map to 0 and be remembered.
        assert_eq!(index_for(&mut t, 0x44), 0);
        assert!(t.is_unresolved(0x44));
        assert_eq!(index_for(&mut t, 0x44), 0);
        assert_eq!(t.len(), 3, "a failed lookup must not grow the table");
        // A null handle never costs a query at all.
        assert_eq!(index_for(&mut t, 0), 0);
        assert!(!t.is_unresolved(0));
    }

    #[test]
    fn known_handles_resolve_without_touching_the_os() {
        let mut t = table();
        assert_eq!(index_for(&mut t, 0x22), 2);
        assert_eq!(index_for(&mut t, 0x11), 1);
        assert!(t.unresolved.is_empty());
    }

    #[test]
    fn the_table_saturates_instead_of_overflowing_a_u8() {
        let mut t = DeviceTable::default();
        for h in 1..MAX_DEVICES as isize {
            assert_eq!(t.append(h, format!("d{h}")), h as u8);
        }
        assert_eq!(t.len(), MAX_DEVICES);
        // One past the end: reported as unknown, not wrapped to 0-as-a-device.
        assert_eq!(t.append(9_999, "overflow".into()), 0);
        assert!(t.is_unresolved(9_999));
        assert_eq!(t.len(), MAX_DEVICES);
    }
}
