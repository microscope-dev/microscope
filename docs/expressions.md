# Expressions

microscope reads expressions in one language for every program it debugs,
whether it is written in C, C++, Rust, Go, Zig, Odin, Fortran, D, Nim, or
Ada. The language is small and exact, and the same everywhere: it has no
per-language parsers, type checkers, or compilers behind it.

Arithmetic gives the mathematically true answer, never a wrapped or
promoted one, and bit operations keep their operand's width. Nothing converts
implicitly except where this page says so, and a value the program cannot
provide, such as an optimized-out variable, is reported as unavailable rather
than guessed.

Every example on this page runs as one of microscope's tests, against small
worlds of variables. Each row reads `expression => outcome`. An outcome is a
value and its type, `value : type`; `reads as` and the expression's normal form,
the spelling microscope prints it in; `error` and the kind of error with the
text it points at; or `unavailable` and the operand whose value the program
could not provide.

## Names

A name is `[A-Za-z_][A-Za-z0-9_]*`. Names may be qualified with `::`, and
`::name` starts from the outermost scope. Backticks quote any other name,
including one with `/`, `-`, spaces, or angle brackets, and one that is
otherwise a keyword.

A dotted name such as Go's `main.counter` names a global when one has that
whole name; otherwise the dots select members. The longest name the frame
knows is taken first.

`$name` reads a register of the selected frame, such as `$rax` or `$rip`, and
`$pc`, `$sp`, and `$fp` name the program counter, stack pointer, and frame
pointer.

`$task` is the id of the selected task, or of the task the selected thread
runs, an exact integer: in Go, a goroutine's, so a breakpoint's condition
`$task == 7` stops only in goroutine 7. Where the program has no tasks, or microscope cannot tell which one
a thread runs, `$task` is refused; a thread between tasks, such as one idle
in Go's scheduler, has none, and `$task` is unavailable there.

`$future` is the future of the selected frame's async function or block,
which holds the variables it keeps across its awaits and the state it is in.
A frame that runs no future has none.

```microscope-example
ns::counter               => reads as `ns::counter`
::counter                 => reads as `::counter`
`github.com/acme/pkg.x`   => reads as `` `github.com/acme/pkg.x` ``
`Vec<i32>`::len           => reads as `` `Vec<i32>`::len ``
`as` + 1                  => reads as `` `as` + 1 ``
main.counter              => reads as `main.counter`
$rip                      => reads as `$rip`
naïve                     => error syntax at `ï`
$                         => error syntax at `$`
```

A name that several variables share is ambiguous, and the error lists how to
qualify each. Typedefs and qualifiers such as `const` keep their names but
behave as the types they stand for, and a C++ reference stands for what it
refers to. In a caller's frame, registers hold what unwinding recovered, and
one it could not recover is unavailable.

A word that C reserves for a type, such as `long`, `int`, `class`, or
`const`, is a name wherever a type cannot be, since a Go, Rust, or Zig
program may name a variable or member with it: `long * 2` multiplies the
variable `long`. Inside a cast's parentheses, after `as`, and in `sizeof`, the
word is the type.

```microscope-example
world: memory
s.a                    => 5 : int
ptr->b                 => 7 : long int
$rip                   => 4198400 : u64
$pc                    => 4198400 : u64
missing                => error unknown-name at `missing`
`s`.a                  => 5 : int
count + 1              => 13 : integer
count << 1             => 24 : counter_t
limit * 2              => 200 : integer
first                  => 11 : int
&first == &arr[0]      => true : bool
$rbp                   => unavailable at `$rbp`
$rbp + 1               => unavailable at `$rbp`
$nope                  => error unknown-name at `$nope`
twice                  => error ambiguous-name at `twice`
$task                  => 7 : integer
$task == 7             => true : bool
$future.a              => 5 : int
long                   => 3 : int
long * 2               => 6 : integer
words.int + words.class => 10 : integer
(long)-1               => -1 : long int
sizeof(long)           => 8 : integer
```

```microscope-example
world: scalars
$task                  => error unsupported at `$task`
$future                => error unknown-name at `$future`
```

Where a system call stopped the thread, as at a `catch syscall` stop, `$arg0`
to `$arg5` are its arguments as its registers held them, `$ret` is what it
returned, a negative errno when it failed, and `$path` is the path it names.
Each is an exact integer, or text for `$path`. Before the call returns,
`$ret` is refused, and so is `$path` for a call that names no path. A stop of
any other kind has none of them.

```microscope-example
world: syscall
$ret == -2             => true : bool
$arg0                  => 18446744073709551516 : integer
(int)$arg0             => -100 : int
$arg1                  => 2147287040 : integer
$path == "/etc/hosts"  => true : bool
$path                  => "/etc/hosts" : string
$arg6                  => error unknown-name at `$arg6`
```

```microscope-example
world: scalars
$ret                   => error unsupported at `$ret`
$arg0                  => error unsupported at `$arg0`
```

## Literals

Integers are decimal, or `0x`, `0o`, or `0b` prefixed, and may contain `_`.
A suffix naming a built-in type gives a literal that type: `255u8`, `1_i64`,
`3usize`. A literal without a suffix is an exact integer. The suffix `n`
writes a JavaScript BigInt, `12n`, which computes only with JavaScript
values (see JavaScript values).

A leading zero is refused, because C reads `017` as octal and other languages
read it as decimal. C suffixes such as `UL` are refused too; cast instead.

