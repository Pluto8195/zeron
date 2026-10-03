//! Pure rectangle-packing geometry for the multichat overview canvas —
//! ported from the reference `session_canvas.html`'s `packRects` (best-area-
//! fit free-rectangle packing, `/Users/mikey/Projects/agent-mode-tools/
//! session_canvas.html:1281`) and the two overlap-repair sweeps that sit on
//! top of it: one embedded inside `packRects` itself (repairs a single
//! group's local packing), and a separate global pass in
//! `organizeByDimensions` (~line 1562) that re-sweeps every group's items
//! together in absolute board-space, because a per-group check alone can't
//! catch a group's measured size drifting against another's, or a
//! background PR/ticket/category reclassification moving a session between
//! groups mid-computation.
//!
//! No `gpui` types here on purpose — this is pure geometry, unit-testable in
//! isolation and reusable from both the flat and grouped canvas layouts.

use std::collections::HashMap;

/// One rectangle to pack, keyed by an opaque id.
#[derive(Debug, Clone)]
pub struct PackItem {
    pub id: String,
    pub width: f32,
    pub height: f32,
}

/// A packed (or repaired) item's top-left position.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PackedPosition {
    pub x: f32,
    pub y: f32,
}

/// Result of [`pack_rects`]: every input item's position, plus the packed
/// bounding box (gap-trimmed, matching the reference's `{positions, width,
/// height}`).
#[derive(Debug, Clone)]
pub struct PackResult {
    pub positions: HashMap<String, PackedPosition>,
    pub width: f32,
    pub height: f32,
}

#[derive(Debug, Clone, Copy)]
struct FreeRect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

fn rects_overlap(ax: f32, ay: f32, aw: f32, ah: f32, bx: f32, by: f32, bw: f32, bh: f32) -> bool {
    ax < bx + bw && ax + aw > bx && ay < by + bh && ay + ah > by
}

/// Does free rect `a` fully contain free rect `b`?
fn rect_contains(a: FreeRect, b: FreeRect) -> bool {
    b.x >= a.x && b.y >= a.y && b.x + b.w <= a.x + a.w && b.y + b.h <= a.y + a.h
}

/// Best-area-fit free-rectangle packing, ported from `packRects`
/// (session_canvas.html:1281-1340): pads every item by `gap`, guillotine-
/// splits free space as items land, prefers the free rect that wastes the
/// least area (tie-broken by the smaller leftover short side) so a short
/// card lands in the gap next to a tall one rather than opening a new row.
/// Falls back to a fresh row at the bottom if every free rect is exhausted
/// (shouldn't normally happen — one free rect always spans the full target
/// width). Finishes with the same local overlap-repair sweep the reference
/// runs inside `packRects` itself — belt-and-suspenders against the rare
/// placement that still comes out overlapping at scale.
pub fn pack_rects(items: &[PackItem], gap: f32) -> PackResult {
    if items.is_empty() {
        return PackResult {
            positions: HashMap::new(),
            width: 0.0,
            height: 0.0,
        };
    }

    struct Padded {
        id: String,
        w: f32,
        h: f32,
    }
    let padded: Vec<Padded> = items
        .iter()
        .map(|it| Padded {
            id: it.id.clone(),
            w: it.width + gap,
            h: it.height + gap,
        })
        .collect();

    let total_area: f32 = padded.iter().map(|it| it.w * it.h).sum();
    let max_w = padded.iter().map(|it| it.w).fold(0.0f32, f32::max);
    let target_width = total_area.sqrt() * 1.15;
    let target_width = target_width.max(max_w);

    let mut free_rects: Vec<FreeRect> = vec![FreeRect {
        x: 0.0,
        y: 0.0,
        w: target_width,
        h: f32::INFINITY,
    }];
    let mut positions: HashMap<String, PackedPosition> = HashMap::new();
    let mut max_x = 0.0f32;
    let mut max_y = 0.0f32;

    let mut sorted: Vec<&Padded> = padded.iter().collect();
    sorted.sort_by(|a, b| (b.w * b.h).partial_cmp(&(a.w * a.h)).unwrap());

    for item in sorted {
        let mut best: Option<FreeRect> = None;
        let mut best_leftover = f32::INFINITY;
        let mut best_short_side = f32::INFINITY;
        for &fr in &free_rects {
            if fr.w < item.w || fr.h < item.h {
                continue;
            }
            let leftover = fr.w * fr.h - item.w * item.h;
            let short_side = (fr.w - item.w).min(fr.h - item.h);
            if leftover < best_leftover
                || (leftover == best_leftover && short_side < best_short_side)
            {
                best = Some(fr);
                best_leftover = leftover;
                best_short_side = short_side;
            }
        }
        // Every free rect exhausted (shouldn't normally happen since one
        // always spans the full target width) — open a fresh row at the
        // bottom, matching the reference's fallback exactly.
        let best = best.unwrap_or(FreeRect {
            x: 0.0,
            y: max_y,
            w: target_width,
            h: f32::INFINITY,
        });

        let placed = FreeRect {
            x: best.x,
            y: best.y,
            w: item.w,
            h: item.h,
        };
        positions.insert(
            item.id.clone(),
            PackedPosition {
                x: placed.x,
                y: placed.y,
            },
        );
        max_x = max_x.max(placed.x + item.w - gap);
        max_y = max_y.max(placed.y + item.h - gap);

        let mut next: Vec<FreeRect> = Vec::new();
        for &fr in &free_rects {
            if !rects_overlap(
                fr.x, fr.y, fr.w, fr.h, placed.x, placed.y, placed.w, placed.h,
            ) {
                next.push(fr);
                continue;
            }
            if placed.x > fr.x {
                next.push(FreeRect {
                    x: fr.x,
                    y: fr.y,
                    w: placed.x - fr.x,
                    h: fr.h,
                });
            }
            if placed.x + placed.w < fr.x + fr.w {
                next.push(FreeRect {
                    x: placed.x + placed.w,
                    y: fr.y,
                    w: fr.x + fr.w - (placed.x + placed.w),
                    h: fr.h,
                });
            }
            if placed.y > fr.y {
                next.push(FreeRect {
                    x: fr.x,
                    y: fr.y,
                    w: fr.w,
                    h: placed.y - fr.y,
                });
            }
            if placed.y + placed.h < fr.y + fr.h {
                next.push(FreeRect {
                    x: fr.x,
                    y: placed.y + placed.h,
                    w: fr.w,
                    h: fr.y + fr.h - (placed.y + placed.h),
                });
            }
        }
        // Prune any free rect fully contained within another — same
        // dedup the reference does after every placement.
        free_rects = next
            .iter()
            .copied()
            .enumerate()
            .filter(|&(i, r)| {
                !next
                    .iter()
                    .copied()
                    .enumerate()
                    .any(|(j, other)| i != j && rect_contains(other, r))
            })
            .map(|(_, r)| r)
            .collect();
    }

    // Local overlap-repair sweep, matching packRects' own embedded pass:
    // sort by (y, x), then push anything still colliding straight down past
    // whatever it overlaps. Guarantees zero overlaps by construction.
    let sized: Vec<PositionedItem> = items
        .iter()
        .map(|it| {
            let p = positions[&it.id];
            PositionedItem {
                id: it.id.clone(),
                x: p.x,
                y: p.y,
                width: it.width,
                height: it.height,
            }
        })
        .collect();
    let repaired = repair_overlaps(&sized, gap);
    for (id, pos) in &repaired {
        positions.insert(id.clone(), *pos);
        max_y = max_y.max(pos.y + items.iter().find(|it| &it.id == id).unwrap().height);
    }

    PackResult {
        positions,
        width: max_x,
        height: max_y,
    }
}

