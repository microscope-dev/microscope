//! Suggestions for a mistyped name: the nearest few candidates by edit
//! distance, computed only after something failed and never acted on.

/// The most suggestions one failure offers.
const MAX_SUGGESTIONS: usize = 3;

/// The edit distance between two strings, by characters, counting a swap
/// of neighbors as one edit, or `None` once it exceeds `bound`.
fn distance(left: &str, right: &str, bound: usize) -> Option<usize> {
    let (left, right) = (
        left.chars().collect::<Vec<_>>(),
        right.chars().collect::<Vec<_>>(),
    );
    if left.len().abs_diff(right.len()) > bound {
        return None;
    }

    // Rows of the optimal string alignment table: two back, one back, and
    // the one being filled.
    let mut before = vec![0; right.len() + 1];
    let mut previous = (0..=right.len()).collect::<Vec<_>>();
    let mut current = vec![0; right.len() + 1];

    for row in 1..=left.len() {
        current[0] = row;

        for column in 1..=right.len() {
            let same = left[row - 1] == right[column - 1];
            let mut cost = (previous[column - 1] + usize::from(!same))
                .min(previous[column] + 1)
                .min(current[column - 1] + 1);

            if row > 1
                && column > 1
                && left[row - 1] == right[column - 2]
                && left[row - 2] == right[column - 1]
            {
                cost = cost.min(before[column - 2] + 1);
            }

            current[column] = cost;
        }

        if current.iter().min().is_some_and(|least| *least > bound) {
            return None;
        }

        std::mem::swap(&mut before, &mut previous);
        std::mem::swap(&mut previous, &mut current);
    }

    Some(previous[right.len()]).filter(|distance| *distance <= bound)
}

/// The candidates nearest `word`: those at the least edit distance within
/// a bound that grows with the word's length, then those it begins.
pub fn nearest<'a>(word: &str, candidates: impl IntoIterator<Item = &'a str>) -> Vec<&'a str> {
    let bound = (word.chars().count() / 3).max(1);
    let mut found = candidates
        .into_iter()
        .filter(|candidate| *candidate != word)
        .filter_map(|candidate| {
            let rank = distance(word, candidate, bound).or_else(|| {
                (word.len() >= 3 && candidate.starts_with(word)).then_some(bound + 1)
            })?;
            Some((rank, candidate.len(), candidate))
        })
        .collect::<Vec<_>>();
    found.sort_unstable();
    found.dedup_by_key(|(.., candidate)| *candidate);

    // Only the nearest by distance are suggested, and then those the word
    // begins.
    let nearest = found.first().map_or(0, |(rank, ..)| *rank);
    found
        .into_iter()
        .filter(|(rank, ..)| *rank == nearest || *rank == bound + 1)
        .take(MAX_SUGGESTIONS)
        .map(|(.., candidate)| candidate)
        .collect()
}

/// `did you mean 'a'?`, `'a' or 'b'?`, or `'a', 'b', or 'c'?`, or `None`
/// when nothing is near.
pub fn did_you_mean<'a>(
    word: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    let found = nearest(word, candidates)
        .into_iter()
        .map(|candidate| format!("'{candidate}'"))
        .collect::<Vec<_>>();
    let list = match found.as_slice() {
        [] => return None,
        [only] => only.clone(),
        [first, second] => format!("{first} or {second}"),
        [rest @ .., last] => format!("{}, or {last}", rest.join(", ")),
    };

    Some(format!("did you mean {list}?"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggestions_are_near_or_begun_by_the_word_and_nearest_first() {
        let names = ["process", "process_all", "parse", "print", "main"];
        assert_eq!(
            did_you_mean("proces", names).as_deref(),
            Some("did you mean 'process' or 'process_all'?")
        );
        assert_eq!(
            did_you_mean("colour", ["color", "theme"]).as_deref(),
            Some("did you mean 'color'?")
        );
        assert_eq!(did_you_mean("zzz", names), None);

        // Short words allow one edit, so `p` suggests nothing long.
        assert_eq!(nearest("mian", names), ["main"]);
        assert_eq!(distance("kitten", "sitting", 3), Some(3));
        assert_eq!(distance("kitten", "sitting", 2), None);
    }
}