Floats have a fraction or an exponent: `1.5`, `1e9`, `2.5e-3`. They are
binary64 unless suffixed `f32`, and a float never ends in `.`, so `a[1..4]`
is a range. `'a'` is a character's code point, and `"text"` is a string,
which compares with the program's text. Both take the escapes `\n`, `\r`,
`\t`, `\0`, `\\`, `\'`, `\"`, `\xHH`, and `\u{H…}`.

`true`, `false`, and `null` mean what they say, and `nil` is a second
spelling of `null`, which prints as `null`. `nullptr` and `NULL` are refused
with a hint to write `null`: C's `NULL` is a macro, and the two spellings
are enough for a condition to read the same in every language.

A tuple is written `(a, b)`, `(a,)` for one value, or `()` for none, as
Python and Rust write one; `(a)` only groups. In brackets, `m[a, b]` is
`m[(a, b)]`, as Python reads it. A tuple is only a map's key (see Maps):
it is no value of its own, so it is not printed, compared, or computed
with, and a view's expressions write none.

```microscope-example
0x2a + 0b1010_1010 + 0o17 => reads as `42 + 170 + 15`
0xffu8                    => reads as `255u8`
1_000_i64                 => reads as `1000i64`
-12n                      => reads as `-12n`
1.5n                      => error syntax at `1.5n`
2.5f32 + 1e3              => reads as `2.5f32 + 1000.0`
'\x41' == 'A'             => reads as `'A' == 'A'`
"tab\tquote\""            => reads as `"tab\tquote\""`
017                       => error syntax at `017`
17UL                      => error syntax at `17UL`
1.5u8                     => error syntax at `1.5u8`
1e400                     => error syntax at `1e400`
340282366920938463463374607431768211456 => error syntax at `340282366920938463463374607431768211456`
'ab'                      => error syntax at `'a`
nil                       => reads as `null`
nullptr                   => error syntax at `nullptr`
( 1 ,2 )                  => reads as `(1, 2)`
(a,)                      => reads as `(a,)`
()                        => reads as `()`
m[1, (2, "b")]            => reads as `m[(1, (2, "b"))]`
m[1, 2:3]                 => error syntax at `:`
```

```microscope-example
world: scalars
0x2a                   => 42 : integer
255u8                  => 255 : u8
256u8                  => error arithmetic at `256u8`
-128i8                 => -128 : i8
-129i8                 => error arithmetic at `-129i8`
'a' + 1                => 98 : integer
2.5f32                 => 2.5 : f32
1e3                    => 1000.0 : f64
3usize                 => 3 : u64
```

## Operators

From loosest to tightest:

| Operators | Grouping |
|---|---|
| `=` `+=` `-=` `*=` `/=` `%=` `&=` `\|=` `^=` `<<=` `>>=` | right |
| `?:` | right |
| `\|\|` | left |
| `&&` | left |
| `==` `!=` `<` `<=` `>` `>=` | none |
| `\|` | left |
| `^` | left |
| `&` | left |
| `<<` `>>` | left |
| `+` `-` | left |
| `*` `/` `%` | left |
| `as` | left |
| prefix `-` `!` `~` `*` `&`, casts `(T)x`, `sizeof` | right |
| postfix `.` `->` `[]` | left |

Bit operations bind tighter than comparisons, so `x & 1 == 0` means
`(x & 1) == 0`, and comparisons do not chain. `.` selects a member, and also
selects through one pointer; `->` selects through a pointer as in C;
`t.0` selects a tuple's field. A member of an anonymous struct or union, or
of a base class, is selected by its own name, as C and C++ select it: a
record's own members hide its bases', and a name that two paths reach in
different objects is ambiguous. A member of a Go embedded field is promoted
as Go promotes it: `n.W` selects the `W` of the shallowest embedded field
that has one, through an embedded pointer too, and several at that depth
are ambiguous, the error naming each. `a[start..end]` is a half-open range
of an array or slice, and must be the whole expression; `a[start:end]`
slices (see Slices), and `m[key]` indexes a map (see Maps). `len(x)` is a
length, `cap(x)` a capacity, and `sizeof(x)` a size. The language does not
call functions.

```microscope-example
a+b*c                     => reads as `a + b * c`
x & 1 == 0                => reads as `x & 1 == 0`
(x & 1) == 0              => reads as `x & 1 == 0`
x & (1 == 0)              => reads as `x & (1 == 0)`
1 << 70 >> 68             => reads as `1 << 70 >> 68`
a - (b - c)               => reads as `a - (b - c)`
a ? b : c ? d : e         => reads as `a ? b : c ? d : e`
-x as u8                  => reads as `-x as u8`
-(x as u8)                => reads as `-(x as u8)`
p->items[i + 1].0         => reads as `p->items[i + 1].0`
&(&x)                     => reads as `& &x`
a[1..4]                   => reads as `a[1..4]`
s[1:3]                    => reads as `s[1:3]`
s[:n]                     => reads as `s[:n]`
s[i + 1:]                 => reads as `s[i + 1:]`
s[:]                      => reads as `s[:]`
s[c ? 1 : 2:3]            => reads as `s[c ? 1 : 2:3]`
cap(s)                    => reads as `cap(s)`
1 < 2 < 3                 => error syntax at `<`
a[1..4] + 1               => error syntax at `a[1..4]`
s[1:2:3]                  => error syntax at `:`
f(x)                      => error syntax at `f(`
a +                       => error syntax at ``
```

## Arithmetic is exact

`+ - * / %` compute the true result. No operand's type limits it: two `u8`
values add to a number above 255, and subtracting from an unsigned value can
go below zero. Results range over every value a 128-bit integer of either
signedness holds, from −2^127 to 2^128 − 1; beyond that, and dividing by
zero, are errors. Division truncates toward zero and the remainder takes the
dividend's sign, as C, Rust, and Go agree.

To get a program's wrapping arithmetic, cast the result: `(u8)(uc + 10)`.

```microscope-example
world: scalars
uc + 10                => 260 : integer
(u8)(uc + 10)          => 4 : u8
u32v + u32v            => 8000000000 : integer
0 - u32v               => -4000000000 : integer
i32v * 1000000         => -123456000000 : integer
-7 / 2                 => -3 : integer
-7 % 2                 => -1 : integer
7 % -2                 => 1 : integer
u64v * u64v            => 340282366920938463426481119284349108225 : integer
u64v * u64v * 2        => error arithmetic at `u64v * u64v * 2`
0 - u64v * u64v        => error arithmetic at `0 - u64v * u64v`
1 / 0                  => error arithmetic at `1 / 0`
-u32v                  => -4000000000 : integer
flag + 1               => error type at `flag`
```

Comparisons are exact too, so signed and unsigned values compare as numbers.

```microscope-example
world: scalars
-1 < 1u32              => true : bool
i32v < u32v            => true : bool
u32v - 1 > 0           => true : bool
sc == -7               => true : bool
```

## Bit operations keep their width

`~ & | ^ << >>` work on a value's bits at the width of its type, and the
result keeps that type. When two typed operands meet, the wider one's type
wins, and between equal widths, the unsigned one's. An exact integer meeting
a typed one must fit its width, as either a signed or an unsigned value.
Exact integers alone behave as infinite two's complement.

`>>` is arithmetic for a signed value and logical for an unsigned one, and
`<<` drops the bits it shifts past the width. A shift's result has the type
of the value shifted. A shift by a negative amount, or by the width or more,
is an error.

```microscope-example
world: scalars
~uc                    => 5 : unsigned char
uc << 1                => 244 : unsigned char
uc >> 4                => 15 : unsigned char
sc >> 1                => -4 : signed char
uc & 0x0f              => 10 : unsigned char
uc | 0x100             => error arithmetic at `0x100`
i32v & 0xffff_ffff     => -123456 : int
uc << 8                => error arithmetic at `uc << 8`
1 << 70 >> 68          => 4 : integer
-1 << 1u32             => -2 : integer
~0                     => -1 : integer
-4 >> 1                => -2 : integer
uc << -1               => error arithmetic at `uc << -1`
sc >> 8                => error arithmetic at `sc >> 8`
u16v >> 15             => 1 : short unsigned int
uc & u16v              => 250 : short unsigned int
```

## Floating point

Floats compute in the widest format among their operands, and an integer
operand converts to the nearest value. x87 `long double`, half precision
(`_Float16`, `f16`), bfloat16 (`__bf16`), and quad precision (`__float128`,
`f128`) values compute exactly in their own formats, never rounded through a
double; half and bfloat16 together, neither of which holds the other's
values, compute as `f32`. Floats compare with integers exactly.

```microscope-example
world: scalars
f * 2                  => 3.0 : f32
f + d                  => 11.5 : f64
d / 4                  => 2.5 : f64
ld * 2                 => 2.5 : f80
ld + 1                 => 2.25 : f80
1.0 / 0.0              => inf : f64
f == 1.5               => true : bool
9007199254740993 == 9007199254740993.0 => false : bool
0.0 / 0.0              => NaN : f64
0.0 / 0.0 == 0.0 / 0.0 => false : bool
0.0 / 0.0 != 0.0 / 0.0 => true : bool
-d                     => -10.0 : f64
f & 1                  => error type at `f`
```

## Truth values

`!`, `&&`, `||`, and `?:` take truth values: booleans, or numbers and pointers,
which are true when nonzero. `&&`, `||`, and `?:` evaluate only what they
need, so `ptr != null && ptr->a > 3` never follows a null pointer. Booleans
are not numbers; convert one with `flag as u8`.

```microscope-example
world: memory
null_ptr != null && null_ptr->a > 3 => false : bool
ptr != null && ptr->a > 3          => true : bool
ptr != nil                         => true : bool
!ptr                               => false : bool
s.a > 3 ? 1 : 2                    => 1 : integer
false && gone > 0                  => false : bool
0 ? gone : 1                       => 1 : integer
ptr == null || gone > 0            => unavailable at `gone`
s && true                          => error type at `s`
```

## Pointers, arrays, and members

`*p` dereferences, `&x` takes an address, `p[i]` is `*(p + i)`, and `p + n`
moves by `n` elements. Only a value in memory has an address: one optimized
code keeps in a register, computes, or splits across several places has
none, though a member of it that lies in memory does. `p - q` counts the
elements between two pointers, which must be a whole number of elements
apart. Elements of a zero-sized type, such as Rust's `()`, all share one
address, so a pointer to one moves nowhere and two such pointers have no
count between them. Pointers compare with pointers, `null`, and `0`; to
compare an address with another number, cast the pointer. Arrays index by
each of their dimensions, and decay to a pointer to their first element in
arithmetic. A slice's index is checked against its length when the
expression runs, as is an index of an array the program bounds at run
time: a C variable-length array, a Fortran array's descriptor, or an Ada
array of an unconstrained type. A C flexible array member's length is
not described, so its index is not checked. A value a view presents as another value
([Views](views.md)), as Rust's `Arc` and C++'s `std::unique_ptr` present
what they point to, dereferences to that value: `*arc` is what the `Arc`
shares, and `*p` on one the view presents as empty, such as a null
`unique_ptr`, is an error.

```microscope-example
world: memory
*ip                    => 22 : int
ip[1]                  => 33 : int
*(ip + 2)              => 44 : int
&arr[3] - &arr[0]      => 3 : integer
ptr.a                  => 5 : int
(*ptr).b               => 7 : long int
arr[2]                 => 33 : int
*arr                   => 11 : int
*(arr + 1)             => 22 : int
m[1][2]                => 6 : int
m[1]                   => error type at `m[1]`
arr[4]                 => error bounds at `arr[4]`
items[1]               => 20 : int
items[5]               => unavailable at `items[5]`
len(items)             => 3 : integer
len(arr)               => 4 : integer
*null_ptr              => unavailable at `*null_ptr`
ptr != 0               => true : bool
ptr == 3               => error type at `ptr == 3`
(u64)ip - (u64)&arr[0] => 4 : integer
ip - arr               => 1 : integer
(int*)((u8*)ip + 1) - ip => error arithmetic at `(int*)((u8*)ip + 1) - ip`
&ep[5] == ep           => true : bool
ep - ep                => error type at `ep - ep`
arr + 1 == ip          => true : bool
vp + 1                 => error type at `vp`
*vp                    => error type at `vp`
*s                     => error type at `s`
*shared                => 22 : int
*shared + shared.count => 24 : integer
s.missing              => error type at `missing`
s->a                   => error type at `s`
&r                     => error not-an-lvalue at `r`
gone + 1               => unavailable at `gone`
```

## Slices

`x[start:end]` is the part of an array, a slice, or text from index `start`
up to, not including, `end`. A bound left out is the beginning or the end:
`x[:end]`, `x[start:]`, and `x[:]`. When the expression runs, `0 <= start
<= end <= len(x)` must hold, or it is a bounds error. Unlike Go, a slice's
room beyond its length is not reachable this way, since what is there is
not part of its value; Go's `x[low:high:max]` is not supported.

Slicing an array or slice is the range of its elements `x[start..end]`, and
like a range must be the whole expression. Slicing text, which is a
language's string or what a character pointer points to, is text: a string
that compares with `==` and `!=`, has a length, slices again, and prints.

```microscope-example
world: memory
items[1:3]             => range 1..3
items[:2]              => range 0..2
items[1:]              => range 1..3
arr[:]                 => range 0..4
arr[2:2]               => range 2..2
items[2:1]             => error bounds at `items[2:1]`
items[1:4]             => error bounds at `items[1:4]`
items[1:3] == 1        => error type at `items[1:3]`
name[1:3]              => "el" : string
name[:4] == "hell"     => true : bool
name[1:] != "ello"     => false : bool
len(name[2:])          => 3 : integer
name[2:][1:]           => "lo" : string
"hello"[1:3] == "el"   => true : bool
name[3:9]              => error bounds at `name[3:9]`
s[0:1]                 => error type at `s`
ptr[0:1]               => error type at `ptr`
```

## Maps

`m[key]` is the value a map holds for a key, when a view presents `m` as a
map ([Views](views.md)): Go's maps, and the maps of the C++, Rust, and Zig
standard libraries. A key is a number, truth value, pointer, or string, or
text the program holds, and finds the entry whose key `==` would call equal
to it, so numbers compare exactly and `m[2.0]` finds the key `2`. A key of a
type the map's keys cannot equal, such as a string in a map of integers, is
a type error, and a key the map does not hold is a missing-key error, never
a zero value. The entries are searched in the view's order, as far as the
inspection's budget allows. A map is never indexed by position.

A tuple key finds an entry whose key is a tuple of as many elements, each
equal to the tuple's item at its position: `grid[(1, 2)]`, `grid[1, 2]`,
or `grid[cell]`, where `cell` is a tuple the program holds. Its elements
may be tuples themselves, nested at most 8 deep, with at most 64 keys in
one tuple. A tuple is a Rust or Zig tuple, a C++ `std::pair` or
`std::tuple`, or a value another view presents as one ([Views](views.md)).
A map whose keys are tuples of another length, or are not tuples, is a type
error: a struct's members are not matched by position, so a Go, C, C++, or
Rust struct key, a Rust tuple struct among them, is not found by a tuple.

```microscope-example
world: memory
squares[3]             => 9 : int
squares[3u8]           => 9 : int
squares[2.0]           => 4 : int
squares[4]             => error missing-key at `squares[4]`
squares["3"]           => error type at `squares["3"]`
ages["bob"]            => 41 : int
ages[name]             => error missing-key at `ages[name]`
ages[name[:3]]         => error missing-key at `ages[name[:3]]`
ages["ann"] + squares[1] => 31 : integer
ages[s]                => error type at `s`
ages[arr]              => error type at `arr`
grid[(1, 2)]           => 12 : int
grid[2, 3]             => 23 : int
grid[cell]             => 23 : int
grid[(1, 2.0)]         => 12 : int
grid[(3, 1)]           => error missing-key at `grid[(3, 1)]`
grid[1]                => error type at `grid[1]`
grid[(1, 2, 3)]        => error type at `grid[(1, 2, 3)]`
grid[(1, "2")]         => error type at `grid[(1, "2")]`
squares[1, 2]          => error type at `squares[1, 2]`
spots[(1, 2)]          => error type at `spots[(1, 2)]`
arr[1, 2]              => error type at `1, 2`
m[(1, 2)]              => error type at `(1, 2)`
(1, 2)                 => error type at `(1, 2)`
(1, 2) == cell         => error type at `(1, 2)`
```

## Casts

`(T)x` and `x as T` convert a value. `T` may be a built-in type (`iN` and
`uN` for N from 1 to 128, `isize`, `usize`, `f32`, `f64`, `bool`), a type the
program defines, a C base type written with its words in any order, or a
`struct`, `union`, `enum`, or `class` tag. A C base type the program's debug
information does not describe, such as `unsigned char` in a Go program, is
the one the target's C compiler would lay out. `const`, `volatile`, and `mut` are
accepted and ignored. A pointer type is `T*` inside a cast's parentheses,
where `(*p)` dereferences, and `*T` after `as`, where a trailing `*` would
multiply.

A program type is named by what it is, not by how its compiler spelled it.
Its outer namespaces, modules, and packages may be left off, as may inline
namespaces such as libc++'s `std::__1`, and a template's trailing arguments,
which C++ fills with defaults: `` std::`vector<int>` `` names
`std::vector<int, std::allocator<int> >`. A name that fits several different
types is ambiguous, and the error lists them.

`(name) - 1` subtracts when `name` is a value and casts `-1` when it is a
type, and the two readings group the rest of the expression differently:
`(n) - a * b` is `n - (a * b)`, while `(T) - a * b` is `((T)(-a)) * b`. Such a
parenthesized name before `-`, `*`, or `&` is read whichever way the name
is, as C reads it, with a value in scope winning. Anything else after the
parentheses is unambiguous: `(n) + 1` always adds, and `(T)x` always casts.
An expression may hold at most four such names.

```microscope-example
(unsigned long)-1         => reads as `(unsigned long)-1`
(long unsigned)-1         => reads as `(unsigned long)-1`
(const char*)p            => reads as `(char*)p`
(struct node*)p           => reads as `(struct node*)p`
(T)(-1)                   => reads as `(T)(-1)`
(T)x                      => reads as `(T)x`
x as *const u8            => reads as `x as *u8`
x as unsigned long + 1    => reads as `x as unsigned long + 1`
x as T * 2                => reads as `x as T * 2`
(n) + 1                   => reads as `n + 1`
(n) - 1                   => reads as `(n) - 1`
sizeof(int*)              => reads as `sizeof(int*)`
(int)                     => error syntax at ``
(a)-(b)-(c)-(d)-(e)-f     => error limit at `(`
```

Casting an integer truncates its two's complement. Casting a float to an
integer rounds toward zero and saturates at the type's bounds; a NaN cannot be
cast. Integers and pointers convert into each other as addresses, and anything
with a truth value casts to `bool`. A cast cannot reinterpret a whole record;
reinterpret its storage through a pointer instead. Casting a C++ object to one
of its base classes is that base's part of it, as `static_cast` makes it, and
is ambiguous when the object holds several. Casting a value to the type it
already has changes nothing.

```microscope-example
world: scalars
(short)-70000          => -4464 : short int
(u8)-1                 => 255 : u8
-70000 as i16          => -4464 : i16
(int)-123456000000     => 1098051584 : int
2.9 as i32             => 2 : i32
-2.9 as u8             => 0 : u8
1e10 as i32            => 2147483647 : i32
(f32)i32v              => -123456.0 : f32
(unsigned long)sc      => 18446744073709551609 : long unsigned int
(long unsigned int)sc  => 18446744073709551609 : long unsigned int
d as bool              => true : bool
(bool)0.1              => true : bool
flag as u8 + 1         => 2 : integer
(u1)3                  => 1 : u1
(i1)1                  => -1 : i1
(u128)-1               => 340282366920938463463374607431768211455 : u128
-1e40 as i128          => -170141183460469231731687303715884105728 : i128
(0.0 / 0.0) as i32     => error arithmetic at `(0.0 / 0.0) as i32`
(nothing)1             => error unknown-name at `nothing`
```

```microscope-example
world: memory
*(long*)&s.b           => 7 : long int
*(&s.b as *long)       => 7 : long int
(u8*)vp + 1 - (u8*)vp  => 1 : integer
*(int*)ptr             => 5 : int
(S*)ip == (S*)&arr[1]  => true : bool
(S)s                   => {…} : S
(S)ptr                 => error type at `(S)ptr`
(long)s                => error type at `(long)s`
(count) - 1            => 11 : integer
(counter_t) - 1        => -1 : counter_t
(counter_t) - 1 * 2    => -2 : integer
(count) - 1 * 2        => 10 : integer
(Color)1               => GREEN : Color
(Color)7               => 7 : Color
((Shape)tile).id       => 7 : int
((Named)tile).tag      => 2 : long int
(Shape)tile == 7       => error type at `(Shape)tile == 7`
((Shape)twice_shaped).id => error ambiguous-name at `((Shape)twice_shaped)`
(Tile)s                => error type at `(Tile)s`
(short)70000           => 4464 : short
(unsigned short)-1     => 65535 : unsigned short
(signed char)200       => -56 : signed char
(long long unsigned)-1 => 18446744073709551615 : unsigned long long
(double)1              => 1.0 : double
(float)0.1 == 0.1 as f32 => true : bool
*(unsigned char*)&s.a  => 5 : unsigned char
sizeof(long double)    => 16 : integer
(unsigned long)-1      => 18446744073709551615 : unsigned long
```

## Enumerations

An enumerator's name means its value. Next to a value of an enumeration, a
bare enumerator name is found among that enumeration's enumerators, so
`color == RED` works even where `RED` alone is ambiguous. An integer casts to
an enumeration, whether or not an enumerator has its value.

```microscope-example
world: memory
color                  => BLUE : Color
color == BLUE          => true : bool
color == Color::GREEN  => false : bool
color + 1              => 3 : integer
(f64)BLUE              => 2.0 : f64
sign < POSITIVE        => true : bool
NEGATIVE < POSITIVE    => true : bool
-(Small::TWO << 1)     => -4 : integer
Small::HIGH << 1       => 0 : Small
RED                    => error ambiguous-name at `RED`
color == RED           => false : bool
light == RED           => false : bool
light == Light::AMBER  => true : bool
```

A tagged union, such as a Rust enum, equals the bare name of the variant it
holds, and a member of a variant it does not hold is unavailable.

```microscope-example
world: memory
maybe == Some          => true : bool
maybe != None          => true : bool
nothing == Some        => false : bool
None == nothing        => true : bool
maybe.Some.0           => 5 : int
nothing.Some.0         => unavailable at `nothing.Some`
maybe == Other         => error unknown-name at `Other`
maybe == 1             => error type at `maybe == 1`
```

## Text

A string literal compares with the program's text: a `char*`, a character
array, or a language's string type. Text that could not be read to its end
does not compare at all, unless what was read already differs.

```microscope-example
world: memory
name == "hello"        => true : bool
name != "help"         => true : bool
buf == "abc"           => true : bool
name < "hello"         => error type at `name < "hello"`
partial == "hello"     => unavailable at `partial`
partial == "xyz"       => false : bool
"hello"                => error type at `"hello"`
```

## Sizes and lengths

`sizeof(x)` and `sizeof(T)` give a size in bytes without reading anything.
`len(x)` gives an array's or slice's element count, the first dimension's
for an array of several, or the length in bytes of text: a language's
string, or what a character pointer points to. An array of characters is an
array, so its length is its element count.

`cap(x)` gives how many elements `x` has room for: an array's element count,
the capacity a slice records, as Go's slices do, or what the `capacity`
field of the view that presents `x` says, as the views of Go's channels, of
C++'s vectors and strings, and of Rust's and Zig's lists do. Anything else,
a Go map among them, has no capacity.

```microscope-example
world: memory
sizeof(s)              => 16 : integer
sizeof(S)              => 16 : integer
sizeof(struct S)       => 16 : integer
sizeof(int)            => 4 : integer
sizeof(arr)            => 16 : integer
sizeof(u128)           => 16 : integer
sizeof(ptr)            => 8 : integer
sizeof(1)              => error type at `sizeof(1)`
len(name)              => 5 : integer
len(buf)               => 8 : integer
len(s)                 => error type at `s`
cap(arr)               => 4 : integer
cap(spare)             => 5 : integer
len(spare)             => 3 : integer
cap(items)             => error type at `items`
cap(s)                 => error type at `s`
cap(1)                 => error type at `1`
```

## Python values

In a Python frame, a name is the frame's variable, then a global of its
module, then a built-in, as Python finds it, and `::name` skips to the
native program's globals. A Python variable has no type of its own: it holds
whatever object it was last given, so each operation finds its operands'
types as it runs. A value of any type binds, and an operation its value's
type does not allow is refused when it runs.

`x[i]` indexes a list or tuple, from its end when `i` is negative, and
`d[key]` finds a dict's key as Python's `==` would: numbers compare exactly,
so `d[1]`, `d[1.0]`, and `d[true]` find one key, text equals only text, and
`d[None]` finds a `None` key. A tuple key, written `d[(1, "a")]` or
`d[1, "a"]`, or a `tuple` the program holds, finds a tuple key item by
item, as Python compares tuples. A key taken from the program is that very
object, which the dict finds as Python does, by identity before value: a
NaN finds itself, though it equals nothing. A key of a type whose own
`__eq__` decides what it equals, such as an instance of a class, is refused
where the lookup would have to compare with it, as is a key the program
holds of such a type. A `bytes` or `frozenset` key is not supported yet. An
index past the end is a bounds error, and a key the dict does not hold is a
missing-key error. `len(x)` is the length of a `str` in characters, of
`bytes` in bytes, and of a list, tuple, dict, set, or frozenset in items. A
value is true as Python says: `None`, zero, and empty text and collections
are false.

`x.name` reads an attribute as Python's `object.__getattribute__` finds it:
a slot or other data descriptor of the object's class first, then the
object's own dict, then an attribute of its class or the classes it derives
from. `module.name` is a global of the module, and `Class.name` an
attribute of the class. An attribute the object does not have is a
missing-key error. A property, a method, any other descriptor whose code
gives its value, a class's own `__getattribute__`, and a `__getattr__` that
would find a missing attribute are refused.

An object of a class that derives from a built-in type, such as a
`namedtuple`, holds that type's value, and `x[i]` and `len(x)` use it while
the class keeps the built-in type's own `__getitem__` and `__len__`. A class
that defines its own, a dict whose class defines `__missing__` and lacks the
key, and any other object whose type defines them are refused; an object
whose type defines neither is a type error, as in Python.

Numbers follow CPython's rules. `True` and `False` are the `int`s 1 and 0,
and `&`, `|`, and `^` of two `bool`s give a `bool`. An `int` is exact, and one
too large for 128 bits is shown whole but not computed with. `/` of two
`int`s is the `float` nearest their exact quotient, as CPython rounds it.
`%` floors, so its result has the divisor's sign, for `float`s too. Dividing
or taking `%` by zero is an arithmetic error, as Python's
`ZeroDivisionError` is, and so is a negative shift count, Python's
`ValueError`. The expressions have no `//`. A computed result is shown
as Python's `repr` writes it, with the type `int`, `float`, or `bool`: `7.0`,
`7e+16`, `inf`, `False`. Values of different types are unequal, and only
numbers, `str`, and `bytes` are ordered. Lists, dicts, and objects are
neither compared nor computed with, since only running their types' code
could say what those mean, and microscope never runs code in the program:
not `__eq__`, properties, or calls. Such an operation is a type error only
where Python's would be one, as `None + 1` is; where a type's code could give
a value, as `"a" + "b"` does, it is refused as unsupported.

