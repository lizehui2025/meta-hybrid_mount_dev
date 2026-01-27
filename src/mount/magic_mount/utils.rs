// Copyright 2026 Hybrid Mount Authors
// SPDX-License-Identifier: GPL-3.0-or-later

use std::{
    collections::HashSet,
    fs::{self, DirEntry, Metadata, create_dir, create_dir_all, read_link},
    os::unix::fs::{MetadataExt, symlink},
    path::{Path, PathBuf},
};

use anyhow::{Result, bail, Context};
use rustix::{
    fs::{Gid, Mode, Uid, chmod, chown},
    mount::mount_bind,
};

use crate::{
    defs::{DISABLE_FILE_NAME, REMOVE_FILE_NAME, SKIP_MOUNT_FILE_NAME},
    mount::node::Node,
    utils::{lgetfilecon, lsetfilecon, validate_module_id, detect_all_partitions},
};

/// 获取元数据和路径参考
fn metadata_path<P>(path: P, node: &Node) -> Result<(Metadata, PathBuf)>
where
    P: AsRef<Path>,
{
    let path = path.as_ref();
    if path.exists() {
        Ok((path.metadata()?, path.to_path_buf()))
    } else if let Some(module_path) = &node.module_path {
        Ok((module_path.metadata()?, module_path.clone()))
    } else {
        bail!("cannot mount root dir {}!", path.display());
    }
}

/// 构建 tmpfs 骨架并同步 SELinux 标签
/// 这是防止字体模块等关键组件 Bootloop 的核心逻辑
pub fn tmpfs_skeleton<P>(path: P, work_dir_path: P, node: &Node) -> Result<()>
where
    P: AsRef<Path>,
{
    let path = path.as_ref();
    let work_dir_path = work_dir_path.as_ref();
    
    log::debug!("Building tmpfs skeleton for {}", path.display());

    create_dir_all(work_dir_path)?;

    // 确定参考路径：优先使用系统真实路径获取权限和标签
    let ref_path = if path.exists() {
        path.to_path_buf()
    } else if let Some(mod_path) = &node.module_path {
        mod_path.clone()
    } else {
        bail!("Critical: No reference path for directory {}", path.display());
    };

    let metadata = ref_path.metadata()?;
    
    // 同步 Unix 权限
    chmod(work_dir_path, Mode::from_raw_mode(metadata.mode()))?;
    chown(work_dir_path, Some(Uid::from_raw(metadata.uid())), Some(Gid::from_raw(metadata.gid())))?;

    // 关键：同步 SELinux 标签
    // 确保 tmpfs 目录不被标记为 ksu_file，否则系统进程无法读取
    if let Ok(ctx) = lgetfilecon(&ref_path) {
        lsetfilecon(work_dir_path, &ctx).ok();
    }

    Ok(())
}

/// 递归镜像系统文件到 tmpfs 视图
pub fn mount_mirror<P>(path: P, work_dir_path: P, entry: &DirEntry) -> Result<()>
where
    P: AsRef<Path>,
{
    let src = path.as_ref().join(entry.file_name());
    let dst = work_dir_path.as_ref().join(entry.file_name());
    let file_type = entry.file_type()?;

    if file_type.is_file() {
        // 创建空文件作为挂载点并执行绑定挂载
        fs::File::create(&dst)?;
        mount_bind(&src, &dst)?;
        
        // 同步标签：防止被镜像的系统文件因 tmpfs 默认标签而被拒绝访问
        if let Ok(ctx) = lgetfilecon(&src) {
            lsetfilecon(&dst, &ctx).ok();
        }
    } else if file_type.is_dir() {
        create_dir(&dst)?;
        let metadata = entry.metadata()?;
        chmod(&dst, Mode::from_raw_mode(metadata.mode()))?;
        chown(&dst, Some(Uid::from_raw(metadata.uid())), Some(Gid::from_raw(metadata.gid())))?;
        
        if let Ok(ctx) = lgetfilecon(&src) {
            lsetfilecon(&dst, &ctx).ok();
        }

        // 递归镜像子目录
        for sub_entry in src.read_dir()?.flatten() {
            mount_mirror(&src, &dst, &sub_entry)?;
        }
    } else if file_type.is_symlink() {
        clone_symlink(&src, &dst)?;
    }

    Ok(())
}

/// 动态收集模块文件并构建节点树
/// 移除硬编码，改用 detect_all_partitions 进行动态探测
pub fn collect_module_files(
    module_dir: &Path,
    user_extra_partitions: &[String],
    need_id: HashSet<String>,
) -> Result<Option<Node>> {
    let mut root = Node::new_root("");
    let mut system = Node::new_root("system");
    let mut has_file = HashSet::new();

    // 1. 动态获取系统当前所有分区列表
    let mut all_partitions = detect_all_partitions().unwrap_or_default();
    all_partitions.extend(user_extra_partitions.iter().cloned());
    all_partitions.sort();
    all_partitions.dedup();

    log::debug!("Dynamic partition detection result: {:?}", all_partitions);

    for entry in module_dir.read_dir()?.flatten() {
        if !entry.file_type()?.is_dir() { continue; }

        let id = entry.file_name().to_string_lossy().to_string();
        if !need_id.contains(&id) { continue; }

        let prop = entry.path().join("module.prop");
        if !prop.exists() { continue; }

        // 排除禁用、移除、跳过挂载的模块
        if entry.path().join(DISABLE_FILE_NAME).exists()
            || entry.path().join(REMOVE_FILE_NAME).exists()
            || entry.path().join(SKIP_MOUNT_FILE_NAME).exists()
        {
            continue;
        }

        // 2. 检查模块是否修改了任何探测到的分区
        let mut modified = false;
        for p in &all_partitions {
            if entry.path().join(p).is_dir() {
                modified = true;
                break;
            }
        }

        if !modified { continue; }

        // 3. 将模块修改的文件收集到虚拟 system 节点中
        for p in &all_partitions {
            let part_path = entry.path().join(p);
            if !part_path.exists() { continue; }
            has_file.insert(system.collect_module_files(part_path)?);
        }
    }

    if has_file.contains(&true) {
        // 4. 将独立物理分区从 system 节点移动到 root 节点
        for partition in all_partitions {
            if partition == "system" { continue; }

            let path_of_root = Path::new("/").join(&partition);
            let path_of_system = Path::new("/system").join(&partition);

            // 如果该分区挂载在根目录，且在 /system 下是软链接或不存在，则它是一个独立分区
            if path_of_root.is_dir() && (!path_of_system.exists() || path_of_system.is_symlink()) {
                if let Some(node) = system.children.remove(&partition) {
                    log::debug!("Detaching partition '{}' from system and attaching to root", partition);
                    root.children.insert(partition, node);
                }
            }
        }

        root.children.insert("system".to_string(), system);
        Ok(Some(root))
    } else {
        Ok(None)
    }
}

/// 镜像软链接并还原其标签
pub fn clone_symlink<S>(src: S, dst: S) -> Result<()>
where
    S: AsRef<Path>,
{
    let src_path = src.as_ref();
    let dst_path = dst.as_ref();
    
    let link_target = read_link(src_path)?;
    symlink(&link_target, dst_path)?;
    
    // 软链接也需要同步 SELinux 上下文以满足系统的严格检查
    if let Ok(ctx) = lgetfilecon(src_path) {
        lsetfilecon(dst_path, &ctx).ok();
    }
    Ok(())
}
