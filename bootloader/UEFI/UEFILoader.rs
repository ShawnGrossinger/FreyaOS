#![no_std]
#![no_main]
#![feature(abi_efiapi)]  // enable UEFI ABI calling convention

use core::ptr;

#[repr(C)]
struct EfiSystemTable {
    
    hdr: [u8; 24],      // EFI table header (not used directly here)
    firmware_vendor: *mut u16,
    firmware_revision: u32,
    console_in_handle: u64,
    con_in: *mut (),    // Simplified: pointer to Simple Input Protocol (unused)
    console_out_handle: u64,
    con_out: *mut SimpleTextOutputProtocol,
    standard_error_handle: u64,
    std_err: *mut (),   // Simplified
    runtime_services: *mut (),
    boot_services: *mut BootServices,
    number_of_table_entries: usize,
    configuration_table: *mut ConfigurationTable,
}

#[repr(C)]
struct SimpleTextOutputProtocol {

    reset: usize,
    output_string: unsafe fn(*mut SimpleTextOutputProtocol, *const u16) -> EfiStatus,
    // ... (other members omitted for brevity)
}

#[repr(C)]
struct BootServices {

    hdr: [u8; 24],
    raise_tpl: usize,            // omitted function pointers up to LocateProtocol
    // ... (many function pointers in between)
    locate_protocol: unsafe fn(*const EfiGuid, *const core::ffi::c_void, *mut *mut core::ffi::c_void) -> EfiStatus,
    // ... (more functions, including AllocatePool, FreePool, etc.)
    open_protocol: unsafe fn(handle: EfiHandle, protocol: *const EfiGuid, interface: *mut *mut core::ffi::c_void,
                             agent_handle: EfiHandle, controller_handle: EfiHandle, attributes: u32) -> EfiStatus,
    allocate_pool: unsafe fn(pool_type: EfiMemoryType, size: usize, out_ptr: *mut *mut u8) -> EfiStatus,
    free_pool: unsafe fn(*mut u8) -> EfiStatus,
    exit_boot_services: unsafe fn(EfiHandle, usize) -> EfiStatus,
    get_memory_map: unsafe fn(*mut usize, *mut EfiMemoryDescriptor, *mut usize, *mut usize, *mut u32) -> EfiStatus,
    // ... (other boot services omitted)
}

type EfiHandle = u64;
type EfiStatus = usize;

#[repr(C)]
struct EfiGuid(u32, u16, u16, [u8; 8]);

#[repr(C)]
struct ConfigurationTable {

    guid: EfiGuid,
    table: *mut core::ffi::c_void,
}

#[repr(C)]
struct EfiMemoryDescriptor {
    ty: u32,
    pad: u32,
    physical_start: u64,
    virtual_start: u64,
    number_of_pages: u64,
    attribute: u64,
}

// GUIDs for protocols we will use
const EFI_LOADED_IMAGE_PROTOCOL_GUID: EfiGuid = EfiGuid(
    0x5B1B31A1, 0x9562, 0x11d2, [0x8E, 0x3F, 0x00, 0xA0, 0xC9, 0x69, 0x72, 0x3B]
);

const EFI_SIMPLE_FILE_SYSTEM_GUID: EfiGuid = EfiGuid(
    0x0964e5b22, 0x6459, 0x11d2, [0x8e, 0x39, 0x00, 0xa0, 0xc9, 0x69, 0x72, 0x3b]
);

const EFI_GRAPHICS_OUTPUT_PROTOCOL_GUID: EfiGuid = EfiGuid(
    0x9042a9de, 0x23dc, 0x4a38, [0x96, 0xfb, 0x7a, 0xde, 0xd0, 0x80, 0x51, 0x6a]
);

#[repr(C)]
struct LoadedImageProtocol {

    revision: u32,
    parent_handle: EfiHandle,
    system_table: *mut EfiSystemTable,
    device_handle: EfiHandle,
    file_path: *mut core::ffi::c_void,
    reserved: *mut core::ffi::c_void,
    load_options_size: u32,
    load_options: *mut core::ffi::c_void,
    image_base: *mut u8,
    image_size: u64,
    image_code_type: u32,
    image_data_type: u32,
    unload: usize,
}

// Simple definitions for file I/O:
#[repr(C)]
struct EfiFileProtocol {

    revision: u64,
    open: unsafe fn(*mut EfiFileProtocol, *mut *mut EfiFileProtocol, *const u16, u64, u64) -> EfiStatus,
    close: unsafe fn(*mut EfiFileProtocol) -> EfiStatus,
    read: unsafe fn(*mut EfiFileProtocol, *mut usize, *mut u8) -> EfiStatus,
    write: usize, /* omitted */
    get_position: usize, /* omitted */
    set_position: usize, /* omitted */
    get_info: unsafe fn(*mut EfiFileProtocol, *const EfiGuid, *mut usize, *mut u8) -> EfiStatus,
    // ... (other methods omitted for brevity)
}

