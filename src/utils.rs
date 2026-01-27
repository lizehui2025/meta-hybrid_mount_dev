// Copyright 2026 Hybrid Mount Developers
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    ffi::CString,
    fs::{self, File, OpenOptions, create_dir_all, remove_dir_all, remove_file, write},
    io::Write,
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink},
    },
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{OnceLock, atomic::AtomicBool},
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
#[cfg(any(target_os = "linux", target_os = "android"))]
use extattr::{Flags as XattrFlags, lgetxattr, llistxattr, lsetxattr};
use procfs::process::Process;
use regex_lite::Regex;
use rustix::{
    fs::ioctl_ficlone,
    mount::{MountFlags, mount},
};
use walkdir::WalkDir;

const SELINUX_XATTR: &str = "security.selinux";
const OVERLAY_OPAQUE_XATTR: &str = "trusted.overlay.opaque";
const CONTEXT_SYSTEM: &str = "u:object_r:system_file:s0";
const CONTEXT_VENDOR: &str = "u:object_r:vendor_file:s0";
const CONTEXT_HAL: &str = "u:object_r:same_process_hal_file:s0";
const CONTEXT_VENDOR_EXEC: &str = "u:object_r:vendor_file:s0";
const CONTEXT_ROOTFS: &str = "u:object_r:rootfs:s0";

pub static KSU: AtomicBool = AtomicBool::new(false);

#[allow(dead_code)]
const XATTR_TEST_FILE: &str = ".xattr_test";

static MODULE_ID_REGEX: OnceLock<Regex> = OnceLock::new();

pub fn check_ksu() {
    let status = ksu::version().is_some_and(|v| {
        log::info!("KernelSU Version: {v}");
        true
    });
    KSU.store(status, std::sync::atomic::Ordering::Relaxed);
}

pub fn detect_mount_source() -> String {
    if ksu::version().is_some() {
        return "KSU".to_string();
    }
    "APatch".to_string()
}

pub fn init_logging(verbose: bool) -> Result<()> {
    let level = if verbose {
        log::LevelFilter::Debug
    } else {
        log::LevelFilter::Info
    };
    #[cfg(target_os = "android")]
    {
        android_logger::init_once(
            android_logger::Config::default()
                .with_max_level(level)
                .with_tag("mhm"),
        );
    }

    #[cfg(not(target_os = "android"))]
    {
        use std::io::Write;

        let mut builder = env_logger::Builder::new();

        builder.format(|buf, record| {
            writeln!(
                buf,
                "[{}] [{}] {}",
                record.level(),
                record.target(),
                record.args()
            )
        });
        builder.filter_level(level).init();
    }
    Ok(())
}

/// 原子性写入文件，包含清理守卫
pub fn atomic_write<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, content: C) -> Result<()> {
    let path = path.as_ref();
    let dir = path.parent().unwrap_or_else(|| Path::new("."));

    let temp_name = format!(".mhm_tmp_{}_{}.tmp", std::process::id(), 
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos());
    let temp_file = dir.join(temp_name);

    // 清理守卫：如果函数中途出错，自动删除临时文件
    struct CleanupGuard<'a>(&'a Path);
    impl Drop for CleanupGuard<'_> {
        fn drop(&mut self) { let _ = fs::remove_file(self.0); }
    }
    let guard = CleanupGuard(&temp_file);

    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_file)
            .context("Failed to create temporary file for atomic write")?;
        file.write_all(content.as_ref())?;
        file.sync_all()?; // 确保数据落盘
    }

    fs::rename(&temp_file, path).context("Failed to rename atomic temporary file")?;
    std::mem::forget(guard); // 成功后释放守卫，不执行 drop
    Ok(())
}

pub fn validate_module_id(module_id: &str) -> Result<()> {
    let re = MODULE_ID_REGEX
        .get_or_init(|| Regex::new(r"^[a-zA-Z][a-zA-Z0-9._-]+$").expect("Invalid Regex pattern"));
    if re.is_match(module_id) {
        Ok(())
    } else {
        bail!("Invalid module ID: '{module_id}'. Must match /^[a-zA-Z][a-zA-Z0-9._-]+$/")
    }
}

pub fn check_zygisksu_enforce_status() -> bool {
    std::fs::read_to_string("/data/adb/zygisksu/denylist_enforce")
        .map(|s| s.trim() != "0")
        .unwrap_or(false)
}

