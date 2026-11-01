# Expressions

Use the same expression language in every program microscope debugs, whether
written in C, C++, Rust, Go, Zig, Odin, Fortran, D, Nim, or Ada. There are no
per-language parsers, type checkers, or compilers.

Arithmetic computes exact results without wrapping or promotion. Bit operations
keep their operand's width. Conversions are explicit except where this page
says otherwise. Values the program cannot provide, such as optimized-out
variables, are reported as unavailable.

The examples use small sets of variables and run as tests. Each row reads
`expression => outcome`, with one of these outcomes:

- `value : type`: the result and its type.
- `reads as`: the expression in the form microscope prints.
- `error`: the error kind and the text it points at.
- `unavailable`: the operand whose value the program could not provide.

## Names

A name is `[A-Za-z_][A-Za-z0-9_]*`. Names may be qualified with `::`, and
`::name` starts from the outermost scope. Backticks quote any other name,
including one with `/`, `-`, spaces, or angle brackets, and one that is
otherwise a keyword.

A dotted name such as Go's `main.counter` refers to a global with that full
name if one exists; otherwise the dots select members. microscope matches
the longest name known in the frame first.

`$name` reads a register of the selected frame, such as `$rax` or `$rip`, and
`$pc`, `$sp`, and `$fp` name the program counter, stack pointer, and frame
pointer.

`$task` is the selected task's id, or the id of the task running on the selected
thread, as an exact integer. In Go, it is a goroutine id: the breakpoint
condition `$task == 7` stops only in goroutine 7. `$task` is unsupported when
the program has no tasks or microscope cannot identify a thread's task. It is
unavailable when the thread is between tasks, such as when idle in Go's scheduler.

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

A name shared by several variables is ambiguous; the error lists how to
qualify each. Typedefs and qualifiers such as `const` keep their names but
behave as their underlying types. A C++ reference behaves as the value it
refers to. Registers in a caller's frame contain the values recovered by
unwinding; unrecovered registers are unavailable.

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

At a system-call stop, such as `catch syscall`, `$arg0` to `$arg5` contain the
arguments as held in registers. `$ret` is the return value (a negative errno
on failure), and `$path` is the path named by the call. These are exact
integers except for `$path`, which is text. `$ret` is unsupported before the
call returns, as is `$path` for a call without a path. None are supported at
other kinds of stop.

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

`true` and `false` are booleans. `null` and `nil` both denote null and print
as `null`. `nullptr` and `NULL` are rejected with a hint to use `null`.
C's `NULL` is a macro; the expression language uses the same null spellings
in every source language.

Write a tuple as `(a, b)`, `(a,)` for one value, or `()` for none, as in
Python and Rust; `(a)` only groups. In brackets, `m[a, b]` means
`m[(a, b)]`, as in Python. Tuples can only be map keys (see Maps), not
standalone values to print, compare, or compute with. View expressions
cannot contain tuples.

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
`(x & 1) == 0`. Comparisons do not chain.

`.` selects a member, including through one pointer; `->` selects through
a pointer as in C. `t.0` selects a tuple field. Members of anonymous structs,
unions, and base classes are accessible by their own names, as in C and C++.
A record's own members hide its bases' members. A name reached through two
paths into different objects is ambiguous.

Go embedded fields follow Go's promotion rules: `n.W` selects `W` from the
shallowest embedded field that has it, including through embedded pointers.
Multiple matches at that depth are ambiguous, and the error lists each.

`a[start..end]` is a half-open range of an array or slice and must be the
whole expression. `a[start:end]` slices (see Slices), and `m[key]` indexes
a map (see Maps). `len(x)` gives a length, `cap(x)` a capacity, and
`sizeof(x)` a size. Function calls are not supported.

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

`+ - * / %` compute exact results regardless of operand types: two `u8`
values can add to more than 255, and subtraction from an unsigned value can
go below zero. Results may range from −2^127 to 2^128 − 1, covering both
signed and unsigned 128-bit integers. Results outside that range and division
by zero are errors. Division truncates toward zero; the remainder has the
dividend's sign, as in C, Rust, and Go.

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

`~ & | ^ << >>` operate at the operand type's width and preserve that type.
With two typed operands, the result uses the wider type; at equal widths,
it uses the unsigned type. An exact integer combined with a typed operand
must fit that operand's width as either a signed or an unsigned value.
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

