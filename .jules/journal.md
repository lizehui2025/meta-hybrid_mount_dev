## 2025-02-23 - Filesystem Safety
**洞察:** Handling filenames from the filesystem using `unwrap()` on `to_str()` is dangerous because filenames are not guaranteed to be valid UTF-8.
**准则:** Always handle `to_str()` returning `None` by logging a warning and skipping, or handling the `OsString` directly.