fn copy_extended_attributes(src: &Path, dst: &Path) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // 1. 同步 SELinux 上下文
        if let Ok(mut ctx) = lgetfilecon(src) {
            if ctx.contains(CONTEXT_ROOTFS) {
                ctx = CONTEXT_SYSTEM.to_string();
            }
            lsetfilecon(dst, &ctx).warn_on_err();
        }

        // 2. 同步 OverlayFS 不透明属性
        if let Ok(opaque) = lgetxattr(src, OVERLAY_OPAQUE_XATTR) {
            lsetxattr(dst, OVERLAY_OPAQUE_XATTR, &opaque, XattrFlags::empty())
                .context("Failed to set opaque xattr")?;
        }

        // 3. 同步其他受信任的 Overlay 属性
        if let Ok(xattrs) = llistxattr(src) {
            for xattr_name in xattrs {
                let name_str = String::from_utf8_lossy(xattr_name.as_bytes());
                if name_str.starts_with("trusted.overlay.") && name_str != OVERLAY_OPAQUE_XATTR {
                    if let Ok(val) = lgetxattr(src, &xattr_name) {
                        lsetxattr(dst, &xattr_name, &val, XattrFlags::empty()).ok();
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn set_overlay_opaque<P: AsRef<Path>>(path: P) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        lsetxattr(
            path.as_ref(),
            OVERLAY_OPAQUE_XATTR,
            b"y",
            XattrFlags::empty(),
        )?;
    }
    Ok(())
}

pub fn lsetfilecon<P: AsRef<Path>>(path: P, con: &str) -> Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        if let Err(e) = lsetxattr(
            path.as_ref(),
            SELINUX_XATTR,
            con.as_bytes(),
            XattrFlags::empty(),
        ) {
            let io_err = std::io::Error::from(e);
            log::debug!(
                "lsetfilecon: {} -> {} failed: {}",
                path.as_ref().display(),
                con,
                io_err
            );
        }
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn lgetfilecon<P: AsRef<Path>>(path: P) -> Result<String> {
    let con = extattr::lgetxattr(path.as_ref(), SELINUX_XATTR).with_context(|| {
        format!(
            "Failed to get SELinux context for {}",
            path.as_ref().display()
        )
    })?;
    let con_str = String::from_utf8_lossy(&con).trim_matches('\0').to_string();

    Ok(con_str)
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
pub fn lgetfilecon<P: AsRef<Path>>(_path: P) -> Result<String> {
    unimplemented!();
}

#[allow(dead_code)]
pub fn copy_path_context<S: AsRef<Path>, D: AsRef<Path>>(src: S, dst: D) -> Result<()> {
    let mut context = if src.as_ref().exists() {
        lgetfilecon(&src).unwrap_or_else(|_| CONTEXT_SYSTEM.to_string())
    } else {
        CONTEXT_SYSTEM.to_string()
    };

    if context.contains("u:object_r:rootfs:s0") {
        context = CONTEXT_SYSTEM.to_string();
    }

    lsetfilecon(dst, &context)
}

pub fn ensure_dir_exists<T: AsRef<Path>>(dir: T) -> Result<()> {
    if !dir.as_ref().exists() {
        create_dir_all(&dir)?;
    }
    Ok(())
}

pub fn camouflage_process(name: &str) -> Result<()> {
    let c_name = CString::new(name)?;
    unsafe {
        libc::prctl(libc::PR_SET_NAME, c_name.as_ptr() as u64, 0, 0, 0);
    }
    Ok(())
}

pub fn random_kworker_name() -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos().hash(&mut hasher);
    std::process::id().hash(&mut hasher);
    let hash = hasher.finish();
    
    let x = hash % 16;
    let y = (hash >> 4) % 10;
    format!("kworker/u{}:{}", x, y)
}

#[allow(dead_code)]
pub fn is_xattr_supported(path: &Path) -> bool {
    let test_file = path.join(XATTR_TEST_FILE);
    if let Err(e) = write(&test_file, b"test") {
        log::debug!("XATTR Check: Failed to create test file: {}", e);
        return false;
    }
    let result = lsetfilecon(&test_file, "u:object_r:system_file:s0");
    let supported = result.is_ok();
    let _ = remove_file(test_file);
    supported
}

pub fn is_overlay_xattr_supported(path: &Path) -> bool {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        let mut buf = [0u8; 1];
        !matches!(
            rustix::fs::lgetxattr(path, "user.hybrid_check", &mut buf),
            Err(rustix::io::Errno::OPNOTSUPP)
        )
    }
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    true
}

pub fn is_mounted<P: AsRef<Path>>(path: P) -> bool {
    let path_str = path.as_ref().to_string_lossy();
    let search = path_str.trim_end_matches('/');

    if let Ok(process) = Process::myself()
        && let Ok(mountinfo) = process.mountinfo()
    {
        return mountinfo
            .into_iter()
            .any(|m| m.mount_point.to_string_lossy() == search);
    }

    if let Ok(content) = fs::read_to_string("/proc/mounts") {
        for line in content.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() > 1 && parts[1] == search {
                return true;
            }
        }
    }
    false
}

