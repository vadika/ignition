// Minimal arm64 Linux `Image` loader. Parses the 64-byte image header and copies
// the image into guest RAM at the address the boot protocol requires.
//
// Header layout (Linux Documentation/arm64/booting.rst):
//   offset 8  : text_offset (LE u64)  load offset from the 2 MiB-aligned base
//   offset 16 : image_size  (LE u64)  effective image size (0 on old kernels)
//   offset 56 : magic       (LE u32)  = 0x644D5241 ("ARM\x64")

use std::fmt::{self, Display, Formatter};

const ARM64_IMAGE_MAGIC: u32 = 0x644D_5241;
const ARM64_HEADER_LEN: usize = 64;
/// Load offset assumed by kernels that report image_size == 0 (pre-3.17).
const LEGACY_TEXT_OFFSET: u64 = 0x8_0000;

/// Errors loading an arm64 Image.
#[derive(Debug, PartialEq, Eq)]
pub enum KernelError {
    /// Image shorter than the 64-byte arm64 header.
    TooShort,
    /// Header magic was not 0x644D5241 ("ARM\x64").
    BadMagic,
    /// Image does not fit in the supplied guest RAM at its load offset.
    DoesNotFit,
}

impl Display for KernelError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            KernelError::TooShort => write!(f, "kernel image shorter than the arm64 header"),
            KernelError::BadMagic => write!(f, "kernel image has a bad arm64 magic number"),
            KernelError::DoesNotFit => write!(f, "kernel image does not fit in guest RAM"),
        }
    }
}

impl std::error::Error for KernelError {}

/// The fields we use from the 64-byte arm64 Image header.
#[derive(Debug, PartialEq, Eq)]
pub struct Arm64Header {
    pub text_offset: u64,
    pub image_size: u64,
}

/// Parse the arm64 Image header from the start of `image`.
pub fn parse_arm64_header(image: &[u8]) -> Result<Arm64Header, KernelError> {
    if image.len() < ARM64_HEADER_LEN {
        return Err(KernelError::TooShort);
    }
    let magic = u32::from_le_bytes(image[56..60].try_into().unwrap());
    if magic != ARM64_IMAGE_MAGIC {
        return Err(KernelError::BadMagic);
    }
    let text_offset = u64::from_le_bytes(image[8..16].try_into().unwrap());
    let image_size = u64::from_le_bytes(image[16..24].try_into().unwrap());
    Ok(Arm64Header { text_offset, image_size })
}

/// Copy `image` into guest RAM (`ram` is the host mapping of the region based at
/// `ram_base`) at `ram_base + effective_offset`, where `effective_offset` is the
/// header's `text_offset`, or `LEGACY_TEXT_OFFSET` when `image_size == 0`.
/// Returns the guest entry address.
pub fn load_kernel(ram: &mut [u8], ram_base: u64, image: &[u8]) -> Result<u64, KernelError> {
    let header = parse_arm64_header(image)?;
    // text_offset == 0 is valid for modern kernels (load at the 2 MiB-aligned
    // base). image_size == 0 means a pre-3.17 kernel with the legacy offset.
    let offset_u64 = if header.image_size == 0 {
        LEGACY_TEXT_OFFSET
    } else {
        header.text_offset
    };
    // usize::try_from instead of `as usize` so a 64-bit offset can't silently
    // truncate on a hypothetical 32-bit host.
    let offset = usize::try_from(offset_u64).map_err(|_| KernelError::DoesNotFit)?;

    let end = offset.checked_add(image.len()).ok_or(KernelError::DoesNotFit)?;
    if end > ram.len() {
        return Err(KernelError::DoesNotFit);
    }
    ram[offset..end].copy_from_slice(image);
    // Only image.len() bytes are copied; any image_size > image.len() delta
    // (e.g. BSS) is satisfied by pre-zeroed guest RAM.
    ram_base.checked_add(offset_u64).ok_or(KernelError::DoesNotFit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic 64-byte arm64 header with the given fields.
    fn header(text_offset: u64, image_size: u64) -> Vec<u8> {
        let mut h = vec![0u8; ARM64_HEADER_LEN];
        h[8..16].copy_from_slice(&text_offset.to_le_bytes());
        h[16..24].copy_from_slice(&image_size.to_le_bytes());
        h[56..60].copy_from_slice(&ARM64_IMAGE_MAGIC.to_le_bytes());
        h
    }

    #[test]
    fn kernel_error_is_std_error() {
        fn assert_std_error<E: std::error::Error>() {}
        assert_std_error::<KernelError>();
    }

    #[test]
    fn parse_reads_text_offset_and_image_size() {
        let h = header(0, 0x1000);
        assert_eq!(
            parse_arm64_header(&h),
            Ok(Arm64Header { text_offset: 0, image_size: 0x1000 })
        );
    }

    #[test]
    fn parse_rejects_short_image() {
        assert_eq!(parse_arm64_header(&[0u8; 40]), Err(KernelError::TooShort));
    }

    #[test]
    fn parse_rejects_bad_magic() {
        let mut h = header(0, 0x1000);
        h[56] ^= 0xff; // corrupt the magic
        assert_eq!(parse_arm64_header(&h), Err(KernelError::BadMagic));
    }

    #[test]
    fn load_modern_kernel_at_base() {
        let mut image = header(0, 0x2000);
        image.extend_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]);
        let mut ram = vec![0u8; 0x10_0000];
        let entry = load_kernel(&mut ram, 0x4000_0000, &image).unwrap();
        assert_eq!(entry, 0x4000_0000);
        assert_eq!(&ram[..image.len()], image.as_slice());
    }

    #[test]
    fn load_legacy_kernel_at_0x80000() {
        let image = header(0, 0); // image_size == 0 -> legacy offset
        let mut ram = vec![0u8; 0x10_0000];
        let entry = load_kernel(&mut ram, 0x4000_0000, &image).unwrap();
        assert_eq!(entry, 0x4000_0000 + 0x8_0000);
        assert_eq!(&ram[0x8_0000..0x8_0000 + image.len()], image.as_slice());
    }

    #[test]
    fn load_rejects_oversized_image() {
        let mut image = header(0, 0x100); // image_size != 0 -> offset 0
        image.resize(ARM64_HEADER_LEN + 64, 0); // 128 bytes total
        let mut ram = vec![0u8; 64]; // smaller than the image
        assert_eq!(
            load_kernel(&mut ram, 0x4000_0000, &image),
            Err(KernelError::DoesNotFit)
        );
        assert!(ram.iter().all(|&b| b == 0), "ram must be unchanged on failure");
    }

    #[test]
    fn load_propagates_bad_magic() {
        let mut image = header(0, 0x100);
        image[56] ^= 0xff;
        let mut ram = vec![0u8; 0x1000];
        assert_eq!(
            load_kernel(&mut ram, 0x4000_0000, &image),
            Err(KernelError::BadMagic)
        );
    }
}
