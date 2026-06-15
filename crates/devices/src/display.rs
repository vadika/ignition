//! Host display seam. Mirrors the `IrqLine`/`NoopIrq` pattern: a trait the device
//! crate owns, a no-op default for the manager/tests, and a real implementation
//! supplied by the binary. The virtio-gpu device (a later milestone) will hold a
//! `Box<dyn DisplaySink>` and call `present` on FLUSH.

use std::sync::{Arc, Mutex};

/// A rectangle of the scanout that changed, in pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DirtyRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// One presentable frame: a handle to the scanout's host pixel buffer plus the
/// geometry needed to blit it. `pixels` is shared (not copied) so a FLUSH hands
/// over a handle rather than a memcpy. Pixel format is fixed B8G8R8A8 for v1.
#[derive(Clone, Debug)]
pub struct Frame {
    pub scanout_id: u32,
    pub width: u32,
    pub height: u32,
    /// Bytes per row of `pixels`.
    pub stride: u32,
    pub dirty: DirtyRect,
    pub pixels: Arc<Mutex<Vec<u8>>>,
}

/// Host display sink. Implementations must be `Send + Sync`, and `present` must be
/// non-blocking — drop or coalesce frames rather than block a vCPU thread.
pub trait DisplaySink: Send + Sync {
    fn present(&self, frame: Frame);
}

/// A `DisplaySink` that discards every frame — for the manager and tests, and for
/// the default headless (no `--gui`) path.
pub struct NoopSink;

impl DisplaySink for NoopSink {
    fn present(&self, _frame: Frame) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn dummy_frame() -> Frame {
        Frame {
            scanout_id: 0,
            width: 4,
            height: 2,
            stride: 16,
            dirty: DirtyRect { x: 0, y: 0, w: 4, h: 2 },
            pixels: Arc::new(Mutex::new(vec![0u8; 4 * 2 * 4])),
        }
    }

    #[test]
    fn noop_sink_discards_without_panic() {
        let sink = NoopSink;
        sink.present(dummy_frame());
    }

    #[test]
    fn frame_clone_shares_pixel_arc() {
        let f = dummy_frame();
        let before = Arc::strong_count(&f.pixels);
        let g = f.clone();
        assert_eq!(Arc::strong_count(&f.pixels), before + 1);
        assert_eq!(g.width, 4);
    }

    #[test]
    fn dirty_rect_equality() {
        assert_eq!(
            DirtyRect { x: 1, y: 2, w: 3, h: 4 },
            DirtyRect { x: 1, y: 2, w: 3, h: 4 }
        );
        assert_ne!(
            DirtyRect { x: 0, y: 0, w: 1, h: 1 },
            DirtyRect { x: 0, y: 0, w: 2, h: 1 }
        );
    }
}
