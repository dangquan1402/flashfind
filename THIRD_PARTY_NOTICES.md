# Third-party notices

## fsearch

Parts of `crates/flashfind-core/src/walk/macos.rs` (the `getattrlistbulk(2)`
crawler: attribute layout parsing, `openat` fan-out, `setiopolicy_np`
no-materialize policy) and the file-type extension table in
`crates/flashfind-core/src/query.rs` are adapted from
[fsearch](https://github.com/noahdunnagan/fsearch). Several design ideas
(interned names with a character-class mask, name → entries inverted list,
DFS-ordered entries for range scoping, narrowing while typing) also come
from fsearch. The benchmark harness in `bench/fsearch-baseline` links
fsearch as a library to measure it.

```
MIT License

Copyright (c) 2026 Noah Dunnagan

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## Algorithms (no code copied)

- Bit-parallel approximate matching: G. Myers, "A fast bit-vector algorithm
  for approximate string matching based on dynamic programming" (1999);
  H. Hyyrö, "A bit-vector algorithm for computing Levenshtein and Damerau
  edit distances" (2003).
- `days_from_civil`: Howard Hinnant's public-domain date algorithms.

## Rust crates

rayon, memchr, memmap2, libc, serde, serde_json – MIT and/or Apache-2.0.
