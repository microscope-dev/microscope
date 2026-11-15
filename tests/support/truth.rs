use std::path::Path;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    lines: Vec<Vec<String>>,
}

pub type Fields = [String];

impl Report {
    pub fn parse(text: &str) -> Self {
        Self {
            lines: text
                .lines()
                .filter_map(|line| line.strip_prefix("TRUTH\t"))
                .map(|line| line.split('\t').map(unescape).collect())
                .collect(),
        }
    }

    pub fn read(path: &Path) -> Self {
        let text = std::fs::read_to_string(path).unwrap_or_else(|error| {
            panic!("read the fixture's output {}: {error}", path.display())
        });
        Self::parse(&text)
    }

    pub fn lines(&self) -> impl Iterator<Item = &Fields> {
        self.lines.iter().map(Vec::as_slice)
    }

    pub fn checkpoint(&self, name: &str) -> Option<&[Vec<String>]> {
        let start = self.lines.iter().rposition(|fields| {
            is_checkpoint(fields) && fields.get(1).is_some_and(|n| n == name)
        })?;

        let lines = &self.lines[start + 1..];
        let end = lines
            .iter()
            .position(|fields| is_checkpoint(fields))
            .unwrap_or(lines.len());

        Some(&lines[..end])
    }

    pub fn latest(&self) -> Option<(&str, &[Vec<String>])> {
        let start = self
            .lines
            .iter()
            .rposition(|fields| is_checkpoint(fields))?;

        let name = self.lines[start].get(1)?;

        Some((name, &self.lines[start + 1..]))
    }
}

fn is_checkpoint(fields: &[String]) -> bool {
    fields.first().is_some_and(|first| first == "checkpoint")
}

pub fn unescape(field: &str) -> String {
    let mut text = String::with_capacity(field.len());
    let mut characters = field.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            text.push(character);
            continue;
        }

        match characters.next() {
            Some('t') => text.push('\t'),
            Some('n') => text.push('\n'),
            Some('\\') | None => text.push('\\'),
            Some(other) => {
                text.push('\\');
                text.push(other);
            }
        }
    }

    text
}
