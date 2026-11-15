









import std/strutils

type
  Point = object
    x, y: int32
  Segment = object
    start, finish: Point
    tag: uint8
  Color = enum
    red, green, blue

var sink {.volatile.}: pointer


proc reached(checkpoint: string) {.noinline.} =
  sink = unsafeAddr checkpoint


proc keep(value: pointer) {.noinline.} =
  sink = value

proc truth(checkpoint, path, kind, value: string) =
  stdout.write "TRUTH\t" & checkpoint & "\t" & path & "\t" & kind & "\t" & value & "\n"
  stdout.flushFile

proc bits(value: float32): string = "0x" & toHex(cast[uint32](value)).toLowerAscii.strip(trailing = false, chars = {'0'})
proc bits(value: float64): string = "0x" & toHex(cast[uint64](value)).toLowerAscii.strip(trailing = false, chars = {'0'})

proc add(a, b: int): int {.noinline.} =
  var total = a + b
  keep(addr total)
  total

proc scalars() {.noinline.} =
  var small: int8 = -5
  var wide: uint16 = 65000
  var big: int64 = -(1'i64 shl 40)
  var single: float32 = 1.5
  var precise: float64 = -0.1
  var flag = true
  var letter = 'q'
  truth("scalars", "small", "int", $small)
  truth("scalars", "wide", "int", $wide)
  truth("scalars", "big", "int", $big)
  truth("scalars", "single", "f32", bits(single))
  truth("scalars", "precise", "f64", bits(precise))
  truth("scalars", "flag", "summary", "true")
  truth("scalars", "letter", "int", $ord(letter))
  reached("scalars")
  keep(addr small); keep(addr wide); keep(addr big); keep(addr single)
  keep(addr precise); keep(addr flag); keep(addr letter)

proc records() {.noinline.} =
  var point = Point(x: 3, y: -4)
  var segment = Segment(start: Point(x: 1, y: 2), finish: Point(x: 5, y: 6), tag: 9)
  var numbers: array[3, int32] = [10'i32, 20, 30]
  var color = green
  truth("records", "point.x", "int", $point.x)
  truth("records", "point.y", "int", $point.y)
  truth("records", "segment.finish.y", "int", $segment.finish.y)
  truth("records", "segment.tag", "int", $segment.tag)
  for index, number in numbers:
    truth("records", "numbers." & $index, "int", $number)

  truth("records", "color", "int", $ord(color))
  reached("records")
  keep(addr point); keep(addr segment); keep(addr numbers); keep(addr color)

proc strings() {.noinline.} =
  var text = "héllo"
  var empty = ""
  truth("strings", "text", "summary", "\"" & text & "\"")
  truth("strings", "empty", "summary", "\"\"")
  reached("strings")
  keep(addr text); keep(addr empty)

proc seqs() {.noinline.} =
  var items = @[11'i32, 22, 33]
  var none: seq[int32] = @[]
  var words = @["ab", "cd"]
  for index, item in items:
    truth("seqs", "items." & $index, "int", $item)
  truth("seqs", "none", "summary", "len=0 []")
  truth("seqs", "words.1", "summary", "\"cd\"")
  reached("seqs")
  keep(addr items); keep(addr none); keep(addr words)

scalars()
records()
strings()
seqs()
if add(2, 3) != 5:
  quit "add"
