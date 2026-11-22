//! Position keys: the sparse integers that order a backend's queue.
//!
//! A task's key is written only when the task is created, when the owner
//! moves it, and when a move needs room and shifts a few neighbours. Every
//! other transition leaves it alone, so a task keeps its place.
//!
//! [`plan_move`] decides a whole move before anything is written: the keys
//! the neighbours get, in the order to write them, and the moved task's key
//! last. It is pure, so a move that finds no room writes nothing.

use chrono::{DateTime, Utc};

/// The key of a task created at `time`: microseconds since the Unix epoch,
/// 0 for a time before it.
pub(crate) fn key_at(time: DateTime<Utc>) -> u64 {
    u64::try_from(time.timestamp_micros()).unwrap_or(0)
}

/// The virtual key after the last task: `now` if it is greater than the
/// last key, else the last key + 2, so there is always one free integer
/// after the last task while the keys stay below `u64::MAX`.
pub(crate) fn upper_bound(now: u64, last: Option<u64>) -> u64 {
    match last {
        Some(last) if now <= last => last.saturating_add(2),
        _ => now,
    }
}

/// No integer is free where the task goes, even after shifting every
/// neighbour as far as the bounds allow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NoSpace;

/// The key writes that move `moved` to place `slot` of `order`.
///
/// `order` is the backend's whole order, lowest key first, with `moved` at
/// its old key. `slot` counts places in that order with `moved` taken out:
/// 0 is before the first task, `order.len() - 1` after the last. `lower`
/// and `upper` are the virtual keys before the first and after the last
/// task.
///
/// The result lists the writes in the order to make them, the moved task
/// last. When the gap at `slot` has no free integer, the neighbours move
/// away from it, nearest last in each round: the one before, then the one
/// after; then the two before (farther one first) and the two after; and so
/// on, until the gap has room. Each neighbour moves to the middle between
/// itself and its outer neighbour in the whole order (`moved` included at
/// its old key), so every single write keeps the order strict.
pub(crate) fn plan_move<Id: Clone + PartialEq>(
    order: &[(Id, u64)],
    moved: &Id,
    slot: usize,
    lower: u64,
    upper: u64,
) -> Result<Vec<(Id, u64)>, NoSpace> {
    let mut keys: Vec<u64> = order.iter().map(|(_, key)| *key).collect();
    // Indices into `order` of every task but the moved one.
    let rest: Vec<usize> = (0..order.len())
        .filter(|&index| order[index].0 != *moved)
        .collect();
    let slot = slot.min(rest.len());
    let before_gap = slot;
    let after_gap = rest.len() - slot;
    let gap = |keys: &[u64]| {
        let low = if slot == 0 {
            lower
        } else {
            keys[rest[slot - 1]]
        };
        let high = if slot == rest.len() {
            upper
        } else {
            keys[rest[slot]]
        };
        between(low, high)
    };

    let mut writes = Vec::new();
    let mut round = 1;
    loop {
        if let Some(key) = gap(&keys) {
            writes.push((moved.clone(), key));
            return Ok(writes);
        }
        if round > before_gap && round > after_gap {
            return Err(NoSpace);
        }
        // Farthest first, so each nearer one finds the room the farther one
        // made.
        for distance in (1..=round.min(before_gap)).rev() {
            let index = rest[slot - distance];
            let outer = if index == 0 { lower } else { keys[index - 1] };
            if let Some(key) = between(outer, keys[index]) {
                keys[index] = key;
                writes.push((order[index].0.clone(), key));
            }
            if let Some(key) = gap(&keys) {
                writes.push((moved.clone(), key));
                return Ok(writes);
            }
        }
        for distance in (1..=round.min(after_gap)).rev() {
            let index = rest[slot + distance - 1];
            let outer = if index + 1 == order.len() {
                upper
            } else {
                keys[index + 1]
            };
            if let Some(key) = between(keys[index], outer) {
                keys[index] = key;
                writes.push((order[index].0.clone(), key));
            }
            if let Some(key) = gap(&keys) {
                writes.push((moved.clone(), key));
                return Ok(writes);
            }
        }
        round += 1;
    }
}