In any frame of a CPython process, `(py)x` casts an address to the Python
object there, so that a C extension's native frame reaches into the objects
its `PyObject *` locals point to as a Python frame would: `((py)args).x`,
`len((py)list)`. A Python value casts to itself, and an address is a pointer
or an integer; anything else is a type error. A null address, or one that
holds no object of a type microscope reads, is unavailable. A `PyObject *`
pointer itself already shows the object it points to.

```microscope-example
world: python
count                  => 7 : int
count + 1              => 8 : int
count * ratio          => 3.5 : float
-count                 => -7 : int
count & 3              => 3 : int
True + 1               => 2 : int
count / 2              => 3.5 : float
count % 4              => 3 : int
-count % 3             => 2 : int
count % -2             => -1 : int
-ratio % 2             => 1.5 : float
True & False           => False : bool
count / 2 * 2          => 7.0 : float
count * 1e15           => 7000000000000000.0 : float
count * 1e16           => 7e+16 : float
ratio * 1e308 * 10     => inf : float
count / 0              => error arithmetic at `count / 0`
ratio / 0              => error arithmetic at `ratio / 0`
count << -1            => error arithmetic at `count << -1`
count == 7             => true : bool
count > 6.5            => true : bool
count == name          => false : bool
count < name           => error type at `count < name`
name == "ann"          => true : bool
data == "ab"           => false : bool
nothing == None        => true : bool
count != None          => true : bool
numbers[0]             => 10 : int
numbers[-1]            => 30 : int
numbers[3]             => error bounds at `numbers[3]`
numbers[0] + numbers[1] => 30 : int
pair[1]                => 'two' : str
point.x                => 3 : int
point.x * point.y      => 12 : int
point.z                => error missing-key at `point.z`
((py)&point).y         => 4 : int
(py)count              => 7 : int
(py)ratio              => 0.5 : float
(py)1.5                => error type at `1.5`
ages["bob"]            => 41 : int
ages["eve"]            => error missing-key at `ages["eve"]`
squares[2]             => 4 : int
squares[2.0]           => 4 : int
squares[true]          => 1 : int
squares[count - 4]     => 9 : int
labels[None]           => 'none' : str
labels[1]              => 'one' : str
labels["one"]          => error missing-key at `labels["one"]`
grid[(1, 2)]           => 'a' : str
grid[1, 2]             => 'a' : str
grid[cell]             => 'a' : str
grid[1.0, True + 1]    => 'a' : str
grid[(1, (2, 3))]      => 'b' : str
grid["ann", count - 4] => 'c' : str
grid[()]               => 'd' : str
grid[1,]               => 'e' : str
grid[(2, 1)]           => error missing-key at `grid[(2, 1)]`
grid[(1, 2, 3)]        => error missing-key at `grid[(1, 2, 3)]`
grid[1]                => error missing-key at `grid[1]`
numbers[1, 2]          => error type at `numbers[1, 2]`
numbers[count - 6]     => 20 : int
ages[name] < 40        => true : bool
name[0]                => error unsupported at `name[0]`
count[0]               => error type at `count[0]`
len(name)              => 3 : integer
len(numbers)           => 3 : integer
len(ages)              => 2 : integer
len(count)             => error type at `len(count)`
!empty                 => true : bool
numbers && count       => true : bool
!point                 => error unsupported at `point`
numbers == numbers     => error unsupported at `numbers == numbers`
big                    => 1361129467683753853853498429727072845824 : int
big + 1                => error arithmetic at `big`
*count                 => error type at `count`
count->real            => error type at `count`
```

