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
                    out.push(DiskInfo {
                        path,
                        size,
                        error: None,
                    });
                }
                Err(e) => {
                    if i == 0 {
                        out.push(DiskInfo {
                            path,
                            size: None,
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
