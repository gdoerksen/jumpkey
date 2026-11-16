//! Normalized representation of on-screen interactable elements.

/// Stable, opaque identifier for an [`Element`] within a single enumeration
/// pass. Backends are free to choose any encoding; consumers must not assume
/// any structure.
///
/// IDs are not guaranteed to remain valid across enumerations — call
/// [`crate::Backend::enumerate_foreground`] again to refresh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ElementId(pub u64);

/// Pixel-space rectangle in physical (DPI-aware) coordinates relative to the
/// virtual desktop origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bounds {
    /// Left edge, in physical pixels.
    pub x: i32,
    /// Top edge, in physical pixels.
    pub y: i32,
    /// Width in physical pixels.
    pub width: i32,
    /// Height in physical pixels.
    pub height: i32,
}

impl Bounds {
    /// Returns the geometric center of the rectangle.
    pub fn center(&self) -> (i32, i32) {
        (self.x + self.width / 2, self.y + self.height / 2)
    }

    /// Returns true when the rectangle has positive area.
    pub fn is_visible(&self) -> bool {
        self.width > 0 && self.height > 0
    }

    /// Exclusive right edge.
    fn right(&self) -> i32 {
        self.x + self.width
    }

    /// Exclusive bottom edge.
    fn bottom(&self) -> i32 {
        self.y + self.height
    }

    /// True when the two rectangles share at least one pixel of area.
    /// Rectangles that merely touch along an edge do not intersect.
    pub fn intersects(&self, other: &Bounds) -> bool {
        self.is_visible()
            && other.is_visible()
            && self.x < other.right()
            && other.x < self.right()
            && self.y < other.bottom()
            && other.y < self.bottom()
    }

    /// True when `inner` lies entirely within `self` (edges may coincide).
    pub fn contains(&self, inner: &Bounds) -> bool {
        inner.x >= self.x
            && inner.y >= self.y
            && inner.right() <= self.right()
            && inner.bottom() <= self.bottom()
    }

    /// True when any part of `self` remains uncovered by the union of
    /// `occluders`.
    ///
    /// Works by repeatedly subtracting each occluder from a working set
    /// of rectangles (each subtraction splits a piece into at most four).
    /// Zero-area rectangles are never visible. If the working set grows
    /// past [`MAX_VISIBILITY_FRAGMENTS`] the answer fails safe to `true`
    /// — a pathological layout should cost a spurious hint, not time or
    /// a hidden target.
    pub fn is_partly_visible(&self, occluders: &[Bounds]) -> bool {
        if !self.is_visible() {
            return false;
        }
        let mut pieces: Vec<Bounds> = vec![*self];
        for occ in occluders {
            if !occ.is_visible() {
                continue;
            }
            let mut next = Vec::with_capacity(pieces.len() + 3);
            for piece in &pieces {
                piece.subtract_into(occ, &mut next);
            }
            if next.is_empty() {
                return false;
            }
            if next.len() > MAX_VISIBILITY_FRAGMENTS {
                return true;
            }
            pieces = next;
        }
        true
    }

    /// Push the parts of `self` not covered by `cut` onto `out`
    /// (top band, bottom band, then left/right of the middle band).
    fn subtract_into(&self, cut: &Bounds, out: &mut Vec<Bounds>) {
        if !self.intersects(cut) {
            out.push(*self);
            return;
        }
        let (l, t, r, b) = (self.x, self.y, self.right(), self.bottom());
        let mid_top = t.max(cut.y);
        let mid_bottom = b.min(cut.bottom());
        if cut.y > t {
            out.push(Bounds::from_edges(l, t, r, cut.y));
        }
        if cut.bottom() < b {
            out.push(Bounds::from_edges(l, cut.bottom(), r, b));
        }
        if cut.x > l {
            out.push(Bounds::from_edges(l, mid_top, cut.x, mid_bottom));
        }
        if cut.right() < r {
            out.push(Bounds::from_edges(cut.right(), mid_top, r, mid_bottom));
        }
    }

    fn from_edges(left: i32, top: i32, right: i32, bottom: i32) -> Bounds {
        Bounds {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        }
    }
}

