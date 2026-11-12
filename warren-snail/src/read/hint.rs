//! The HINT PostgreSQL's `errorMissingColumn` gives under a column no relation in reach holds: the
//! column of those relations spelled most like it (`searchRangeTableForCol`, Levenshtein distances
//! of 1 for each insertion, deletion and substitution, no more than 3 in all), the relation's name
//! counted in where the reference names one; none where none comes within half the column's
//! letters, and none where two tie but for a nearer one.

/// The fewest edits that turn one string into the other.
fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, x) in a.iter().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, y) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (above + 1).min(row[j] + 1).min(diagonal + usize::from(x != y));
            diagonal = above;
        }
    }
    row[b.len()]
}

const MOST: usize = 3;

/// The HINT for column `column` (after `alias.` where the reference names one) among `relations`,
/// each its name and its columns' names, in the order the query lays them; none where none is near
/// enough or two are equally near.
pub(super) fn column_hint(
    relations: &[(String, Vec<String>)],
    alias: Option<&str>,
    column: &str,
) -> Option<String> {
    let mut distance = MOST + 1;
    let mut first: Option<(&str, &str)> = None;
    let mut second: Option<(&str, &str)> = None;
    let half = column.chars().count() / 2;
    for (name, columns) in relations {
        let penalty = alias.map_or(0, |a| levenshtein(a, name));
        for c in columns {
            if penalty > distance {
                break;
            }
            if c.is_empty() {
                continue;
            }
            let d = levenshtein(c, column);
            if d > half {
                continue;
            }
            let d = d + penalty;
            if d < distance {
                distance = d;
                first = Some((name, c));
                second = None;
            } else if d == distance {
                match (first, second) {
                    (_, Some(_)) => {
                        first = None;
                        second = None;
                    }
                    (Some(_), None) => second = Some((name, c)),
                    (None, None) => {}
                }
            }
        }
    }
    match (first, second) {
        (Some((r, c)), None) => Some(format!(
            "\nHINT:  Perhaps you meant to reference the column \"{r}.{c}\"."
        )),
        (Some((r, c)), Some((s, d))) => Some(format!(
            "\nHINT:  Perhaps you meant to reference the column \"{r}.{c}\" or the column \
             \"{s}.{d}\"."
        )),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_nearest_column_is_hinted_as_postgresql_hints_it() {
        let a = vec![(
            "a".to_string(),
            ["id", "g", "k", "name"].map(String::from).to_vec(),
        )];
        assert_eq!(levenshtein("nope", "name"), 2);
        assert_eq!(
            column_hint(&a, Some("a"), "nam").as_deref(),
            Some("\nHINT:  Perhaps you meant to reference the column \"a.name\".")
        );
        assert_eq!(column_hint(&a, Some("a"), "zz"), None);
    }
}