pub fn mount_tmpfs(target: &Path, source: &str) -> Result<()> {
    ensure_dir_exists(target)?;
    let data = CString::new("mode=0755")?;
    mount(
        source,
        target,
        "tmpfs",
        MountFlags::empty(),
        Some(data.as_c_str()),
    )
    .context("Failed to mount tmpfs")?;
    Ok(())
}

pub fn repair_image(image_path: &Path) -> Result<()> {
    log::info!("Running e2fsck on {}", image_path.display());
    let status = Command::new("e2fsck")
        .args(["-y", "-f"])
        .arg(image_path)
        .status()
        .context("Failed to execute e2fsck")?;

    if let Some(code) = status.code()
        && code > 2
    {
        bail!("e2fsck failed with exit code: {}", code);
    }
    Ok(())
}

pub fn reflink_or_copy(src: &Path, dest: &Path) -> Result<u64> {
    let src_file = File::open(src)?;
    let dest_file = File::create(dest)?;

    if ioctl_ficlone(&dest_file, &src_file).is_ok() {
        let metadata = src_file.metadata()?;
        let len = metadata.len();
        dest_file.set_permissions(metadata.permissions())?;
        return Ok(len);
    }
    drop(dest_file);
    drop(src_file);
    fs::copy(src, dest).map_err(|e| e.into())
}

fn make_device_node(path: &Path, mode: u32, rdev: u64) -> Result<()> {
    let c_path = CString::new(path.as_os_str().as_encoded_bytes())?;
    let dev = rdev as libc::dev_t;
    unsafe {
        if libc::mknod(c_path.as_ptr(), mode as libc::mode_t, dev) != 0 {
            let err = std::io::Error::last_os_error();
            bail!("mknod failed for {}: {}", path.display(), err);
        }
    }
    Ok(())
}

fn guess_context_by_path(path: &Path) -> &'static str {
    let path_str = path.to_string_lossy();

    if path_str.starts_with("/vendor") || path_str.starts_with("/odm") {
        if path_str.contains("/lib/") || path_str.contains("/lib64/") || path_str.ends_with(".so") {
            return CONTEXT_HAL;
        }

        if path_str.contains("/bin/") {
            return CONTEXT_VENDOR_EXEC;
        }

        if path_str.contains("/firmware") {
            return CONTEXT_VENDOR;
        }

        return CONTEXT_VENDOR;
    }

    CONTEXT_SYSTEM
}

/// 修复版 SELinux 上下文恢复逻辑
/// 即使失败也返回 Ok(()) 以免中断 OverlayFS 准备流程
fn apply_system_context(current: &Path, relative: &Path) -> Result<()> {
    if let Some(name) = current.file_name().and_then(|n| n.to_str())
        && (name == "upperdir" || name == "workdir")
        && let Some(parent) = current.parent()
        && let Ok(ctx) = lgetfilecon(parent)
    {
        let _ = lsetfilecon(current, &ctx);
        return Ok(());
    }

    let current_ctx = lgetfilecon(current).ok();
    if let Some(ctx) = &current_ctx
        && !ctx.is_empty()
        && ctx != CONTEXT_ROOTFS
        && ctx != "u:object_r:unlabeled:s0"
    {
        return Ok(());
    }

    let system_path = Path::new("/").join(relative);
    if system_path.exists() {
        if let Ok(sys_ctx) = lgetfilecon(&system_path) {
            let target_ctx = if sys_ctx == CONTEXT_ROOTFS {
                CONTEXT_SYSTEM
            } else {
                &sys_ctx
            };
            let _ = lsetfilecon(current, target_ctx);
            return Ok(());
        }
    } else if let Some(parent) = system_path.parent()
        && parent.exists()
        && let Ok(parent_ctx) = lgetfilecon(parent)
        && parent_ctx != CONTEXT_ROOTFS
    {
        // 尝试继承父目录
        let guessed = guess_context_by_path(&system_path);
        if guessed == CONTEXT_HAL && parent_ctx == CONTEXT_VENDOR {
            let _ = lsetfilecon(current, CONTEXT_HAL);
        } else {
            let _ = lsetfilecon(current, &parent_ctx);
        }
        return Ok(());
    }

    let target_context = guess_context_by_path(&system_path);
    let _ = lsetfilecon(current, target_context);
    Ok(())
}

