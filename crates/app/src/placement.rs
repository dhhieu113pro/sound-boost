//! Where the flyout panel goes.
//!
//! The shell tells us where the user clicked (the icon's anchor point) and the
//! monitor tells us its work area, the screen minus the taskbar. The panel goes
//! beside the taskbar, as close to the icon as fits, and never off-screen. The
//! taskbar can be on any edge, and the icon can be in the overflow flyout, where
//! the anchor sits inside the work area -- this handles all of those without
//! having to ask where the taskbar is.

/// A screen rectangle, right and bottom exclusive (the Win32 convention).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

/// Top-left corner for a `width` x `height` panel.
pub fn panel_origin(
    anchor: (i32, i32),
    work: Rect,
    width: i32,
    height: i32,
    margin: i32,
) -> (i32, i32) {
    let (x, y) = anchor;

    // Lowest and highest legal positions. If the panel is bigger than the work
    // area, pin it to the top-left so at least the title is reachable.
    let x_min = work.left + margin;
    let y_min = work.top + margin;
    let x_max = (work.right - width - margin).max(x_min);
    let y_max = (work.bottom - height - margin).max(y_min);

    let left = if x >= work.right {
        x_max // taskbar on the right
    } else if x < work.left {
        x_min // taskbar on the left
    } else {
        (x - width / 2).clamp(x_min, x_max)
    };

    let top = if y >= work.bottom {
        y_max // taskbar at the bottom, the usual case
    } else if y < work.top {
        y_min // taskbar at the top
    } else {
        // Side taskbar or overflow flyout: open upwards from the click if
        // there is room, as the shell's own flyouts do.
        (y - height).clamp(y_min, y_max)
    };

    (left, top)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN_WITH_BOTTOM_BAR: Rect = Rect {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1032,
    };

    #[test]
    fn bottom_taskbar_puts_the_panel_just_above_the_icon() {
        let (left, top) = panel_origin((1700, 1050), SCREEN_WITH_BOTTOM_BAR, 300, 260, 12);
        assert_eq!(top, 1032 - 260 - 12);
        assert_eq!(left, 1700 - 150);
    }

    #[test]
    fn an_icon_near_the_corner_keeps_the_panel_on_screen() {
        let (left, _) = panel_origin((1910, 1050), SCREEN_WITH_BOTTOM_BAR, 300, 260, 12);
        assert_eq!(left, 1920 - 300 - 12);
    }

    #[test]
    fn top_and_side_taskbars() {
        let top_bar = Rect {
            left: 0,
            top: 48,
            right: 1920,
            bottom: 1080,
        };
        assert_eq!(panel_origin((1800, 20), top_bar, 300, 260, 12).1, 60);

        let left_bar = Rect {
            left: 64,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let (left, top) = panel_origin((30, 900), left_bar, 300, 260, 12);
        assert_eq!(left, 76);
        assert_eq!(top, 900 - 260);

        let right_bar = Rect {
            left: 0,
            top: 0,
            right: 1856,
            bottom: 1080,
        };
        assert_eq!(
            panel_origin((1880, 900), right_bar, 300, 260, 12).0,
            1856 - 312
        );
    }

    #[test]
    fn second_monitor_to_the_left_has_negative_coordinates() {
        let left_monitor = Rect {
            left: -1920,
            top: 0,
            right: 0,
            bottom: 1032,
        };
        let (left, top) = panel_origin((-200, 1050), left_monitor, 300, 260, 12);
        assert_eq!((left, top), (-350, 1032 - 272));
    }

    #[test]
    fn a_panel_bigger_than_the_screen_pins_to_the_top_left() {
        let tiny = Rect {
            left: 0,
            top: 0,
            right: 200,
            bottom: 150,
        };
        assert_eq!(panel_origin((100, 200), tiny, 300, 260, 12), (12, 12));
    }
}