Floats compute in the widest operand format. Integer operands convert to the
nearest value. x87 `long double`, half precision
(`_Float16`, `f16`), bfloat16 (`__bf16`), and quad precision (`__float128`,
`f128`) compute in their own formats without intermediate rounding to double.
Half precision and bfloat16 together use `f32`, since neither format can
represent all of the other's values. Float comparisons with integers are exact.

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
moves by `n` elements. Only values in memory have addresses. A value kept in
a register, computed, or split across several locations has no address,
though a member stored in memory does.

`p - q` counts the elements between pointers, which must be a whole number
of elements apart. Elements of a zero-sized type, such as Rust's `()`, share
one address. Pointer arithmetic on them does not move the pointer, and
subtracting two such pointers is an error.

Pointers compare with pointers, `null`, and `0`. To compare an address with
another number, cast the pointer. Arrays index by dimension and decay to
pointers to their first elements in arithmetic.

Slice indices are checked against the length at evaluation time. The same
applies to arrays with runtime bounds: C variable-length arrays, Fortran
array descriptors, and Ada arrays of unconstrained types. C flexible array
members have no described length, so their indices are not checked.

A value presented as another value by a [view](views.md) dereferences to that
value. For example, Rust's `Arc` and C++'s `std::unique_ptr` present their
targets, so `*arc` gives the shared value. Dereferencing an empty presentation,
such as a null `unique_ptr`, is an error.

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

`x[start:end]` selects part of an array, slice, or text from index `start`
up to, but not including, `end`. Omitted bounds default to the beginning
or end: `x[:end]`, `x[start:]`, and `x[:]`. At evaluation time,
`0 <= start <= end <= len(x)` must hold; otherwise, the result is a bounds
error. Unlike Go, slicing cannot reach spare capacity beyond the length,
which is not part of the value. Go's `x[low:high:max]` is not supported.

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

`m[key]` looks up a key when a [view](views.md) presents `m` as a map. This
includes Go maps and C++, Rust, and Zig standard-library maps. A key can be
a number, boolean, pointer, string literal, or text from the program. Lookup
uses `==`, so numbers compare exactly and `m[2.0]` finds the key `2`.

An incompatible key type, such as a string in an integer-keyed map, is a
type error. A missing key is a missing-key error, not a zero value. Lookup
searches entries in the view's order within the inspection budget. Maps
cannot be indexed by position.

A tuple key matches a tuple of the same length with equal elements at each
position. Write `grid[(1, 2)]`, `grid[1, 2]`, or `grid[cell]`, where `cell`
is a tuple from the program. Tuples may nest up to 8 deep and contain at
most 64 keys per tuple. Supported tuples are Rust or Zig tuples, C++
`std::pair` or `std::tuple`, and values a [view](views.md) presents as tuples.
A map with non-tuple keys or tuples of a different length gives a type error.
Struct members are not matched by position: a tuple cannot find a Go, C,
C++, or Rust struct key, including a Rust tuple struct.

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
`struct`, `union`, `enum`, or `class` tag. C base types absent from the debug
information, such as `unsigned char` in a Go program, use the target C
compiler's layout. `const`, `volatile`, and `mut` are accepted but ignored.
Write pointer types as `T*` inside cast parentheses, where `(*p)` means
dereference, or `*T` after `as`, where a trailing `*` means multiplication.

Type names are normalized across compiler spellings. You may omit outer
namespaces, modules, and packages, inline namespaces such as libc++'s
`std::__1`, and trailing default template arguments. For example,
`` std::`vector<int>` `` names `std::vector<int, std::allocator<int> >`.
A name matching several different types is ambiguous; the error lists them.

`(name) - 1` subtracts when `name` is a value and casts `-1` when it is a
type. This affects grouping: `(n) - a * b` means `n - (a * b)`, while
`(T) - a * b` means `((T)(-a)) * b`. Before `-`, `*`, or `&`, a parenthesized
name is resolved as in C, with a value in scope taking precedence over a type.
Other tokens after the parentheses are unambiguous: `(n) + 1` always adds,
and `(T)x` always casts. An expression may contain at most four such
ambiguous names.

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
integer rounds toward zero and saturates at the type's bounds; NaN cannot be
cast. Integers and pointers convert into each other as addresses. Any value
with a truth value can be cast to `bool`.

