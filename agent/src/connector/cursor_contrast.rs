//! A pointer picture that can be seen on any background.
//!
//! Windows' text cursor (the I-beam) has no colour of its own: it inverts
//! what is under it. A picture of it cannot do that, so a host has to choose
//! colours. RustDesk's host (src/platform/windows.rs `get_cursor_data`, with
//! `handleMask` and `fix_cursor_mask` after TigerVNC) turns such pixels into
//! one colour and may add an outline; what reaches a viewer of a Windows 11
//! computer is an I-beam that is all but white, which vanishes over a white
//! text field, the very place it is used.
//!
//! What is done here is Lunaris's own, not taken from a protocol: a picture
//! whose solid pixels are nearly all light gets a dark outline, one pixel
//! wide, the way libwebrtc's desktop capture gives an inverting cursor an
//! outline of the other colour (modules/desktop_capture/win/cursor.cc
//! `AddCursorOutline`: every transparent pixel next to a cursor pixel is
//! painted). A picture that already has dark pixels (the arrow with its
//! black edge, a coloured cursor) is left as it is.

/// A pixel counts as part of the pointer from this opacity on.
const SOLID_ALPHA: u8 = 128;
/// Luma (0 to 255) below which a solid pixel is dark.
const DARK_LUMA: u32 = 100;
/// A pointer with fewer dark pixels than this share of its solid ones has
/// no edge to speak of.
const DARK_SHARE_PERCENT: usize = 12;

fn luma(pixel: &[u8]) -> u32 {
    // Rec. 601, integer.
    (pixel[0] as u32 * 299 + pixel[1] as u32 * 587 + pixel[2] as u32 * 114) / 1000
}

/// Whether the picture needs an outline to be seen on a light background.
fn lacks_dark_edge(rgba: &[u8]) -> bool {
    let (mut solid, mut dark) = (0usize, 0usize);
    for pixel in rgba.chunks_exact(4) {
        if pixel[3] >= SOLID_ALPHA {
            solid += 1;
            if luma(pixel) < DARK_LUMA {
                dark += 1;
            }
        }
    }
    solid > 0 && dark * 100 < solid * DARK_SHARE_PERCENT
}

