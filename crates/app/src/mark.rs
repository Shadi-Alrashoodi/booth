// The mark, "nook": a booth seen from above, benches on three sides around
// one table, the fourth side open to the aisle. Always whole-pixel
// rectangles, never a stroked path, so it stays sharp at 16 px.
//
//      0123456789012345
//   1  ...##########...
//   2  ...##########...
//   4  .##..........##.
//   5  .##..######..##.
//  10  .##..######..##.
//  11  .##..........##.
//  14  .##..........##.
//
// Rows 6 to 9 match row 5, rows 12 and 13 match row 11; rows 0, 3 and 15 are
// empty.

// Where the bars start and end on the 16 px master, across and down.
const ACROSS: [u32; 6] = [1, 3, 5, 11, 13, 15];
const DOWN: [u32; 6] = [1, 3, 4, 5, 11, 15];
// 24 px is Windows at 150 percent. Plain rounding keeps 3 px bars there but
// leaves the margins and the table uneven, so it is fitted by hand.
const ACROSS_24: [u32; 6] = [1, 4, 7, 17, 20, 23];
const DOWN_24: [u32; 6] = [1, 4, 6, 7, 17, 23];

// The four bars at `size` device pixels square, each as left, top, right and
// bottom in whole pixels: the bench across the top, the two down the sides,
// and the table.
pub fn rects(size: u32) -> [[u32; 4]; 4] {
    let (x, y) = if size == 24 {
        (ACROSS_24, DOWN_24)
    } else {
        let fit = |edges: [u32; 6]| edges.map(|edge| (edge * size + 8) / 16);
        (fit(ACROSS), fit(DOWN))
    };
    [
        [x[1], y[0], x[4], y[1]],
        [x[0], y[2], x[1], y[5]],
        [x[4], y[2], x[5], y[5]],
        [x[2], y[3], x[3], y[4]],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drawn(size: u32) -> Vec<String> {
        let bars = rects(size);
        (0..size)
            .map(|y| {
                (0..size)
                    .map(|x| {
                        let on = bars
                            .iter()
                            .any(|[l, t, r, b]| (*l..*r).contains(&x) && (*t..*b).contains(&y));
                        if on { '#' } else { '.' }
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn the_master_is_the_pixel_map() {
        let side = ".##..........##.";
        let table = ".##..######..##.";
        let top = "...##########...";
        let empty = "................";
        let mut map = vec![empty, top, top, empty, side];
        map.extend([table; 6]);
        map.extend([side; 4]);
        map.push(empty);
        assert_eq!(drawn(16), map);
    }

    // Nothing thinner than 2 px at any size Windows asks for, and the bars
    // are 3 px at 20 and 24 and 5 px at 40.
    #[test]
    fn every_size_keeps_its_bars() {
        for size in [16, 20, 24, 28, 32, 40, 48, 64, 256] {
            for [l, t, r, b] in rects(size) {
                assert!(r <= size && b <= size, "{size}");
                assert!(r - l >= 2 && b - t >= 2, "{size}: {l} {t} {r} {b}");
            }
        }
        for (size, bar) in [(20, 3), (24, 3), (40, 5)] {
            let [_, side, _, _] = rects(size);
            assert_eq!(side[2] - side[0], bar, "{size}");
        }
        // Square table, equal margins at the hand-fitted size.
        let [top, left, right, table] = rects(24);
        assert_eq!(table[2] - table[0], table[3] - table[1]);
        assert_eq!(left[0], 24 - right[2]);
        assert_eq!(top[0], left[2]);
    }
}
