// Copyright 2026 Hybrid Mount Developers
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    ffi::CString,
    os::fd::AsFd,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use procfs::process::Process;
use rustix::{
    fs::CWD,
    mount::{
        FsMountFlags, FsOpenFlags, MountAttrFlags, MountFlags, MoveMountFlags, fsconfig_create,
        fsconfig_set_string, fsmount, fsopen, mount, move_mount,
    },
};

use crate::{mount::overlayfs::utils::umount_dir, try_umount::send_umountable};

/// Overlay 配置封装，用于在函数间安全传递挂载参数
struct OverlayOptions<'a> {
    lower_dirs: &'a [String],
    lowest: &'a str,
    upper_dir: Option<PathBuf>,
    work_dir: Option<PathBuf>,
    mount_source: &'a str,
}

/// 核心函数：以原子化特征执行 OverlayFS 挂载
/// 逻辑：New API (fsopen) -> fsmount -> move_mount => Fallback to mount()
pub fn mount_overlayfs(
    lower_dirs: &[String],
    lowest: &str,
    upperdir: Option<PathBuf>,
    workdir: Option<PathBuf>,
    dest: impl AsRef<Path>,
    mount_source: &str,
) -> Result<()> {
    let dest_path = dest.as_ref();
    
    // 构建 lowerdir 字符串：将模块层与底层(lowest)组合
    let lowerdir_config = lower_dirs
        .iter()
        .map(|s| s.as_ref())
        .chain(std::iter::once(lowest))
        .collect::<Vec<_>>()
        .join(":");

    log::debug!("Attempting atomic mount on {:?}", dest_path);

    // 预处理路径：转义逗号以兼容传统 mount 和新 API 的潜在特殊字符处理
    let safe_lower = lowerdir_config.replace(',', "\\,");
    let up_s = upperdir.as_ref().filter(|p| p.exists()).map(|p| p.to_string_lossy().to_string());
    let wk_s = workdir.as_ref().filter(|p| p.exists()).map(|p| p.to_string_lossy().to_string());

    // --- 阶段 A: 尝试最新 API (原子化特征) ---
    let new_api_result = (|| -> Result<()> {
        // 1. fsopen: 创建文件系统上下文，不影响实际目录
        let fs = fsopen("overlay", FsOpenFlags::FSOPEN_CLOEXEC)
            .context("fsopen failed")?;
        let fs_fd = fs.as_fd();

        // 2. fsconfig: 逐步配置参数。如果其中一步失败，上下文会被销毁
        fsconfig_set_string(fs_fd, "lowerdir", &lowerdir_config)?;
        if let (Some(u), Some(w)) = (&up_s, &wk_s) {
            fsconfig_set_string(fs_fd, "upperdir", u)?;
            fsconfig_set_string(fs_fd, "workdir", w)?;
        }
        fsconfig_set_string(fs_fd, "source", mount_source)?;
        
        // 3. 提交配置并创建挂载对象
        fsconfig_create(fs_fd)?;
        let mnt = fsmount(fs_fd, FsMountFlags::FSMOUNT_CLOEXEC, MountAttrFlags::empty())?;
        
        // 4. move_mount: 将挂载点挂载到目标路径 (真正的原子切换点)
        move_mount(
            mnt.as_fd(),
            "",
            CWD,
            dest_path,
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
        ).context("move_mount failed")?;
        
        Ok(())
    })();

    // --- 阶段 B: 退回机制 (Fallback) ---
    if let Err(e) = new_api_result {
        log::warn!("New API mount failed ({:#}), falling back to legacy mount...", e);
        
        let mut data = format!("lowerdir={safe_lower}");
        if let (Some(u), Some(w)) = (up_s, wk_s) {
            data.push_str(&format!(
                ",upperdir={},workdir={}",
                u.replace(',', "\\,"),
                w.replace(',', "\\,")
            ));
        }

        mount(
            mount_source,
            dest_path,
            "overlay",
            MountFlags::empty(),
            Some(CString::new(data)?.as_c_str()),
        ).context("Legacy mount fallback failed")?;
    }

    log::info!("OverlayFS successfully mounted on {:?}", dest_path);
    Ok(())
}

