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
    let lowerdir_config = lower_dirs.iter()
        .map(|s| s.as_ref())
        .chain(std::iter::once(lowest))
        .collect::<Vec<_>>()
        .join(":");

    let up_s = upperdir.as_ref().filter(|p| p.exists()).map(|p| p.to_string_lossy().to_string());
    let wk_s = workdir.as_ref().filter(|p| p.exists()).map(|p| p.to_string_lossy().to_string());

    // 尝试 New API
    let res = (|| -> Result<()> {
        let fs = fsopen("overlay", FsOpenFlags::FSOPEN_CLOEXEC)?;
        let fd = fs.as_fd();
        fsconfig_set_string(fd, "lowerdir", &lowerdir_config)?;
        if let (Some(u), Some(w)) = (&up_s, &wk_s) {
            fsconfig_set_string(fd, "upperdir", u)?;
            fsconfig_set_string(fd, "workdir", w)?;
        }
        fsconfig_set_string(fd, "source", mount_source)?;
        fsconfig_create(fd)?;
        let mnt = fsmount(fd, FsMountFlags::FSMOUNT_CLOEXEC, MountAttrFlags::empty())?;
        move_mount(mnt.as_fd(), "", CWD, dest_path, MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH)?;
        Ok(())
    })();

    if res.is_err() {
        // Fallback
        let mut data = format!("lowerdir={}", lowerdir_config.replace(',', "\\,"));
        if let (Some(u), Some(w)) = (up_s, wk_s) {
            data.push_str(&format!(",upperdir={},workdir={}", u.replace(',', "\\,"), w.replace(',', "\\,")));
        }
        mount(mount_source, dest_path, "overlay", MountFlags::empty(), Some(CString::new(data)?.as_c_str()))?;
    }
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
    // 扫描并保护子挂载点
    let mounts = Process::myself()?.mountinfo()?;
    let mut children: Vec<_> = mounts.0.iter()
        .filter(|m| m.mount_point.starts_with(root) && m.mount_point.to_string_lossy() != *root)
        .map(|m| m.mount_point.to_string_lossy().to_string())
        .collect();
    children.sort();

    mount_overlayfs(module_roots, root, upperdir, workdir, root, mount_source)?;

    for mnt in children {
        let rel = mnt.replacen(root, "", 1);
        let sub_lowers: Vec<_> = module_roots.iter()
            .map(|r| Path::new(r).join(rel.trim_start_matches('/')).to_string_lossy().to_string())
            .filter(|p| Path::new(p).is_dir())
            .collect();

        if !sub_lowers.is_empty() {
            let _ = mount_overlayfs(&sub_lowers, &mnt, None, None, &mnt, mount_source);
        }
    }
    let _ = send_umountable(root);
    Ok(())
}
