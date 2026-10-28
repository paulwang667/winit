//! How much of a window the soft keyboard covers, from the keyboard's
//! frame in `UIKeyboardWillChangeFrameNotification`.

/// `(x, y, width, height)`, one coordinate space for both rects.
pub(crate) type Rect = (f64, f64, f64, f64);

/// The height of `window`'s bottom part that `keyboard` covers: zero for a
/// hidden (off-screen or empty) keyboard or one beside the window, at most
/// the window's height.
pub(crate) fn keyboard_overlap(window: Rect, keyboard: Rect) -> f64 {
    let (wx, wy, ww, wh) = window;
    let (kx, ky, kw, kh) = keyboard;
    let beside = kx >= wx + ww || kx + kw <= wx;
    if kw <= 0.0 || kh <= 0.0 || beside {
        return 0.0;
    }
    let top = ky.max(wy);
    (wy + wh - top).clamp(0.0, wh)
}

#[cfg(test)]
mod tests {
    use super::*;

    const WINDOW: Rect = (0.0, 0.0, 402.0, 874.0);

    #[test]
    fn keyboard_overlap_counts_only_the_part_over_the_window() {
        assert_eq!(keyboard_overlap(WINDOW, (0.0, 566.0, 402.0, 308.0)), 308.0);
        // A keyboard taller than the window clamps to the window.
        assert_eq!(keyboard_overlap(WINDOW, (0.0, -10.0, 402.0, 2000.0)), 874.0);
        // A window above the screen's bottom edge only loses what it covers.
        assert_eq!(keyboard_overlap((0.0, 0.0, 402.0, 700.0), (0.0, 566.0, 402.0, 308.0)), 134.0);
    }

    #[test]
    fn keyboard_overlap_is_zero_when_hidden_or_offscreen() {
        assert_eq!(keyboard_overlap(WINDOW, (0.0, 874.0, 402.0, 308.0)), 0.0, "below the screen");
        assert_eq!(keyboard_overlap(WINDOW, (0.0, 566.0, 0.0, 0.0)), 0.0, "empty");
        assert_eq!(keyboard_overlap(WINDOW, (500.0, 566.0, 402.0, 308.0)), 0.0, "beside");
    }
}
