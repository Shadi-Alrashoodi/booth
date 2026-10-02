// The pointer between the two sides. Its shape is sent only when it
// changes, so a newer position must never lose a shape that was not drawn
// yet: not while it waits in the inbox, and not while it waits for the next
// present.

use capture::CursorUpdate;
use viewer::{Cursor, CursorKind, CursorShape};

// `update` replaces `waiting`, which the viewer has not taken yet; a new
// position without a shape keeps the waiting one's.
pub(crate) fn newer(waiting: Option<CursorUpdate>, mut update: CursorUpdate) -> CursorUpdate {
    if update.shape.is_none()
        && let Some(waiting) = waiting
    {
        update.shape = waiting.shape;
    }
    update
}

// capture's pointer as the viewer takes it; the two crates do not know each
// other. A position without a shape keeps the shape not yet drawn.
pub(crate) fn pointer(update: CursorUpdate, waiting: Option<Cursor>) -> Cursor {
    let shape = update.shape.map(|shape| CursorShape {
        kind: match shape.kind {
            capture::CursorKind::Monochrome => CursorKind::Monochrome,
            capture::CursorKind::Color => CursorKind::Color,
            capture::CursorKind::MaskedColor => CursorKind::MaskedColor,
        },
        width: shape.width,
        height: shape.height,
        pitch: shape.pitch,
        hotspot_x: shape.hotspot_x,
        hotspot_y: shape.hotspot_y,
        bytes: shape.bytes,
    });
    Cursor {
        x: update.x,
        y: update.y,
        visible: update.visible,
        scale: update.scale,
        shape: shape.or_else(|| waiting.and_then(|cursor| cursor.shape)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(x: i32, kind: Option<capture::CursorKind>) -> CursorUpdate {
        CursorUpdate {
            x,
            y: 5,
            visible: true,
            scale: 0.5,
            shape: kind.map(|kind| capture::CursorShape {
                kind,
                width: 2,
                height: 4,
                pitch: 1,
                hotspot_x: 1,
                hotspot_y: 2,
                bytes: vec![0xf0; 4],
            }),
        }
    }

    #[test]
    fn the_pointer_crosses_field_for_field() {
        let cursor = pointer(update(3, Some(capture::CursorKind::Monochrome)), None);
        assert_eq!(
            (cursor.x, cursor.y, cursor.visible, cursor.scale),
            (3, 5, true, 0.5)
        );
        let shape = cursor.shape.expect("a shape");
        assert_eq!(shape.kind, CursorKind::Monochrome);
        assert_eq!((shape.width, shape.height, shape.pitch), (2, 4, 1));
        assert_eq!((shape.hotspot_x, shape.hotspot_y), (1, 2));
        assert_eq!(shape.bytes, [0xf0; 4]);
        let masked = pointer(update(0, Some(capture::CursorKind::MaskedColor)), None);
        assert_eq!(masked.shape.map(|s| s.kind), Some(CursorKind::MaskedColor));
    }

    // A shape is sent once; a move before it was drawn must not lose it.
    #[test]
    fn a_move_keeps_a_shape_not_drawn_yet() {
        let first = pointer(update(1, Some(capture::CursorKind::Color)), None);
        let moved = pointer(update(9, None), Some(first));
        assert_eq!(moved.x, 9);
        assert_eq!(moved.shape.map(|s| s.kind), Some(CursorKind::Color));
        let drawn = Cursor {
            shape: None,
            ..moved
        };
        assert!(pointer(update(10, None), Some(drawn)).shape.is_none());
    }

    #[test]
    fn a_newer_position_keeps_a_shape_not_taken_yet() {
        let first = update(1, Some(capture::CursorKind::Color));
        let kept = newer(Some(first.clone()), update(2, None));
        assert_eq!((kept.x, &kept.shape), (2, &first.shape));
        // A new shape wins over the waiting one.
        let masked = newer(
            Some(first),
            update(3, Some(capture::CursorKind::MaskedColor)),
        );
        assert_eq!(
            masked.shape.map(|s| s.kind),
            Some(capture::CursorKind::MaskedColor)
        );
        assert!(newer(None, update(4, None)).shape.is_none());
    }
}
