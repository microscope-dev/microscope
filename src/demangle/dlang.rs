//! Minimal D (`_D`) demangler: extracts qualified names, walking trailing type
//! encodings only to reject malformed or partial symbols.

const DEPTH_LIMIT: u32 = 64;

pub fn qualified_name(mangled: &str) -> Option<String> {
    if mangled == "_Dmain" {
        return Some("D main".to_owned());
    }

    let mut reader = Reader {
        text: mangled.as_bytes(),
        at: 0,
        depth: 0,
    };

    let name = reader.mangled_name()?;
    (reader.at == reader.text.len()).then_some(name)
}

struct Reader<'a> {
    text: &'a [u8],
    at: usize,
    depth: u32,
}

impl<'a> Reader<'a> {
    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn peek_at(&self, offset: usize) -> Option<u8> {
        self.text.get(self.at + offset).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.at += 1;
        Some(byte)
    }

    fn eat(&mut self, byte: u8) -> bool {
        let found = self.peek() == Some(byte);
        if found {
            self.at += 1;
        }

        found
    }

    fn starts_with(&self, prefix: &[u8]) -> bool {
        self.text
            .get(self.at..)
            .is_some_and(|rest| rest.starts_with(prefix))
    }

    fn nested<T>(&mut self, read: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        if self.depth >= DEPTH_LIMIT {
            return None;
        }

        self.depth += 1;
        let read = read(self);
        self.depth -= 1;
        read
    }

    fn mangled_name(&mut self) -> Option<String> {
        if !self.starts_with(b"_D") {
            return None;
        }

        self.at += 2;
        let name = self.qualified_name()?;
        if self.eat(b'Z') {
            return Some(name);
        }

        self.symbol_type()?;
        Some(name)
    }

    fn symbol_type(&mut self) -> Option<()> {
        if self.eat(b'M') {
            self.modifiers();
        }

        self.read_type()
    }

    fn symbol(&mut self) -> Option<()> {
        if !self.starts_with(b"_D") {
            return None;
        }

        self.at += 2;
        self.qualified_name()?;
        self.symbol_type()
    }

    fn qualified_name(&mut self) -> Option<String> {
        self.nested(|reader| {
            let mut name = String::new();
            loop {
                if !name.is_empty() {
                    name.push('.');
                }

                reader.symbol_name(&mut name)?;

                let before = reader.at;
                if reader.peek() == Some(b'M') || reader.peek().is_some_and(call_convention) {
                    if reader.eat(b'M') {
                        reader.modifiers();
                    }

                    if reader.function_without_return().is_some() && reader.starts_symbol_name() {
                        continue;
                    }

                    reader.at = before;
                }

                if !reader.starts_symbol_name() {
                    return Some(name);
                }
            }
        })
    }

    fn starts_symbol_name(&self) -> bool {
        match self.peek() {
            Some(b'1'..=b'9') => true,
            Some(b'_') => self.starts_with(b"__T") || self.starts_with(b"__U"),
            Some(b'Q') => {
                let mut probe = Reader {
                    text: self.text,
                    at: self.at,
                    depth: self.depth,
                };

                probe
                    .back_reference()
                    .and_then(|target| self.text.get(target))
                    .is_some_and(u8::is_ascii_digit)
            }
            _ => false,
        }
    }

    fn symbol_name(&mut self, name: &mut String) -> Option<()> {
        if self.starts_with(b"__T") || self.starts_with(b"__U") {
            self.at += 3;
            name.push_str(self.named()?);
            name.push_str("!(…)");
            return self.template_arguments();
        }

        name.push_str(self.named()?);
        Some(())
    }

    fn named(&mut self) -> Option<&'a str> {
        if self.peek() != Some(b'Q') {
            return self.identifier();
        }

        let target = self.back_reference()?;
        let mut referred = Reader {
            text: self.text,
            at: target,
            depth: self.depth,
        };

