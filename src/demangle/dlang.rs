//! D's mangled names, as the D ABI's name mangling specifies them: what a
//! symbol names, without its type. A function's parameters and a template
//! instance's arguments are left out, the arguments as `!(…)`, so that
//! `_D3std5stdio__T7writelnTAyaZQnFNfQjZv` names `std.stdio.writeln!(…)`.
//! A name that does not follow the grammar exactly, or uses a part of it
//! this reader does not know, is not decoded at all.

/// Bounds nesting, so that adversarial names cannot exhaust the stack.
const DEPTH_LIMIT: u32 = 64;

/// What a D symbol names, scopes joined by dots, or `None` when `mangled`
/// is not a D name this reader decodes exactly.
pub fn qualified_name(mangled: &str) -> Option<String> {
    // The program's main function.
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
        self.text[self.at..].starts_with(prefix)
    }

    /// Runs `read` one level deeper, failing past the limit.
    fn nested<T>(&mut self, read: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        if self.depth >= DEPTH_LIMIT {
            return None;
        }

        self.depth += 1;
        let read = read(self);
        self.depth -= 1;
        read
    }

    /// `_D`, a qualified name, then the symbol's type, or `Z` for a symbol
    /// the compiler generates, such as a class's `__vtbl`.
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

    /// A symbol's type: a member function's `this`, and how it is
    /// qualified, then the type.
    fn symbol_type(&mut self) -> Option<()> {
        if self.eat(b'M') {
            self.modifiers();
        }

        self.read_type()
    }

    /// A symbol a template argument or value names: `_D`, its name, and
    /// its type.
    fn symbol(&mut self) -> Option<()> {
        if !self.starts_with(b"_D") {
            return None;
        }

        self.at += 2;
        self.qualified_name()?;
        self.symbol_type()
    }

    /// Symbol names joined by dots. A function holding the next symbol is
    /// followed by its type, without its return type; what follows a
    /// symbol's last name is its own type, which may also begin as a
    /// function's does, so a type is a parent's only when a name follows.
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

    /// Whether a symbol name begins here: a length, a template instance,
    /// or a back reference to an identifier.
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
                    .is_some_and(|target| self.text[target].is_ascii_digit())
            }
            _ => false,
        }
    }

    /// One symbol name, appended to `name`.
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

    /// An identifier, or a back reference to one written before.
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

    /// An identifier: a decimal length, then that many characters of an
    /// identifier. An identifier that is itself a template instance, as
    /// older compilers wrote them, is not read.
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

    /// `Q` and a position before it, in base 26 written with upper-case
    /// letters for its higher digits and a lower-case one for its last.
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

    /// Template arguments, through the `Z` that closes them.
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

    /// The letter that says what kind of type begins here, past its
    /// modifiers and any back reference to it.
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

    /// A template argument's value.
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

    /// A value within an array or struct literal, whose own type is not
    /// written: one that is itself an associative array is not read.
    fn element_value(&mut self) -> Option<()> {
        self.value(false)
    }

    /// `NAN`, `INF`, `NINF`, or hexadecimal digits with an exponent, each
    /// part possibly negative.
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

    /// Type modifiers: const, immutable, shared, and inout.
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

            // Modifiers, arrays, and pointers.
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
                reader.text[target].is_ascii_alphabetic().then_some(())
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

    /// A function type with its return type.
    fn function(&mut self) -> Option<()> {
        self.function_without_return()?;
        self.read_type()
    }

    /// A calling convention, attributes, parameters, and how the
    /// parameters close.
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
                    // Scope, and return.
                    self.eat(b'M');
                    if self.starts_with(b"Nk") {
                        self.at += 2;
                    }

                    if matches!(self.peek()?, b'I' | b'J' | b'K' | b'L') {
                        // A storage class: in, out, ref, or lazy. `I` also
                        // begins an identifier's type, which is read when
                        // no type follows it.
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

/// The letters of D's basic types.
const BASIC_TYPES: &[u8] = b"vghstiklmfdeopjqrcbauwn";

/// The second letters of a function's attributes: pure, nothrow, ref,
/// property, trusted, safe, nogc, return, scope, and live.
const ATTRIBUTES: &[u8] = b"abcdefijlm";

/// Whether a byte begins a function type: D's, C's, Windows', Pascal's,
/// C++'s, or Objective-C's calling convention.
const fn call_convention(byte: u8) -> bool {
    matches!(byte, b'F' | b'U' | b'W' | b'V' | b'R' | b'Y')
}
