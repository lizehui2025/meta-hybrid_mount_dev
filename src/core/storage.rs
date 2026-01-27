// Copyright 2026 Hybrid Mount Developers
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{Context, Result, bail, ensure};
use jwalk::WalkDir;
use rustix::{
    fs::Mode,
    mount::{MountPropagationFlags, UnmountFlags, mount_change, unmount as umount},
};
use serde::Serialize;

#[cfg(any(target_os = "linux", target_os = "android"))]
use crate::try_umount::send_umountable;
use crate::{core::state::RuntimeState, mount::overlayfs::utils as overlay_utils, utils};

const DEFAULT_SELINUX_CONTEXT: &str = "u:object_r:system_file:s0";

pub struct StorageHandle {
    pub mount_point: PathBuf,
    pub mode: String,
}

impl StorageHandle {
    pub fn commit(&mut self, disable_umount: bool) -> Result<()> {
        if self.mode == "erofs_staging" {
            let image_path = self
                .backing_image
                .as_ref()
                .context("EROFS backing image path missing")?;

            utils::create_erofs_image(&self.mount_point, image_path)
                .context("Failed to pack EROFS image")?;

            umount(&self.mount_point, UnmountFlags::DETACH)
                .context("Failed to unmount staging tmpfs")?;

            utils::mount_erofs_image(image_path, &self.mount_point)
                .context("Failed to mount finalized EROFS image")?;

            if let Err(e) = mount_change(&self.mount_point, MountPropagationFlags::PRIVATE) {
                log::warn!("Failed to make EROFS storage private: {}", e);
            }

            #[cfg(any(target_os = "linux", target_os = "android"))]
            if !disable_umount {
                let _ = send_umountable(&self.mount_point);
            }

            self.mode = "erofs".to_string();
        }

        Ok(())
    }
}

#[derive(Serialize)]
struct StorageStatus {
    #[serde(rename = "type")]
    mode: String,
    mount_point: String,
    usage_percent: u8,
    total_size: u64,
    used_size: u64,
    supported_modes: Vec<String>,
}

pub fn get_usage(path: &Path) -> (u64, u64, u8) {
    if let Ok(stat) = rustix::fs::statvfs(path) {
        let total = stat.f_blocks * stat.f_frsize;

        let free = stat.f_bfree * stat.f_frsize;

        let used = total - free;

        let percent = (used * 100).checked_div(total).unwrap_or(0) as u8;

        (total, used, percent)
    } else {
        (0, 0, 0)
    }
}

fn calculate_total_size(path: &Path) -> Result<u64> {
    let mut total_size = 0;
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let file_type = entry.file_type()?;
            if file_type.is_file() {
                total_size += entry.metadata()?.len();
            } else if file_type.is_dir() {
                total_size += calculate_total_size(&entry.path())?;
            }
        }
    }
    Ok(total_size)
}

fn check_image<P>(img: P) -> Result<()>
where
    P: AsRef<Path>,
{
    let path = img.as_ref();
    let path_str = path.to_str().context("Invalid path string")?;
    let result = Command::new("e2fsck")
        .args(["-yf", path_str])
        .status()
        .with_context(|| format!("Failed to exec e2fsck {}", path.display()))?;
    let code = result.code();

    log::info!("e2fsck exit code: {}", code.unwrap_or(-1));
    Ok(())
}

pub fn setup(
    mnt_base: &Path,
    mode: &OverlayMode,
    mount_source: &str,
) -> Result<StorageHandle> {
    log::info!(">> Preparing transient workspace in [{:?}] mode...", mode);

    // 确保挂载点父目录存在
    if let Some(parent) = mnt_base.parent() {
        utils::ensure_dir_exists(parent)?;
    }
    utils::ensure_dir_exists(mnt_base)?;

    match mode {
        OverlayMode::Tmpfs => {
            // 模式 1: 纯内存 TMPFS
            rustix::mount::mount(
                "tmpfs", mnt_base, "tmpfs",
                rustix::mount::MountFlags::empty(), None,
            ).context("Failed to mount tmpfs workspace")?;
        }
        OverlayMode::Ext4 => {
            // 模式 2: 瞬时 EXT4 (Mountify 风格)
            let img_path = mnt_base.parent().unwrap().join("meta_temp.img");
            
            // 创建 2GB Sparse 镜像
            let cmd = std::process::Command::new("dd")
                .args(["if=/dev/zero", &format!("of={}", img_path.display()), "bs=1M", "count=0", "seek=2048"])
                .status()?;
            
            if !cmd.success() { bail!("Failed to create sparse image"); }

            // 格式化 (禁用日志以提升性能)
            std::process::Command::new("mkfs.ext4")
                .args(["-O", "^has_journal", "-F", &img_path.to_string_lossy()])
                .status()?;

            // 挂载
            std::process::Command::new("mount")
                .args(["-t", "ext4", "-o", "loop,rw", &img_path.to_string_lossy()])
                .arg(mnt_base)
                .status()?;

            // 核心步骤：挂载成功后立即删除镜像文件 (Unlink)
            // 文件在磁盘上消失，但内核通过 loop 设备保留引用，重启自动释放
            let _ = std::fs::remove_file(&img_path);
        }
        OverlayMode::Erofs => {
            // 模式 3: 瞬时 EROFS (需要先在临时目录准备内容，这里演示挂载逻辑)
            // 实际操作中需要先同步到 tmpfs，mkfs.erofs 镜像，挂载后 unlink
            log::warn!("EROFS transient mode requires pre-staged content.");
            // ... 类似 Ext4 的 unlink 逻辑 ...
        }
    }

    Ok(StorageHandle {
        mount_point: mnt_base.to_path_buf(),
        mode: mode.to_string(),
    })
}