        referred.identifier()
    }

    fn identifier(&mut self) -> Option<&'a str> {
        if !self.peek()?.is_ascii_digit() || self.peek() == Some(b'0') {
            return None;
        }

        let length = self.number()?;
        let end = self.at.checked_add(length)?;
        let bytes = self.text.get(self.at..end)?;
        let text = std::str::from_utf8(bytes).ok()?;
        let identifier = text.starts_with(|first: char| first == '_' || !first.is_ascii_digit())
            && !text.starts_with("__T")
            && !text.starts_with("__U")
            && text
                .chars()
                .all(|character| character == '_' || character.is_alphanumeric());
        if !identifier {
            return None;
        }

        self.at = end;
        Some(text)
    }

    fn number(&mut self) -> Option<usize> {
        let start = self.at;
        let mut value = 0_usize;
        while let Some(digit @ b'0'..=b'9') = self.peek() {
            value = value
                .checked_mul(10)?
                .checked_add(usize::from(digit - b'0'))?;
            self.at += 1;
        }

        (self.at > start).then_some(value)
    }

    fn back_reference(&mut self) -> Option<usize> {
        let at = self.at;
        if !self.eat(b'Q') {
            return None;
        }

        let mut distance = 0_usize;
        loop {
            let digit = self.next()?;
            let (value, last) = match digit {
                b'A'..=b'Z' => (digit - b'A', false),
                b'a'..=b'z' => (digit - b'a', true),
                _ => return None,
            };

            distance = distance.checked_mul(26)?.checked_add(usize::from(value))?;
            if last {
                break;
            }
        }

        (distance != 0 && distance <= at).then(|| at - distance)
    }

    fn template_arguments(&mut self) -> Option<()> {
        self.nested(|reader| {
            loop {
                reader.eat(b'H');
                match reader.next()? {
                    b'Z' => return Some(()),
                    b'T' => reader.read_type()?,
                    b'V' => {
                        let associative = reader.type_kind() == Some(b'H');
                        reader.read_type()?;
                        reader.value(associative)?;
                    }
                    b'S' if reader.starts_with(b"_D") => reader.symbol()?,
                    b'S' => {
                        reader.qualified_name()?;
                    }
                    b'X' => {
                        let length = reader.number()?;
                        reader.at = reader.at.checked_add(length)?;
                        if reader.at > reader.text.len() {
                            return None;
                        }
                    }
                    _ => return None,
                }
            }
        })
    }

    fn type_kind(&self) -> Option<u8> {
        let mut probe = Reader {
            text: self.text,
            at: self.at,
            depth: self.depth,
        };

        probe.modifiers();
        if probe.peek() == Some(b'Q') {
            let target = probe.back_reference()?;
            return self.text.get(target).copied();
        }

        probe.peek()
    }

    fn value(&mut self, associative: bool) -> Option<()> {
        self.nested(|reader| match reader.next()? {
            b'n' => Some(()),
            b'i' | b'N' => reader.number().map(drop),
            b'e' => reader.hex_float(),
            b'c' => {
                reader.hex_float()?;
                reader.eat(b'c').then_some(())?;
                reader.hex_float()
            }
            b'a' | b'w' | b'd' => {
                let length = reader.number()?;
                reader.eat(b'_').then_some(())?;
                for _ in 0..length.checked_mul(2)? {
                    reader.next().filter(u8::is_ascii_hexdigit)?;
                }

                Some(())
            }
            b'A' => {
                let count = reader.number()?;
                let values = if associative {
                    count.checked_mul(2)?
                } else {
                    count
                };

                for _ in 0..values {
                    reader.element_value()?;
                }

                Some(())
            }
            b'S' => {
                let count = reader.number()?;
                for _ in 0..count {
                    reader.element_value()?;
                }

                Some(())
            }
            b'f' => reader.symbol(),
            _ => None,
        })
    }

    fn element_value(&mut self) -> Option<()> {
        self.value(false)
    }

    fn hex_float(&mut self) -> Option<()> {
        for special in [&b"NAN"[..], b"NINF", b"INF"] {
            if self.starts_with(special) {
                self.at += special.len();
                return Some(());
            }
        }

        self.eat(b'N');
        let start = self.at;
        while self
            .peek()
            .is_some_and(|byte| byte.is_ascii_digit() || (b'A'..=b'F').contains(&byte))
        {
            self.at += 1;
        }

        if self.at == start || !self.eat(b'P') {
            return None;
        }

        self.eat(b'N');
        self.number().map(drop)
    }

    fn modifiers(&mut self) {
        loop {
            if self.eat(b'x') || self.eat(b'y') || self.eat(b'O') {
                continue;
            }

            if self.starts_with(b"Ng") {
                self.at += 2;
                continue;
            }

            return;
        }
    }

    fn read_type(&mut self) -> Option<()> {
        self.nested(|reader| match reader.peek()? {
            b'N' => {
                reader.at += 1;
                match reader.next()? {
                    b'g' | b'h' => reader.read_type(),
                    b'n' => Some(()),
                    _ => None,
                }
            }

            b'x' | b'y' | b'O' | b'A' | b'P' => {
                reader.at += 1;
                reader.read_type()
            }
            b'G' => {
                reader.at += 1;
                reader.number()?;
                reader.read_type()
            }
            b'H' => {
                reader.at += 1;
                reader.read_type()?;
                reader.read_type()
            }
            b'I' | b'C' | b'S' | b'E' | b'T' => {
                reader.at += 1;
                reader.qualified_name().map(drop)
            }
            b'D' => {
                reader.at += 1;
                reader.modifiers();
                if reader.peek() == Some(b'Q') {
                    return reader.read_type();
                }

                reader.function()
            }
            byte if call_convention(byte) => reader.function(),
            b'Q' => {
                let target = reader.back_reference()?;
                reader.text.get(target)?.is_ascii_alphabetic().then_some(())
            }
            b'z' => {
                reader.at += 1;
                matches!(reader.next()?, b'i' | b'k').then_some(())
            }
            byte if BASIC_TYPES.contains(&byte) => {
                reader.at += 1;
                Some(())
            }
            _ => None,
        })
    }

    fn function(&mut self) -> Option<()> {
        self.function_without_return()?;
        self.read_type()
    }

    fn function_without_return(&mut self) -> Option<()> {
        if !call_convention(self.next()?) {
            return None;
        }

        while self.peek() == Some(b'N')
            && self
                .peek_at(1)
                .is_some_and(|byte| ATTRIBUTES.contains(&byte))
        {
            self.at += 2;
        }

        loop {
            match self.peek()? {
                b'X' | b'Y' | b'Z' => {
                    self.at += 1;
                    return Some(());
                }
                _ => {
                    self.eat(b'M');
                    if self.starts_with(b"Nk") {
                        self.at += 2;
                    }

                    if matches!(self.peek()?, b'I' | b'J' | b'K' | b'L') {
                        let class = self.at;
                        self.at += 1;
                        if self.read_type().is_some() {
                            continue;
                        }

                        self.at = class;
                    }

                    self.read_type()?;
                }
            }
        }
    }
}

const BASIC_TYPES: &[u8] = b"vghstiklmfdeopjqrcbauwn";

const ATTRIBUTES: &[u8] = b"abcdefijlm";

const fn call_convention(byte: u8) -> bool {
    matches!(byte, b'F' | b'U' | b'W' | b'V' | b'R' | b'Y')
}
