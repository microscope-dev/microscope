const MAX_SUGGESTIONS: usize = 3;

#[expect(
    clippy::indexing_slicing,
    reason = "rows hold right.len() + 1 cells, and row and column stay within 1..=len"
)]
fn distance(left: &str, right: &str, bound: usize) -> Option<usize> {
    let (left, right) = (
        left.chars().collect::<Vec<_>>(),
        right.chars().collect::<Vec<_>>(),
    );
    if left.len().abs_diff(right.len()) > bound {
        return None;
    }

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

    let nearest = found.first().map_or(0, |(rank, ..)| *rank);
    found
        .into_iter()
        .filter(|(rank, ..)| *rank == nearest || *rank == bound + 1)
        .take(MAX_SUGGESTIONS)
        .map(|(.., candidate)| candidate)
        .collect()
}

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

        assert_eq!(nearest("mian", names), ["main"]);
        assert_eq!(distance("kitten", "sitting", 3), Some(3));
        assert_eq!(distance("kitten", "sitting", 2), None);
    }
}
