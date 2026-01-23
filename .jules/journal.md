## 2026-01-23 - Secure Temporary Files
**洞察:** `SystemTime` based naming and `File::create` are insufficient for secure temporary file creation in shared environments.
**准则:** Use `/dev/urandom` for naming and `OpenOptions::new().create_new(true)` for atomic creation.
