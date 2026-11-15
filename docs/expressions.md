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

```microscope-example
world: scalars
-1 < 1u32              => true : bool
i32v < u32v            => true : bool
u32v - 1 > 0           => true : bool
sc == -7               => true : bool
```

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

```microscope-example
world: python
assign: point.x = 1    => error not-an-lvalue at `point.x`
assign: name = "\xff"  => error assignment at `"\xff"`
assign: count = &point => error assignment at `&point`
```

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