## JavaScript values

In a Node.js process, a JavaScript value follows ECMAScript's operators, and
a literal the expression writes is a JavaScript value: `1` is a Number and
`1n` a BigInt. A Python value and a JavaScript value are never combined.

Every Number is a double. `/` by zero gives an infinity or NaN, `%` has the
dividend's sign, and `&`, `|`, `^`, `~`, `<<`, and `>>` work on the Number's
32-bit integer, with a shift count's low five bits. A BigInt is exact: `/`
and `%` truncate toward zero, dividing by `0n` is an arithmetic error, as
JavaScript's `RangeError` is, and a BigInt with a Number in arithmetic is a
type error, as JavaScript's `TypeError` is. A BigInt beyond 128 bits is
shown whole but not computed with. `true` and `false` count as 1 and 0,
`null` as 0, and `undefined` as NaN.

`==` is JavaScript's loose equality: `null == undefined`, a truth value
equals the number it counts as, `1n == 1`, and NaN equals nothing.
Comparing a BigInt with a Number is exact. Strings are ordered by their
UTF-16 code units, as JavaScript orders them, so `"\u{1F600}" < "\u{FFFF}"`.
`0`, `-0`, NaN, `0n`, `""`, `null`, `undefined`, and `false` are false, and
every object is true, an empty array or Map too.

