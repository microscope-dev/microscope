



module containers;

import std.array : Appender, appender;
import std.typecons : Nullable, nullable;

__gshared const(void)* sink;

struct Triple
{
    ulong a, b, c;
}


pragma(inline, false) void barrier(const(void)*[] fixtures...)
{
    foreach (fixture; fixtures)
        sink = fixture;
}

void main()
{
    auto text = appender!string(); // VIEW: text => "hello, world"
    text.put("hello, ");
    text.put("world");
    auto numbers = appender!(int[])(); // VIEW: numbers => len=3 [1, 2, 3]
    numbers.put(1);
    numbers.put(2);
    numbers.put(3);
    Appender!(int[]) unused; // VIEW: unused => len=0 []
    auto present = nullable(42); // VIEW: present => 42
    Nullable!int absent; // VIEW: absent => null
    int[string] counts; // VIEW: counts => len=2 {"one": 1, "two": 2} (any order)
    counts["one"] = 1;
    counts["two"] = 2;
    double[int] none; // VIEW: none => len=0 {}
    uint[uint] many; // VIEW: many => count: 300
    foreach (i; 0 .. 300)
        many[i] = i * 2;

    int[int] pruned; // VIEW: pruned => len=1 {2: 20}
    pruned[1] = 10;
    pruned[2] = 20;
    pruned.remove(1);

    Triple[ubyte] triples; // VIEW: triples => count: 20
    foreach (ubyte i; 0 .. 20)
        triples[i] = Triple(i, 0, 0);
    barrier(&text, &numbers, &unused, &present, &absent, &counts, &none, &many, &pruned,
        &triples);
}
