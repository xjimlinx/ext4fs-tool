//! Windows raw disk / volume access helpers (requires administrator rights).

#[cfg(windows)]
pub mod windows {
    use std::fs::File;
    use std::os::windows::io::AsRawHandle;
    use std::ptr;

    #[link(name = "kernel32")]
    extern "system" {
        fn DeviceIoControl(
            h_device: *mut core::ffi::c_void,
            io_control_code: u32,
            in_buffer: *mut core::ffi::c_void,
            in_buffer_size: u32,
            out_buffer: *mut core::ffi::c_void,
            out_buffer_size: u32,
            bytes_returned: *mut u32,
            overlapped: *mut core::ffi::c_void,
        ) -> i32;
    }

    #[repr(C)]
    struct DiskGeometryEx {
        cylinders: u64,
        media_type: i32,
        tracks_per_cylinder: u32,
        sectors_per_track: u32,
        bytes_per_sector: u32,
        disk_size: u64,
    }

    const IOCTL_DISK_GET_DRIVE_GEOMETRY_EX: u32 = 0x000700A0;
    const IOCTL_STORAGE_QUERY_PROPERTY: u32 = 0x002D1400;

    /// Query the storage device model/vendor string (e.g. "Samsung SSD 970").
    pub fn disk_model(file: &File) -> Option<String> {
        let handle = file.as_raw_handle() as *mut core::ffi::c_void;
        // STORAGE_PROPERTY_QUERY { PropertyId=0, QueryType=0 }
        let mut query = [0u8; 12];
        let mut out = vec![0u8; 4096];
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_STORAGE_QUERY_PROPERTY,
                query.as_mut_ptr() as *mut core::ffi::c_void,
                query.len() as u32,
                out.as_mut_ptr() as *mut core::ffi::c_void,
                out.len() as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return None;
        }
        let vendor_off = u32_at(&out, 16);
        let product_off = u32_at(&out, 20);
        let serial_off = u32_at(&out, 28);
        let vendor = cstr_at(&out, vendor_off);
        let product = cstr_at(&out, product_off);
        let serial = cstr_at(&out, serial_off);
        let mut s = String::new();
        if !vendor.is_empty() && vendor != "N/A" {
            s.push_str(&vendor);
        }
        if !product.is_empty() && product != "N/A" {
            if !s.is_empty() {
                s.push(' ');
            }
            s.push_str(&product);
        }
        if !serial.is_empty() && serial != "N/A" {
            s.push_str(&format!("  SN:{}", serial));
        }
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    /// Whether the device is a rotational (HDD) drive, via the seek-penalty
    /// property. Returns `None` if it cannot be determined.
    pub fn disk_rotational(file: &File) -> Option<bool> {
        let handle = file.as_raw_handle() as *mut core::ffi::c_void;
        // STORAGE_PROPERTY_QUERY { PropertyId=StorageDeviceSeekPenaltyProperty(7), QueryType=0 }
        let mut query = [0u8; 12];
        query[0] = 7;
        let mut out = [0u8; 64];
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_STORAGE_QUERY_PROPERTY,
                query.as_mut_ptr() as *mut core::ffi::c_void,
                query.len() as u32,
                out.as_mut_ptr() as *mut core::ffi::c_void,
                out.len() as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok == 0 {
            return None;
        }
        // STORAGE_DEVICE_SEEK_PENALTY_DESCRIPTOR: IncursSeekPenalty at byte 8.
        Some(out[8] != 0)
    }

    /// Check whether the drive that `path` lives on is rotational.
    pub fn drive_rotational(path: &str) -> Option<bool> {
        let drive = path.get(0..2)?;
        let vol = format!("\\\\.\\{}", drive);
        let f = File::open(&vol).ok()?;
        disk_rotational(&f)
    }

    fn u32_at(b: &[u8], o: usize) -> u32 {
        u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
    }

    fn cstr_at(b: &[u8], off: u32) -> String {
        let off = off as usize;
        if off == 0 || off >= b.len() {
            return String::new();
        }
        let end = b[off..].iter().position(|&c| c == 0).map(|i| off + i).unwrap_or(b.len());
        String::from_utf8_lossy(&b[off..end]).trim().to_string()
    }

    pub fn is_device_path(path: &str) -> bool {
        path.starts_with("\\\\.\\")
    }

    /// Query disk geometry (total size and sector size) via IOCTL.
    /// Returns `None` for volumes or if the IOCTL is not supported.
    pub fn disk_geometry(file: &File) -> Option<(u64, u32)> {
        let handle = file.as_raw_handle() as *mut core::ffi::c_void;
        let mut geo = DiskGeometryEx {
            cylinders: 0,
            media_type: 0,
            tracks_per_cylinder: 0,
            sectors_per_track: 0,
            bytes_per_sector: 0,
            disk_size: 0,
        };
        let mut returned: u32 = 0;
        let ok = unsafe {
            DeviceIoControl(
                handle,
                IOCTL_DISK_GET_DRIVE_GEOMETRY_EX,
                ptr::null_mut(),
                0,
                (&mut geo as *mut DiskGeometryEx) as *mut core::ffi::c_void,
                std::mem::size_of::<DiskGeometryEx>() as u32,
                &mut returned,
                ptr::null_mut(),
            )
        };
        if ok != 0 && geo.disk_size > 0 {
            Some((geo.disk_size, geo.bytes_per_sector.max(512)))
        } else {
            None
        }
    }

    pub struct DiskInfo {
        pub path: String,
        pub size: Option<u64>,
        pub model: Option<String>,
        pub error: Option<String>,
    }

    /// Enumerate `\\.\PhysicalDrive0..N` until opening fails.
    pub fn enumerate_disks() -> Vec<DiskInfo> {
        let mut out = Vec::new();
        for i in 0..64u32 {
            let path = format!("\\\\.\\PhysicalDrive{}", i);
            match File::open(&path) {
                Ok(f) => {
                    let size = disk_geometry(&f).map(|g| g.0);
                    let model = disk_model(&f);
                    out.push(DiskInfo {
                        path,
                        size,
                        model,
                        error: None,
                    });
                }
                Err(e) => {
                    if i == 0 {
                        out.push(DiskInfo {
                            path,
                            size: None,
                            model: None,
                            error: Some(format!("access denied: {}", e)),
                        });
                    }
                    break;
                }
            }
        }
        out
    }

    pub fn sector_size_of(file: &File) -> u64 {
        disk_geometry(file).map(|g| g.1 as u64).unwrap_or(512)
    }
}
