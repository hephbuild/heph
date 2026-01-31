def abs(x):
  """`abs(x)` returns the absolute value of its argument `x`, which must be an int or float. The result has the same type as `x`."""
  pass

def any(x) -> bool:
  """`any(x)` returns `True` if any element of the iterable sequence x has a truth value of true. If the iterable is empty, it returns `False`."""
  pass

def all(x) -> bool:
  """`all(x)` returns `False` if any element of the iterable sequence x has a truth value of false. If the iterable is empty, it returns `True`."""
  pass

def bool(x) -> bool:
  """`bool(x)` interprets `x` as a Boolean value---`True` or `False`. With no argument, `bool()` returns `False`."""
  pass

def chr(i):
  """`chr(i)` returns a string that encodes the single Unicode code point whose value is specified by the integer `i`. `chr` fails unless 0 ≤ `i` ≤ 0x10FFFF."""
  pass

def dict() -> Dict:
  """`dict` creates a dictionary.  It accepts up to one positional argument, which is interpreted as an iterable of two-element sequences (pairs), each specifying a key/value pair in the resulting dictionary."""
  pass

def dir(x) -> List[String]:
  """`dir(x)` returns a new sorted list of the names of the attributes (fields and methods) of its operand. The attributes of a value `x` are the names `f` such that `x.f` is a valid expression."""
  pass

def enumerate(x) -> List[Tuple[int, any]]:
  """`enumerate(x)` returns a list of (index, value) pairs, each containing successive values of the iterable sequence xand the index of the value within the sequence."""
  pass

def fail(*args, sep=" "):
  """The `fail(*args, sep=" ")` function causes execution to fail with the specified error message. Like `print`, arguments are formatted as if by `str(x)` and separated by a space, unless an alternative separator is specified by a `sep` named argument."""
  pass

def float(x) -> float:
  """`float(x)` interprets its argument as a floating-point number."""
  pass

def getattr(x, name):
  """`getattr(x, name)` returns the value of the attribute (field or method) of x named `name`. It is a dynamic error if x has no such attribute."""
  pass

def hasattr(x, name) -> bool:
  """`hasattr(x, name)` reports whether x has an attribute (field or method) named `name`."""
  pass

def hash(x) -> int:
  """`hash(x)` returns an integer hash of a string x such that two equal strings have the same hash. In other words `x == y` implies `hash(x) == hash(y)`."""
  pass

def int(x) -> int:
  """`int(x[, base])` interprets its argument as an integer."""
  pass

def len(x) -> int:
  """`len(x)` returns the number of elements in its argument."""
  pass

def list() -> List:
  """`list` constructs a list."""
  pass

def max(x):
  """`max(x)` returns the greatest element in the iterable sequence x."""
  pass

def min(x):
  """`min(x)` returns the least element in the iterable sequence x."""
  pass

def ord(s):
  """`ord(s)` returns the integer value of the sole Unicode code point encoded by the string `s`."""
  pass

def print(*args, sep=" "):
  """`print(*args, sep=" ")` prints its arguments, followed by a newline. Arguments are formatted as if by `str(x)` and separated with a space, unless an alternative separator is specified by a `sep` named argument."""
  pass

def range() -> List[int]:
  """`range` returns an immutable sequence of integers defined by the specified interval and stride."""
  pass

def repr(x) -> String:
  """`repr(x)` formats its argument as a string."""
  pass

def reversed(x) -> List:
  """`reversed(x)` returns a new list containing the elements of the iterable sequence x in reverse order."""
  pass

def set(x):
  """`set(x)` returns a new set containing the elements of the iterable x. With no argument, `set()` returns a new empty set."""
  pass

def sorted(x) -> List:
  """`sorted(x)` returns a new list containing the elements of the iterable sequence x, in sorted order.  The sort algorithm is stable."""
  pass

def str(x) -> String:
  """`str(x)` formats its argument as a string."""
  pass

def tuple(x):
  """`tuple(x)` returns a tuple containing the elements of the iterable x."""
  pass

def type(x) -> String:
  """type(x) returns a string describing the type of its operand."""
  pass

def zip() -> List:
  """`zip()` returns a new list of n-tuples formed from corresponding elements of each of the n iterable sequences provided as arguments to `zip`.  That is, the first tuple contains the first element of each of the sequences, the second element contains the second element of each of the sequences, and so on.  The result list is only as long as the shortest of the input sequences."""
  pass