A cast cannot reinterpret a whole record; use a pointer to reinterpret its
storage instead. Casting a C++ object to a base class selects that base's
subobject, as `static_cast` does. The cast is ambiguous if the object has
several subobjects of that base class. Casting to the value's existing type
changes nothing.

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

An enumerator's name denotes its value. When compared with an enum value,
a bare name is looked up in that enum, so `color == RED` works even where
`RED` alone is ambiguous. Integers can be cast to an enum even when no
enumerator has that value.

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

A string literal compares with program text: a `char*`, character array,
or language string. If the text cannot be read to the end, the comparison
is unavailable unless the readable part already differs.

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

`sizeof(x)` and `sizeof(T)` give a size in bytes without reading the value.
`len(x)` gives an array's or slice's element count, using the first dimension
for multidimensional arrays. For text (a language string or character
pointer's target), it gives the length in bytes. Character arrays use their
element count, like other arrays.

`cap(x)` gives the number of elements `x` can hold: an array's element count,
a slice's recorded capacity (as in Go), or the presenting view's `capacity`
field. Views provide capacity for Go channels, C++ vectors and strings, and
Rust and Zig lists. Other values, including Go maps, have no capacity.

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

In a Python frame, names resolve to frame variables, then module globals,
then built-ins, as in Python. `::name` looks in the native program's globals.
Python variables have no fixed type, so operations check operand types at
evaluation time. Any type can bind; unsupported operations fail when evaluated.

`x[i]` indexes a list or tuple; negative indices count from the end.
`d[key]` looks up a dict key using Python equality. Numbers compare exactly,
so `d[1]`, `d[1.0]`, and `d[true]` find the same key. Text equals only text,
and `d[None]` finds a `None` key. Tuple keys, written `d[(1, "a")]` or
`d[1, "a"]` or taken from the program, compare item by item as in Python.

A key taken from the program is the original object. Lookup checks identity
before equality, so a NaN key finds itself even though it equals nothing.
Keys requiring a custom `__eq__`, such as class instances, are unsupported
when lookup would need to compare with them. Using such an object from the
program as a key is also unsupported. `bytes` and `frozenset` keys are not
supported. An out-of-range index is a bounds error; an absent dict key is a
missing-key error.

`len(x)` counts characters in `str`, bytes in `bytes`, and items in lists,
tuples, dicts, sets, and frozensets. Truth follows Python: `None`, zero,
empty text, and empty collections are false.

`x.name` follows Python's `object.__getattribute__` lookup order: a slot or
other class data descriptor, the object's dict, then attributes of its class
and base classes. `module.name` reads a module global; `Class.name` reads a
class attribute. An absent attribute is a missing-key error. Properties,
methods, other descriptors that execute code, custom `__getattribute__`,
and `__getattr__` for missing attributes are unsupported.

Subclasses of built-in types, such as `namedtuple`, support `x[i]` and
`len(x)` while they retain the built-in `__getitem__` and `__len__`.
Overrides and implementations on other types are unsupported. A missing
dict key is also unsupported if the dict's class defines `__missing__`.
An object without the relevant operation gives a type error, as in Python.

Numbers follow CPython's rules. `True` and `False` act as the `int`s 1 and 0;
`&`, `|`, and `^` on two `bool`s return a `bool`. Integers are exact; values
larger than 128 bits display in full but cannot be used in calculations.
`/` on two `int`s returns the `float` nearest their exact quotient.
`%` uses floor division, so the result has the divisor's sign, including
for floats. Division or `%` by zero and negative shift counts are arithmetic
errors, corresponding to Python's `ZeroDivisionError` and `ValueError`.
`//` is not supported. Results use Python's `repr` with type `int`, `float`,
or `bool`: `7.0`, `7e+16`, `inf`, `False`.

Numbers compare across numeric types. Other supported value kinds compare
unequal to each other; only numbers, `str`, and `bytes` support ordering.
Lists, dicts, and objects cannot be compared or used in calculations because
those operations could run their types' code. microscope never runs program
code, including `__eq__`, properties, or calls. An operation gives a type
error where Python would, such as `None + 1`.
If the type's code could return a value, as for `"a" + "b"`, the operation
is unsupported instead.

In any CPython frame, `(py)x` reads the Python object at address `x`. This
lets native C-extension frames inspect objects through `PyObject *` locals
as Python frames do: `((py)args).x`, `len((py)list)`. The operand must be a
pointer, integer address, or Python value (which casts to itself); anything
else is a type error. A null address or an object of an unsupported type is
unavailable. A `PyObject *` pointer already displays its target object.

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

In a Node.js process, JavaScript values follow ECMAScript operator rules.
Expression literals are JavaScript values: `1` is a Number and `1n` a BigInt.
Python and JavaScript values cannot be combined.

Numbers are doubles. Division by zero gives infinity or NaN; `%` has the
dividend's sign. `&`, `|`, `^`, `~`, `<<`, and `>>` convert Numbers to
32-bit integers; shifts use the count's low five bits.

BigInts are exact. `/` and `%` truncate toward zero. Division by `0n` is an
arithmetic error (JavaScript's `RangeError`); mixing a BigInt and a Number
in arithmetic is a type error (JavaScript's `TypeError`). BigInts beyond
128 bits display in full but cannot be used in calculations. Numeric
conversion maps `true` to 1, `false` and `null` to 0, and `undefined` to NaN.

`==` uses JavaScript's loose equality: `null == undefined`, booleans equal
their numeric equivalents, `1n == 1`, and NaN equals nothing. Comparisons
between BigInts and Numbers are exact. Strings are ordered by UTF-16 code
units, so `"\u{1F600}" < "\u{FFFF}"`. `0`, `-0`, NaN, `0n`, `""`, `null`,
`undefined`, and `false` are false. Every object is true, including empty
arrays and Maps.

String concatenation with `+`, arithmetic on objects (which calls `valueOf`),
and comparisons between strings and numbers are unsupported. microscope does
not run program code or create new strings. Object equality, which compares
identities, is also unsupported. `x[i]` accepts a string, BigInt, or integer
Number with magnitude below 2^53.

Computed values use Node's `util.inspect` format with type `number`, `bigint`,
or `boolean`: `1.764e+23`, `Infinity`, `NaN`, `-0`, and BigInts with an `n`
suffix, such as `-24691357802469135780n`.

## Assignment

Use `=` and compound operators (`+=`, `<<=`, …) in the console or `set var`
to assign. Assignment must be the whole expression; hovers, watches, and
breakpoint conditions cannot assign. Native assignment supports numbers,
booleans, enums, and pointers. The value must fit the target type exactly;
cast first to store a wrapped or rounded value. The result is the target
read back after the write.

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

In a Python frame, `=` and compound operators assign frame variables as
`frame.f_locals[name] = value` would. The value can be an integer, float,
boolean, `None`, text computed by microscope, or an object held by another
frame variable. For example, `held = other` makes both names refer to the
same list.

The agent stores the value on the frame's thread, which runs alone until
the agent reports completion. It then publishes a new stop with that frame
selected and reads the variable back as the result. No program code runs.
The old object is released when the thread resumes.

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

Python assignment requires CPython 3.14 or later with the GIL and an
agent-created stop: a Python breakpoint, the end of a Python step, or a
Python exception. The interpreter must have no thread other than the frame's
thread. Otherwise, assignment is refused without running anything:

- Another thread could be holding the objects it changes.
- A free-threaded interpreter's threads can reach those objects at any time.
- A native stop leaves the thread somewhere the agent cannot act.

Assignment to globals, attributes, and items is unsupported, as are non-UTF-8
text and values of the program's native types.

```microscope-example
world: python
assign: point.x = 1    => error not-an-lvalue at `point.x`
assign: name = "\xff"  => error assignment at `"\xff"`
assign: count = &point => error assignment at `&point`
```

In a JavaScript frame, `=` and compound operators assign a number, BigInt,
text, boolean, `null`, `undefined`, or an object held by another frame variable.
V8's debugger makes the change at a V8 pause. All threads run during the
assignment, but no program JavaScript runs. microscope then publishes a new
stop and reads the variable back from memory as the result. An integer must
be exactly representable as a JavaScript number or written as a BigInt, such
as `5n`. The Node [Assignments](languages.md#assignments) section lists V8's
restrictions.

Registers can also be assigned. The value must fit the register's unsigned
width or be a pointer. Only the innermost frame's registers are writable;
callers' registers are recovered by unwinding and cannot be assigned.
Assigning `$pc` changes where the thread resumes and publishes a new stop
at that location.

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
