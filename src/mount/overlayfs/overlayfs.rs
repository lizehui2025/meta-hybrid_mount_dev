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

pub fn mount_overlayfs(
    lower_dirs: &[String],
    lowest: &str,
    upperdir: Option<PathBuf>,
    workdir: Option<PathBuf>,
    dest: impl AsRef<Path>,
    mount_source: &str,
) -> Result<()> {
    let lowerdir_config = lower_dirs
        .iter()
        .map(|s| s.as_ref())
        .chain(std::iter::once(lowest))
        .collect::<Vec<_>>()
        .join(":");
    log::info!(
        "mount overlayfs on {:?}, lowerdir={}, upperdir={:?}, workdir={:?}, source={}",
        dest.as_ref(),
        lowerdir_config,
        upperdir,
        workdir,
        mount_source
    );

    let upperdir_s = upperdir
        .as_ref()
        .filter(|up| up.exists())
        .map(|e| e.display().to_string());
    let workdir_s = workdir
        .as_ref()
        .filter(|wd| wd.exists())
        .map(|e| e.display().to_string());

    let result = (|| {
        let fs = fsopen("overlay", FsOpenFlags::FSOPEN_CLOEXEC)?;
        let fs = fs.as_fd();
        fsconfig_set_string(fs, "lowerdir", &lowerdir_config)?;
        if let (Some(upperdir), Some(workdir)) = (&upperdir_s, &workdir_s) {
            fsconfig_set_string(fs, "upperdir", upperdir)?;
            fsconfig_set_string(fs, "workdir", workdir)?;
        }
        fsconfig_set_string(fs, "source", mount_source)?;
        fsconfig_create(fs)?;
        let mount = fsmount(fs, FsMountFlags::FSMOUNT_CLOEXEC, MountAttrFlags::empty())?;
        move_mount(
            mount.as_fd(),
            "",
            CWD,
            dest.as_ref(),
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
        )
    })();

    if let Err(e) = result {
        log::warn!("fsopen mount failed: {:#}, fallback to mount", e);
        // Escape commas in paths
        let safe_lower = lowerdir_config.replace(',', "\\,");
        let mut data = format!("lowerdir={safe_lower}");

        if let (Some(upperdir), Some(workdir)) = (upperdir_s, workdir_s) {
            data = format!(
                "{data},upperdir={},workdir={}",
                upperdir.replace(',', "\\,"),
                workdir.replace(',', "\\,")
            );
        }
        mount(
            mount_source,
            dest.as_ref(),
            "overlay",
            MountFlags::empty(),
            Some(CString::new(data)?.as_c_str()),
        )?;
    }
    Ok(())
}

pub fn mount_overlay_with_protection(
    root: &Path,
    module_roots: &[String],
    upper: Option<PathBuf>,
    work: Option<PathBuf>,
    mount_source: &str,
) -> Result<()> {
    // 1. 获取当前系统的挂载信息，防止覆盖已有的挂载点
    let mounts = Process::myself()?.mountinfo().context("Failed to get mountinfo")?;
    let mut active_mounts: Vec<_> = mounts.0.iter()
        .filter(|m| m.mount_point.starts_with(root) && m.mount_point != root)
        .map(|m| m.mount_point.clone())
        .collect();
    active_mounts.sort();

    // 2. 挂载根路径
    let root_ctx = OverlayContext {
        target: root,
        lower_dirs: module_roots.to_vec(),
        upper_dir: upper,
        work_dir: work,
        mount_source,
    };
    do_mount(&root_ctx).with_context(|| format!("Failed to mount root overlay on {:?}", root))?;

    // 3. 处理子挂载点（Shadowing）
    // 效仿 Mountify 保护已有的分区挂载
    for mount_point in active_mounts {
        let relative = mount_point.strip_prefix(root).unwrap_or(Path::new(""));
        
        // 只有当模块中确实包含修改时，才进行嵌套 Overlay
        let has_mod = module_roots.iter().any(|r| Path::new(r).join(relative).exists());
        
        if has_mod {
            log::info!("Nested mount detected for {:?}, applying sub-overlay", mount_point);
            let sub_lower: Vec<String> = module_roots.iter()
                .map(|r| Path::new(r).join(relative).to_string_lossy().to_string())
                .filter(|p| Path::new(p).is_dir())
                .collect();

            if !sub_lower.is_empty() {
                let sub_ctx = OverlayContext {
                    target: &mount_point,
                    lower_dirs: sub_lower,
                    upper_dir: None, // 子挂载通常不设 upperdir 以保持只读一致性
                    work_dir: None,
                    mount_source,
                };
                let _ = do_mount(&sub_ctx);
            }
        }
    }

    Ok(())
}

/// OverlayFS 挂载上下文
/// 效仿 Mountify 的配置，管理挂载的源和目标
pub struct OverlayContext<'a> {
    pub target: &'a Path,
    pub lower_dirs: Vec<String>,
    pub upper_dir: Option<PathBuf>,
    pub work_dir: Option<PathBuf>,
    pub mount_source: &'a str,
}

