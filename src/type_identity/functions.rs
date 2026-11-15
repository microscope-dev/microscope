use super::{NameSyntax, split_arguments, split_top_level};

pub fn packaged_name(
    name: &str,
    syntax: NameSyntax,
    is_package: impl Fn(&str) -> bool,
) -> Option<(&str, String)> {
    if syntax != NameSyntax::Go {
        return None;
    }

    let qualified = &name[..name.find(['(', '[']).unwrap_or(name.len())];
    let package = qualified
        .rmatch_indices('.')
        .map(|(dot, _)| &name[..dot])
        .find(|path| is_package(path))
        .or_else(|| {
            let after_slash = qualified.rfind('/').map_or(0, |slash| slash + 1);
            let dot = after_slash + qualified[after_slash..].find('.')?;
            Some(&name[..dot])
        })
        .filter(|path| is_import_path(path))?;
    let local = plain_parts(&name[package.len() + 1..], true)?;
    Some((package, local))
}

pub fn ada_packaged_name(name: &str) -> Option<(&str, String)> {
    let (unit, local) = name.split_once('.')?;
    Some((unit, local.to_owned()))
}

pub fn plain_location(location: &str) -> Option<String> {
    if location.contains('[') {
        return None;
    }

    plain_parts(location, false)
}

fn plain_parts(name: &str, instances: bool) -> Option<String> {
    let parts = split_top_level(name, ".", NameSyntax::Go)?;
    let mut plain = Vec::with_capacity(parts.len());
    for part in parts {
        let receiver = part
            .strip_prefix('(')
            .and_then(|inner| inner.strip_suffix(')'))
            .map_or(part, |inner| inner.strip_prefix('*').unwrap_or(inner));
        let base = if instances {
            split_arguments(receiver, '[', ']', NameSyntax::Go)?.0
        } else {
            receiver
        };

        if base.is_empty() {
            return None;
        }

        plain.push(base);
    }

    Some(plain.join("."))
}

fn is_import_path(path: &str) -> bool {
    !path.is_empty()
        && path
            .chars()
            .all(|character| character.is_alphanumeric() || "/._-~".contains(character))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_names_split_into_their_package_and_plain_local_name() {
        let packages = ["main", "net/http", "gopkg.in/yaml.v3", "sync"];
        let split = |name| {
            packaged_name(name, NameSyntax::Go, |path| packages.contains(&path))
                .map(|(package, local)| (package.to_owned(), local))
        };

        for (name, package, local) in [
            ("main.main", "main", "main"),
            ("main.main.func1", "main", "main.func1"),
            ("main.Sum[go.shape.int]", "main", "Sum"),
            ("main.Sum[go.shape.int].func1", "main", "Sum.func1"),
            ("main.(*Stack[go.shape.int]).Push", "main", "Stack.Push"),
            ("main.Stack[go.shape.string].Len", "main", "Stack.Len"),
            ("net/http.(*Server).Serve", "net/http", "Server.Serve"),
            (
                "gopkg.in/yaml.v3.Unmarshal",
                "gopkg.in/yaml.v3",
                "Unmarshal",
            ),
            (
                "sync.OnceValue[go.shape.interface { Error() string }].func1",
                "sync",
                "OnceValue.func1",
            ),
            ("math/rand/v2.IntN", "math/rand/v2", "IntN"),
            ("main.main-range1", "main", "main-range1"),
        ] {
            assert_eq!(
                split(name),
                Some((package.to_owned(), local.to_owned())),
                "{name}"
            );
        }

        assert_eq!(split("type:.eq.main.T"), None);
        assert_eq!(packaged_name("ns::run", NameSyntax::Angle, |_| true), None);

        assert_eq!(
            plain_location("net/http.(*Server).Serve").as_deref(),
            Some("net/http.Server.Serve")
        );
        assert_eq!(plain_location("(*T).M").as_deref(), Some("T.M"));
        assert_eq!(plain_location("main.Sum[int]"), None);
    }
}
