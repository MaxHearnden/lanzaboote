//! This module implements the protocols to hand an initrd to the
//! Linux kernel.
//!
//! XXX The initrd signature validation is vulnerable to TOCTOU,
//! because we read the initrd multiple times. The code needs to be
//! restructured to solve this.

use core::{ffi::c_void, ops::Range, pin::Pin, ptr::slice_from_raw_parts_mut};

use alloc::{boxed::Box, vec::Vec};
use uefi::{
    prelude::BootServices,
    proto::{
        device_path::{DevicePath, FfiDevicePath},
        Protocol,
    },
    table::boot::LoadImageSource,
    unsafe_guid, Handle, Identify, Result, ResultExt, Status,
};

/// The Linux kernel's initrd loading device path.
///
/// The Linux kernel points us to
/// [u-boot](https://github.com/u-boot/u-boot/commit/ec80b4735a593961fe701cc3a5d717d4739b0fd0#diff-1f940face4d1cf74f9d2324952759404d01ee0a81612b68afdcba6b49803bdbbR28)
/// for documentation.
// XXX This should actually be something like:
// static const struct {
// 	struct efi_vendor_dev_path	vendor;
// 	struct efi_generic_dev_path	end;
// } __packed initrd_dev_path = {
// 	{
// 		{
// 			EFI_DEV_MEDIA,
// 			EFI_DEV_MEDIA_VENDOR,
// 			sizeof(struct efi_vendor_dev_path),
// 		},
// 		LINUX_EFI_INITRD_MEDIA_GUID
// 	}, {
// 		EFI_DEV_END_PATH,
// 		EFI_DEV_END_ENTIRE,
// 		sizeof(struct efi_generic_dev_path)
// 	}
// };
static mut DEVICE_PATH_PROTOCOL: [u8; 24] = [
    0x04, 0x03, 0x14, 0x00, 0x27, 0xe4, 0x68, 0x55, 0xfc, 0x68, 0x3d, 0x4f, 0xac, 0x74, 0xca, 0x55,
    0x52, 0x31, 0xcc, 0x68, 0x7f, 0xff, 0x04, 0x00,
];

/// The UEFI LoadFile2 protocol.
///
/// This protocol has a single method to load a file.
#[repr(C)]
#[unsafe_guid("4006c0c1-fcb3-403e-996d-4a6c8724e06d")]
#[derive(Protocol)]
struct LoadFile2Protocol {
    load_file: unsafe extern "efiapi" fn(
        this: &mut LoadFile2Protocol,
        file_path: *const FfiDevicePath,
        boot_policy: bool,
        buffer_size: *mut usize,
        buffer: *mut c_void,
    ) -> Status,

    // This is not part of the official protocol struct.
    initrd_data: Vec<u8>,
}

impl LoadFile2Protocol {
    fn load_file(
        &mut self,
        _file_path: *const FfiDevicePath,
        _boot_policy: bool,
        buffer_size: *mut usize,
        buffer: *mut c_void,
    ) -> Result<()> {
        if buffer.is_null() || unsafe { *buffer_size } < self.initrd_data.len() {
            unsafe {
                *buffer_size = self.initrd_data.len();
            }
            return Err(Status::BUFFER_TOO_SMALL.into());
        };

        unsafe {
            *buffer_size = self.initrd_data.len();
        }

        let output_slice: &mut [u8] =
            unsafe { &mut *slice_from_raw_parts_mut(buffer as *mut u8, *buffer_size) };

        output_slice.copy_from_slice(&self.initrd_data);

        Ok(())
    }
}

unsafe extern "efiapi" fn raw_load_file(
    this: &mut LoadFile2Protocol,
    file_path: *const FfiDevicePath,
    boot_policy: bool,
    buffer_size: *mut usize,
    buffer: *mut c_void,
) -> Status {
    this.load_file(file_path, boot_policy, buffer_size, buffer)
        .status()
}