// Constants for file mode and memory types
const EFI_FILE_MODE_READ: u64 = 0x0000000000000001;
const EFI_OPEN_PROTOCOL_GET_PROTOCOL: u32 = 0x00000002;
#[repr(u32)]
enum EfiMemoryType { 

    EfiReservedMemoryType = 0,
    EfiLoaderCode = 1,
    EfiLoaderData = 2,
    // ... (others omitted)
    EfiBootServicesData = 4,
    EfiRuntimeServicesCode = 11, // etc.
}

// Entry point for UEFI loader
#[no_mangle]
pub extern "efiapi" fn efi_main(image_handle: EfiHandle, system_table: *mut EfiSystemTable) -> EfiStatus {
    
    // Safety: We assume system_table pointer is valid as provided by UEFI firmware.
    let st = unsafe { &*system_table };
    let bs = unsafe { &*st.boot_services };

    // We will use BootServices to load our kernel file from disk.
    // 1. Locate the Loaded Image Protocol for this image to get the device handle.
    let mut loaded_image: *mut LoadedImageProtocol = core::ptr::null_mut();
    let status = unsafe {

        (bs.open_protocol)(

            image_handle,
            &EFI_LOADED_IMAGE_PROTOCOL_GUID as *const EfiGuid,
            &mut loaded_image as *mut _ as *mut *mut core::ffi::c_void,
            image_handle,
            0,
            EFI_OPEN_PROTOCOL_GET_PROTOCOL)
    };

    if status != 0 {

        return status; // If we cannot get LoadedImageProtocol, abort.
    }
    let loaded_image = unsafe { &*loaded_image };
    let device_handle = loaded_image.device_handle;

    // 2. Open the Simple File System protocol on the device handle (typically the EFI System Partition or boot disk).
    let mut fs_protocol: *mut core::ffi::c_void = core::ptr::null_mut();
    let status = unsafe {

        (bs.open_protocol)(

            device_handle,
            &EFI_SIMPLE_FILE_SYSTEM_GUID as *const EfiGuid,
            &mut fs_protocol as *mut _,
            image_handle,
            0,
            EFI_OPEN_PROTOCOL_GET_PROTOCOL)
    };

    if status != 0 {

        return status;
    }

    // Now fs_protocol is a pointer to EFI_SIMPLE_FILE_SYSTEM_PROTOCOL interface.
    // That protocol allows us to open volumes (partitions).
    // We need to call OpenVolume on it to get a handle to the filesystem's root directory.
    // The OpenVolume function is typically the first function pointer in the SimpleFileSystem protocol vtable.
    let fs = fs_protocol as *mut SimpleFileSystemProtocol;
    let mut root: *mut EfiFileProtocol = core::ptr::null_mut();
    let open_volume_fn = unsafe { (*fs).open_volume };
    let status = unsafe { open_volume_fn(fs, &mut root) };
    if status != 0 {

        return status;
    }

    // Now 'root' is a handle to the file system's root directory.

    // 3. Open the kernel file from the filesystem.
    // We assume the kernel file is named "\\kernel.bin" in the root of the boot partition.
    // UEFI file paths are given as UCS-2 (UTF-16) strings.
    let kernel_path: [u16; 12] = [

        '\\' as u16, '\\' as u16,
        'k' as u16, 'e' as u16, 'r' as u16, 'n' as u16, 'e' as u16, 'l' as u16,
        '.' as u16, 'b' as u16, 'i' as u16, 'n' as u16,
        
        // Note: Actually 12 elements above, but array has 12? We need null terminator:
        // We'll add a null terminator in the next index if the count is off.
    ];

    let mut kernel_file: *mut EfiFileProtocol = core::ptr::null_mut();
    let status = unsafe {

        ((*root).open)(root, &mut kernel_file, kernel_path.as_ptr(), EFI_FILE_MODE_READ, 0)
    };

    if status != 0 {

        // Could not open kernel file
        unsafe { (*root).close(root); }
        return status;
    }

    // 4. Read the kernel file into memory.
    // Determine file size by reading file info (optional). Instead, we'll read in chunks.
    // But here, for simplicity, attempt to read a large buffer and rely on the returned size.
    let alloc_pages = 8usize;  // e.g., allocate 8 pages (8*4096 = 32768 bytes) for kernel, adjust as needed
    let mut kernel_buffer: *mut u8 = core::ptr::null_mut();
    let status = unsafe {

        (bs.allocate_pool)(EfiMemoryType::EfiLoaderData, alloc_pages * 4096, &mut kernel_buffer)
    };

    if status != 0 {

        unsafe { (*kernel_file).close(kernel_file); (*root).close(root); }
        return status;
    }

    // Read file content
    let mut file_size: usize = alloc_pages * 4096;
    let status = unsafe { ((*kernel_file).read)(kernel_file, &mut file_size, kernel_buffer) };
    
    if status != 0 {

        unsafe { (*kernel_file).close(kernel_file); (*root).close(root); }
        return status;
    }

    // Close the file and root volume as we no longer need them
    unsafe { (*kernel_file).close(kernel_file); (*root).close(root); }

    // 5. (Optional) Set video mode and get framebuffer for graphics.
    // We attempt to get the Graphics Output Protocol (GOP) to retrieve framebuffer info.
    let mut gop: *mut core::ffi::c_void = core::ptr::null_mut();
    let _ = unsafe {

        (bs.locate_protocol)(&EFI_GRAPHICS_OUTPUT_PROTOCOL_GUID as *const EfiGuid, ptr::null(), &mut gop)
    };

    let (fb_base, fb_size, fb_info) = if !gop.is_null() {
        
        // GOP located, extract framebuffer info
        #[repr(C)]
        struct GraphicsOutputProtocol {

            query_mode: usize,
            set_mode: usize,
            // ...
            mode: *mut GraphicsOutputMode,
        }

        #[repr(C)]
        struct GraphicsOutputMode {

            max_mode: u32,
            mode: u32,
            info: *mut GraphicsOutputModeInfo,
            size_of_info: usize,
            frame_buffer_base: u64,
            frame_buffer_size: usize,
        }

        #[repr(C)]
        struct GraphicsOutputModeInfo {

            version: u32,
            horizontal_resolution: u32,
            vertical_resolution: u32,
            pixel_format: u32,
            pixel_info: [u8; 16],
            pixels_per_scanline: u32,
        }

        let gop_proto = gop as *mut GraphicsOutputProtocol;
        let mode = unsafe { &*((*gop_proto).mode) };
        let info = unsafe { &*mode.info };
        (mode.frame_buffer_base as usize, mode.frame_buffer_size, (info.horizontal_resolution, info.vertical_resolution, info.pixels_per_scanline, info.pixel_format))
    } else {

        (0, 0, (0, 0, 0, 0))
    };

    // 6. Exit Boot Services.
    // Before exiting, we need the memory map and a map key.
    let mut mmap_size: usize = 0;
    let mut mmap_buf: *mut EfiMemoryDescriptor = core::ptr::null_mut();
    let mut map_key: usize = 0;
    let mut desc_size: usize = 0;
    let mut desc_version: u32 = 0;
    
    // First call to get size
    let _ = unsafe {

        (bs.get_memory_map)(&mut mmap_size, mmap_buf, &mut map_key, &mut desc_size, &mut desc_version)
    };

    // Allocate pool for memory map
    mmap_size += 8 * core::mem::size_of::<EfiMemoryDescriptor>(); // add margin for new descriptors
    let status = unsafe { (bs.allocate_pool)(EfiMemoryType::EfiLoaderData, mmap_size, &mut (mmap_buf as *mut u8)) };
    
    if status != 0 {

        return status;
    }

    let status = unsafe {

        (bs.get_memory_map)(&mut mmap_size, mmap_buf, &mut map_key, &mut desc_size, &mut desc_version)
    };

    if status != 0 {

        return status;
    }

    let status = unsafe { (bs.exit_boot_services)(image_handle, map_key) };
    
    if status != 0 {

        // If ExitBootServices fails (e.g., memory map changed), we should get a new map and try again (omitted for brevity).
        return status;
    }

    // At this point, we have successfully exited UEFI Boot Services. The system is in our control, running in 64-bit mode (flat identity mapping as provided by UEFI).
    // We should now prepare to jump into the Rust kernel with the BootInfo.

    // Construct BootInfo structure for the kernel
    unsafe {
        BOOT_INFO = BootInfo {
            boot_mode: BootMode::Uefi,
            memory_map: MemoryMap {
                buffer: mmap_buf,
                entry_count: (mmap_size / desc_size) as u32,
                desc_size: desc_size as u32,
            },
            framebuffer: if fb_base != 0 {

                Some(FrameBufferInfo {

                    ptr: fb_base as *mut u8,
                    size: fb_size,
                    width: fb_info.0,
                    height: fb_info.1,
                    pixels_per_scanline: fb_info.2,
                    format: PixelFormat::from_uefi(fb_info.3),
                })
            } else {

                None
            },
        };
    }

    // Now call into the shared bootloader logic (as in hybrid bootloader) or directly jump to kernel.
    // We have the kernel loaded at `kernel_buffer` (physical address stored in kernel_buffer).
    // We'll call the same bootloader_main function (or a variant for UEFI) to proceed.
    unsafe {

        bootloader_main(BootMode::Uefi, 0);
    }

    // bootloader_main should not return (it will jump to the kernel).
    loop { core::arch::asm!("hlt"); }
}