/// The picture with a dark outline when it has none: one pixel larger on
/// each side (so that the outline of a pointer that touches the edge fits),
/// the hot spot moved with it. `None` when the picture is fine as it is, or
/// is not what its size says.
pub fn with_dark_outline(width: u32, height: u32, hot_x: u32, hot_y: u32, rgba: &[u8]) -> Option<(u32, u32, u32, u32, Vec<u8>)> {
    let (w, h) = (width as usize, height as usize);
    if w == 0 || h == 0 || rgba.len() != w.checked_mul(h)?.checked_mul(4)? || !lacks_dark_edge(rgba) {
        return None;
    }
    let (ow, oh) = (w + 2, h + 2);
    let mut out = vec![0u8; ow * oh * 4];
    for y in 0..h {
        let from = y * w * 4;
        let to = ((y + 1) * ow + 1) * 4;
        out[to..to + w * 4].copy_from_slice(&rgba[from..from + w * 4]);
    }
    let solid = |x: isize, y: isize| -> bool {
        x >= 0 && y >= 0 && (x as usize) < w && (y as usize) < h && rgba[(y as usize * w + x as usize) * 4 + 3] >= SOLID_ALPHA
    };
    for oy in 0..oh as isize {
        for ox in 0..ow as isize {
            // The same pixel in the picture as it came.
            let (x, y) = (ox - 1, oy - 1);
            if solid(x, y) {
                continue;
            }
            let touches = (-1..=1).any(|dy| (-1..=1).any(|dx| (dx != 0 || dy != 0) && solid(x + dx, y + dy)));
            if touches {
                let at = (oy as usize * ow + ox as usize) * 4;
                out[at..at + 4].copy_from_slice(&[0, 0, 0, 255]);
            }
        }
    }
    Some((ow as u32, oh as u32, hot_x + 1, hot_y + 1, out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(w: usize, h: usize, paint: impl Fn(usize, usize) -> [u8; 4]) -> Vec<u8> {
        let mut out = Vec::with_capacity(w * h * 4);
        for y in 0..h {
            for x in 0..w {
                out.extend_from_slice(&paint(x, y));
            }
        }
        out
    }

    fn at(rgba: &[u8], w: u32, x: u32, y: u32) -> [u8; 4] {
        let i = ((y * w + x) * 4) as usize;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    #[test]
    fn a_white_text_cursor_gets_a_dark_outline() {
        // A white bar, one pixel wide, down the middle of 5 x 7.
        let rgba = picture(5, 7, |x, y| if x == 2 && (1..6).contains(&y) { [255, 255, 255, 255] } else { [0, 0, 0, 0] });
        let (w, h, hot_x, hot_y, out) = with_dark_outline(5, 7, 2, 3, &rgba).unwrap();
        assert_eq!((w, h, hot_x, hot_y), (7, 9, 3, 4));
        // The bar is where it was, one pixel further in.
        assert_eq!(at(&out, w, 3, 4), [255, 255, 255, 255]);
        // Black on both sides, above and below it, and at the corners.
        for (x, y) in [(2, 4), (4, 4), (3, 1), (3, 7), (2, 1), (4, 7)] {
            assert_eq!(at(&out, w, x, y), [0, 0, 0, 255], "({}, {})", x, y);
        }
        // Further away nothing is painted.
        assert_eq!(at(&out, w, 0, 4), [0, 0, 0, 0]);
        assert_eq!(at(&out, w, 6, 4), [0, 0, 0, 0]);
    }

    #[test]
    fn a_pointer_that_touches_the_edge_keeps_its_outline() {
        let rgba = picture(2, 2, |_, _| [250, 250, 250, 255]);
        let (w, h, _, _, out) = with_dark_outline(2, 2, 0, 0, &rgba).unwrap();
        assert_eq!((w, h), (4, 4));
        assert_eq!(at(&out, w, 0, 0), [0, 0, 0, 255]);
        assert_eq!(at(&out, w, 3, 3), [0, 0, 0, 255]);
        assert_eq!(at(&out, w, 1, 1), [250, 250, 250, 255]);
    }

    #[test]
    fn a_pointer_with_a_dark_edge_is_left_alone() {
        // White inside, black around: the usual arrow.
        let rgba = picture(6, 6, |x, y| {
            if (2..4).contains(&x) && (2..4).contains(&y) {
                [255, 255, 255, 255]
            } else if (1..5).contains(&x) && (1..5).contains(&y) {
                [0, 0, 0, 255]
            } else {
                [0, 0, 0, 0]
            }
        });
        assert!(with_dark_outline(6, 6, 1, 1, &rgba).is_none());
        // All black (a text cursor of a dark theme), or coloured and dark.
        let black = picture(3, 3, |x, _| if x == 1 { [0, 0, 0, 255] } else { [0, 0, 0, 0] });
        assert!(with_dark_outline(3, 3, 1, 1, &black).is_none());
    }

    #[test]
    fn faint_grey_around_a_white_pointer_is_no_edge() {
        // Soft, see-through grey beside the white bar, as smoothing leaves it.
        let rgba = picture(5, 5, |x, _| match x {
            2 => [255, 255, 255, 255],
            1 | 3 => [60, 60, 60, 70],
            _ => [0, 0, 0, 0],
        });
        let (w, _, _, _, out) = with_dark_outline(5, 5, 2, 2, &rgba).unwrap();
        assert_eq!(at(&out, w, 2, 3), [0, 0, 0, 255]);
        assert_eq!(at(&out, w, 4, 3), [0, 0, 0, 255]);
    }

    #[test]
    fn what_is_not_a_picture_is_refused() {
        assert!(with_dark_outline(0, 4, 0, 0, &[]).is_none());
        assert!(with_dark_outline(2, 2, 0, 0, &[255; 15]).is_none());
        // Nothing solid in it: nothing to outline.
        assert!(with_dark_outline(2, 2, 0, 0, &[0; 16]).is_none());
    }
}