/// A RAII wrapper to install and uninstall the Linux initrd loading
/// protocol.
///
/// **Note:** You need to call [`InitrdLoader::uninstall`], before
/// this is dropped.
pub struct InitrdLoader {
    proto: Pin<Box<LoadFile2Protocol>>,
    handle: Handle,
    registered: bool,
}

/// Returns the data range of the initrd in the PE binary.
///
/// The initrd has to be embedded in the file as a .initrd PE section.
fn initrd_location(initrd_efi: &[u8]) -> Result<Range<usize>> {
    let pe_binary = goblin::pe::PE::parse(initrd_efi).map_err(|_| Status::INVALID_PARAMETER)?;

    pe_binary
        .sections
        .iter()
        .find(|s| s.name().unwrap() == ".initrd")
        .map(|s| {
            let section_start: usize = s.pointer_to_raw_data.try_into().unwrap();
            let section_size: usize = s.size_of_raw_data.try_into().unwrap();

            Range {
                start: section_start,
                end: section_start + section_size,
            }
        })
        .ok_or_else(|| Status::END_OF_FILE.into())
}

/// Check the signature of the initrd.
///
/// For this to work, the initrd needs to be a PE binary. We misuse
/// [`BootServices::load_image`] for this.
fn initrd_verify(boot_services: &BootServices, initrd_efi: &[u8]) -> Result<()> {
    let initrd_handle = boot_services.load_image(
        boot_services.image_handle(),
        LoadImageSource::FromBuffer {
            buffer: &initrd_efi,
            file_path: None,
        },
    )?;

    // If we get here, the security policy allowed loading the
    // image. This means that it was signed with an acceptable key in
    // the Secure Boot scenario.

    boot_services.unload_image(initrd_handle)?;

    Ok(())
}

impl InitrdLoader {
    /// Create a new [`InitrdLoader`].
    ///
    /// `handle` is the handle where the protocols are registered
    /// on. `file` is the file that is served to Linux.
    pub fn new(
        boot_services: &BootServices,
        handle: Handle,
        mut initrd_data: Vec<u8>,
    ) -> Result<Self> {
        initrd_verify(boot_services, &initrd_data)?;

        let range = initrd_location(&initrd_data)?;

        // Remove the PE wrapper from the initrd. We do this in place
        // to avoid having to keep the initrd in memory twice.
        initrd_data.drain(0..range.start);
        initrd_data.resize(range.end - range.start, 0);
        initrd_data.shrink_to_fit();

        let mut proto = Box::pin(LoadFile2Protocol {
            load_file: raw_load_file,
            initrd_data,
        });

        // Linux finds the right handle by looking for something that
        // implements the device path protocol for the specific device
        // path.
        unsafe {
            let dp_proto: *mut u8 = DEVICE_PATH_PROTOCOL.as_mut_ptr();

            boot_services.install_protocol_interface(
                Some(handle),
                &DevicePath::GUID,
                dp_proto as *mut c_void,
            )?;

            let lf_proto: *mut LoadFile2Protocol = proto.as_mut().get_mut();

            boot_services.install_protocol_interface(
                Some(handle),
                &LoadFile2Protocol::GUID,
                lf_proto as *mut c_void,
            )?;
        }

        Ok(InitrdLoader {
            handle,
            proto,
            registered: true,
        })
    }

    pub fn uninstall(&mut self, boot_services: &BootServices) -> Result<()> {
        // This should only be called once.
        assert!(self.registered);

        unsafe {
            let dp_proto: *mut u8 = &mut DEVICE_PATH_PROTOCOL[0];
            boot_services.uninstall_protocol_interface(
                self.handle,
                &DevicePath::GUID,
                dp_proto as *mut c_void,
            )?;

            let lf_proto: *mut LoadFile2Protocol = self.proto.as_mut().get_mut();

            boot_services.uninstall_protocol_interface(
                self.handle,
                &LoadFile2Protocol::GUID,
                lf_proto as *mut c_void,
            )?;
        }

        self.registered = false;

        Ok(())
    }
}

impl Drop for InitrdLoader {
    fn drop(&mut self) {
        // Dropped without unregistering!
        assert!(!self.registered);
    }
}
