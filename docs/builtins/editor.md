---
title : Terminal & I/O
sidebar:
  order: 10
---

### `write`

Writes a string to standard output **without** a trailing newline. Returns `()`.

```
(write str)  →  ()
```

```lisp
(write "hello ")  ;; prints "hello " with no newline
```

---

### `writeline`

Writes a string to standard output followed by a newline. Returns `()`.

```
(writeline str)  →  ()
```

```lisp
(writeline "hello")  ;; prints "hello\n"
```

---

### `read-byte`

Reads a single byte from standard input. Returns an integer in `0–255`, or `-1` on end-of-file.

```
(read-byte)  →  Int
```

---

### `exit`

Terminates the process immediately with an optional exit code. Code defaults to `0` when omitted.

```
(exit)         ;; exit 0
(exit code)    ;; exit with code
```

---

### `string-ref`

Returns the Unicode codepoint (integer) of the character at the given index, or `-1` if the index is out of range. Indexing is by Unicode character (not byte).

```
(string-ref s index)  →  Int
```

```lisp
(string-ref "abcdef" 2)  ;; → 99  (the codepoint for #\c)
```

---

### `string-split`

Splits a string by a separator string and returns a list of substrings. An empty separator splits by character.

```
(string-split s separator)  →  List of Strings
```

```lisp
(string-split "a,b,c" ",")   ;; → ("a" "b" "c")
(string-split "hello" "")    ;; → ("h" "e" "l" "l" "o")
```

---

### `string-index-of`

Returns the starting index (integer) of the first occurrence of `substr` in `s`, or `-1` if not found.

```
(string-index-of s substr)  →  Int
```

```lisp
(string-index-of "foobar" "bar")  ;; → 3
(string-index-of "foobar" "baz")  ;; → -1
```

---

### `string-contains?`

Returns `#t` if the string contains the given substring, `#f` otherwise.

```
(string-contains? s substr)  →  Bool
```

```lisp
(string-contains? "hello world" "world")  ;; → #t
(string-contains? "hello" "xyz")          ;; → #f
```

---

### `string`

Converts a Unicode codepoint (integer) into a single-character string. Returns an empty string for out-of-range values.

```
(string codepoint)  →  String
```

```lisp
(string 65)  ;; → "A"
(string 0x1F600)  ;; → "😀"
```

---

### `equal?`

Returns `#t` if two values are structurally equal, `#f` otherwise. Works for all types: numbers (including complex), strings, booleans, symbols, lists (recursive), and functions (by identity).

```
(equal? a b)  →  Bool
```

```lisp
(equal? 3 3)           ;; → #t
(equal? "abc" "abc")   ;; → #t
(equal? '(1 2 3) '(1 2 3))  ;; → #t
(equal? 3 3.0)         ;; → #t
```

---

### `command-line`

Returns the command-line arguments passed to the process as a list of strings. The first element is the program name.

```
(command-line)  →  List of Strings
```

```lisp
(command-line)  ;; → ("./pilisp" "file.pi")
```

---

### `read-key`

Reads a single key event from the terminal in raw mode. Returns an integer code:
- Printable ASCII characters return their byte value (32–126).
- Control characters (`C-a` through `C-z`) return 1–26.
- Enter = 13, Tab = 9, Backspace = 127.
- Special keys return negative values:
  - `-1` = EOF, `-2` = Up, `-3` = Down, `-4` = Right, `-5` = Left
  - `-6` = Home, `-7` = End, `-8` = Page Up, `-9` = Page Down
  - `-10` = Delete, `-11` = Escape

Uses `poll()` with a 10ms timeout after receiving ESC to distinguish the Escape key from escape sequences (arrow keys, etc.).

```
(read-key)  →  Int
```

---

### `load`

Loads a pi-lisp source file and evaluates all top-level forms in sequence. Returns `()` on success. The file is parsed and each form is evaluated immediately, so definitions made by `load` are available to subsequent forms.

```
(load path)  →  ()
```

```lisp
(load "my-library.pi")
```