/// An already-positioned, sized item — input to [`repair_overlaps`].
#[derive(Debug, Clone)]
pub struct PositionedItem {
    pub id: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// Global overlap-repair sweep, ported from `organizeByDimensions`'s final
/// safety net (session_canvas.html:~1562-1583): sorts items by `(y, x)`,
/// then for each one in that order, nudges it straight down (never
/// sideways, so group membership / column position always stays visually
/// intact) past any already-settled item it still overlaps. Guarantees zero
/// overlaps in the output by construction.
///
/// This is the same sweep [`pack_rects`] runs internally on one group's
/// local coordinates; exposed standalone here so a caller combining several
/// groups' absolute (already-translated) positions can run one more global
/// pass across everything, exactly matching the reference's two-tier
/// repair (local packing repair, then a final cross-group repair right
/// before render).
pub fn repair_overlaps(items: &[PositionedItem], gap: f32) -> HashMap<String, PackedPosition> {
    let mut order: Vec<&PositionedItem> = items.iter().collect();
    order.sort_by(|a, b| {
        a.y.partial_cmp(&b.y)
            .unwrap()
            .then(a.x.partial_cmp(&b.x).unwrap())
    });

    struct Settled {
        x: f32,
        y: f32,
        w: f32,
        h: f32,
    }
    let mut settled: Vec<Settled> = Vec::new();
    let mut result: HashMap<String, PackedPosition> = HashMap::new();

    for item in order {
        let mut y = item.y;
        let mut moved = true;
        let mut guard = 0;
        while moved && guard < 2000 {
            moved = false;
            guard += 1;
            for other in &settled {
                if item.x < other.x + other.w
                    && item.x + item.width > other.x
                    && y < other.y + other.h
                    && y + item.height > other.y
                {
                    y = other.y + other.h + gap;
                    moved = true;
                }
            }
        }
        result.insert(item.id.clone(), PackedPosition { x: item.x, y });
        settled.push(Settled {
            x: item.x,
            y,
            w: item.width,
            h: item.height,
        });
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn overlaps(a: (f32, f32, f32, f32), b: (f32, f32, f32, f32)) -> bool {
        rects_overlap(a.0, a.1, a.2, a.3, b.0, b.1, b.2, b.3)
    }

    fn assert_no_overlaps(rects: &[(String, f32, f32, f32, f32)]) {
        for i in 0..rects.len() {
            for j in (i + 1)..rects.len() {
                let (_, ax, ay, aw, ah) = rects[i].clone();
                let (_, bx, by, bw, bh) = rects[j].clone();
                assert!(
                    !overlaps((ax, ay, aw, ah), (bx, by, bw, bh)),
                    "overlap between {} and {}: ({ax},{ay},{aw},{ah}) vs ({bx},{by},{bw},{bh})",
                    rects[i].0,
                    rects[j].0
                );
            }
        }
    }

    #[test]
    fn packs_same_size_items_with_no_overlaps() {
        let items: Vec<PackItem> = (0..12)
            .map(|i| PackItem {
                id: format!("t{i}"),
                width: 100.0,
                height: 80.0,
            })
            .collect();
        let result = pack_rects(&items, 8.0);
        assert_eq!(result.positions.len(), 12);
        let rects: Vec<_> = items
            .iter()
            .map(|it| {
                let p = result.positions[&it.id];
                (it.id.clone(), p.x, p.y, it.width, it.height)
            })
            .collect();
        assert_no_overlaps(&rects);
    }

    #[test]
    fn packs_mixed_size_items_with_no_overlaps() {
        let sizes = [
            (120.0, 60.0),
            (60.0, 200.0),
            (90.0, 90.0),
            (200.0, 40.0),
            (50.0, 50.0),
            (150.0, 150.0),
            (40.0, 300.0),
            (300.0, 40.0),
        ];
        let items: Vec<PackItem> = sizes
            .iter()
            .enumerate()
            .map(|(i, &(w, h))| PackItem {
                id: format!("m{i}"),
                width: w,
                height: h,
            })
            .collect();
        let result = pack_rects(&items, 6.0);
        assert_eq!(result.positions.len(), items.len());
        let rects: Vec<_> = items
            .iter()
            .map(|it| {
                let p = result.positions[&it.id];
                (it.id.clone(), p.x, p.y, it.width, it.height)
            })
            .collect();
        assert_no_overlaps(&rects);
        assert!(result.width > 0.0 && result.height > 0.0);
    }

    #[test]
    fn packing_is_empty_for_no_items() {
        let result = pack_rects(&[], 8.0);
        assert!(result.positions.is_empty());
        assert_eq!(result.width, 0.0);
        assert_eq!(result.height, 0.0);
    }

    #[test]
    fn repair_overlaps_resolves_a_deliberately_overlapping_input() {
        // Three 100x100 boxes all placed at the exact same spot — maximally
        // overlapping input that pack_rects itself would never produce, to
        // exercise the repair pass in isolation.
        let items = vec![
            PositionedItem {
                id: "a".into(),
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
            PositionedItem {
                id: "b".into(),
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
            PositionedItem {
                id: "c".into(),
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
        ];
        let repaired = repair_overlaps(&items, 4.0);
        assert_eq!(repaired.len(), 3);
        let rects: Vec<_> = items
            .iter()
            .map(|it| {
                let p = repaired[&it.id];
                (it.id.clone(), p.x, p.y, it.width, it.height)
            })
            .collect();
        assert_no_overlaps(&rects);
        // x must never move — the repair pass only nudges straight down, so
        // group/column membership stays visually intact.
        for it in &items {
            assert_eq!(repaired[&it.id].x, it.x, "{} moved sideways", it.id);
        }
    }

    #[test]
    fn repair_overlaps_leaves_already_disjoint_items_untouched() {
        let items = vec![
            PositionedItem {
                id: "left".into(),
                x: 0.0,
                y: 0.0,
                width: 50.0,
                height: 50.0,
            },
            PositionedItem {
                id: "right".into(),
                x: 100.0,
                y: 0.0,
                width: 50.0,
                height: 50.0,
            },
        ];
        let repaired = repair_overlaps(&items, 4.0);
        assert_eq!(repaired["left"], PackedPosition { x: 0.0, y: 0.0 });
        assert_eq!(repaired["right"], PackedPosition { x: 100.0, y: 0.0 });
    }

    #[test]
    fn repair_overlaps_cascades_through_a_stack() {
        // Four identical boxes stacked at the same (x, y) — the sweep must
        // settle every one of them without residual overlap, not just fix
        // the first pair.
        let items: Vec<PositionedItem> = (0..4)
            .map(|i| PositionedItem {
                id: format!("s{i}"),
                x: 10.0,
                y: 10.0,
                width: 40.0,
                height: 40.0,
            })
            .collect();
        let repaired = repair_overlaps(&items, 2.0);
        let rects: Vec<_> = items
            .iter()
            .map(|it| {
                let p = repaired[&it.id];
                (it.id.clone(), p.x, p.y, it.width, it.height)
            })
            .collect();
        assert_no_overlaps(&rects);
    }
}