trait WarnErr { fn warn_on_err(self); }
impl<T, E: std::fmt::Display> WarnErr for Result<T, E> {
    fn warn_on_err(self) {
        if let Err(e) = self { log::warn!("Suppressed error: {}", e); }
    }
}

fn iterative_sync(src: &Path, dst: &Path, repair: bool) -> Result<()> {
    // 显式指定 Vec 的元组类型
    let mut stack: Vec<(PathBuf, PathBuf, PathBuf)> = vec![(src.to_path_buf(), dst.to_path_buf(), PathBuf::new())];

    while let Some((curr_src, curr_dst, rel_path)) = stack.pop() {
        if !curr_dst.exists() {
            if curr_src.is_dir() {
                create_dir_all(&curr_dst)?;
            }
            if let Ok(src_meta) = curr_src.metadata() {
                let _ = fs::set_permissions(&curr_dst, src_meta.permissions());
            }

            if repair {
                let _ = apply_system_context(&curr_dst, &rel_path);
            } else {
                let _ = copy_extended_attributes(&curr_src, &curr_dst);
            }
        }

        if curr_src.is_dir() {
            for entry in fs::read_dir(&curr_src)? {
                let entry = entry?;
                let s = entry.path();
                let name = entry.file_name();
                let d = curr_dst.join(&name);
                let next_rel = rel_path.join(&name);
                
                let metadata = entry.metadata()?;
                let ft = metadata.file_type();

                // 处理不同类型的文件
                if ft.is_dir() {
                    stack.push((s, d, next_rel));
                } else {
                    if ft.is_symlink() {
                        if d.exists() { remove_file(&d)?; }
                        symlink(fs::read_link(&s)?, &d)?;
                    } else if ft.is_char_device() || ft.is_block_device() || ft.is_fifo() {
                        if d.exists() { remove_file(&d)?; }
                        make_device_node(&d, metadata.permissions().mode(), metadata.rdev())?;
                    } else {
                        reflink_or_copy(&s, &d)?;
                    }
                    
                    // 同步属性
                    let _ = copy_extended_attributes(&s, &d);
                    if repair { let _ = apply_system_context(&d, &next_rel); }
                }
            }
        }
    }
    Ok(())
}

pub fn detect_all_partitions() -> Result<Vec<String>> {
    let mut partitions = Vec::new();
    let mountinfo = procfs::process::Process::myself()?.mountinfo()
        .context("Failed to read mountinfo")?;

    // 需要排除的非系统路径
    let excludes = ["/data", "/dev", "/proc", "/sys", "/mnt", "/storage", "/apex"];

    for mnt in mountinfo.0 {
        let path_buf = &mnt.mount_point;
        let path_str = path_buf.to_string_lossy();
        
        // 逻辑：必须是根目录下的第一级目录 (例如 /vendor, /product)
        if path_str.starts_with('/') && path_str.split('/').count() == 2 {
            let name = path_str.trim_start_matches('/');
            if name.is_empty() { continue; }

            if excludes.iter().any(|&ex| path_str.starts_with(ex)) {
                continue;
            }

            // 修复点：mnt.fs_type 已经是 String，直接引用即可
            let fstype = &mnt.fs_type;

            match fstype.as_str() {
                "ext4" | "erofs" | "f2fs" => {
                    partitions.push(name.to_string());
                }
                _ => continue,
            }
        }
    }

    partitions.sort();
    partitions.dedup();
    Ok(partitions)
}

fn native_cp_r(src: &Path, dst: &Path, relative: &Path, repair: bool) -> Result<()> {
    if !dst.exists() {
        if src.is_dir() {
            create_dir_all(dst)?;
        }
        if let Ok(src_meta) = src.metadata() {
            let _ = fs::set_permissions(dst, src_meta.permissions());
        }

        if repair && relative.as_os_str().is_empty() {
            let _ = apply_system_context(dst, relative);
        } else if !repair {
            let _ = copy_extended_attributes(src, dst);
        }
    }

    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let file_name = entry.file_name();
        let dst_path = dst.join(&file_name);
        let next_relative = relative.join(&file_name);

        let metadata = entry.metadata()?;
        let ft = metadata.file_type();

        if ft.is_dir() {
            native_cp_r(&src_path, &dst_path, &next_relative, repair)?;
        } else if ft.is_symlink() {
            if dst_path.exists() {
                remove_file(&dst_path)?;
            }
            let link_target = fs::read_link(&src_path)?;
            symlink(&link_target, &dst_path)?;
        } else if ft.is_char_device() || ft.is_block_device() || ft.is_fifo() {
            if dst_path.exists() {
                remove_file(&dst_path)?;
            }
            let mode = metadata.permissions().mode();
            let rdev = metadata.rdev();
            make_device_node(&dst_path, mode, rdev)?;
        } else {
            reflink_or_copy(&src_path, &dst_path)?;
        }

        let _ = copy_extended_attributes(&src_path, &dst_path);

        if repair {
            let _ = apply_system_context(&dst_path, &next_relative);
        }
    }
    Ok(())
}