microscope refuses, as unsupported, what would run the program's code or
make a new string: `+` with a string, arithmetic on an object, which calls
its `valueOf`, a string compared with a number, and `==` of two objects,
which compares their identities. `x[i]` takes a string, a BigInt, or a
Number that is an integer below 2^53 in magnitude.

A computed value is shown as Node's `util.inspect` writes it, with the type
`number`, `bigint`, or `boolean`: `1.764e+23`, `Infinity`, `NaN`, `-0`, and a
BigInt with its `n`, `-24691357802469135780n`.

## Assignment

In the console and with `set var`, `=` and the compound operators (`+=`,
`<<=`, …) assign, and an assignment must be the whole expression. The value
must fit the target's type exactly; to store a wrapped or rounded value, cast
it first. Only numbers, truth values, enumerations, and pointers are
assigned. The result is the target read again. Hovering, watching, and
breakpoint conditions cannot assign.

```microscope-example
world: scalars
assign: uc = 7         => 7 : unsigned char
assign: uc -= 10       => 240 : unsigned char
assign: uc += 10       => error assignment at `uc += 10`
assign: uc = 256       => error assignment at `256`
assign: uc = (u8)256   => 0 : unsigned char
assign: uc = -6        => error assignment at `-6`
assign: flag = 1       => true : _Bool
assign: flag = 2       => error assignment at `2`
assign: d = 3          => 3.0 : double
assign: f = 0.1        => error assignment at `0.1`
assign: f = 0.5        => 0.5 : float
assign: f = 0.1f32     => 0.1 : float
assign: i32v = 2.0     => 2 : int
assign: i32v = 2.5     => error assignment at `2.5`
uc = 7                 => error mode at `uc = 7`
assign: uc + 1 = 2     => error not-an-lvalue at `uc + 1`
assign: uc = sc = 1    => error mode at `sc = 1`
```

