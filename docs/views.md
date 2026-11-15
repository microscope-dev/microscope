```microscope-view-example
microscope-views 1

view c intvec {
    check n <= cap
    show sequence(n) for i in range(n) => data[i]
    field capacity = cap
}
---
v => len=3 [10, 20, 30]
v => children: [0] = 10, [1] = 20, [2] = 30, capacity = 4, [raw]
none => len=0 []
bad => problem: check `n <= cap` failed: `n` is 9, `cap` is 4
```

```microscope-view-example
microscope-views 1
view c++ app::**::Pair<T, N> {
    show sequence(N) for i in range(N) => items[i]
}
---
p => len=3 [2, 3, 4]
p => children: [0] = 2, [1] = 3, [2] = 4, [raw]
```

```microscope-view-example
microscope-views 1
view c str_t {
    show text(p, len)
}
view rust alloc::**::Box<T> {
    show value(*inner(self))
}
---
s => "hello"
b => 42
```

```microscope-view-example
microscope-views 1
view c tagged {
    show match kind {
        0 => empty("None")
        1 => value(value)
    }
}
---
nothing => None
something => 7
strange => problem: `kind` is 5, which no arm of the match names
```

```microscope-view-example
microscope-views 1
view c list {
    show sequence(count) for x in list(head, p => p->next) => x->value
}
---
three => len=3 [1, 2, 3]
short => problem: the view declares 5 elements and generates 3
```

```microscope-view-example
microscope-views 1
view c tree {
    show map(count) for x in inorder(root, p => p->left, p => p->right)
        => x->key : x->value
}
---
balanced => len=3 {1: 10, 2: 20, 3: 30}
balanced => children: 1: 10, 2: 20, 3: 30, [raw]
```

```microscope-view-example
microscope-views 1
extend c tagged {
    hide value
    format kind as hex
    field next_kind = kind + 1
}
---
something => {kind: 0x1}
something => children: kind = 0x1, next_kind = 2, [raw]
```

```microscope-view-example
microscope-views 1
view python app::Point {
    show record { x = self.x, y = self.y }
    summary "Point({self.x}, {self.y})"
}
---
point => Point(3, 4)
point => children: x = 3, y = 4, [raw]
gone => problem: the `app.Point` has no attribute `x`
```