pub fn sync_dir(src: &Path, dst: &Path, repair_context: bool) -> Result<()> {
    if !src.exists() { return Ok(()); }
    ensure_dir_exists(dst)?;
    iterative_sync(src, dst, repair_context).with_context(|| {
        format!("Failed to sync {} to {}", src.display(), dst.display())
    })
}

#[allow(dead_code)]
pub fn cleanup_temp_dir(temp_dir: &Path) {
    if let Err(e) = remove_dir_all(temp_dir) {
        log::warn!(
            "Failed to clean up temp dir {}: {:#}",
            temp_dir.display(),
            e
        );
    }
}

#[allow(dead_code)]
pub fn ensure_temp_dir(temp_dir: &Path) -> Result<()> {
    if temp_dir.exists() {
        remove_dir_all(temp_dir).ok();
    }
    create_dir_all(temp_dir)?;
    Ok(())
}

pub fn is_erofs_supported() -> bool {
    fs::read_to_string("/proc/filesystems")
        .map(|content| content.contains("erofs"))
        .unwrap_or(false)
}

pub fn create_erofs_image(src_dir: &Path, image_path: &Path) -> Result<()> {
    let mkfs_bin = Path::new("/data/adb/metamodule/tools/mkfs.erofs");
    let cmd_name = if mkfs_bin.exists() {
        mkfs_bin.as_os_str()
    } else {
        std::ffi::OsStr::new("mkfs.erofs")
    };

    log::info!("Packing EROFS image: {}", image_path.display());

    let output = Command::new(cmd_name)
        .arg("-z")
        .arg("lz4hc")
        .arg("-x")
        .arg("256")
        .arg(image_path)
        .arg(src_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to execute mkfs.erofs")?;

    let log_lines = |bytes: &[u8]| {
        let s = String::from_utf8_lossy(bytes);
        for line in s.lines() {
            if !line.trim().is_empty() {
                log::debug!("{}", line);
            }
        }
    };

    log_lines(&output.stdout);
    log_lines(&output.stderr);

    if !output.status.success() {
        bail!("Failed to create EROFS image");
    }

    log::info!("Build Completed.");
    let _ = fs::set_permissions(image_path, fs::Permissions::from_mode(0o644));
    lsetfilecon(image_path, "u:object_r:ksu_file:s0")?;
    Ok(())
}

pub fn mount_erofs_image(image_path: &Path, target: &Path) -> Result<()> {
    ensure_dir_exists(target)?;
    lsetfilecon(image_path, "u:object_r:ksu_file:s0").ok();
    let status = Command::new("mount")
        .args(["-t", "erofs", "-o", "loop,ro,nodev,noatime"])
        .arg(image_path)
        .arg(target)
        .status()
        .context("Failed to execute mount command for EROFS")?;

    if !status.success() {
        bail!("EROFS Mount command failed");
    }
    Ok(())
}

pub fn extract_module_id(path: &Path) -> Option<String> {
    let mut current = path;
    loop {
        if current.join("module.prop").exists() {
            return current.file_name().map(|s| s.to_string_lossy().to_string());
        }
        match current.parent() {
            Some(p) => current = p,
            None => break,
        }
    }

    path.parent()
        .and_then(|p| p.file_name())
        .map(|s| s.to_string_lossy().to_string())
}

pub fn prune_empty_dirs<P: AsRef<Path>>(root: P) -> Result<()> {
    let root = root.as_ref();
    if !root.exists() {
        return Ok(());
    }

    for entry in WalkDir::new(root)
        .min_depth(1)
        .contents_first(true)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_dir() {
            let path = entry.path();
            if let Err(e) = fs::remove_dir(path) {
                log::debug!("Keeping dir (not empty): {} [{}]", path.display(), e);
            } else {
                log::debug!("Pruned empty dir: {}", path.display());
            }
        }
    }
    Ok(())
}