/// Cap on the working set in [`Bounds::is_partly_visible`].
const MAX_VISIBILITY_FRAGMENTS: usize = 64;

/// High-level semantic role of an element. Mirrors the most useful subset of
/// the UI Automation / AT-SPI / ARIA role taxonomies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Role {
    /// A clickable button.
    Button,
    /// A hyperlink.
    Link,
    /// A single-line or multi-line text input.
    TextInput,
    /// An item inside a menu.
    MenuItem,
    /// A tab in a tab strip.
    Tab,
    /// A checkbox.
    Checkbox,
    /// A radio option.
    Radio,
    /// A combo / drop-down.
    ComboBox,
    /// A list item.
    ListItem,
    /// A tree item.
    TreeItem,
    /// Anything else that exposes an invoke pattern.
    Other,
}

/// A single interactable thing discovered on screen.
#[derive(Debug, Clone)]
pub struct Element {
    /// Backend-assigned identifier.
    pub id: ElementId,
    /// Semantic role.
    pub role: Role,
    /// Accessible name, when available.
    pub name: Option<String>,
    /// On-screen bounding box.
    pub bounds: Bounds,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(x: i32, y: i32, width: i32, height: i32) -> Bounds {
        Bounds {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn intersects_excludes_touching_edges() {
        assert!(b(0, 0, 10, 10).intersects(&b(5, 5, 10, 10)));
        assert!(!b(0, 0, 10, 10).intersects(&b(10, 0, 10, 10)));
        assert!(!b(0, 0, 10, 10).intersects(&b(0, 0, 0, 10)));
    }

    #[test]
    fn contains_allows_coincident_edges() {
        assert!(b(0, 0, 10, 10).contains(&b(0, 0, 10, 10)));
        assert!(b(0, 0, 10, 10).contains(&b(2, 2, 3, 3)));
        assert!(!b(0, 0, 10, 10).contains(&b(5, 5, 10, 10)));
    }

    #[test]
    fn no_occluders_is_visible() {
        assert!(b(0, 0, 10, 10).is_partly_visible(&[]));
    }

    #[test]
    fn zero_size_is_never_visible() {
        assert!(!b(0, 0, 0, 10).is_partly_visible(&[]));
        assert!(!b(0, 0, 10, 0).is_partly_visible(&[]));
    }

    #[test]
    fn fully_covered_is_hidden() {
        assert!(!b(10, 10, 20, 20).is_partly_visible(&[b(0, 0, 100, 100)]));
        assert!(!b(10, 10, 20, 20).is_partly_visible(&[b(10, 10, 20, 20)]));
    }

    #[test]
    fn partly_covered_is_visible() {
        // Occluder covers the left half only.
        assert!(b(0, 0, 20, 20).is_partly_visible(&[b(0, 0, 10, 20)]));
        // Occluder punches a hole in the middle; the ring stays visible.
        assert!(b(0, 0, 20, 20).is_partly_visible(&[b(5, 5, 10, 10)]));
        // Disjoint occluder.
        assert!(b(0, 0, 20, 20).is_partly_visible(&[b(50, 50, 10, 10)]));
    }

    #[test]
    fn covered_by_union_of_two_is_hidden() {
        let left = b(0, 0, 10, 20);
        let right = b(10, 0, 10, 20);
        let target = b(5, 5, 10, 10);
        assert!(target.is_partly_visible(&[left]));
        assert!(target.is_partly_visible(&[right]));
        assert!(!target.is_partly_visible(&[left, right]));
    }

    #[test]
    fn one_pixel_gap_is_visible() {
        let left = b(0, 0, 10, 20);
        let right = b(11, 0, 10, 20);
        assert!(b(5, 5, 10, 10).is_partly_visible(&[left, right]));
    }

    #[test]
    fn fragment_cap_fails_safe_to_visible() {
        // A grid of 1px "pinholes" shatters the target into many pieces
        // without ever fully covering it.
        let target = b(0, 0, 200, 200);
        let occluders: Vec<Bounds> = (0..40)
            .map(|i| b(5 + i * 4, 5 + (i % 7) * 20, 1, 1))
            .collect();
        assert!(target.is_partly_visible(&occluders));
    }
}