/// 核心：执行底层的 OverlayFS 挂载
/// 优先使用新的 Mount API (fsopen)，失败后回退到传统 mount
pub fn do_mount(ctx: &OverlayContext) -> Result<()> {
    let lowerdir_config = ctx.lower_dirs.join(":");
    
    log::info!(
        "Mounting OverlayFS: target={:?}, lowerdirs={} layers",
        ctx.target,
        ctx.lower_dirs.len()
    );

    // 预备参数字符串（用于回退模式）
    let safe_lower = lowerdir_config.replace(',', "\\,");
    let mut data = format!("lowerdir={}", safe_lower);

    // 处理 Upper 和 Work 目录 (如果开启了存储后端)
    let (up_s, wk_s) = (
        ctx.upper_dir.as_ref().map(|p| p.to_string_lossy().to_string()),
        ctx.work_dir.as_ref().map(|p| p.to_string_lossy().to_string()),
    );

    // 尝试使用 fsopen (Linux 5.2+)
    let result = (|| {
        let fs = fsopen("overlay", FsOpenFlags::FSOPEN_CLOEXEC)?;
        let fs = fs.as_fd();
        fsconfig_set_string(fs, "lowerdir", &lowerdir_config)?;
        if let (Some(u), Some(w)) = (&up_s, &wk_s) {
            fsconfig_set_string(fs, "upperdir", u)?;
            fsconfig_set_string(fs, "workdir", w)?;
        }
        fsconfig_set_string(fs, "source", ctx.mount_source)?;
        fsconfig_create(fs)?;
        let mount_fd = fsmount(fs, FsMountFlags::FSMOUNT_CLOEXEC, MountAttrFlags::empty())?;
        move_mount(
            mount_fd.as_fd(),
            "",
            CWD,
            ctx.target,
            MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
        )
    })();

    if let Err(e) = result {
        log::warn!("New Mount API failed ({:#}), falling back to traditional mount", e);
        
        if let (Some(u), Some(w)) = (up_s, wk_s) {
            data.push_str(&format!(",upperdir={},workdir={}", u.replace(',', "\\,"), w.replace(',', "\\,")));
        }

        mount(
            ctx.mount_source,
            ctx.target,
            "overlay",
            MountFlags::empty(),
            Some(CString::new(data)?.as_c_str()),
        ).context("Traditional mount failed")?;
    }

    // 注册卸载任务
    let _ = send_umountable(ctx.target.to_string_lossy().as_ref());
    Ok(())
}

pub fn bind_mount(from: impl AsRef<Path>, to: impl AsRef<Path>) -> Result<()> {
    log::info!(
        "bind mount {} -> {}",
        from.as_ref().display(),
        to.as_ref().display()
    );
    use rustix::mount::{OpenTreeFlags, open_tree};
    match open_tree(
        CWD,
        from.as_ref(),
        OpenTreeFlags::OPEN_TREE_CLOEXEC
            | OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::AT_RECURSIVE,
    ) {
        Result::Ok(tree) => {
            move_mount(
                tree.as_fd(),
                "",
                CWD,
                to.as_ref(),
                MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
            )?;
        }
        _ => {
            mount(
                from.as_ref(),
                to.as_ref(),
                "",
                MountFlags::BIND | MountFlags::REC,
                None,
            )?;
        }
    }
    Ok(())
}

fn mount_overlay_child(
    mount_point: &str,
    relative: &String,
    module_roots: &Vec<String>,
    stock_root: &String,
    mount_source: &str,
) -> Result<()> {
    if !module_roots
        .iter()
        .any(|lower| Path::new(&format!("{lower}{relative}")).exists())
    {
        return bind_mount(stock_root, mount_point);
    }
    if !Path::new(&stock_root).is_dir() {
        return Ok(());
    }
    let mut lower_dirs: Vec<String> = vec![];
    for lower in module_roots {
        let lower_dir = format!("{lower}{relative}");
        let path = Path::new(&lower_dir);
        if path.is_dir() {
            lower_dirs.push(lower_dir);
        } else if path.exists() {
            return Ok(());
        }
    }
    if lower_dirs.is_empty() {
        return Ok(());
    }
    if let Err(e) = mount_overlayfs(
        &lower_dirs,
        stock_root,
        None,
        None,
        mount_point,
        mount_source,
    ) {
        log::warn!("failed: {:#}, fallback to bind mount", e);
        bind_mount(stock_root, mount_point)?;
    }
    let _ = send_umountable(mount_point);
    Ok(())
}

pub fn mount_overlay(
    root: &String,
    module_roots: &Vec<String>,
    workdir: Option<PathBuf>,
    upperdir: Option<PathBuf>,
    mount_source: &str,
) -> Result<()> {
    log::info!("mount overlay for {}", root);
    std::env::set_current_dir(root).with_context(|| format!("failed to chdir to {root}"))?;
    let stock_root = ".";

    let mounts = Process::myself()?
        .mountinfo()
        .with_context(|| "get mountinfo")?;
    let mut mount_seq = mounts
        .0
        .iter()
        .filter(|m| {
            m.mount_point.starts_with(root) && !Path::new(&root).starts_with(&m.mount_point)
        })
        .map(|m| m.mount_point.to_str())
        .collect::<Vec<_>>();
    mount_seq.sort();
    mount_seq.dedup();

    mount_overlayfs(module_roots, root, upperdir, workdir, root, mount_source)
        .with_context(|| "mount overlayfs for root failed")?;
    for mount_point in mount_seq.iter() {
        let Some(mount_point) = mount_point else {
            continue;
        };
        let relative = mount_point.replacen(root, "", 1);
        let stock_root: String = format!("{stock_root}{relative}");
        if !Path::new(&stock_root).exists() {
            continue;
        }
        if let Err(e) = mount_overlay_child(
            mount_point,
            &relative,
            module_roots,
            &stock_root,
            mount_source,
        ) {
            log::warn!(
                "failed to mount overlay for child {}: {:#}, revert",
                mount_point,
                e
            );
            umount_dir(root).with_context(|| format!("failed to revert {root}"))?;
            bail!(e);
        }
    }
    Ok(())
}
