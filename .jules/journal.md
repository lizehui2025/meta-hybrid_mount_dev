## 2025-05-23 - [Handling Non-UTF8 Filenames]
**洞察:** Using `entry.file_name().to_str().unwrap()` on directory entries is a common source of panics because Linux filenames are byte sequences that may not be valid UTF-8.
**准则:** When iterating directories, always match on `to_str()` result. If `None`, log a warning and skip/handle strictly, but never `unwrap()`.