```microscope-example
world: memory
assign: color = GREEN  => GREEN : Color
assign: ptr = 0        => 0x0 : S*
assign: ptr = 1.5      => error type at `1.5`
assign: s = s          => error type at `s`
assign: r = 3          => 3 : int
```

In a Python frame, `=` and the compound operators give a variable of the
frame a new value, as `frame.f_locals[name] = value` would. The value is an
integer, a float, a truth value, `None`, or text that microscope computes,
or an object another name of the frame already holds: `held = other` makes
both names hold the same list. microscope's agent stores it on the frame's
thread, which runs alone until the agent reports the store. The stop is then
published again, under a new stop, with the variable's frame selected. The
result is the variable read again. None of the program's code runs. The
object the variable held before is released once its thread runs on.

```text
(microscope) set var count = 41
(int) count = 41
(microscope) set var count += 1
(int) count = 42
(microscope) set var name = "after"
(str) name = 'after'
(microscope) set var held = other
(list) held = [1, 2]
```

The assignment is made only where it can be exact. That means CPython 3.14
or later, built with the GIL, and a stop that the agent made: a Python
breakpoint, the end of a Python step, or a Python exception. The frame's
thread must also be the only thread the interpreter runs. Anywhere else, the
assignment is refused and nothing runs:

- Another thread could be holding the objects it changes.
- A free-threaded interpreter's threads can reach those objects at any time.
- A native stop leaves the thread somewhere the agent cannot act.