/// The rounded-down middle of `low` and `high`, if it is strictly between
/// them.
fn between(low: u64, high: u64) -> Option<u64> {
    let key = low.checked_add(high.checked_sub(low)? / 2)?;
    (low < key && key < high).then_some(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(keys: &[(&'static str, u64)]) -> Vec<(&'static str, u64)> {
        keys.to_vec()
    }

    /// Applies `writes` one at a time and checks that the order stays
    /// strict after each neighbour write; returns the final order.
    fn apply(start: &[(&'static str, u64)], writes: &[(&'static str, u64)]) -> Vec<&'static str> {
        let mut keys = start.to_vec();
        for (written, (id, key)) in writes.iter().enumerate() {
            keys.iter_mut().find(|(other, _)| other == id).unwrap().1 = *key;
            if written + 1 < writes.len() {
                assert!(
                    keys.windows(2).all(|pair| pair[0].1 < pair[1].1),
                    "a neighbour write broke the order: {keys:?}"
                );
            }
        }
        keys.sort_by_key(|(_, key)| *key);
        assert!(
            keys.windows(2).all(|pair| pair[0].1 < pair[1].1),
            "{keys:?}"
        );
        keys.into_iter().map(|(id, _)| id).collect()
    }

    #[test]
    fn the_moved_task_takes_the_middle_of_a_gap_with_room_and_nothing_else_changes() {
        let start = order(&[("p", 1000), ("n", 2000), ("x", 3000)]);
        let writes = plan_move(&start, &"x", 1, 0, 4000).unwrap();
        assert_eq!(writes, [("x", 1500)]);
    }

    #[test]
    fn a_full_gap_moves_the_nearest_neighbour_before_it_first() {
        let start = order(&[
            ("a", 500),
            ("p", 1000),
            ("n", 1001),
            ("b", 3000),
            ("x", 4000),
        ]);
        let writes = plan_move(&start, &"x", 2, 0, 5000).unwrap();
        assert_eq!(writes, [("p", 750), ("x", 875)]);
    }

    #[test]
    fn moves_to_the_ends_use_the_virtual_bounds() {
        struct Case {
            name: &'static str,
            start: &'static [(&'static str, u64)],
            slot: usize,
            upper: u64,
            writes: &'static [(&'static str, u64)],
        }
        let cases = [
            Case {
                name: "top with the first key at 1 shifts the first task right",
                start: &[("a", 1), ("b", 10), ("x", 20)],
                slot: 0,
                upper: 30,
                writes: &[("a", 5), ("x", 2)],
            },
            Case {
                name: "bottom with the clock ahead takes the middle up to now",
                start: &[("x", 10), ("a", 20)],
                slot: 1,
                upper: 100,
                writes: &[("x", 60)],
            },
            Case {
                name: "bottom with the clock behind takes the last key + 1",
                start: &[("x", 10), ("a", 20)],
                slot: 1,
                upper: upper_bound(5, Some(20)),
                writes: &[("x", 21)],
            },
        ];
        for case in cases {
            let writes = plan_move(case.start, &"x", case.slot, 0, case.upper).unwrap();
            assert_eq!(writes, case.writes, "{}", case.name);
            apply(case.start, &writes);
        }
    }

    #[test]
    fn a_neighbour_never_passes_the_moved_task_at_its_old_key() {
        // x sits right before p: p cannot move left, so n makes the room.
        let start = order(&[("x", 10), ("p", 11), ("n", 12), ("b", 20)]);
        let writes = plan_move(&start, &"x", 1, 0, 30).unwrap();
        assert_eq!(writes, [("n", 16), ("x", 13)]);
        assert_eq!(apply(&start, &writes), ["p", "x", "n", "b"]);
    }

    #[test]
    fn a_crowded_order_is_spread_round_by_round_and_stays_strict_after_every_write() {
        let start = order(&[
            ("a", 100),
            ("b", 101),
            ("c", 102),
            ("d", 103),
            ("e", 104),
            ("f", 105),
            ("x", 106),
        ]);
        let writes = plan_move(&start, &"x", 3, 0, 108).unwrap();
        // Every key is next to another one, so rounds 1 and 2 move nothing.
        // Round 3 reaches a, which has room down to 0, and then b and c
        // each move into the room the one before made.
        assert_eq!(writes, [("a", 50), ("b", 75), ("c", 88), ("x", 95)]);
        assert_eq!(apply(&start, &writes), ["a", "b", "c", "x", "d", "e", "f"]);
    }

    #[test]
    fn no_room_up_to_both_bounds_plans_no_write() {
        let start = order(&[("a", 1), ("b", 2), ("c", 3), ("x", 4)]);
        assert_eq!(plan_move(&start, &"x", 1, 0, 5), Err(NoSpace));
        let top = order(&[("x", u64::MAX - 1), ("a", u64::MAX)]);
        assert_eq!(plan_move(&top, &"x", 1, 0, u64::MAX), Err(NoSpace));
    }
}