/// 绑定挂载：使用 open_tree 提供更好的挂载树一致性
pub fn bind_mount(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
    let from = from.as_ref();
    let to = to.as_ref();

    use rustix::mount::{OpenTreeFlags, open_tree};
    
    // 优先使用 open_tree + move_mount，这允许我们在挂载前克隆挂载树
    let result = (|| {
        let tree = open_tree(
            CWD,
            from,
            OpenTreeFlags::OPEN_TREE_CLOEXEC | OpenTreeFlags::OPEN_TREE_CLONE | OpenTreeFlags::AT_RECURSIVE,
        )?;
        move_mount(
            tree.as_fd(),
            "",
            CWD,
            to,
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
        )
    })();

    if result.is_err() {
        log::debug!("open_tree failed, falling back to traditional bind mount");
        mount(from, to, "", MountFlags::BIND | MountFlags::REC, None)
            .context("Traditional bind mount failed")?;
    }
    
    Ok(())
}

/// 处理嵌套子挂载点
fn mount_overlay_child(
    mount_point: &str,
    relative: &str,
    module_roots: &[String],
    stock_root: &str,
    mount_source: &str,
) -> Result<()> {
    // 筛选出确实包含该子路径的模块
    let sub_lowers: Vec<String> = module_roots
        .iter()
        .map(|r| Path::new(r).join(relative.trim_start_matches('/')).to_string_lossy().to_string())
        .filter(|p| Path::new(p).is_dir())
        .collect();

    if sub_lowers.is_empty() {
        // 如果没有任何模块涉及此路径，使用 bind_mount 保持物理透传
        return bind_mount(stock_root, mount_point);
    }

    // 执行嵌套挂载
    mount_overlayfs(&sub_lowers, stock_root, None, None, mount_point, mount_source)?;
    let _ = send_umountable(mount_point);
    Ok(())
}

/// 挂载主入口：执行根挂载并原子化处理子挂载保护
pub fn mount_overlay(
    root: &String,
    module_roots: &Vec<String>,
    workdir: Option<PathBuf>,
    upperdir: Option<PathBuf>,
    mount_source: &str,
) -> Result<()> {
    log::info!("Starting robust overlay sequence for {}", root);

    // 1. 扫描当前 root 下的所有活跃挂载点（如 /vendor/dsp）
    let mounts = Process::myself()?.mountinfo().context("Failed to access mountinfo")?;
    let mut child_mounts: Vec<String> = mounts.0.iter()
        .filter(|m| m.mount_point.starts_with(root) && m.mount_point.to_string_lossy() != *root)
        .map(|m| m.mount_point.to_string_lossy().to_string())
        .collect();
    child_mounts.sort(); // 按拓扑顺序排序

    // 2. 执行根 Overlay 挂载
    mount_overlayfs(module_roots, root, upperdir, workdir, root, mount_source)
        .with_context(|| format!("Failed to establish root overlay at {root}"))?;

    // 3. 处理子挂载点的“透传”或“重 Overlay”
    // 参考了 Mountify 的层级保护逻辑，确保分区不会因为 Overlay 而“消失”
    for mnt in child_mounts {
        let rel = mnt.replacen(root, "", 1);
        if rel.is_empty() { continue; }

        let stock = format!("{root}{rel}"); 
        if let Err(e) = mount_overlay_child(&mnt, &rel, module_roots, &stock, mount_source) {
            log::error!("Critical error during child mount [{}]: {:#}", mnt, e);
            // 原子性补救：如果子挂载失败，尝试撤销根挂载以防止系统不稳定
            let _ = umount_dir(root);
            bail!("Consistency failure: could not restore child mounts under {}", root);
        }
    }

    let _ = send_umountable(root);
    Ok(())
}
