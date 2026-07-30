//! F-1-5 vocabulary builder — pure Leitner-box scheduling. No DB, no clock:
//! `now` is always passed in so callers (and tests) stay deterministic.

/// Interval, in days, before box N (1-indexed) comes due again.
const BOX_INTERVAL_DAYS: [i64; 5] = [1, 3, 7, 14, 30];
const SECS_PER_DAY: i64 = 86_400;

/// Given the current box and whether the review was correct, return the
/// `(new_box, next_due_at)` the row should be updated to. `now` is epoch
/// seconds. Out-of-range `box_num` (e.g. 0 or > 5) is clamped into `1..=5`
/// before scoring, so a corrupt row can't panic or index out of bounds.
pub fn next_review(box_num: i64, correct: bool, now: i64) -> (i64, i64) {
    let box_num = box_num.clamp(1, 5);
    let new_box = if correct { (box_num + 1).min(5) } else { 1 };
    let interval_days = BOX_INTERVAL_DAYS[(new_box - 1) as usize];
    (new_box, now + interval_days * SECS_PER_DAY)
}

#[cfg(test)]
mod tests {
    use super::next_review;

    #[test]
    fn correct_advances_box() {
        let (new_box, _) = next_review(1, true, 0);
        assert_eq!(new_box, 2);
    }

    #[test]
    fn correct_caps_at_five() {
        let (new_box, _) = next_review(5, true, 0);
        assert_eq!(new_box, 5);
    }

    #[test]
    fn wrong_resets_to_one() {
        let (new_box, _) = next_review(4, false, 0);
        assert_eq!(new_box, 1);
    }

    #[test]
    fn next_due_at_matches_interval_per_box() {
        let now = 1_000_000;
        // correct from box N lands in box min(N+1, 5); due offset must match
        // that landing box's interval (days -> secs).
        for (box_num, landing_interval_days) in (1..=5).zip([3, 7, 14, 30, 30]) {
            let (_, due) = next_review(box_num, true, now);
            assert_eq!(
                due,
                now + landing_interval_days * 86_400,
                "box {box_num} correct"
            );
        }
    }

    #[test]
    fn out_of_range_box_is_clamped() {
        let (new_box_low, _) = next_review(0, true, 0);
        assert_eq!(new_box_low, 2, "0 clamps to 1, then advances to 2");

        let (new_box_high, due_high) = next_review(9, true, 0);
        assert_eq!(new_box_high, 5, "9 clamps to 5, stays capped at 5");
        assert_eq!(due_high, 30 * 86_400);

        let (new_box_wrong, _) = next_review(9, false, 0);
        assert_eq!(
            new_box_wrong, 1,
            "wrong always resets to 1 regardless of input box"
        );
    }
}