fn try_setup_tmpfs(target: &Path, mount_source: &str) -> Result<bool> {
    if utils::mount_tmpfs(target, mount_source).is_ok() {
        if utils::is_overlay_xattr_supported(target) {
            log::info!("Tmpfs mounted and supports xattrs (CONFIG_TMPFS_XATTR=y).");
            return Ok(true);
        } else {
            log::warn!("Tmpfs mounted but XATTRs (trusted.*) are NOT supported.");
            log::warn!(">> Your kernel likely lacks CONFIG_TMPFS_XATTR=y.");
            log::warn!(">> Falling back to legacy Ext4 image mode.");
            let _ = umount(target, UnmountFlags::DETACH);
        }
    }

    Ok(false)
}

fn setup_ext4_image(target: &Path, img_path: &Path, moduledir: &Path) -> Result<StorageHandle> {
    if !img_path.exists() || check_image(img_path).is_err() {
        log::info!("Modules image missing or corrupted. Fallback to creation.");

        if img_path.exists()
            && let Err(e) = fs::remove_file(img_path)
        {
            log::warn!("Failed to remove old image: {}", e);
        }

        log::info!("- Preparing image");

        let total_size = calculate_total_size(moduledir)?;
        log::info!(
            "Total size of files in '{}': {} bytes",
            moduledir.display(),
            total_size,
        );

        let grow_size = 128 * 1024 * 1024 + total_size;

        fs::File::create(img_path)
            .context("Failed to create ext4 image file")?
            .set_len(grow_size)
            .context("Failed to extend ext4 image")?;

        let result = Command::new("mkfs.ext4")
            .arg("-b")
            .arg("1024")
            .arg(img_path)
            .stdout(std::process::Stdio::piped())
            .output()?;

        ensure!(
            result.status.success(),
            "Failed to format ext4 image: {}",
            String::from_utf8(result.stderr)?
        );

        log::info!("Checking Image");
        check_image(img_path)?;
    }

    utils::lsetfilecon(img_path, "u:object_r:ksu_file:s0").ok();

    log::info!("- Mounting image");

    utils::ensure_dir_exists(target)?;
    if overlay_utils::AutoMountExt4::try_new(img_path, target, false).is_err() {
        if utils::repair_image(img_path).is_ok() {
            overlay_utils::AutoMountExt4::try_new(img_path, target, false)
                .context("Failed to mount modules.img after repair")
                .map(|_| ())?;
        } else {
            bail!("Failed to repair modules.img");
        }
    }

    log::info!("mounted {} to {}", img_path.display(), target.display());

    Ok(StorageHandle {
        mount_point: target.to_path_buf(),
        mode: "ext4".to_string(),
        backing_image: Some(img_path.to_path_buf()),
    })
}

#[allow(dead_code)]
pub fn finalize_storage_permissions(target: &Path) {
    if let Err(e) = rustix::fs::chmod(target, Mode::from(0o755)) {
        log::warn!("Failed to chmod storage root: {}", e);
    }

    if let Err(e) = rustix::fs::chown(
        target,
        Some(rustix::fs::Uid::from_raw(0)),
        Some(rustix::fs::Gid::from_raw(0)),
    ) {
        log::warn!("Failed to chown storage root: {}", e);
    }

    if let Err(e) = utils::lsetfilecon(target, DEFAULT_SELINUX_CONTEXT) {
        log::warn!("Failed to set SELinux context: {}", e);
    }
}

pub fn print_status() -> Result<()> {
    let state = RuntimeState::load().ok();
    let fallback_mnt = crate::conf::config::Config::load_default()
        .map(|c| c.hybrid_mnt_dir)
        .unwrap_or_else(|_| crate::defs::DEFAULT_HYBRID_MNT_DIR.to_string());
    let (mnt_base, expected_mode) = if let Some(ref s) = state {
        (s.mount_point.clone(), s.storage_mode.clone())
    } else {
        (PathBuf::from(fallback_mnt), "unknown".to_string())
    };

    let mut mode = "unknown".to_string();

    let mut total = 0;

    let mut used = 0;

    let mut percent = 0;

    if utils::is_mounted(&mnt_base)
        && let Ok(stat) = rustix::fs::statvfs(&mnt_base)
    {
        mode = if expected_mode != "unknown" {
            expected_mode
        } else {
            "active".to_string()
        };

        total = stat.f_blocks * stat.f_frsize;

        let free = stat.f_bfree * stat.f_frsize;

        used = total - free;

        percent = (used * 100).checked_div(total).unwrap_or(0) as u8;
    }

    let mut supported_modes = vec!["ext4".to_string(), "erofs".to_string()];
    let check_dir = Path::new("/data/local/tmp/.mh_xattr_chk");
    if utils::mount_tmpfs(check_dir, "mh_check").is_ok() {
        if utils::is_overlay_xattr_supported(check_dir) {
            supported_modes.insert(0, "tmpfs".to_string());
        }
        let _ = umount(check_dir, UnmountFlags::DETACH);
        let _ = fs::remove_dir(check_dir);
    }

    let status = StorageStatus {
        mode,
        mount_point: mnt_base.to_string_lossy().to_string(),
        usage_percent: percent,
        total_size: total,
        used_size: used,
        supported_modes,
    };

    println!("{}", serde_json::to_string(&status)?);

    Ok(())
}