A global, an attribute, an item, text that is not UTF-8, and a value of one
of the program's native types are refused too.

```microscope-example
world: python
assign: point.x = 1    => error not-an-lvalue at `point.x`
assign: name = "\xff"  => error assignment at `"\xff"`
assign: count = &point => error assignment at `&point`
```

In a JavaScript frame, `=` and the compound operators give a variable a
number, a BigInt, text, `true` or `false`, `null`, `undefined`, or the object
another variable of the frame holds. V8's debugger makes the change, at a
stop where V8 paused the thread, while every thread runs and none of the
program's JavaScript does; the stop is then published again, under a new
stop, and the result is the variable read again from memory. An integer
must be exactly a JavaScript number, or written as a BigInt such as `5n`.
[Assignments](languages.md#assignments) in the Node section lists what V8
refuses.

A register is assigned the same way. It holds an unsigned number of its own
width, which the value must fit, or a pointer. Only the innermost frame's
registers belong to the thread; a caller's are what unwinding recovered, and
are refused. Assigning `$pc` moves where the thread resumes: the stop is
published again at its new place, under a new stop.

```microscope-example
world: memory
assign: $rsp = 0x7ffe0010    => 2147352592 : u64
assign: $sp -= 16            => 2147352560 : u64
assign: $pc = 0x401010       => 4198416 : u64
assign: $rsp = (u64)-1       => 18446744073709551615 : u64
assign: $rsp = -1            => error assignment at `-1`
assign: $rsp = 1.5           => error assignment at `1.5`
assign: $rsp = s             => error type at `s`
assign: $rsp + 1 = 2         => error not-an-lvalue at `$rsp + 1`
assign: $rbp += 1            => unavailable at `$rbp`
$rsp = 1                     => error mode at `$rsp = 1`
```

## Limits

An expression may be at most 4096 bytes long, nest at most 64 deep, and have
at most 1024 parts.
